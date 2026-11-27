//! Page settlement heuristics, network activity tracking, and DOM stability.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use flashbrowse_cdp::protocol::{dom, input, runtime};
use flashbrowse_cdp::EventEmitter;
use serde_json::Value;

use crate::actions::box_center;
use crate::driver::FrameSession;
use crate::tree::{ActionMap, SemanticNode};

use super::error::CoreError;
use super::BrowserSession;

pub const SETTLE_DOM_STATS_JS: &str = r#"(() => {
  let count = 0;
  let textLen = document.body ? document.body.innerText.length : 0;
  const walk = (root) => {
    const els = root.querySelectorAll('*');
    count += els.length;
    for (const el of els) {
      if (el.shadowRoot) {
        textLen += (el.shadowRoot.textContent || '').length;
        walk(el.shadowRoot);
      }
    }
  };
  walk(document);
  return `${document.readyState}|${count}|${textLen}`;
})()"#;

/// Tracks in-flight network requests so the settle can wait for the page to
/// actually finish loading (an SPA can be quiet in the DOM while its JS
/// bundle is still downloading).
#[derive(Debug, Clone, Default)]
pub(crate) struct NetActivity {
    inflight: Arc<AtomicUsize>,
    last_activity_ms: Arc<AtomicU64>,
    /// Timestamp (ms) when the current batch of requests started - tells a
    /// slow API call (must wait) from a hung socket (must not stall).
    started_at_ms: Arc<AtomicU64>,
}

/// A request in flight for longer than this is considered hung (e.g. a
/// cancelled favicon request that never reports `loadingFinished`).
pub(super) const HUNG_REQUEST_MS: u64 = 1000;

impl NetActivity {
    pub(super) fn new() -> Self {
        Self {
            inflight: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: Arc::new(AtomicU64::new(0)),
            started_at_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub(super) fn start(&self) {
        if self.inflight.fetch_add(1, Ordering::Relaxed) == 0 {
            self.started_at_ms.store(now_ms(), Ordering::Relaxed);
        }
        self.touch();
    }

    pub(super) fn finish(&self) {
        let _ = self
            .inflight
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
        self.touch();
    }

    /// True when the network is effectively idle: nothing in flight (or only
    /// hung requests) and nothing happened for `quiet_ms`.
    pub(super) fn idle(&self, quiet_ms: u64) -> bool {
        if now_ms().saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)) < quiet_ms {
            return false;
        }
        let inflight = self.inflight.load(Ordering::Relaxed);
        if inflight == 0 {
            return true;
        }
        now_ms().saturating_sub(self.started_at_ms.load(Ordering::Relaxed)) >= HUNG_REQUEST_MS
    }

    /// Subscribe to the network events that drive the tracker.
    pub(super) fn attach(&self, events: &EventEmitter) {
        let this = self.clone();
        let mut rx = events.subscribe("Network.requestWillBeSent");
        tokio::spawn(async move {
            while let Ok(v) = rx.recv().await {
                let url = v
                    .get("request")
                    .and_then(|r| r.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                // A favicon request never completes in headless Chrome and
                // would block the settle forever - it cannot change the AX
                // tree, so it is ignored.
                if url.contains("/favicon.ico") {
                    tracing::debug!(url, "ignoring favicon request for network-idle tracking");
                    continue;
                }
                this.start();
            }
        });
        let this = self.clone();
        let mut rx = events.subscribe("Network.loadingFinished");
        tokio::spawn(async move {
            while rx.recv().await.is_ok() {
                this.finish();
            }
        });
        let this = self.clone();
        let mut rx = events.subscribe("Network.loadingFailed");
        tokio::spawn(async move {
            while rx.recv().await.is_ok() {
                this.finish();
            }
        });
    }
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn settle_shadow_enabled() -> bool {
    std::env::var("FLASHBROWSE_SETTLE_SHADOW").is_ok_and(|value| value == "1")
}

/// Legacy settle result kept while the candidate is measured in shadow mode.
pub(super) struct SettleSnapshot {
    pub(super) nodes: Vec<SemanticNode>,
    pub(super) map: ActionMap,
    pub(super) action_sessions: HashMap<u32, FrameSession>,
    pub(super) raw_nodes: Vec<flashbrowse_cdp::protocol::accessibility::AXNode>,
}

/// Cheap content fingerprint of a compressed tree: roles, names and the
/// action-id -> backend-id mapping. Detects semantic changes (role
/// upgrades, text changes) that a node count would miss.
pub(super) fn tree_fingerprint(nodes: &[SemanticNode], map: &ActionMap) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for n in nodes {
        n.role.hash(&mut h);
        n.name.hash(&mut h);
        n.action_id.hash(&mut h);
    }
    // Action map order-independent hash.
    let mut ids: Vec<u32> = map.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        id.hash(&mut h);
        map.get(&id).hash(&mut h);
    }
    h.finish()
}

