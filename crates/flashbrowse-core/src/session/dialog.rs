//! Native JavaScript dialog handling policy and state.

use std::sync::Arc;

use flashbrowse_cdp::{CdpClient, CdpError};
use serde_json::Value;

use super::BrowserSession;

/// Native JavaScript dialog handling policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DialogPolicy {
    /// Accept alerts; dismiss confirms and prompts to avoid blocking the page.
    #[default]
    SafeDefault,
    /// Accept all dialog types (prompts receive no text).
    AcceptAll,
    /// Dismiss all dialog types.
    DismissAll,
}

#[derive(Debug, Clone)]
pub struct DialogInfo {
    pub dialog_type: String,
    pub message: String,
    pub accepted: bool,
}

#[derive(Debug, Default)]
pub(crate) struct DialogState {
    pub(crate) policy: DialogPolicy,
    pub(crate) override_decision: Option<(bool, Option<String>)>,
    pub(crate) last_dialog: Option<DialogInfo>,
}

pub(super) fn install_dialog_handler(
    client: &Arc<CdpClient>,
    session_id: &str,
    policy: DialogPolicy,
) -> Arc<std::sync::Mutex<DialogState>> {
    let state = Arc::new(std::sync::Mutex::new(DialogState {
        policy,
        ..DialogState::default()
    }));
    let mut events = client.events().subscribe("Page.javascriptDialogOpening");
    let state_for_task = Arc::downgrade(&state);
    let client = client.clone();
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    tracing::warn!(count, "missed JavaScript dialog events");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let Some(state_arc) = state_for_task.upgrade() else {
                break;
            };
            let dialog_type = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("alert")
                .to_string();
            let message = event
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let (accepted, prompt_text) = {
                let Ok(mut state) = state_arc.lock() else {
                    continue;
                };
                let (accepted, prompt_text) = match state.override_decision.take() {
                    Some(decision) => decision,
                    None => match state.policy {
                        DialogPolicy::SafeDefault => (dialog_type == "alert", None),
                        DialogPolicy::AcceptAll => (true, None),
                        DialogPolicy::DismissAll => (false, None),
                    },
                };
                (accepted, prompt_text)
            };
            let mut params = serde_json::json!({"accept": accepted});
            if let Some(prompt_text) = prompt_text {
                params["promptText"] = Value::String(prompt_text);
            }
            let response: Result<Value, CdpError> = client
                .send(Some(&session_id), "Page.handleJavaScriptDialog", params)
                .await;
            match response {
                Ok(_) => {
                    if let Ok(mut state) = state_arc.lock() {
                        state.last_dialog = Some(DialogInfo {
                            dialog_type,
                            message,
                            accepted,
                        });
                    }
                }
                Err(error) => tracing::warn!(%error, "failed to resolve JavaScript dialog"),
            }
        }
    });
    state
}

impl BrowserSession {
    /// Set a one-action override. It is consumed by the next dialog event.
    pub fn set_dialog_override(&self, accept: bool, prompt_text: Option<String>) {
        if let Ok(mut state) = self.dialog_state.lock() {
            state.override_decision = Some((accept, prompt_text));
            state.last_dialog = None;
        }
    }

    pub fn clear_dialog_override(&self) {
        if let Ok(mut state) = self.dialog_state.lock() {
            state.override_decision = None;
        }
    }

    /// Return and clear the most recently handled dialog, if any.
    pub fn take_dialog_info(&self) -> Option<DialogInfo> {
        self.dialog_state
            .lock()
            .ok()
            .and_then(|mut state| state.last_dialog.take())
    }
}
