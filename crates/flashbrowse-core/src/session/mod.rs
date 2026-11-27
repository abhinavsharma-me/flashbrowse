//! High-level browser session controller.

pub(crate) mod config;
pub(crate) mod detached;
pub(crate) mod dialog;
pub(crate) mod error;
pub(crate) mod popup;
pub(crate) mod screenshot;
pub(crate) mod settle;
pub(crate) mod transfer;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flashbrowse_cdp::protocol::{browser, dom, input, network, page, runtime, target};
use flashbrowse_cdp::{CdpClient, ChromeProcess, LaunchOptions};
use serde_json::Value;

use crate::actions::fill::{backspace_event, select_all_event};
use crate::actions::{box_center, ScrollDirection};
use crate::driver::{BrowserDriver, CdpDriver, FrameSession};
use crate::interceptor::blocked_patterns;
use crate::markdown::EXTRACT_SCRIPT;
use crate::resource_policy::ResourcePolicy;
use crate::stealth::{
    build_ua_override, stealth_script, StealthProfile, SETTLE_DIAGNOSTICS_SCRIPT,
};
use crate::tree::{compress, render_nested, ActionMap, SemanticNode};

pub use config::{
    FillFormOutcome, ScreenshotOptions, SessionConfig, SCREENSHOT_PREPARE_MS_DEFAULT,
    SCREENSHOT_PREPARE_MS_MAX,
};
pub use detached::DetachedSession;
pub use dialog::{DialogInfo, DialogPolicy};
pub use error::CoreError;
pub use transfer::DownloadInfo;

use dialog::{install_dialog_handler, DialogState};
use popup::{install_popup_tracker, PopupTracker};
use settle::{
    build_action_sessions, settle_shadow_enabled, sha256_hex, tree_fingerprint, NetActivity,
    SettleSnapshot, SETTLE_DOM_STATS_JS,
};
use transfer::{download_directory, install_download_tracker, DownloadTracker};

async fn apply_stealth_profile(
    client: &Arc<CdpClient>,
    session_id: &str,
    profile: StealthProfile,
) -> Result<(), CoreError> {
    let version = browser::get_version(client).await?;
    let params = build_ua_override(version.user_agent.as_deref().unwrap_or(""), profile);
    let _: Value = client
        .send(Some(session_id), "Emulation.setUserAgentOverride", params)
        .await?;
    Ok(())
}

/// One headless browser + one flattened page target.
///
/// Action IDs are ephemeral: every `open`/`click`/`fill`/`scroll` rebuilds
/// the tree and replaces `action_map`. Stale IDs return `ActionNotFound`.
pub struct BrowserSession {
    pub(super) chrome: Option<ChromeProcess>,
    pub(super) browser_ws_url: String,
    pub(super) client: Arc<CdpClient>,
    pub(super) session_id: String,
    pub(super) target_id: String,
    pub(super) resource_policy: ResourcePolicy,
    /// Engine-neutral primitives. Held behind the trait object so a BiDi
    /// driver can replace `CdpDriver` without touching the call sites.
    pub(super) driver: Box<dyn BrowserDriver>,
    pub(super) net: NetActivity,
    pub(super) action_map: ActionMap,
    pub(super) action_sessions: HashMap<u32, FrameSession>,
    pub(super) title: String,
    pub(super) url: String,
    pub(super) last_tree: String,
    /// Timing breakdown of the most recent `open` (Phase 0 instrumentation).
    pub(super) last_settle: Option<crate::observability::SettleBreakdown>,
    pub(super) dialog_state: Arc<std::sync::Mutex<DialogState>>,
    pub(super) downloads: DownloadTracker,
    pub(super) download_dir: PathBuf,
    pub(super) popups: PopupTracker,
}

