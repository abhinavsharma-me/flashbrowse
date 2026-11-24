//! MCP tool definitions and execution.

use std::sync::Arc;

use flashbrowse_core::actions::ScrollDirection;
use flashbrowse_core::{
    normalize_url, BrowserSession, ResourcePolicy, ScreenshotOptions, SessionConfig,
    SCREENSHOT_PREPARE_MS_DEFAULT, SCREENSHOT_PREPARE_MS_MAX,
};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::protocol::{McpError, ToolResult};

/// The single browser session shared by all `tools/call` invocations.
pub type SessionHandle = Arc<Mutex<Option<BrowserSession>>>;

/// Parameter schema for tools that need no arguments except an explicit
/// `{}` object is still valid per MCP's `inputSchema` requirement.
fn empty_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// Tool definitions exposed via `tools/list`.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        serde_json::json!({
            "name": "browser_open",
            "description": "Navigates to a URL, applies stealth and network filtering, and returns the token-compressed, numbered semantic action tree. Use browser_screenshot on this same session for visuals (no second browser tool).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Target URL. The scheme may be omitted: https is assumed, and local hosts (localhost, 127.0.0.1, LAN addresses) use http. Allowed schemes: http, https, data, about:blank"},
                    "text_only": {"type": "boolean", "description": "Legacy: selects text-only resource policy", "default": false},
                    "resource_policy": {"type": "string", "enum": ["agent-default", "text-only", "full-render"], "description": "Network load policy (tree size unchanged); full-render loads images/fonts for faithful screenshots"}
                },
                "required": ["url"]
            }
        }),
        serde_json::json!({
            "name": "browser_act",
            "description": "Executes an interaction (click, fill, hover, press_key, scroll) against a numbered action element ID.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action_id": {"type": "integer", "description": "Element ID number from the action tree"},
                    "action": {"type": "string", "enum": ["click", "fill", "hover", "press_key", "scroll", "select"], "description": "Interaction to perform"},
                    "value": {"type": "string", "description": "Text payload for 'fill', key name for 'press_key', direction for 'scroll', or option label for 'select'"},
                    "amount": {"type": "number", "description": "Pixel distance for 'scroll' (default 300)"},
                    "dialog_action": {"type": "string", "enum": ["accept", "dismiss"], "description": "Optional disposition for a native JavaScript dialog opened by this action"},
                    "prompt_text": {"type": "string", "description": "Text supplied only when dialog_action is accept and a prompt opens"}
                },
                "required": ["action_id", "action"]
            }
        }),
        serde_json::json!({
            "name": "browser_extract_markdown",
            "description": "Extracts readable article and document text formatted as clean Markdown, stripping all boilerplate.",
            "inputSchema": empty_schema()
        }),
        serde_json::json!({
            "name": "browser_evaluate",
            "description": "Evaluate a JavaScript expression in the live page and return the JSON-serialized result. Used for task success checks and diagnostics.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expression": {"type": "string", "description": "JS expression evaluated in the page (returnByValue)"}
                },
                "required": ["expression"]
            }
        }),
        serde_json::json!({
            "name": "browser_screenshot",
            "description": "Captures a viewport or full-page PNG of the current session (after browser_open/browser_act). Waits for images/network to settle. Use this tool instead of a separate browser MCP.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "full_page": {"type": "boolean", "default": false},
                    "prepare_lazy_content": {"type": "boolean", "description": "Scroll the page to surface lazy-loaded images", "default": true},
                    "timeout_ms": {"type": "integer", "description": "Max ms to wait before best-effort capture (500-60000)", "default": 8000}
                }
            }
        }),
        serde_json::json!({
            "name": "browser_tabs",
            "description": "Lists, selects, or closes pages in the current browser session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "select", "close"]},
                    "target_id": {"type": "string", "description": "Page target ID returned by list"}
                },
                "required": ["action"]
            }
        }),
        serde_json::json!({
            "name": "browser_storage_state",
            "description": "Imports or exports browser cookies and per-origin localStorage to a named JSON file confined to the configured storage-state root.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["import", "export"]},
                    "name": {"type": "string", "description": "Simple file name without a path or extension"}
                },
                "required": ["action", "name"]
            }
        }),
        serde_json::json!({
            "name": "browser_upload",
            "description": "Uploads one or more files to a file input. Paths are resolved inside the configured MCP file root.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action_id": {"type": "integer", "description": "Action ID of an input[type=file]"},
                    "files": {"type": "array", "items": {"type": "string"}, "description": "File names or relative paths under the configured MCP file root"}
                },
                "required": ["action_id", "files"]
            }
        }),
        serde_json::json!({
            "name": "browser_downloads",
            "description": "Lists download progress or waits for one download to complete or fail. Downloads are saved under the configured MCP file root's downloads directory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "wait"], "default": "list"},
                    "guid": {"type": "string", "description": "Download identifier returned by the list action"},
                    "timeout_ms": {"type": "integer", "default": 10000, "description": "Maximum wait, capped at 20000 ms"}
                }
            }
        }),
    ]
}

