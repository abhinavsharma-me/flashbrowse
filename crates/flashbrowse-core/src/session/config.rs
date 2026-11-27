//! Session launch configuration and policy parameters.

use std::path::PathBuf;

use crate::engine::BrowserEngine;
use crate::resource_policy::ResourcePolicy;
use crate::stealth::StealthProfile;

use super::dialog::DialogPolicy;
use super::error::CoreError;

/// Default cap on screenshot prepare time (milliseconds).
pub const SCREENSHOT_PREPARE_MS_DEFAULT: u64 = 8_000;

/// Hard ceiling on screenshot prepare time (milliseconds).
///
/// An agent could otherwise ask for a 24-hour wait and pin the (serial) MCP
/// server for the whole time. 60 s is far beyond any real page and still
/// bounded.
pub const SCREENSHOT_PREPARE_MS_MAX: u64 = 60_000;

/// Session launch configuration.
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// Explicit Chrome path (`--chrome-path` / `CHROME_PATH`).
    pub chrome_path: Option<PathBuf>,
    /// Network resource load policy (AX tree format is unchanged).
    pub resource_policy: ResourcePolicy,
    /// Browser engine (`firefox` requires `flashbrowse-bidi`, not yet wired in CLI).
    pub engine: BrowserEngine,
    /// Pass `--no-sandbox` (containers/CI). `None` -> `FLASHBROWSE_NO_SANDBOX` env.
    pub no_sandbox: Option<bool>,
    /// Extra Chromium CLI args.
    pub extra_args: Vec<String>,
    /// Keep Chromium alive after parent CLI exit (`open --session-id`).
    pub persistent: bool,
    /// Default disposition for native JavaScript dialogs.
    pub dialog_policy: DialogPolicy,
    /// Use an existing Chrome profile when launching a managed browser.
    pub user_data_dir: Option<PathBuf>,
    /// Attach to an existing Chrome debugging endpoint instead of launching.
    pub cdp_url: Option<String>,
    /// Root directory allowed for MCP storage-state files.
    pub storage_state_root: Option<PathBuf>,
    /// Root for MCP upload inputs and browser downloads.
    pub mcp_file_root: Option<PathBuf>,
    /// Stealth User-Agent/platform/Client-Hints profile (`host-native` default).
    pub stealth_profile: StealthProfile,
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
        if self.cdp_url.is_some() && self.user_data_dir.is_some() {
            return Err(CoreError::NavigationFailed(
                "--cdp-url and --user-data-dir cannot be used together".into(),
            ));
        }
        Ok(())
    }
}

/// Result of [`BrowserSession::fill_form`].
#[derive(Debug, Clone)]
pub struct FillFormOutcome {
    /// Action IDs that were filled before completion or failure.
    pub completed: Vec<u32>,
    /// `(action_id, error_message)` if the batch stopped early on a field.
    pub failed: Option<(u32, String)>,
    /// Authoritative refreshed semantic tree after the batch.
    pub tree: String,
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
            timeout_ms: SCREENSHOT_PREPARE_MS_DEFAULT,
        }
    }
}
