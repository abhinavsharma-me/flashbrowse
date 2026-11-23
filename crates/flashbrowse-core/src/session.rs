//! High-level browser session controller.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flashbrowse_cdp::protocol::{dom, input, network, page, runtime, target};
use flashbrowse_cdp::{CdpClient, CdpError, ChromeProcess, LaunchOptions};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::actions::fill::{backspace_event, select_all_event};
use crate::actions::{box_center, ScrollDirection};
use crate::driver::{BrowserDriver, CdpDriver};
use crate::engine::BrowserEngine;
use crate::interceptor::{blocked_patterns, blocked_patterns_for_screenshot};
use crate::markdown::EXTRACT_SCRIPT;
use crate::resource_policy::ResourcePolicy;
use crate::stealth::STEALTH_SCRIPT;
use crate::tree::{compress, render, ActionMap};

/// Errors surfaced by the core engine (maps to MCP `tools/call` error).
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("CDP error: {0}")]
    Cdp(#[from] CdpError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("action id [{0}] not in current action map")]
    ActionNotFound(u32),
    #[error("action id [{0}] has no backend node handle")]
    NoBackendId(u32),
    #[error("action id [{0}] has no box model")]
    NoBoxModel(u32),
    #[error("invalid scroll direction: {0}")]
    InvalidScrollDirection(String),
    #[error("navigation failed (browser error page): {0}")]
    NavigationFailed(String),
    #[error("unsupported browser engine: {0}")]
    UnsupportedEngine(String),
}

/// Session launch configuration.
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// Explicit Chrome path (`--chrome-path` / `CHROME_PATH`).
    pub chrome_path: Option<PathBuf>,
    /// Network resource load policy (AX tree format is unchanged).
    pub resource_policy: ResourcePolicy,
    /// Browser engine (`firefox` requires `flashbrowse-bidi`, not yet wired in CLI).
    pub engine: BrowserEngine,
    /// Pass `--no-sandbox` (containers/CI). `None` → `FLASHBROWSE_NO_SANDBOX` env.
    pub no_sandbox: Option<bool>,
    /// Extra Chromium CLI args.
    pub extra_args: Vec<String>,
    /// Keep Chromium alive after parent CLI exit (`open --session-id`).
    pub persistent: bool,
}

impl SessionConfig {
    /// Launch is Chrome-only until BiDi Firefox lands.
    pub fn validate_engine(&self) -> Result<(), CoreError> {
        if self.engine != BrowserEngine::Chrome {
            return Err(CoreError::UnsupportedEngine(format!(
                "{} (chrome only today)",
                self.engine.as_str()
            )));
        }
        Ok(())
    }
}

/// Options for [`BrowserSession::screenshot`].
#[derive(Debug, Clone)]
pub struct ScreenshotOptions {
    pub full_page: bool,
    /// Scroll the document to surface lazy-loaded images before capture.
    pub prepare_lazy_content: bool,
    /// Max time to wait for network/images before best-effort capture.
    pub timeout_ms: u64,
}

impl Default for ScreenshotOptions {
    fn default() -> Self {
        Self {
            full_page: false,
            prepare_lazy_content: true,
            timeout_ms: 8_000,
        }
    }
}

/// Default cap on screenshot prepare time (milliseconds).
pub const SCREENSHOT_PREPARE_MS_DEFAULT: u64 = 8_000;

/// Hard ceiling on screenshot prepare time (milliseconds).
///
/// An agent could otherwise ask for a 24-hour wait and pin the (serial) MCP
/// server for the whole time. 60 s is far beyond any real page and still
/// bounded.
pub const SCREENSHOT_PREPARE_MS_MAX: u64 = 60_000;

/// Tracks in-flight network requests so the settle can wait for the page to
/// actually finish loading (an SPA can be quiet in the DOM while its JS
/// bundle is still downloading).
#[derive(Debug, Clone, Default)]
struct NetActivity {
    inflight: Arc<AtomicUsize>,
    last_activity_ms: Arc<AtomicU64>,
    /// Timestamp (ms) when the current batch of requests started - tells a
    /// slow API call (must wait) from a hung socket (must not stall).
    started_at_ms: Arc<AtomicU64>,
}

/// A request in flight for longer than this is considered hung (e.g. a
/// cancelled favicon request that never reports `loadingFinished`).
const HUNG_REQUEST_MS: u64 = 1000;

