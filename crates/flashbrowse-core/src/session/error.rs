//! Session-level error definitions.

use flashbrowse_cdp::CdpError;

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