impl BrowserSession {
    /// Launch Chromium, attach, apply stealth + network filtering.
    pub async fn launch(config: SessionConfig) -> Result<Self, CoreError> {
        config.validate_engine()?;
        if let Some(endpoint) = config.cdp_url.as_deref() {
            let ws_url =
                flashbrowse_cdp::protocol::browser::resolve_websocket_url(endpoint).await?;
            return Self::attach(ws_url, None, &config).await;
        }
        let mut chrome = flashbrowse_cdp::launch_chrome(LaunchOptions {
            chrome_path: config.chrome_path.clone(),
            extra_args: config.extra_args.clone(),
            no_sandbox: config.no_sandbox,
            user_data_dir: config.user_data_dir.clone(),
            window_size: (1280, 800),
            persistent: config.persistent,
        })
        .await?;
        let client = Arc::new(CdpClient::connect_chrome(&mut chrome).await?);
        target::set_discover_targets(&client, true).await?;
        let popups = install_popup_tracker(&client);
        let t = target::create_target(&client, "about:blank").await?;
        let s = target::attach_to_target(&client, &t.target_id).await?;
        let session_id = s.session_id.clone();
        let dialog_state = install_dialog_handler(&client, &session_id, config.dialog_policy);

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
        let download_dir = download_directory(&config);
        std::fs::create_dir_all(&download_dir).map_err(|error| {
            CoreError::NavigationFailed(format!("cannot create download directory: {error}"))
        })?;
        let downloads = install_download_tracker(&client);
        flashbrowse_cdp::protocol::browser::set_download_behavior(
            &client,
            &download_dir.to_string_lossy(),
        )
        .await?;
        page::set_download_behavior(&client, &session_id, &download_dir.to_string_lossy()).await?;
        let _: Value = acc_r.unwrap_or(serde_json::json!({}));

        let stealth_js = stealth_script(config.stealth_profile);
        let (s_r, b_r, u_r) = tokio::join!(
            page::add_script_on_new_document(&client, &session_id, &stealth_js),
            network::set_blocked_urls(
                &client,
                &session_id,
                blocked_patterns(config.resource_policy),
            ),
            apply_stealth_profile(&client, &session_id, config.stealth_profile),
        );
        s_r?;
        b_r?;
        u_r?;
        if settle_shadow_enabled() {
            page::add_script_on_new_document(&client, &session_id, SETTLE_DIAGNOSTICS_SCRIPT)
                .await?;
        }

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
            action_sessions: HashMap::new(),
            title: String::new(),
            url: String::new(),
            last_tree: String::new(),
            last_settle: None,
            dialog_state,
            downloads,
            download_dir,
            popups,
        })
    }

    /// Attach to an already-running Chromium (session-file persistence).
    pub async fn attach(
        ws_url: String,
        target_id: Option<String>,
        config: &SessionConfig,
    ) -> Result<Self, CoreError> {
        config.validate_engine()?;
        let client = Arc::new(CdpClient::connect(&ws_url).await?);
        Self::attach_with_client(client, ws_url, target_id, config).await
    }

    pub(super) async fn attach_with_client(
        client: Arc<CdpClient>,
        ws_url: String,
        target_id: Option<String>,
        config: &SessionConfig,
    ) -> Result<Self, CoreError> {
        config.validate_engine()?;
        target::set_discover_targets(&client, true).await?;
        let popups = install_popup_tracker(&client);
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
        let dialog_state = install_dialog_handler(&client, &session_id, config.dialog_policy);

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
        let download_dir = download_directory(config);
        std::fs::create_dir_all(&download_dir).map_err(|error| {
            CoreError::NavigationFailed(format!("cannot create download directory: {error}"))
        })?;
        let downloads = install_download_tracker(&client);
        flashbrowse_cdp::protocol::browser::set_download_behavior(
            &client,
            &download_dir.to_string_lossy(),
        )
        .await?;
        page::set_download_behavior(&client, &session_id, &download_dir.to_string_lossy()).await?;
        let _: Value = a_r.unwrap_or(serde_json::json!({}));

        // Per-session settings: always re-apply (parallel).
        let stealth_js = stealth_script(config.stealth_profile);
        let (s_r, b_r, u_r) = tokio::join!(
            page::add_script_on_new_document(&client, &session_id, &stealth_js),
            network::set_blocked_urls(
                &client,
                &session_id,
                blocked_patterns(config.resource_policy),
            ),
            apply_stealth_profile(&client, &session_id, config.stealth_profile),
        );
        s_r?;
        b_r?;
        u_r?;
        if settle_shadow_enabled() {
            page::add_script_on_new_document(&client, &session_id, SETTLE_DIAGNOSTICS_SCRIPT)
                .await?;
        }

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
            action_sessions: HashMap::new(),
            title: String::new(),
            url: String::new(),
            last_tree: String::new(),
            last_settle: None,
            dialog_state,
            downloads,
            download_dir,
            popups,
        };
        sess.wait_for_popup_settle().await;
        sess.refresh_title_url().await;
        sess.rebuild_tree().await?;
        Ok(sess)
    }

    async fn wait_for_popup_settle(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut previous = None;
        let mut stable_since = std::time::Instant::now();
        loop {
            let state = self
                .driver
                .evaluate(SETTLE_DOM_STATS_JS)
                .await
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            let now = std::time::Instant::now();
            if let Some(state) = state {
                let mut parts = state.split('|');
                let complete = parts.next() == Some("complete");
                let dom_signature = (
                    parts.next().and_then(|v| v.parse::<usize>().ok()),
                    parts.next().and_then(|v| v.parse::<usize>().ok()),
                );
                if !complete {
                    previous = None;
                    stable_since = now;
                } else if previous != Some(dom_signature) {
                    previous = Some(dom_signature);
                    stable_since = now;
                } else if now.duration_since(stable_since) >= Duration::from_millis(1200) {
                    break;
                }
            } else {
                previous = None;
                stable_since = now;
            }
            if now >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn browser_ws_url(&self) -> &str {
        &self.browser_ws_url
    }

    pub fn pid(&self) -> Option<u32> {
        self.chrome.as_ref().and_then(|c| c.pid())
    }

    pub fn user_data_dir(&self) -> Option<&PathBuf> {
        self.chrome.as_ref().map(|c| c.user_data_dir())
    }

    pub fn detach(mut self) -> DetachedSession {
        let ws_url = self.browser_ws_url.clone();
        let tid = self.target_id.clone();
        let resource_policy = self.resource_policy;
        let (pid, user_data_dir) = match self.chrome.take() {
            Some(proc) => {
                let pid = proc.pid().unwrap_or(0);
                let dir = proc.user_data_dir().clone();
                if let Some((child, _dir)) = proc.detach() {
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

    /// Open a URL, wait for settlement, and return the rendered semantic tree.
    pub async fn open(&mut self, url: &str) -> Result<String, CoreError> {
        self.open_with_raw_snapshot(url)
            .await
            .map(|(rendered, _)| rendered)
    }

    /// Open a URL and return the raw AX snapshot used for the authoritative
    /// rendered tree.
    pub async fn open_with_raw_snapshot(
        &mut self,
        url: &str,
    ) -> Result<
        (
            String,
            Vec<flashbrowse_cdp::protocol::accessibility::AXNode>,
        ),
        CoreError,
    > {
        let t_open = std::time::Instant::now();
        let shadow_enabled = settle_shadow_enabled();
        let mut ax_complete_rx = self.client.events().subscribe("Accessibility.loadComplete");
        let mut lifecycle_rx = self.client.events().subscribe("Page.lifecycleEvent");
        let t_nav = std::time::Instant::now();
        let nav = page::navigate(&self.client, &self.session_id, url).await?;
        let nav_ms = t_nav.elapsed().as_millis() as u64;
        let nav_loader = nav.loader_id.clone();

        self.url = url.to_string();
        let client_c = self.client.clone();
        let sid = self.session_id.clone();
        let t_titles = std::time::Instant::now();
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
        let mut title_url_ms = t_titles.elapsed().as_millis() as u64;
        if self.url.starts_with("chrome-error://") {
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(self.url.clone()));
        }

        let mut raw_ax_nodes = 0usize;
        let mut nodes: Vec<SemanticNode> = Vec::new();
        let mut map = ActionMap::new();
        let mut action_sessions: HashMap<u32, FrameSession> = HashMap::new();
        let mut fingerprint = 0u64;
        let mut poll_ms = 0u64;
        let mut sleep_ms = 0u64;
        let mut ax_fetch_ms = 0u64;
        let mut compress_ms = 0u64;
        let mut iterations = 0u64;
        let mut ax_fetches = 0u64;
        let mut exit_reason = "deadline";
        let mut last_raw_nodes = Vec::new();
        let mut mutation_count = None;
        let mut mutation_last_ms = None;
        let mut mutation_quiet_ms = None;
        let mut mutation_would_delay_candidate = None;
        let mut candidate_accept_ms = None;
        let mut legacy_snapshot: Option<SettleSnapshot> = None;
        let mut candidate_nodes: Option<Vec<SemanticNode>> = None;

        const QUIET_MS_STATIC: u64 = 250;
        const QUIET_MS_DYNAMIC: u64 = 600;
        const QUIET_MS_SMALL_TREE: u64 = 1200;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(15000);
        let tick_ms: u64 = 100;
        let prefetch_lead_ms: u64 = 150;
        let mut last_change = tokio::time::Instant::now();
        let mut prev_dom = (0u64, 0u64);
        let mut legacy_saw_change = false;
        let mut candidate_saw_change = false;
        let mut ax_complete = false;
        let mut nav_committed = nav_loader.is_none();
        let commit_fallback = tokio::time::Instant::now() + Duration::from_millis(3000);

        loop {
            iterations += 1;
            if tokio::time::Instant::now() >= deadline {
                tracing::debug!(
                    raw_ax_nodes,
                    semantic = nodes.len(),
                    "settle deadline reached - using latest tree"
                );
                break;
            }
            while ax_complete_rx.try_recv().is_ok() {
                ax_complete = true;
            }
            while let Ok(ev) = lifecycle_rx.try_recv() {
                let matches = match nav_loader.as_deref() {
                    Some(loader) => ev.get("loaderId").and_then(|v| v.as_str()) == Some(loader),
                    None => true,
                };
                if matches && !nav_committed {
                    nav_committed = true;
                    last_change = tokio::time::Instant::now();
                }
            }
            if !nav_committed {
                if tokio::time::Instant::now() >= commit_fallback {
                    tracing::debug!("no commit lifecycle event; proceeding on own checks");
                    nav_committed = true;
                    last_change = tokio::time::Instant::now();
                } else {
                    if iterations > 1 {
                        let t_sleep = std::time::Instant::now();
                        tokio::time::sleep(Duration::from_millis(tick_ms)).await;
                        sleep_ms += t_sleep.elapsed().as_millis() as u64;
                    }
                    continue;
                }
            }

            let t_sleep = std::time::Instant::now();
            tokio::time::sleep(Duration::from_millis(tick_ms)).await;
            sleep_ms += t_sleep.elapsed().as_millis() as u64;

            let mut ready_state = String::from("loading");
            let mut dom_count = 0u64;
            let mut text_len = 0u64;
            let mut provably_static = false;
            let t_poll = std::time::Instant::now();
            let needs_static_classification = raw_ax_nodes < 100 && !legacy_saw_change;
            let cheap_expression = if shadow_enabled {
                format!(
                    "{SETTLE_DOM_STATS_JS} + '|' + (document.scripts.length === 0 && document.querySelectorAll('iframe').length === 0 && !document.querySelector('meta[http-equiv=refresh]')) + '|' + (window.__flashbrowseMutationState?.count ?? 0) + '|' + (window.__flashbrowseMutationState?.last_ms ?? 0) + '|' + (window.__flashbrowseMutationState ? performance.now() - window.__flashbrowseMutationState.last_ms : 0)"
                )
            } else if needs_static_classification {
                format!(
                    "{SETTLE_DOM_STATS_JS} + '|' + (document.scripts.length === 0 && document.querySelectorAll('iframe').length === 0 && !document.querySelector('meta[http-equiv=refresh]'))"
                )
            } else {
                format!("{SETTLE_DOM_STATS_JS} + '|false'")
            };
            if let Ok(cheap) =
                runtime::evaluate(&self.client, &self.session_id, &cheap_expression).await
            {
                if let Some(Value::String(s)) = cheap.result.value.as_ref() {
                    let mut it = s.split('|');
                    ready_state = it.next().unwrap_or("loading").to_string();
                    dom_count = it.next().unwrap_or("0").parse().unwrap_or(0);
                    text_len = it.next().unwrap_or("0").parse().unwrap_or(0);
                    provably_static = it.next().unwrap_or("false") == "true";
                    if shadow_enabled {
                        mutation_count = it.next().and_then(|v| v.parse().ok());
                        mutation_last_ms = it
                            .next()
                            .and_then(|v| v.parse::<f64>().ok())
                            .map(|ms| ms.max(0.0) as u64);
                        mutation_quiet_ms = it
                            .next()
                            .and_then(|v| v.parse::<f64>().ok())
                            .map(|ms| ms.max(0.0) as u64);
                    }
                }
            }
            poll_ms += t_poll.elapsed().as_millis() as u64;
            let page_complete = ready_state.eq_ignore_ascii_case("complete");
            let parsed = !ready_state.eq_ignore_ascii_case("loading");
            if !parsed {
                last_change = tokio::time::Instant::now();
                continue;
            }
            let cheap_now = (dom_count, text_len);
            if prev_dom == (0, 0) {
                prev_dom = cheap_now;
            } else if cheap_now != prev_dom {
                prev_dom = cheap_now;
                last_change = tokio::time::Instant::now();
                legacy_saw_change = true;
                candidate_saw_change = true;
                continue;
            }

            let legacy_quiet_ms = if legacy_saw_change {
                QUIET_MS_DYNAMIC
            } else if raw_ax_nodes < 100 && !provably_static {
                QUIET_MS_SMALL_TREE
            } else {
                QUIET_MS_STATIC
            };
            let candidate_quiet_ms = if shadow_enabled && candidate_saw_change {
                QUIET_MS_DYNAMIC
            } else if raw_ax_nodes < 100 && !provably_static {
                QUIET_MS_SMALL_TREE
            } else {
                QUIET_MS_STATIC
            };
            let legacy_quiet_enough =
                last_change.elapsed() >= Duration::from_millis(legacy_quiet_ms);
            let candidate_quiet_enough =
                last_change.elapsed() >= Duration::from_millis(candidate_quiet_ms);

            let tree_short = raw_ax_nodes == 0 || (raw_ax_nodes < 20 && dom_count >= 20);
            let sparse_final = tree_short && ax_complete && page_complete;
            let candidate_unready = tree_short && !sparse_final;
            let legacy_unready =
                raw_ax_nodes == 0 || (raw_ax_nodes < 20 && dom_count >= 20 && !sparse_final);

            let elapsed_ms = last_change.elapsed().as_millis() as u64;
            let prefetch_due = prefetch_lead_ms > 0
                && !candidate_quiet_enough
                && elapsed_ms + prefetch_lead_ms >= candidate_quiet_ms;

            if (legacy_snapshot.is_some() || (!legacy_unready && !legacy_quiet_enough))
                && (candidate_accept_ms.is_some()
                    || (!candidate_unready && !candidate_quiet_enough && !prefetch_due))
            {
                continue;
            }

            let t_fetch = std::time::Instant::now();
            let (fresh, node_sessions) = self.driver.get_full_ax_tree_with_sessions().await?;
            ax_fetch_ms += t_fetch.elapsed().as_millis() as u64;
            ax_fetches += 1;
            let t_compress = std::time::Instant::now();
            let raw_ax_nodes_before = raw_ax_nodes;
            raw_ax_nodes = fresh.len();
            let (n2, m2) = compress(&fresh);
            let fp2 = tree_fingerprint(&n2, &m2);
            let ratio_ok = dom_count < 100 || (raw_ax_nodes as u64) * 4 >= dom_count;
            let same = raw_ax_nodes_before == raw_ax_nodes && fp2 == fingerprint;
            action_sessions = build_action_sessions(&n2, &node_sessions);
            nodes = n2;
            map = m2;
            fingerprint = fp2;
            compress_ms += t_compress.elapsed().as_millis() as u64;

            let net_idle = self.net.idle(250);
            tracing::debug!(
                dom_count,
                text_len,
                raw_ax_nodes,
                semantic = nodes.len(),
                same,
                legacy_quiet_enough,
                candidate_quiet_enough,
                ax_complete,
                prefetch_due,
                sparse_final,
                net_idle,
                "settle poll"
            );

            let first_fetch = raw_ax_nodes_before == 0;
            let stable = same && ratio_ok && !nodes.is_empty() && (page_complete || net_idle);
            let legacy_stable = stable
                && (!first_fetch || !legacy_saw_change)
                && legacy_quiet_enough
                && !legacy_unready;
            let candidate_stable = stable
                && shadow_enabled
                && (!first_fetch || !candidate_saw_change)
                && candidate_quiet_enough
                && !candidate_unready;
            if legacy_stable && !shadow_enabled {
                last_raw_nodes = fresh;
                exit_reason = "settled";
                break;
            }
            if legacy_stable && legacy_snapshot.is_none() {
                legacy_snapshot = Some(SettleSnapshot {
                    nodes: nodes.clone(),
                    map: map.clone(),
                    action_sessions: action_sessions.clone(),
                    raw_nodes: fresh.clone(),
                });
            }
            if candidate_stable && candidate_accept_ms.is_none() {
                candidate_nodes = Some(nodes.clone());
                candidate_accept_ms = Some(t_open.elapsed().as_millis() as u64);
                mutation_would_delay_candidate =
                    mutation_quiet_ms.map(|quiet| quiet < candidate_quiet_ms);
            }
            if !same {
                last_change = tokio::time::Instant::now();
                if shadow_enabled && raw_ax_nodes_before > 0 {
                    candidate_saw_change = true;
                }
            }
            last_raw_nodes = fresh;
            if shadow_enabled && legacy_snapshot.is_some() && candidate_accept_ms.is_some() {
                exit_reason = "settled";
                break;
            }
        }

        let accepted_raw_nodes;
        if let Some(snapshot) = legacy_snapshot {
            nodes = snapshot.nodes;
            map = snapshot.map;
            action_sessions = snapshot.action_sessions;
            accepted_raw_nodes = snapshot.raw_nodes;
        } else {
            accepted_raw_nodes = last_raw_nodes;
        }

        if nodes.is_empty() {
            self.last_settle = Some(crate::observability::SettleBreakdown {
                nav_ms,
                poll_ms,
                sleep_ms,
                ax_fetch_ms,
                compress_ms,
                title_url_ms,
                render_ms: 0,
                iterations,
                ax_fetches,
                exit_reason: "empty".to_string(),
                candidate_accept_ms,
                legacy_tree_sha256: None,
                candidate_tree_sha256: None,
                shadow_diverged: None,
                raw_ax_nodes: raw_ax_nodes as u64,
                semantic_nodes: nodes.len() as u64,
                mutation_count,
                mutation_last_ms,
                mutation_quiet_ms,
                mutation_would_delay_candidate,
            });
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(format!(
                "page produced no accessibility content: {}",
                self.url
            )));
        }

        let t_titles_final = std::time::Instant::now();
        self.refresh_title_url().await;
        title_url_ms += t_titles_final.elapsed().as_millis() as u64;
        let t_render = std::time::Instant::now();
        let rendered = render_nested(&self.title, &self.url, &nodes);
        let render_ms = t_render.elapsed().as_millis() as u64;
        let (legacy_tree_sha256, candidate_tree_sha256, shadow_diverged) = if shadow_enabled {
            let legacy_hash = sha256_hex(rendered.as_bytes());
            let candidate_hash = candidate_nodes.as_ref().map(|candidate| {
                sha256_hex(render_nested(&self.title, &self.url, candidate).as_bytes())
            });
            let diverged = candidate_hash
                .as_ref()
                .map(|candidate| candidate != &legacy_hash);
            (Some(legacy_hash), candidate_hash, diverged)
        } else {
            (None, None, None)
        };
        self.last_settle = Some(crate::observability::SettleBreakdown {
            nav_ms,
            poll_ms,
            sleep_ms,
            ax_fetch_ms,
            compress_ms,
            title_url_ms,
            render_ms,
            iterations,
            ax_fetches,
            exit_reason: exit_reason.to_string(),
            candidate_accept_ms,
            legacy_tree_sha256,
            candidate_tree_sha256,
            shadow_diverged,
            raw_ax_nodes: raw_ax_nodes as u64,
            semantic_nodes: nodes.len() as u64,
            mutation_count,
            mutation_last_ms,
            mutation_quiet_ms,
            mutation_would_delay_candidate,
        });
        self.action_map = map;
        self.action_sessions = action_sessions;
        self.last_tree = rendered.clone();
        tracing::debug!(
            ms = t_open.elapsed().as_millis(),
            nodes = nodes.len(),
            settle = ?self.last_settle,
            "open complete"
        );
        Ok((rendered, accepted_raw_nodes))
    }

    /// Current action map (`ID -> BackendNodeId`).
    pub fn action_map(&self) -> &ActionMap {
        &self.action_map
    }

    /// Drop the action map and the rendered tree.
    fn invalidate_tree(&mut self) {
        self.action_map.clear();
        self.action_sessions.clear();
        self.last_tree.clear();
    }

    /// Last rendered tree (from the most recent rebuild).
    pub fn last_tree(&self) -> &str {
        &self.last_tree
    }

    /// Timing breakdown of the most recent `open`, when one has completed.
    pub fn last_settle(&self) -> Option<&crate::observability::SettleBreakdown> {
        self.last_settle.as_ref()
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

    /// Update the active resource policy via `Network.setBlockedURLs` without
    /// restarting the browser. Only updates session state after CDP succeeds.
    pub async fn set_resource_policy(&mut self, policy: ResourcePolicy) -> Result<(), CoreError> {
        if self.resource_policy == policy {
            return Ok(());
        }
        self.driver.set_resource_policy(policy).await?;
        self.resource_policy = policy;
        Ok(())
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
        let (raw, node_sessions) = self.driver.get_full_ax_tree_with_sessions().await?;
        let (nodes, map) = compress(&raw);
        let rendered = render_nested(&self.title, &self.url, &nodes);
        self.action_sessions = build_action_sessions(&nodes, &node_sessions);
        self.action_map = map;
        self.last_tree = rendered.clone();
        Ok(rendered)
    }

    pub(super) fn frame_for_action(&self, action_id: u32) -> (String, Option<i64>) {
        match self.action_sessions.get(&action_id) {
            Some(fs) => (fs.session_id.clone(), fs.owner_backend),
            None => (self.session_id.clone(), None),
        }
    }

    pub(super) async fn frame_offset(&self, owner_backend: Option<i64>) -> (f64, f64) {
        let Some(owner) = owner_backend else {
            return (0.0, 0.0);
        };
        if let Ok(resp) = dom::get_box_model(&self.client, &self.session_id, owner).await {
            if resp.model.content.len() >= 2 {
                return (resp.model.content[0], resp.model.content[1]);
            }
        }
        (0.0, 0.0)
    }

    /// `click(action_id)`: scroll -> box -> center -> mouse events -> rebuild.
    pub async fn click(&mut self, action_id: u32) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, owner) = self.frame_for_action(action_id);
        self.click_at_backend(&dom_sid, owner, backend).await?;
        self.post_action_settle(Duration::from_millis(400)).await;
        self.refresh_title_url().await;
        self.rebuild_tree().await
    }

    /// `hover(action_id)`: move mouse to element center without clicking -> rebuild.
    pub async fn hover(&mut self, action_id: u32) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, owner) = self.frame_for_action(action_id);
        if let Err(e) = dom::scroll_into_view_if_needed(&self.client, &dom_sid, backend).await {
            tracing::warn!(backend, "scrollIntoViewIfNeeded failed (proceeding): {e}");
        }
        let resp = dom::get_box_model(&self.client, &dom_sid, backend).await?;
        let (cx, cy) = box_center(&resp.model).ok_or(CoreError::NoBoxModel(action_id))?;
        let (ox, oy) = self.frame_offset(owner).await;
        let (x, y) = (cx + ox, cy + oy);
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
        self.post_action_settle(Duration::from_millis(200)).await;
        self.rebuild_tree().await
    }

    pub(super) async fn fill_backend(
        &self,
        dom_sid: &str,
        owner: Option<i64>,
        backend: i64,
        text: &str,
    ) -> Result<(), CoreError> {
        self.click_at_backend(dom_sid, owner, backend).await?;
        for ev in [select_all_event(), backspace_event()] {
            let down = ev.clone();
            input::dispatch_key_event(&self.client, &self.session_id, &down).await?;
            let mut up = down;
            up.type_ = "keyUp".to_string();
            input::dispatch_key_event(&self.client, &self.session_id, &up).await?;
        }
        input::insert_text(&self.client, &self.session_id, text).await?;
        Ok(())
    }

    /// `fill(action_id, text)`: focus -> Ctrl+A -> Backspace -> insert -> rebuild.
    pub async fn fill(&mut self, action_id: u32, text: &str) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, owner) = self.frame_for_action(action_id);
        self.fill_backend(&dom_sid, owner, backend, text).await?;
        self.post_action_settle(Duration::from_millis(200)).await;
        self.rebuild_tree().await
    }

    /// Fill multiple form fields sequentially from the current action map,
    /// stopping on the first unavailable/failing field and always returning
    /// the refreshed final tree.
    pub async fn fill_form(
        &mut self,
        fields: &[(u32, String)],
    ) -> Result<FillFormOutcome, CoreError> {
        let mut completed = Vec::with_capacity(fields.len());
        let mut failed = None;
        for (action_id, value) in fields {
            let Some(&backend) = self.action_map.get(action_id) else {
                failed = Some((
                    *action_id,
                    CoreError::ActionNotFound(*action_id).to_string(),
                ));
                break;
            };
            let (dom_sid, owner) = self.frame_for_action(*action_id);
            if let Err(err) = self.fill_backend(&dom_sid, owner, backend, value).await {
                failed = Some((*action_id, err.to_string()));
                break;
            }
            completed.push(*action_id);
        }
        self.post_action_settle(Duration::from_millis(250)).await;
        let tree = self.rebuild_tree().await?;
        Ok(FillFormOutcome {
            completed,
            failed,
            tree,
        })
    }

    /// `scroll(direction, amount_px)`: synthetic `mouseWheel` -> rebuild.
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
        self.post_action_settle(Duration::from_millis(300)).await;
        self.rebuild_tree().await
    }

    /// `select(action_id, text)`: pick a `<select>` option by visible text
    /// or value, dispatch `change` + `input`, then rebuild.
    pub async fn select(&mut self, action_id: u32, text: &str) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, _) = self.frame_for_action(action_id);
        let resolved = dom::resolve_node(&self.client, &dom_sid, backend).await?;
        let object_id = resolved
            .object
            .object_id
            .ok_or(CoreError::NoBackendId(action_id))?;
        let needle = serde_json::to_string(text)?;
        let resp = runtime::call_function_on(
            &self.client,
            &dom_sid,
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
        self.post_action_settle(Duration::from_millis(200)).await;
        self.rebuild_tree().await
    }

    /// `press_key(key)`: single key down+up -> rebuild.
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
        self.post_action_settle(Duration::from_millis(250)).await;
        self.refresh_title_url().await;
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

    /// Drain pending RPCs, kill Chromium (if owned), drop profile dir.
    /// Attached sessions (session-file mode) only disconnect.
    pub async fn shutdown(mut self) {
        self.client.shutdown();
        if let Some(mut proc) = self.chrome.take() {
            proc.shutdown().await;
        }
    }
}