/// Parse `text_only` from args (default `false`).
fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Parse required string arg, or `InvalidParams`.
fn arg_string(args: &Value, key: &str) -> Result<String, McpError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| McpError::InvalidParams(format!("missing required '{key}'")))
}

/// Parse optional string arg.
fn arg_opt_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Parse required integer arg (`action_id`), rejecting overflow.
fn arg_u32(args: &Value, key: &str) -> Result<u32, McpError> {
    let raw = args
        .get(key)
        .ok_or_else(|| McpError::InvalidParams(format!("missing required '{key}'")))?;
    let n = raw
        .as_u64()
        .ok_or_else(|| McpError::InvalidParams(format!("'{key}' must be an integer")))?;
    u32::try_from(n).map_err(|_| McpError::InvalidParams(format!("'{key}' out of range (u32 max)")))
}

/// Resolve the resource policy for a `browser_open` call.
///
/// An unparseable `resource_policy` is an error, not a silent fallback: the
/// agent asked for a specific network policy and would otherwise get a
/// different one with no indication.
fn arg_resource_policy(
    arguments: &Value,
    default_config: &SessionConfig,
) -> Result<ResourcePolicy, McpError> {
    if let Some(s) = arguments.get("resource_policy").and_then(|v| v.as_str()) {
        return s.parse::<ResourcePolicy>().map_err(McpError::InvalidParams);
    }
    if arguments
        .get("text_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(ResourcePolicy::TextOnly);
    }
    Ok(default_config.resource_policy)
}

async fn ensure_session<'a>(
    handle: &'a SessionHandle,
    default_config: &SessionConfig,
    want_policy: ResourcePolicy,
) -> Result<tokio::sync::MutexGuard<'a, Option<BrowserSession>>, McpError> {
    let mut guard = handle.lock().await;
    let needs_relaunch = match guard.as_ref() {
        Some(s) => s.resource_policy() != want_policy,
        None => true,
    };
    if needs_relaunch {
        if let Some(old) = guard.take() {
            old.shutdown().await;
        }
        let mut cfg = default_config.clone();
        cfg.resource_policy = want_policy;
        let fresh = BrowserSession::launch(cfg)
            .await
            .map_err(|e| McpError::Internal(e.to_string()))?;
        *guard = Some(fresh);
    }
    Ok(guard)
}

fn require_session(guard: &mut Option<BrowserSession>) -> Result<&mut BrowserSession, McpError> {
    guard.as_mut().ok_or_else(|| {
        McpError::Internal("no browser session: call browser_open first".to_string())
    })
}