pub(super) fn build_action_sessions(
    nodes: &[SemanticNode],
    node_sessions: &HashMap<String, FrameSession>,
) -> HashMap<u32, FrameSession> {
    let mut out = HashMap::new();
    if node_sessions.is_empty() {
        return out;
    }
    for n in nodes {
        if let Some(id) = n.action_id {
            if let Some(fs) = node_sessions.get(&n.node_id) {
                out.insert(id, fs.clone());
            }
        }
    }
    out
}

pub(super) fn sha256_hex(input: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut data = input.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    for chunk in data.chunks(64) {
        let mut w = [0u32; 64];
        for (i, bytes) in chunk.chunks(4).take(16).enumerate() {
            w[i] = u32::from_be_bytes(bytes.try_into().expect("four-byte word"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

impl BrowserSession {
    /// `Title`/`URL` header fields via in-page evaluation (best-effort).
    pub(super) async fn refresh_title_url(&mut self) {
        if let Ok(r) = runtime::evaluate(&self.client, &self.session_id, "document.title").await {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.title = s;
                }
            }
        }
        if let Ok(r) = runtime::evaluate(&self.client, &self.session_id, "location.href").await {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.url = s;
                }
            }
        }
    }

    /// Wait for post-action page activity (in-flight network + DOM stability)
    /// instead of a hardcoded sleep.
    pub(super) async fn post_action_settle(&self, max_wait: Duration) {
        let deadline = tokio::time::Instant::now() + max_wait;
        let mut prev_state: Option<String> = None;
        loop {
            let state = self
                .driver
                .evaluate(SETTLE_DOM_STATS_JS)
                .await
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            let net_quiet = self.net.idle(30);
            let ready = state.as_deref().is_some_and(|s| s.starts_with("complete|"));
            if net_quiet && ready && prev_state.is_some() && prev_state == state {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            prev_state = state;
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait until `backend` is visible + stable (actionability check -
    /// Playwright-parity, beats fixed-sleep clicking). Polls box center;
    /// stable = same center twice in a row, visible = center inside viewport.
    pub(super) async fn wait_for_actionable(
        &self,
        dom_session_id: &str,
        backend: i64,
    ) -> Result<(f64, f64), CoreError> {
        let mut prev: Option<(f64, f64)> = None;
        let mut stable_count = 0u8;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Err(e) =
                dom::scroll_into_view_if_needed(&self.client, dom_session_id, backend).await
            {
                tracing::warn!(backend, "scrollIntoViewIfNeeded failed (proceeding): {e}");
            }
            let resp = match dom::get_box_model(&self.client, dom_session_id, backend).await {
                Ok(r) => r,
                Err(e) => {
                    // Transient: node not in the layout tree yet.
                    tracing::warn!(backend, "getBoxModel failed (retrying): {e}");
                    if tokio::time::Instant::now() >= deadline {
                        return Err(e.into());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    continue;
                }
            };
            let center = box_center(&resp.model).ok_or(CoreError::NoBoxModel(0))?;
            let (x, y) = center;
            let visible = x > 0.0
                && y > 0.0
                && x < 1280.0
                && y < 800.0
                && resp.model.width > 0.0
                && resp.model.height > 0.0;
            if visible {
                if prev == Some(center) {
                    stable_count += 1;
                    if stable_count >= 1 {
                        return Ok(center);
                    }
                } else {
                    stable_count = 0;
                }
            } else {
                stable_count = 0;
            }
            prev = Some(center);
            if tokio::time::Instant::now() >= deadline {
                // Fall back to last observed center (best-effort, never hang).
                return Ok(center);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Drive a single mouse click at the backend node's box center.
    pub(super) async fn click_at_backend(
        &self,
        dom_session_id: &str,
        owner_backend: Option<i64>,
        backend: i64,
    ) -> Result<(), CoreError> {
        let (cx, cy) = self.wait_for_actionable(dom_session_id, backend).await?;
        let (ox, oy) = self.frame_offset(owner_backend).await;
        let (x, y) = (cx + ox, cy + oy);
        for ty in ["mouseMoved", "mousePressed", "mouseReleased"] {
            input::dispatch_mouse_event(
                &self.client,
                &self.session_id,
                &input::DispatchMouseEventParams {
                    type_: ty.to_string(),
                    x,
                    y,
                    button: Some("left".to_string()),
                    click_count: Some(1),
                    delta_x: None,
                    delta_y: None,
                    modifiers: None,
                },
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sha256_matches_standard_vector() {
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
