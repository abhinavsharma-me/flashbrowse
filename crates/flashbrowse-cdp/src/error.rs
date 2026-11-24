//! Central error type for flashbrowse-cdp.
//!
//! All public fallible functions return [`CdpError`]. Variants are grouped by
//! layer: launcher (no binary, spawn, stderr), transport (connect, timeout,
//! disconnect, protocol), and JSON errors from serde.

use std::path::PathBuf;

/// Errors produced by launcher, transport and protocol layers.
#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    #[error("no Chromium binary found (searched: {searched:?})")]
    NoChromeBinary {
        /// Human-readable list of locations checked, in order.
        searched: Vec<String>,
    },

    #[error("failed to spawn Chromium at {path}: {source}")]
    SpawnFailed {
        /// Path that was attempted.
        path: PathBuf,
        #[source]
        /// Underlying OS error.
        source: std::io::Error,
    },

    #[error("timed out waiting for DevTools WebSocket URL on stderr after {seconds}s")]
    DevToolsTimeout {
        /// Timeout in seconds.
        seconds: u64,
    },

    #[error("Chromium exited before publishing DevTools WebSocket URL (code: {code:?})")]
    ChromiumExitedEarly {
        /// Exit status code if available.
        code: Option<i32>,
    },

    #[error("failed to connect WebSocket to {url}: {detail}")]
    WsConnectFailed {
        /// Target WebSocket URL.
        url: String,
        /// Underlying error rendered by tungstenite.
        detail: String,
    },

    #[error("CDP call timed out: {method}")]
    Timeout {
        /// Method that was called.
        method: String,
    },

    #[error("invalid CDP endpoint {endpoint}: {detail}")]
    InvalidCdpEndpoint { endpoint: String, detail: String },

    #[error("CDP socket disconnected while awaiting response for {method}")]
    Disconnected {
        /// Method that was in flight.
        method: String,
    },

    #[error("CDP protocol error for {method}: code {code}: {message}")]
    ProtocolError {
        /// Method that was called.
        method: String,
        /// CDP error code.
        code: i64,
        /// CDP error message.
        message: String,
    },

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