/// Execute one `tools/call`. Returns a `ToolResult` (success or `isError`).
///
/// Every call is timed and appended to the metrics trail so operators can
/// see latency, token cost, and error rate per tool.
pub async fn call_tool(
    name: &str,
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let started = std::time::Instant::now();
    let result = match name {
        "browser_open" => call_open(arguments, handle, default_config).await,
        "browser_act" => call_act(arguments, handle, default_config).await,
        "browser_extract_markdown" => call_markdown(handle).await,
        "browser_evaluate" => call_evaluate(arguments, handle).await,
        "browser_screenshot" => call_screenshot(arguments, handle).await,
        "browser_tabs" => call_tabs(arguments, handle, default_config).await,
        "browser_storage_state" => call_storage_state(arguments, handle, default_config).await,
        "browser_upload" => call_upload(arguments, handle, default_config).await,
        "browser_downloads" => call_downloads(arguments, handle).await,
        other => Err(McpError::InvalidParams(format!("unknown tool '{other}'"))),
    };
    // `open` is the only tool that owns a settle; read it back from the
    // session (the handler has released the lock by now).
    let settle = if name == "browser_open" {
        let guard = handle.lock().await;
        guard.as_ref().and_then(|s| s.last_settle().cloned())
    } else {
        None
    };
    record_tool_metric(name, arguments, &result, started, settle);
    result
}

/// Payload size (chars) and actionable-element count of a tool result.
fn payload_of(result: &Result<ToolResult, McpError>) -> (usize, usize) {
    let Ok(tr) = result else {
        return (0, 0);
    };
    let mut chars = 0usize;
    let mut actions = 0usize;
    for item in &tr.content {
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            chars += text.len();
            actions += flashbrowse_core::observability::count_actions(text);
        }
    }
    (chars, actions)
}

/// Append one MCP tool call to the metrics trail.
fn record_tool_metric(
    name: &str,
    arguments: &Value,
    result: &Result<ToolResult, McpError>,
    started: std::time::Instant,
    settle: Option<flashbrowse_core::observability::SettleBreakdown>,
) {
    use flashbrowse_core::observability;

    let target = arguments
        .get("url")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            arguments
                .get("action")
                .and_then(|a| a.as_str())
                .map(|a| format!("mcp:{a}"))
                .unwrap_or_else(|| format!("mcp:{name}"))
        });
    let (chars, actions) = payload_of(result);
    let (ok, error) = match result {
        Ok(tr) => (tr.is_error != Some(true), None),
        Err(e) => (false, Some(e.to_string())),
    };
    let r = observability::record_op_with_settle(
        &format!("mcp:{name}"),
        &target,
        started,
        Some((chars, actions)),
        ok,
        error,
        settle,
    );
    tracing::debug!(
        tool = name,
        ms = r.ms,
        tokens = r.tokens,
        actions = r.actions,
        ok = r.ok,
        "mcp tool call"
    );
}