impl NetActivity {
    fn new() -> Self {
        Self {
            inflight: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: Arc::new(AtomicU64::new(0)),
            started_at_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn start(&self) {
        if self.inflight.fetch_add(1, Ordering::Relaxed) == 0 {
            self.started_at_ms.store(now_ms(), Ordering::Relaxed);
        }
        self.touch();
    }

    fn finish(&self) {
        let _ = self
            .inflight
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
        self.touch();
    }

    /// True when the network is effectively idle: nothing in flight (or only
    /// hung requests) and nothing happened for `quiet_ms`.
    fn idle(&self, quiet_ms: u64) -> bool {
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
    fn attach(&self, events: &flashbrowse_cdp::EventEmitter) {
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One headless browser + one flattened page target.
///
/// Action IDs are ephemeral: every `open`/`click`/`fill`/`scroll` rebuilds
/// the tree and replaces `action_map`. Stale IDs return `ActionNotFound`.
pub struct BrowserSession {
    chrome: Option<ChromeProcess>,
    browser_ws_url: String,
    client: Arc<CdpClient>,
    session_id: String,
    target_id: String,
    resource_policy: ResourcePolicy,
    /// Engine-neutral primitives. Held behind the trait object so a BiDi
    /// driver can replace `CdpDriver` without touching the call sites.
    driver: Box<dyn BrowserDriver>,
    net: NetActivity,
    action_map: ActionMap,
    title: String,
    url: String,
    last_tree: String,
}

impl BrowserSession {
    /// Launch Chromium, attach, apply stealth + network filtering.
    pub async fn launch(config: SessionConfig) -> Result<Self, CoreError> {
        config.validate_engine()?;
        let chrome = flashbrowse_cdp::launch_chrome(LaunchOptions {
            chrome_path: config.chrome_path.clone(),
            extra_args: config.extra_args.clone(),
            no_sandbox: config.no_sandbox,
            user_data_dir: None,
            window_size: (1280, 800),
            persistent: config.persistent,
        })
        .await?;
        let client = Arc::new(CdpClient::connect(chrome.browser_ws_url()).await?);
        let t = target::create_target(&client, "about:blank").await?;
        let s = target::attach_to_target(&client, &t.target_id).await?;
        let session_id = s.session_id.clone();

        // Domains must be enabled before navigate so events + AXTree work.
        // Parallelized (was 3 sequential round-trips).
        let (page_r, net_r, acc_r) = tokio::join!(
            page::enable(&client, &session_id),
            network::enable(&client, &session_id),
            client.send(
                Some(&session_id),
                "Accessibility.enable",
                serde_json::json!({})
            )
        );
        page_r?;
        net_r?;
        let _: Value = acc_r.unwrap_or(serde_json::json!({}));

        let (_, _) = tokio::join!(
            page::add_script_on_new_document(&client, &session_id, STEALTH_SCRIPT),
            network::set_blocked_urls(
                &client,
                &session_id,
                blocked_patterns(config.resource_policy),
            )
        );

        let browser_ws_url = chrome.browser_ws_url().to_string();
        let net = NetActivity::new();
        net.attach(&client.events());
        let driver: Box<dyn BrowserDriver> =
            Box::new(CdpDriver::new(client.clone(), session_id.clone()));
        Ok(Self {
            chrome: Some(chrome),
            browser_ws_url,
            client,
            session_id,
            target_id: t.target_id,
            resource_policy: config.resource_policy,
            driver,
            net,
            action_map: ActionMap::new(),
            title: String::new(),
            url: String::new(),
            last_tree: String::new(),
        })
    }

    /// Attach to an already-running Chromium (session-file persistence).
    ///
    /// If `target_id` is `Some`, re-attaches to that live target in flatten
    /// mode. If `None`, creates a fresh target.
    ///
    /// NOTE: `Page.addScriptToEvaluateOnNewDocument` and
    /// `Network.setBlockedURLs` are scoped to the CDP *session*, so they are
    /// re-applied on every attach (a new `sessionId` is minted each time).
    pub async fn attach(
        ws_url: String,
        target_id: Option<String>,
        config: &SessionConfig,
    ) -> Result<Self, CoreError> {
        config.validate_engine()?;
        let client = Arc::new(CdpClient::connect(&ws_url).await?);
        let tid = match target_id {
            Some(t) => t,
            None => {
                target::create_target(&client, "about:blank")
                    .await?
                    .target_id
            }
        };
        let s = target::attach_to_target(&client, &tid).await?;
        let session_id = s.session_id.clone();

        // Domains (parallel - 1 round-trip instead of 3).
        let (p_r, n_r, a_r) = tokio::join!(
            page::enable(&client, &session_id),
            network::enable(&client, &session_id),
            client.send(
                Some(&session_id),
                "Accessibility.enable",
                serde_json::json!({})
            )
        );
        p_r?;
        n_r?;
        let _: Value = a_r.unwrap_or(serde_json::json!({}));

        // Per-session settings: always re-apply (parallel).
        let (s_r, b_r) = tokio::join!(
            page::add_script_on_new_document(&client, &session_id, STEALTH_SCRIPT),
            network::set_blocked_urls(
                &client,
                &session_id,
                blocked_patterns(config.resource_policy),
            )
        );
        s_r?;
        b_r?;

        let net = NetActivity::new();
        net.attach(&client.events());
        let driver: Box<dyn BrowserDriver> =
            Box::new(CdpDriver::new(client.clone(), session_id.clone()));
        let mut sess = Self {
            chrome: None,
            browser_ws_url: ws_url,
            client,
            session_id,
            target_id: tid,
            resource_policy: config.resource_policy,
            driver,
            net,
            action_map: ActionMap::new(),
            title: String::new(),
            url: String::new(),
            last_tree: String::new(),
        };
        // Seed title/url + action_map so `act` never uses a blank map.
        // Propagate failures (H6): a blank map must not masquerade as
        // "ActionNotFound" later.
        sess.refresh_title_url().await;
        sess.rebuild_tree().await?;
        Ok(sess)
    }

    /// Browser-level WebSocket URL (for session files).
    pub fn browser_ws_url(&self) -> &str {
        &self.browser_ws_url
    }

    /// Owned process id (ephemeral sessions only; `None` after attach).
    pub fn pid(&self) -> Option<u32> {
        self.chrome.as_ref().and_then(|c| c.pid())
    }

    /// Owned profile dir (ephemeral sessions only).
    pub fn user_data_dir(&self) -> Option<&PathBuf> {
        self.chrome.as_ref().map(|c| c.user_data_dir())
    }

    /// Detach the browser process so it survives CLI exit and return
    /// metadata for the session file. The CdpClient socket is closed
    /// (pending calls drained); the target page persists in Chromium.
    pub fn detach(mut self) -> DetachedSession {
        let ws_url = self.browser_ws_url.clone();
        let tid = self.target_id.clone();
        let resource_policy = self.resource_policy;
        let (pid, user_data_dir) = match self.chrome.take() {
            Some(proc) => {
                let pid = proc.pid().unwrap_or(0);
                let dir = proc.user_data_dir().clone();
                if let Some((child, _dir)) = proc.detach() {
                    // Forget Child so kill_on_drop does not kill the daemon browser.
                    std::mem::forget(child);
                }
                (pid, dir)
            }
            None => (0, PathBuf::new()),
        };
        self.client.shutdown();
        DetachedSession {
            browser_ws_url: ws_url,
            target_id: tid,
            pid,
            user_data_dir,
            resource_policy,
        }
    }

    /// Navigate and return the compressed, numbered action tree.
    ///
    /// ACCURACY FIRST:
    /// 1. Wait for the `load` lifecycle event of THIS navigation
    ///    (`Page.lifecycleEvent` carries `loaderId`, unlike
    ///    `Page.loadEventFired`, so stale events cannot fool us).
    /// 2. Then poll the AX tree until it is *stable* (same semantic node
    ///    count twice in a row, 100 ms apart, max 500 ms) so late
    ///    hydration is never truncated.
    ///
    /// Cost control: the first tree fetch is reused (never discarded), and
    /// title/url are fetched in the same round-trip batch.
    pub async fn open(&mut self, url: &str) -> Result<String, CoreError> {
        let t_open = std::time::Instant::now();
        let nav = page::navigate(&self.client, &self.session_id, url).await?;
        // NOTE: we deliberately do NOT wait for the `load` event - it waits
        // for every subresource (8+ s on heavy pages) while the AX tree is
        // usually final much earlier. Readiness is decided by the settle loop
        // below (document parsed + AX tree complete vs DOM + tree quiet).
        let _ = nav;

        self.url = url.to_string();
        let client_c = self.client.clone();
        let sid = self.session_id.clone();
        let (title_r, url_r) = tokio::join!(
            runtime::evaluate(&client_c, &sid, "document.title"),
            runtime::evaluate(&client_c, &sid, "location.href")
        );
        if let Ok(r) = title_r {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.title = s;
                }
            }
        }
        if let Ok(r) = url_r {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.url = s;
                }
            }
        }
        // Chrome error pages (network lost, DNS failure, ...) look like a
        // tiny page. Report them instead of returning a truncated tree.
        if self.url.starts_with("chrome-error://") {
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(self.url.clone()));
        }
        // The AX tree is fetched inside the settle loop (the tree right
        // after `navigate` belongs to the previous page - fetching it twice
        // wasted ~200 KB of JSON per open).
        let mut raw_ax_nodes = 0usize;
        let mut nodes: Vec<crate::tree::SemanticNode> = Vec::new();
        let mut map = ActionMap::new();
        let mut fingerprint = 0u64;

        // Settle loop.
        //
        // Two-level strategy (fast AND complete):
        //  1. Cheap poll every 100 ms - one `Runtime.evaluate` returning
        //     `readyState|domCount|textLength`. If the DOM is still moving,
        //     the AX tree cannot be final.
        //  2. Once the cheap metrics have been quiet for `QUIET_MS`, fetch
        //     the AX tree once more and compare the content fingerprint.
        //     Only an identical fingerprint ends the wait.
        //
        // Why not just poll the AX tree? Each `getFullAXTree` moves ~200 KB
        // of JSON; three polls cost more than the whole cheap-poll budget.
        //
        // Why the fingerprint and not the node count? Chromium upgrades table
        // semantics after layout (LayoutTableCell -> Link) without changing
        // the node count.
        // Adaptive quiet window: a page that never changed (server-rendered)
        // only needs a short window; a page that is still moving gets a long
        // one. The AX fingerprint check is the final gate in both cases.
        //
        // The small-tree window must cover common hydration delays: a page
        // that fills in content from a `setTimeout(..., 1000)` fires *after*
        // `readyState == "complete"`, and exiting too early truncates that
        // content. 1200 ms of stillness is the compromise between catching
        // late hydration and not doubling the cost of every tiny page.
        const QUIET_MS_STATIC: u64 = 250;
        const QUIET_MS_DYNAMIC: u64 = 600;
        const QUIET_MS_SMALL_TREE: u64 = 1200;
        // The deadline covers the whole settle (page load + hydration + AX
        // population). A slow network can need several seconds just to
        // deliver the HTML, so the budget must not be shorter than the CDP
        // call timeout we already allow elsewhere (15s).
        let deadline = tokio::time::Instant::now() + Duration::from_millis(15000);
        let mut last_change = tokio::time::Instant::now();
        let mut prev_dom = (0u64, 0u64);
        let mut saw_change = false;
        loop {
            // Deadline check FIRST: every `continue` path below must be
            // able to exit the loop (a page that keeps changing would
            // otherwise spin forever).
            if tokio::time::Instant::now() >= deadline {
                tracing::debug!(
                    raw_ax_nodes,
                    semantic = nodes.len(),
                    "settle deadline reached - using latest tree"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;

            // (1) cheap change detection: is the document still moving?
            let mut ready_state = String::from("loading");
            let mut dom_count = 0u64;
            let mut text_len = 0u64;
            if let Ok(cheap) = runtime::evaluate(
                &self.client,
                &self.session_id,
                "document.readyState + '|' + document.getElementsByTagName('*').length + '|' + (document.body ? document.body.innerText.length : 0)",
            )
            .await
            {
                if let Some(Value::String(s)) = cheap.result.value.as_ref() {
                    let mut it = s.split('|');
                    ready_state = it.next().unwrap_or("loading").to_string();
                    dom_count = it.next().unwrap_or("0").parse().unwrap_or(0);
                    text_len = it.next().unwrap_or("0").parse().unwrap_or(0);
                }
            }
            // "loading" = the document is still streaming; "interactive" = the
            // DOM is parsed but subresources (JS bundles!) are still coming;
            // "complete" = the browser considers the page fully loaded.
            let page_complete = ready_state.eq_ignore_ascii_case("complete");
            let parsed = !ready_state.eq_ignore_ascii_case("loading");
            if !parsed {
                // Still parsing - the tree cannot be final.
                last_change = tokio::time::Instant::now();
                continue;
            }
            let cheap_now = (dom_count, text_len);
            if prev_dom == (0, 0) {
                prev_dom = cheap_now; // baseline, not a change
            } else if cheap_now != prev_dom {
                prev_dom = cheap_now;
                last_change = tokio::time::Instant::now();
                saw_change = true;
                continue;
            }

            // Quiet window:
            //   - the DOM was moving          -> wait longer (600 ms)
            //   - the tree is still small      -> wait longer (800 ms): a
            //     small tree is either a tiny page or a page that has not
            //     hydrated yet (nhrdev.com truncated to 140 tokens with a
            //     250 ms window);
            //   - a big, static tree           -> settle fast (250 ms).
            let quiet_ms = if saw_change {
                QUIET_MS_DYNAMIC
            } else if raw_ax_nodes < 100 {
                QUIET_MS_SMALL_TREE
            } else {
                QUIET_MS_STATIC
            };
            let quiet_enough = last_change.elapsed() >= Duration::from_millis(quiet_ms);

            // (2) the AX tree must not be obviously empty while the DOM
            // already has content.
            //
            // Chromium populates the accessibility tree asynchronously
            // (~400 ms after the DOM is final, measured on Hacker News), so a
            // DOM-stability window alone truncates the page (140 tokens
            // instead of 1770). A pre-hydration tree is tiny (2-21 nodes)
            // while the DOM already holds hundreds of elements.
            // A tree with a handful of nodes cannot be a real page (unless
            // the page itself is tiny) - treat it as "not populated yet".
            // `raw_ax_nodes == 0` means "not fetched yet" -> fetch now.
            let tree_unready = raw_ax_nodes == 0 || (raw_ax_nodes < 20 && dom_count >= 20);

            // Fetch the AX tree when the DOM settled OR when the tree looks
            // unready (it must be refreshed while we wait for it - the
            // previous tree may be stale).
            if !quiet_enough && !tree_unready {
                continue;
            }

            // (3) verify the AX tree itself
            let fresh = self.driver.get_full_ax_tree().await?;
            let raw_ax_nodes_before = raw_ax_nodes;
            raw_ax_nodes = fresh.len();
            let (n2, m2) = compress(&fresh);
            let fp2 = tree_fingerprint(&n2, &m2);
            let ratio_ok = dom_count < 100 || (raw_ax_nodes as u64) * 4 >= dom_count;
            let same = raw_ax_nodes_before == raw_ax_nodes && fp2 == fingerprint;
            nodes = n2;
            map = m2;
            fingerprint = fp2;

            let net_idle = self.net.idle(250);
            tracing::debug!(
                dom_count,
                text_len,
                raw_ax_nodes,
                semantic = nodes.len(),
                same,
                quiet_enough,
                net_idle,
                "settle poll"
            );

            // Accept when the tree is stable and the page is done.
            //
            // `readyState == "complete"` is the browser's own verdict that
            // everything (including JS bundles) has loaded - the
            // authoritative, network-speed-independent signal. The network
            // idle check is only a fallback for pages that keep a socket
            // open forever (never "complete").
            //
            // The tree must contain *something* (`nodes.len() >= 1`), not
            // three nodes: a minimal page ("<h1>Hi</h1>") legitimately
            // compresses to one or two nodes, and requiring three made
            // every such page wait out the whole settle deadline.
            let first_fetch = raw_ax_nodes_before == 0;
            if same
                && ratio_ok
                && !tree_unready
                && !nodes.is_empty()
                && quiet_enough
                && (page_complete || net_idle)
                && (!first_fetch || !saw_change)
            {
                break;
            }
            if !same {
                // The tree still moves - restart the quiet window.
                last_change = tokio::time::Instant::now();
            }
        }
        if nodes.is_empty() {
            // The page never rendered (network hiccup, bot wall, JS-only
            // shell). Report it instead of returning an empty tree.
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(format!(
                "page produced no accessibility content: {}",
                self.url
            )));
        }
        // Re-read the title and URL now that the page has settled. The
        // first read happens right after `navigate`, when the new document
        // often has not been parsed yet - `document.title` is then empty
        // and the header line was blank about one page in five.
        self.refresh_title_url().await;
        let rendered = render(&self.title, &self.url, &nodes);
        self.action_map = map;
        self.last_tree = rendered.clone();
        tracing::debug!(
            ms = t_open.elapsed().as_millis(),
            nodes = nodes.len(),
            "open complete"
        );
        Ok(rendered)
    }

    /// Current action map (`ID → BackendNodeId`).
    pub fn action_map(&self) -> &ActionMap {
        &self.action_map
    }

    /// Drop the action map and the rendered tree.
    ///
    /// Called when a navigation fails: the old map points at backend node
    /// ids from the *previous* page, which no longer exist. Acting on them
    /// makes CDP calls hang until they time out. Clearing them turns a
    /// stale action into an immediate, explicit "action not found".
    fn invalidate_tree(&mut self) {
        self.action_map.clear();
        self.last_tree.clear();
    }

    /// Last rendered tree (from the most recent rebuild).
    pub fn last_tree(&self) -> &str {
        &self.last_tree
    }

    /// Document title from the last navigation.
    pub fn page_title(&self) -> &str {
        &self.title
    }

    /// Resolved URL of the loaded page.
    pub fn page_url(&self) -> &str {
        &self.url
    }

    /// Flattened CDP `sessionId`.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// CDP `Target` id backing this page.
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    pub fn resource_policy(&self) -> ResourcePolicy {
        self.resource_policy
    }

    /// Whether text-only AV blocking is active.
    pub fn text_only(&self) -> bool {
        self.resource_policy == ResourcePolicy::TextOnly
    }

    /// Raw `CdpClient` handle (escape hatch for advanced callers).
    pub fn client(&self) -> Arc<CdpClient> {
        self.client.clone()
    }

    /// Raw (uncompressed) AX tree nodes, for debugging the compressor.
    pub async fn raw_ax_nodes(
        &self,
    ) -> Result<Vec<flashbrowse_cdp::protocol::accessibility::AXNode>, CoreError> {
        self.driver.get_full_ax_tree().await
    }

    /// Evaluate an expression in the page (engine-neutral).
    ///
    /// Used by the screenshot prepare (scroll bookkeeping) and exposed for
    /// callers that need to inspect page state.
    pub async fn evaluate(&self, expression: &str) -> Result<Value, CoreError> {
        self.driver.evaluate(expression).await
    }

    /// Current vertical scroll offset of the page.
    pub async fn evaluate_scroll_y(&self) -> f64 {
        match self.evaluate("window.scrollY").await {
            Ok(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            _ => 0.0,
        }
    }

    /// Re-fetch AXTree, recompress, re-render. Single point of rebuild so
    /// every action funnels through identical state.
    pub async fn rebuild_tree(&mut self) -> Result<String, CoreError> {
        let raw = self.driver.get_full_ax_tree().await?;
        let (nodes, map) = compress(&raw);
        let rendered = render(&self.title, &self.url, &nodes);
        self.action_map = map;
        self.last_tree = rendered.clone();
        Ok(rendered)
    }

    /// `click(action_id)`: scroll → box → center → mouse events → rebuild.
    pub async fn click(&mut self, action_id: u32) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        self.click_at_backend(backend).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.rebuild_tree().await
    }

    /// `hover(action_id)`: move mouse to element center without clicking → rebuild.
    pub async fn hover(&mut self, action_id: u32) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        if let Err(e) =
            dom::scroll_into_view_if_needed(&self.client, &self.session_id, backend).await
        {
            tracing::warn!(backend, "scrollIntoViewIfNeeded failed (proceeding): {e}");
        }
        let resp = dom::get_box_model(&self.client, &self.session_id, backend).await?;
        let (x, y) = box_center(&resp.model).ok_or(CoreError::NoBoxModel(action_id))?;
        input::dispatch_mouse_event(
            &self.client,
            &self.session_id,
            &input::DispatchMouseEventParams {
                type_: "mouseMoved".to_string(),
                x,
                y,
                button: None,
                click_count: None,
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.rebuild_tree().await
    }

    /// `fill(action_id, text)`: focus → Ctrl+A → Backspace → insert → rebuild.
    pub async fn fill(&mut self, action_id: u32, text: &str) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        self.click_at_backend(backend).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Select-all (down+up) then Backspace (down+up) to clear.
        for ev in [select_all_event(), backspace_event()] {
            let down = ev.clone();
            input::dispatch_key_event(&self.client, &self.session_id, &down).await?;
            let mut up = down;
            up.type_ = "keyUp".to_string();
            input::dispatch_key_event(&self.client, &self.session_id, &up).await?;
        }
        input::insert_text(&self.client, &self.session_id, text).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.rebuild_tree().await
    }

    /// `scroll(direction, amount_px)`: synthetic `mouseWheel` → rebuild.
    pub async fn scroll(
        &mut self,
        direction: ScrollDirection,
        amount: f64,
    ) -> Result<String, CoreError> {
        let (dx, dy) = direction.delta(amount);
        input::dispatch_mouse_event(
            &self.client,
            &self.session_id,
            &input::DispatchMouseEventParams {
                type_: "mouseWheel".to_string(),
                x: 640.0,
                y: 400.0,
                button: None,
                click_count: None,
                delta_x: Some(dx),
                delta_y: Some(dy),
                modifiers: None,
            },
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.rebuild_tree().await
    }

    /// `select(action_id, text)`: pick a `<select>` option by visible text
    /// or value, dispatch `change` + `input`, then rebuild.
    ///
    /// Implemented via `DOM.resolveNode` + `Runtime.callFunctionOn` so the
    /// native dropdown popup is never opened (opening it would swallow
    /// subsequent input events and leave the tree in `expanded` state).
    pub async fn select(&mut self, action_id: u32, text: &str) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let resolved = dom::resolve_node(&self.client, &self.session_id, backend).await?;
        let object_id = resolved
            .object
            .object_id
            .ok_or(CoreError::NoBackendId(action_id))?;
        let needle = serde_json::to_string(text)?;
        let resp = runtime::call_function_on(
            &self.client,
            &self.session_id,
            &object_id,
            "function(want) { if (!this || this.tagName !== 'SELECT') return 'not-a-select'; for (const o of this.options) { if (o.text.trim() === want.trim() || o.value === want) { this.value = o.value; this.dispatchEvent(new Event('change', { bubbles: true })); this.dispatchEvent(new Event('input', { bubbles: true })); return 'ok:' + o.value; } } return 'no-match'; }",
            vec![serde_json::json!(needle)],
        )
        .await?;
        let outcome = resp
            .result
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if !outcome.starts_with("ok:") {
            tracing::warn!(action_id, text, outcome, "select failed");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.rebuild_tree().await
    }

    /// `press_key(key)`: single key down+up -> rebuild.
    ///
    /// Accepts named keys ("Enter", "ArrowDown", "Escape") and single
    /// characters ("a", "7"). The DOM `code` is derived correctly
    /// ("a" -> "KeyA").
    pub async fn press_key(&mut self, key: &str) -> Result<String, CoreError> {
        let spec = crate::actions::keys::resolve(key)
            .ok_or_else(|| CoreError::InvalidScrollDirection(format!("unsupported key: {key}")))?;
        let params = input::DispatchKeyEventParams {
            type_: "keyDown".to_string(),
            key: spec.key.clone(),
            code: Some(spec.code.clone()),
            modifiers: None,
            windows_virtual_key_code: spec.vk,
            native_virtual_key_code: None,
        };
        input::dispatch_key_event(&self.client, &self.session_id, &params).await?;
        let mut up = params;
        up.type_ = "keyUp".to_string();
        input::dispatch_key_event(&self.client, &self.session_id, &up).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.rebuild_tree().await
    }

    /// Extract clean Markdown (boss-level `browser_extract_markdown`).
    pub async fn extract_markdown(&self) -> Result<String, CoreError> {
        let resp = runtime::evaluate(&self.client, &self.session_id, EXTRACT_SCRIPT).await?;
        if let Some(v) = resp.result.value {
            match v {
                Value::String(s) => return Ok(s),
                Value::Object(obj) => {
                    if let Some(Value::String(md)) = obj.get("markdown") {
                        return Ok(md.clone());
                    }
                    return Ok(serde_json::to_string(&Value::Object(obj))?);
                }
                other => return Ok(other.to_string()),
            }
        }
        Ok(String::new())
    }

    /// Capture PNG screenshot (base64). See [`ScreenshotOptions`].
    ///
    /// In agent-default mode, font and image blocks are lifted for the prepare
    /// window, then restored. [`ResourcePolicy::FullRender`] skips that dance.
    ///
    /// Lifting the blocklist alone is **not** enough: `Network.setBlockedURLs`
    /// only affects *future* requests, and Chrome never retries a request that
    /// was already blocked. A blocked `<img>` therefore stays broken forever.
    /// So the prepare window also re-requests the images (see
    /// [`Self::reload_blocked_images`]), which is what actually makes pixels
    /// appear.
    pub async fn screenshot(&self, opts: ScreenshotOptions) -> Result<String, CoreError> {
        let needs_lift = self.resource_policy != ResourcePolicy::FullRender;
        if needs_lift {
            self.driver
                .set_blocklist(blocked_patterns_for_screenshot(self.resource_policy))
                .await?;
        }
        // Re-request images that were blocked during the page load. Without
        // this the capture shows broken-image glyphs, no matter how long we
        // wait for them.
        if needs_lift {
            self.reload_blocked_images().await;
        }
        if opts.prepare_lazy_content {
            self.prepare_lazy_content_for_screenshot(opts.full_page)
                .await;
        }
        self.wait_for_visual_ready(opts.timeout_ms).await;
        let data = self.driver.capture_screenshot(opts.full_page).await?;
        if needs_lift {
            self.driver
                .set_resource_policy(self.resource_policy)
                .await?;
        }
        Ok(data)
    }

    /// Convenience wrapper (viewport, default prepare/timeout).
    pub async fn screenshot_viewport(&self) -> Result<String, CoreError> {
        self.screenshot(ScreenshotOptions::default()).await
    }

    /// Re-request images that the network layer blocked during page load.
    ///
    /// `Network.setBlockedURLs` prevents the *request*; it does not prevent
    /// the `<img>` element from existing, and Chrome does not retry a blocked
    /// request on its own. Re-assigning the source issues a fresh request,
    /// which now succeeds because the blocklist has been lifted.
    ///
    /// Images that already loaded are left alone (`complete && naturalWidth`),
    /// so this never re-fetches a working image. Inline `data:` URIs are
    /// never blocked, so they are skipped too.
    async fn reload_blocked_images(&self) {
        // Bounded: a pathological page with thousands of images must not
        // turn one screenshot into thousands of requests.
        const MAX_RETRY: usize = 200;
        let expr = format!(
            r#"(function(){{
  const imgs = Array.from(document.images || []);
  let n = 0;
  for (const img of imgs) {{
    if (n >= {MAX_RETRY}) break;
    if (img.complete && img.naturalWidth > 0) continue; // already loaded
    const src = img.currentSrc || img.src;
    if (!src || src.indexOf('data:') === 0) continue;   // inline data URI
    if (img.srcset) {{
      // Re-trigger the srcset selection rather than pinning one candidate.
      const s = img.srcset;
      img.srcset = '';
      img.srcset = s;
    }} else {{
      img.src = '';
      img.src = src;
    }}
    n++;
  }}
  return n;
}})()"#
        );
        if let Ok(Value::Number(n)) = self.driver.evaluate(&expr).await {
            let count = n.as_u64().unwrap_or(0);
            if count > 0 {
                tracing::debug!(count, "re-requested blocked images");
            }
        }
    }

    /// Surface lazy-loaded content before a capture.
    ///
    /// The page is scrolled through so that `loading="lazy"` images start
    /// fetching, then the agent's original scroll position is restored. The
    /// restore matters: the agent asked for a picture of what it is currently
    /// looking at, so snapping the viewport back to the top would destroy
    /// that context.
    async fn prepare_lazy_content_for_screenshot(&self, full_page: bool) {
        // Remember where the agent was. The value is held here (not on
        // `window`) so nothing is left behind in the page's global scope.
        let saved_y = self.evaluate_scroll_y().await;
        let script = r#"(function(){
  const h = Math.max(document.body.scrollHeight, document.documentElement.scrollHeight);
  const step = Math.max(400, window.innerHeight);
  for (let y = 0; y < h; y += step) { window.scrollTo(0, y); }
  return true;
})()"#;
        let _ = self.driver.evaluate(script).await;
        // Give the lazy loader a tick to start the fetches.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Restore the agent's viewport.
        let target = if full_page { 0.0 } else { saved_y };
        let _ = self
            .driver
            .evaluate(&format!("window.scrollTo(0, {target}); true"))
            .await;
    }

    async fn wait_for_visual_ready(&self, timeout_ms: u64) {
        let budget = timeout_ms.clamp(500, SCREENSHOT_PREPARE_MS_MAX);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(budget);
        let quiet_ms = 350u64;
        loop {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("screenshot prepare deadline reached - capturing best effort");
                break;
            }
            let net_idle = self.net.idle(quiet_ms);
            let images_ok = self.images_ready().await;
            if net_idle && images_ok {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// True when no image is still pending.
    ///
    /// `img.complete` is the readiness signal: it is true once the image has
    /// loaded *or* failed. Requiring `naturalWidth > 0` as well would mean a
    /// permanently broken image (blocked, 404, unsupported format) keeps the
    /// wait loop spinning until the deadline - every screenshot of a page with
    /// one bad image would cost the full timeout.
    async fn images_ready(&self) -> bool {
        let expr = r#"(function(){
  const imgs = Array.from(document.images || []);
  if (imgs.length === 0) return true;
  return imgs.every(function(img){
    if (!img.src) return true;
    return img.complete;
  });
})()"#;
        match self.driver.evaluate(expr).await {
            Ok(Value::Bool(b)) => b,
            _ => true,
        }
    }

    /// `Title`/`URL` header fields via in-page evaluation (best-effort).
    async fn refresh_title_url(&mut self) {
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

    /// Wait until `backend` is visible + stable (actionability check -
    /// Playwright-parity, beats fixed-sleep clicking). Polls box center;
    /// stable = same center twice in a row, visible = center inside viewport.
    async fn wait_for_actionable(&self, backend: i64) -> Result<(f64, f64), CoreError> {
        let mut prev: Option<(f64, f64)> = None;
        let mut stable_count = 0u8;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Err(e) =
                dom::scroll_into_view_if_needed(&self.client, &self.session_id, backend).await
            {
                tracing::warn!(backend, "scrollIntoViewIfNeeded failed (proceeding): {e}");
            }
            let resp = match dom::get_box_model(&self.client, &self.session_id, backend).await {
                Ok(r) => r,
                Err(e) => {
                    // Transient: node not in the layout tree yet.
                    tracing::warn!(backend, "getBoxModel failed (retrying): {e}");
                    if tokio::time::Instant::now() >= deadline {
                        return Err(e.into());
                    }
                    tokio::time::sleep(Duration::from_millis(80)).await;
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
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    }

    /// Drive a single mouse click at the backend node's box center.
    async fn click_at_backend(&self, backend: i64) -> Result<(), CoreError> {
        let (x, y) = self.wait_for_actionable(backend).await?;
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

    /// Drain pending RPCs, kill Chromium (if owned), drop profile dir.
    /// Attached sessions (session-file mode) only disconnect.
    pub async fn shutdown(mut self) {
        self.client.shutdown();
        if let Some(mut proc) = self.chrome.take() {
            proc.shutdown().await;
        }
    }
}

/// Metadata emitted by [`BrowserSession::detach`] for `close`/`act` files.
#[derive(Debug, Clone)]
pub struct DetachedSession {
    /// Browser WebSocket URL (`ws://127.0.0.1:...`).
    pub browser_ws_url: String,
    /// Live page `targetId`.
    pub target_id: String,
    /// Chromium OS pid.
    pub pid: u32,
    /// Profile dir (owned by the session file; removed by `close`).
    pub user_data_dir: PathBuf,
    /// Resource policy at launch time.
    pub resource_policy: ResourcePolicy,
}

/// Cheap content fingerprint of a compressed tree: roles, names and the
/// action-id -> backend-id mapping. Detects semantic changes (role
/// upgrades, text changes) that a node count would miss.
fn tree_fingerprint(nodes: &[crate::tree::SemanticNode], map: &crate::tree::ActionMap) -> u64 {
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
