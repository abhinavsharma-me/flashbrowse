//! `Target` domain types (createTarget, attachToTarget).

use serde::{Deserialize, Serialize};

use crate::error::CdpError;
use crate::transport::CdpClient;

/// Params for `Target.createTarget`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTargetParams {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
}

/// Response of `Target.createTarget`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTargetResponse {
    pub target_id: String,
}

/// Params for `Target.attachToTarget`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachToTargetParams {
    pub target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flatten: Option<bool>,
}

/// Response of `Target.attachToTarget`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachToTargetResponse {
    pub session_id: String,
}

/// Create a new target (tab) at `url`.
pub async fn create_target(
    client: &CdpClient,
    url: &str,
) -> Result<CreateTargetResponse, CdpError> {
    client
        .send_typed(
            None,
            "Target.createTarget",
            &CreateTargetParams {
                url: url.to_string(),
                headless: None,
            },
        )
        .await
}

/// Target metadata from `Target.getTargets`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub opener_id: Option<String>,
}

/// Response of `Target.getTargets`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTargetsResponse {
    pub target_infos: Vec<TargetInfo>,
}

/// List active CDP targets (used to discover out-of-process iframe targets).
pub async fn get_targets(client: &CdpClient) -> Result<GetTargetsResponse, CdpError> {
    client
        .send_typed(None, "Target.getTargets", &serde_json::json!({}))
        .await
}

pub async fn set_discover_targets(client: &CdpClient, discover: bool) -> Result<(), CdpError> {
    client
        .send(
            None,
            "Target.setDiscoverTargets",
            serde_json::json!({"discover": discover}),
        )
        .await?;
    Ok(())
}

pub async fn close_target(client: &CdpClient, target_id: &str) -> Result<bool, CdpError> {
    let response: serde_json::Value = client
        .send(
            None,
            "Target.closeTarget",
            serde_json::json!({"targetId": target_id}),
        )
        .await?;
    Ok(response
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

/// Attach to a target in flatten mode. Returns `sessionId` for all page-level calls.
pub async fn attach_to_target(
    client: &CdpClient,
    target_id: &str,
) -> Result<AttachToTargetResponse, CdpError> {
    client
        .send_typed(
            None,
            "Target.attachToTarget",
            &AttachToTargetParams {
                target_id: target_id.to_string(),
                flatten: Some(true),
            },
        )
        .await
}