/// MCP URL allowlist (H4 fix): agent boundary must not read local files.
/// Only `http(s)://`, `data:`, and `about:blank` are navigable via MCP.
/// CLI commands keep full `file://` support (local-user trust boundary).
///
/// Expects an already-normalized URL: `resolve_mcp_url` adds the scheme for a
/// bare host first, so the shorthand `example.com` is judged as the
/// `https://example.com` it becomes rather than rejected for having no scheme.
fn validate_mcp_url(url: &str) -> Result<(), McpError> {
    let t = url.trim();
    if t.is_empty() {
        return Err(McpError::InvalidParams("url must not be empty".to_string()));
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || lower == "about:blank"
    {
        // Reject file/chrome/view-source smuggling via whitespace/control.
        if t.chars().any(|c| c.is_control()) {
            return Err(McpError::InvalidParams(
                "url contains control characters".to_string(),
            ));
        }
        return Ok(());
    }
    Err(McpError::InvalidParams(format!(
        "url scheme not allowed via MCP (allowed: http, https, data, about:blank): {t}"
    )))
}

/// Resolve a caller's URL, then apply the MCP allowlist to the result.
///
/// Order matters: normalizing first means `example.com` is checked as
/// `https://example.com`, while `file:///etc/passwd` keeps its scheme and is
/// still rejected by the allowlist.
fn resolve_mcp_url(raw: &str) -> Result<String, McpError> {
    let url = normalize_url(raw).map_err(|e| McpError::InvalidParams(e.to_string()))?;
    validate_mcp_url(&url)?;
    Ok(url)
}

async fn call_open(
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let raw = arg_string(arguments, "url")?;
    let url = resolve_mcp_url(&raw)?;
    let policy = arg_resource_policy(arguments, default_config)?;
    let mut guard = ensure_session(handle, default_config, policy).await?;
    let session = require_session(&mut guard)?;
    let opener = session.target_id().to_string();
    let existing: std::collections::HashSet<String> = session
        .list_tabs()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.target_id)
        .collect();
    match session.open(&url).await {
        Ok(tree) => {
            let mut cfg = default_config.clone();
            cfg.resource_policy = session.resource_policy();
            match focus_new_popup(session, &opener, &existing, &cfg).await {
                Ok(Some(popup_tree)) => Ok(ToolResult::text(popup_tree)),
                Ok(None) => Ok(ToolResult::text(tree)),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            }
        }
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

async fn focus_new_popup(
    session: &mut BrowserSession,
    opener: &str,
    existing: &std::collections::HashSet<String>,
    config: &SessionConfig,
) -> Result<Option<String>, flashbrowse_core::CoreError> {
    let tabs = session.list_tabs().await?;
    let popup = tabs.iter().rev().find(|t| {
        t.kind == "page"
            && t.opener_id.as_deref() == Some(opener)
            && !existing.contains(&t.target_id)
    });
    match popup {
        Some(tab) => session.select_tab(&tab.target_id, config).await.map(Some),
        None => Ok(None),
    }
}

async fn call_tabs(
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action = arg_string(arguments, "action")?;
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    let mut config = default_config.clone();
    config.resource_policy = session.resource_policy();
    match action.as_str() {
        "list" => {
            let active = session.target_id().to_string();
            let tabs = session
                .list_tabs()
                .await
                .map_err(|e| McpError::Internal(e.to_string()))?;
            let rows: Vec<Value> = tabs
                .into_iter()
                .map(|t| {
                    serde_json::json!({
                        "target_id": t.target_id,
                        "title": t.title,
                        "url": t.url,
                        "active": t.target_id == active
                    })
                })
                .collect();
            Ok(ToolResult::text(
                serde_json::to_string(&rows).unwrap_or_default(),
            ))
        }
        "select" => {
            let id = arg_string(arguments, "target_id")?;
            session
                .select_tab(&id, &config)
                .await
                .map(ToolResult::text)
                .map_err(|e| McpError::InvalidParams(e.to_string()))
        }
        "close" => {
            let id = arg_string(arguments, "target_id")?;
            session
                .close_tab(&id, &config)
                .await
                .map(|tree| {
                    tree.map(ToolResult::text)
                        .unwrap_or_else(|| ToolResult::text("Tab closed".to_string()))
                })
                .map_err(|e| McpError::InvalidParams(e.to_string()))
        }
        _ => Err(McpError::InvalidParams(
            "action must be list, select, or close".into(),
        )),
    }
}

fn storage_state_path(config: &SessionConfig, name: &str) -> Result<std::path::PathBuf, McpError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || name.ends_with(".json")
    {
        return Err(McpError::InvalidParams(
            "name must be a simple file name without a path or extension".into(),
        ));
    }
    let root = config
        .storage_state_root
        .clone()
        .unwrap_or_else(|| flashbrowse_core::paths::cache_dir().join("storage-state"));
    std::fs::create_dir_all(&root).map_err(|error| McpError::Internal(error.to_string()))?;
    let root =
        std::fs::canonicalize(&root).map_err(|error| McpError::Internal(error.to_string()))?;
    let path = root.join(format!("{name}.json"));
    if path.exists() {
        let canonical =
            std::fs::canonicalize(&path).map_err(|error| McpError::Internal(error.to_string()))?;
        if !canonical.starts_with(&root) {
            return Err(McpError::InvalidParams(
                "storage-state file resolves outside the configured root".into(),
            ));
        }
    }
    Ok(path)
}

fn mcp_file_root(config: &SessionConfig) -> Result<std::path::PathBuf, McpError> {
    let root = config
        .mcp_file_root
        .clone()
        .unwrap_or_else(|| flashbrowse_core::paths::cache_dir().join("files"));
    std::fs::create_dir_all(&root).map_err(|error| McpError::Internal(error.to_string()))?;
    std::fs::canonicalize(root).map_err(|error| McpError::Internal(error.to_string()))
}

fn resolve_upload_files(
    config: &SessionConfig,
    values: &[Value],
) -> Result<Vec<std::path::PathBuf>, McpError> {
    if values.is_empty() || values.len() > 20 {
        return Err(McpError::InvalidParams(
            "files must contain between 1 and 20 file paths".into(),
        ));
    }
    let root = mcp_file_root(config)?;
    values
        .iter()
        .map(|value| {
            let raw = value.as_str().ok_or_else(|| {
                McpError::InvalidParams("each files item must be a path string".into())
            })?;
            let candidate = std::path::Path::new(raw);
            let candidate = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                root.join(candidate)
            };
            let canonical = std::fs::canonicalize(candidate).map_err(|error| {
                McpError::InvalidParams(format!("cannot access upload file: {error}"))
            })?;
            if !canonical.starts_with(&root) || !canonical.is_file() {
                return Err(McpError::InvalidParams(
                    "upload files must be regular files within the configured MCP file root".into(),
                ));
            }
            Ok(canonical)
        })
        .collect()
}

