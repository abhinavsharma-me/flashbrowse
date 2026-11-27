//! File downloads, uploads, and browser storage state (cookies/localStorage).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use flashbrowse_cdp::protocol::{dom, page, runtime};
use flashbrowse_cdp::CdpClient;
use serde_json::Value;

use super::config::SessionConfig;
use super::error::CoreError;
use super::BrowserSession;

#[derive(Debug, Clone, serde::Serialize)]
pub struct DownloadInfo {
    pub guid: String,
    pub url: String,
    pub suggested_filename: String,
    pub received_bytes: f64,
    pub total_bytes: f64,
    pub state: String,
}

pub(super) type DownloadTracker = Arc<std::sync::Mutex<HashMap<String, DownloadInfo>>>;

pub(super) fn download_directory(config: &SessionConfig) -> PathBuf {
    config
        .mcp_file_root
        .clone()
        .unwrap_or_else(|| crate::paths::cache_dir().join("files"))
        .join("downloads")
}

pub(super) fn install_download_tracker(client: &Arc<CdpClient>) -> DownloadTracker {
    let downloads: DownloadTracker = Arc::default();
    let mut begun = client.events().subscribe("Browser.downloadWillBegin");
    let mut progress = client.events().subscribe("Browser.downloadProgress");
    let begin_state = downloads.clone();
    let progress_state = downloads.clone();
    tokio::spawn(async move {
        loop {
            let event = match begun.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let Some(guid) = event.get("guid").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(mut downloads) = begin_state.lock() {
                downloads.insert(
                    guid.to_string(),
                    DownloadInfo {
                        guid: guid.to_string(),
                        url: event
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        suggested_filename: event
                            .get("suggestedFilename")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        received_bytes: 0.0,
                        total_bytes: 0.0,
                        state: "inProgress".to_string(),
                    },
                );
            }
        }
    });
    tokio::spawn(async move {
        loop {
            let event = match progress.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let Some(guid) = event.get("guid").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(mut downloads) = progress_state.lock() {
                if let Some(info) = downloads.get_mut(guid) {
                    info.received_bytes = event
                        .get("receivedBytes")
                        .and_then(Value::as_f64)
                        .unwrap_or(info.received_bytes);
                    info.total_bytes = event
                        .get("totalBytes")
                        .and_then(Value::as_f64)
                        .unwrap_or(info.total_bytes);
                    info.state = event
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or(&info.state)
                        .to_string();
                }
            }
        }
    });
    downloads
}

impl BrowserSession {
    /// Export Playwright-compatible cookies and per-origin localStorage.
    pub async fn export_storage_state(&self) -> Result<Value, CoreError> {
        let cookies: Value = self
            .client
            .send(
                Some(&self.session_id),
                "Network.getAllCookies",
                serde_json::json!({}),
            )
            .await?;
        let page_state = runtime::evaluate(
            &self.client,
            &self.session_id,
            "JSON.stringify({origin: location.origin, localStorage: Object.entries(localStorage).map(([name, value]) => ({name, value}))})",
        )
        .await?;
        let origin_state = page_state
            .result
            .value
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|encoded| serde_json::from_str::<Value>(encoded).ok())
            .filter(|value| {
                value
                    .get("origin")
                    .and_then(Value::as_str)
                    .is_some_and(|o| o != "null")
            });
        Ok(serde_json::json!({
            "cookies": cookies.get("cookies").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
            "origins": origin_state.into_iter().collect::<Vec<_>>(),
        }))
    }

    /// Apply cookies and arrange localStorage restoration before page scripts,
    /// then reload the current document so the imported state takes effect.
    pub async fn import_storage_state(&mut self, state: &Value) -> Result<String, CoreError> {
        let cookies = state
            .get("cookies")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CoreError::NavigationFailed("storage state must contain a cookies array".into())
            })?;
        let origins = state
            .get("origins")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CoreError::NavigationFailed("storage state must contain an origins array".into())
            })?;
        let _: Value = self
            .client
            .send(
                Some(&self.session_id),
                "Network.setCookies",
                serde_json::json!({"cookies": cookies}),
            )
            .await?;
        let encoded_origins = serde_json::to_string(origins)?;
        let script = format!(
            "(() => {{ const states = {encoded_origins}; const state = states.find(item => item.origin === location.origin); if (state && Array.isArray(state.localStorage)) {{ for (const item of state.localStorage) localStorage.setItem(item.name, item.value); }} }})()"
        );
        page::add_script_on_new_document(&self.client, &self.session_id, &script).await?;
        let current_url = if self.url.is_empty() {
            "about:blank".to_string()
        } else {
            self.url.clone()
        };
        self.open(&current_url).await
    }

    /// Populate an `<input type=file>` with existing files and rebuild its tree.
    pub async fn upload_files(
        &mut self,
        action_id: u32,
        files: &[PathBuf],
    ) -> Result<String, CoreError> {
        if files.is_empty() {
            return Err(CoreError::NavigationFailed(
                "upload requires at least one file".into(),
            ));
        }
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, _) = self.frame_for_action(action_id);
        let object = dom::resolve_node(&self.client, &dom_sid, backend).await?;
        let object_id = object
            .object
            .object_id
            .ok_or(CoreError::NoBackendId(action_id))?;
        let input_check = runtime::call_function_on(
            &self.client,
            &dom_sid,
            &object_id,
            "function() { return this instanceof HTMLInputElement && this.type === 'file'; }",
            Vec::new(),
        )
        .await?;
        if !input_check
            .result
            .value
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(CoreError::NavigationFailed(format!(
                "action [{action_id}] is not a file input"
            )));
        }
        let mut canonical_files = Vec::with_capacity(files.len());
        for file in files {
            let canonical = std::fs::canonicalize(file).map_err(|error| {
                CoreError::NavigationFailed(format!(
                    "cannot access upload file {}: {error}",
                    file.display()
                ))
            })?;
            if !canonical.is_file() {
                return Err(CoreError::NavigationFailed(format!(
                    "upload path is not a file: {}",
                    canonical.display()
                )));
            }
            canonical_files.push(canonical.to_string_lossy().into_owned());
        }
        dom::set_file_input_files(&self.client, &dom_sid, backend, canonical_files).await?;
        self.rebuild_tree().await
    }

    pub fn downloads(&self) -> Vec<DownloadInfo> {
        let mut result = self
            .downloads
            .lock()
            .map(|downloads| downloads.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        result.sort_by(|left, right| left.guid.cmp(&right.guid));
        result
    }

    pub fn download_dir(&self) -> &std::path::Path {
        &self.download_dir
    }
}
