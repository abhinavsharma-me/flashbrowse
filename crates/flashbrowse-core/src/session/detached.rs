//! Detached session persistence metadata.

use std::path::PathBuf;

use crate::resource_policy::ResourcePolicy;

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