async fn call_storage_state(
    arguments: &Value,
    handle: &SessionHandle,
    config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action = arg_string(arguments, "action")?;
    let name = arg_string(arguments, "name")?;
    let path = storage_state_path(config, &name)?;
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match action.as_str() {
        "export" => {
            let state = session
                .export_storage_state()
                .await
                .map_err(|error| McpError::Internal(error.to_string()))?;
            let encoded = serde_json::to_vec_pretty(&state)
                .map_err(|error| McpError::Internal(error.to_string()))?;
            tokio::fs::write(&path, encoded)
                .await
                .map_err(|error| McpError::Internal(error.to_string()))?;
            Ok(ToolResult::text(format!(
                "Storage state exported as {name}"
            )))
        }
        "import" => {
            let encoded = tokio::fs::read(&path).await.map_err(|error| {
                McpError::InvalidParams(format!("cannot read storage state: {error}"))
            })?;
            let state: Value = serde_json::from_slice(&encoded).map_err(|error| {
                McpError::InvalidParams(format!("invalid storage-state JSON: {error}"))
            })?;
            session
                .import_storage_state(&state)
                .await
                .map(ToolResult::text)
                .map_err(|error| McpError::Internal(error.to_string()))
        }
        _ => Err(McpError::InvalidParams(
            "action must be import or export".into(),
        )),
    }
}

async fn call_upload(
    arguments: &Value,
    handle: &SessionHandle,
    config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action_id = arg_u32(arguments, "action_id")?;
    let values = arguments
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::InvalidParams("files must be an array of paths".into()))?;
    let files = resolve_upload_files(config, values)?;
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    session
        .upload_files(action_id, &files)
        .await
        .map(ToolResult::text)
        .map_err(|error| McpError::InvalidParams(error.to_string()))
}

async fn call_downloads(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
    let action = arg_opt_string(arguments, "action").unwrap_or_else(|| "list".into());
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    if action == "wait" {
        let guid = arg_string(arguments, "guid")?;
        let timeout_ms = arguments
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(10_000)
            .clamp(100, 20_000);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some(download) = session
                .downloads()
                .into_iter()
                .find(|item| item.guid == guid)
            {
                if download.state != "inProgress" || tokio::time::Instant::now() >= deadline {
                    return Ok(ToolResult::text(
                        serde_json::to_string(&download).unwrap_or_default(),
                    ));
                }
            } else if tokio::time::Instant::now() >= deadline {
                return Err(McpError::InvalidParams(format!(
                    "unknown download guid: {guid}"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    if action != "list" {
        return Err(McpError::InvalidParams(
            "action must be list or wait".into(),
        ));
    }
    let result = serde_json::json!({
        "download_dir": session.download_dir(),
        "downloads": session.downloads(),
    });
    Ok(ToolResult::text(
        serde_json::to_string(&result).unwrap_or_default(),
    ))
}

async fn call_act(
    arguments: &Value,
    handle: &SessionHandle,
    default_config: &SessionConfig,
) -> Result<ToolResult, McpError> {
    let action_id = arg_u32(arguments, "action_id")?;
    let action = arg_string(arguments, "action")?;
    let value = arg_opt_string(arguments, "value").unwrap_or_default();

    let mut guard = handle.lock().await;
    // Reuse existing session; if none, this is an error (must open first).
    if guard.is_none() {
        // Try launching with defaults so a fresh agent can act after open-less flow?
        // Spec ordering is open→act; surface a clear error instead of silent launch.
        return Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        ));
    }
    // Touch default_config so the launch path stays explicit for future use.
    let _ = default_config;
    let session = guard.as_mut().expect("session present");
    let opener = session.target_id().to_string();
    let existing: std::collections::HashSet<String> = session
        .list_tabs()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.target_id)
        .collect();

    if action == "fill" && arguments.get("value").is_none() {
        return Ok(ToolResult::error(
            "fill requires 'value' (text payload, may be empty to clear)".to_string(),
        ));
    }
    if action == "press_key" && value.is_empty() {
        return Ok(ToolResult::error(
            "press_key requires 'value' (key name, e.g. Enter, ArrowDown, a)".to_string(),
        ));
    }
    if action == "select" && arguments.get("value").is_none() {
        return Ok(ToolResult::error(
            "select requires 'value' (option text or value)".to_string(),
        ));
    }
    let prompt_text = arg_opt_string(arguments, "prompt_text");
    if let Some(dialog_action) = arg_opt_string(arguments, "dialog_action") {
        match dialog_action.as_str() {
            "accept" => session.set_dialog_override(true, prompt_text),
            "dismiss" if prompt_text.is_none() => session.set_dialog_override(false, None),
            "dismiss" => {
                return Ok(ToolResult::error(
                    "prompt_text requires dialog_action=accept".into(),
                ))
            }
            _ => {
                return Ok(ToolResult::error(
                    "dialog_action must be accept or dismiss".into(),
                ))
            }
        }
    } else if prompt_text.is_some() {
        return Ok(ToolResult::error(
            "prompt_text requires dialog_action=accept".into(),
        ));
    }

    let result = match action.as_str() {
        "click" => session.click(action_id).await,
        "fill" => session.fill(action_id, &value).await,
        "hover" => session.hover(action_id).await,
        "press_key" => session.press_key(&value).await,
        "select" => session.select(action_id, &value).await,
        "scroll" => {
            let direction = if value.is_empty() {
                ScrollDirection::Down
            } else {
                match ScrollDirection::parse(&value) {
                    Some(d) => d,
                    None => {
                        return Ok(ToolResult::error(format!(
                            "invalid scroll direction '{value}' (expected up|down|left|right)"
                        )))
                    }
                }
            };
            let amount = arguments
                .get("amount")
                .and_then(|v| v.as_f64())
                .unwrap_or(300.0);
            session.scroll(direction, amount).await
        }
        other => {
            return Ok(ToolResult::error(format!(
                "unknown action '{other}' (expected click|fill|hover|press_key|scroll|select)"
            )))
        }
    };
    session.clear_dialog_override();
    let dialog = session.take_dialog_info();
    match result {
        Ok(tree) => {
            let tree = if let Some(dialog) = dialog {
                format!(
                    "{tree}\n\n[handled JavaScript {} dialog: {} ({})]",
                    dialog.dialog_type,
                    if dialog.accepted {
                        "accepted"
                    } else {
                        "dismissed"
                    },
                    dialog.message
                )
            } else {
                tree
            };
            let mut config = default_config.clone();
            config.resource_policy = session.resource_policy();
            match focus_new_popup(session, &opener, &existing, &config).await {
                Ok(Some(popup_tree)) => Ok(ToolResult::text(popup_tree)),
                Ok(None) => Ok(ToolResult::text(tree)),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            }
        }
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

async fn call_evaluate(arguments: &Value, handle: &SessionHandle) -> Result<ToolResult, McpError> {
    let expression = arg_string(arguments, "expression")?;
    if expression.is_empty() {
        return Err(McpError::InvalidParams(
            "expression must not be empty".to_string(),
        ));
    }
    let mut guard = handle.lock().await;
    let session = require_session(&mut guard)?;
    match session.evaluate(&expression).await {
        Ok(value) => Ok(ToolResult::text(value.to_string())),
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

async fn call_markdown(handle: &SessionHandle) -> Result<ToolResult, McpError> {
    let guard = handle.lock().await;
    match guard.as_ref() {
        None => Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        )),
        Some(s) => match s.extract_markdown().await {
            Ok(md) => Ok(ToolResult::text(md)),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        },
    }
}

fn arg_u64(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(default)
        .clamp(500, SCREENSHOT_PREPARE_MS_MAX)
}

async fn call_screenshot(
    arguments: &Value,
    handle: &SessionHandle,
) -> Result<ToolResult, McpError> {
    let opts = ScreenshotOptions {
        full_page: arg_bool(arguments, "full_page"),
        prepare_lazy_content: arguments
            .get("prepare_lazy_content")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        timeout_ms: arg_u64(arguments, "timeout_ms", SCREENSHOT_PREPARE_MS_DEFAULT),
    };
    let guard = handle.lock().await;
    match guard.as_ref() {
        None => Ok(ToolResult::error(
            "no browser session: call browser_open first".to_string(),
        )),
        Some(s) => match s.screenshot(opts).await {
            Ok(data) => Ok(ToolResult::image(data)),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_host_is_resolved_then_allowed() {
        assert_eq!(
            resolve_mcp_url("example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            resolve_mcp_url("localhost:8099").unwrap(),
            "http://localhost:8099"
        );
    }

    #[test]
    fn file_urls_are_still_rejected() {
        // The allowlist runs on the normalized value, so an explicit file
        // scheme survives normalization and is refused here.
        assert!(resolve_mcp_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn paths_and_schemes_without_a_host_are_rejected() {
        // A bare path never becomes a URL, so it cannot slip through as
        // `https://` + path.
        assert!(resolve_mcp_url("/etc/passwd").is_err());
        assert!(resolve_mcp_url("../secret").is_err());
        assert!(resolve_mcp_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn data_and_about_blank_still_work() {
        assert_eq!(
            resolve_mcp_url("data:text/html,<b>x</b>").unwrap(),
            "data:text/html,<b>x</b>"
        );
        assert_eq!(resolve_mcp_url("about:blank").unwrap(), "about:blank");
    }

    #[test]
    fn allowlist_still_rejects_other_schemes() {
        // A scheme the allowlist does not know, with a host, is refused.
        assert!(resolve_mcp_url("ftp://example.com").is_err());
    }

    #[test]
    fn storage_state_names_cannot_escape_root() {
        let config = SessionConfig {
            storage_state_root: Some(std::env::temp_dir().join("flashbrowse-storage-test")),
            ..Default::default()
        };
        assert!(storage_state_path(&config, "../outside").is_err());
        assert!(storage_state_path(&config, "nested/name").is_err());
        assert!(storage_state_path(&config, "valid-state").is_ok());
    }

    #[test]
    fn upload_paths_are_confined_to_the_configured_root() {
        let root =
            std::env::temp_dir().join(format!("flashbrowse-upload-root-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("inside.txt"), "x").unwrap();
        let config = SessionConfig {
            mcp_file_root: Some(root.clone()),
            ..Default::default()
        };
        assert!(resolve_upload_files(&config, &[Value::String("inside.txt".into())]).is_ok());
        assert!(resolve_upload_files(&config, &[Value::String("../outside.txt".into())]).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
