//! Target creation, popup window tracking, and tab lifecycle operations.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use flashbrowse_cdp::protocol::target::{self, TargetInfo};
use flashbrowse_cdp::CdpClient;

use super::config::SessionConfig;
use super::error::CoreError;
use super::BrowserSession;

pub(super) type PopupTracker = Arc<Mutex<Vec<TargetInfo>>>;

pub(super) fn record_popup_target(tracker: &PopupTracker, info: TargetInfo) {
    if info.kind != "page" {
        return;
    }
    if let Ok(mut list) = tracker.lock() {
        if let Some(pos) = list.iter().position(|t| t.target_id == info.target_id) {
            list[pos] = info;
        } else {
            list.push(info);
        }
    }
}

pub(super) fn take_popup_from_tracker(
    tracker: &PopupTracker,
    opener_id: &str,
    existing: &HashSet<String>,
) -> Option<TargetInfo> {
    let Ok(mut list) = tracker.lock() else {
        return None;
    };
    let pos = list.iter().rposition(|t| {
        t.kind == "page"
            && t.opener_id.as_deref() == Some(opener_id)
            && !existing.contains(&t.target_id)
    })?;
    let picked = list.remove(pos);
    list.retain(|t| t.opener_id.as_deref() != Some(opener_id) || existing.contains(&t.target_id));
    Some(picked)
}

pub(super) fn install_popup_tracker(client: &Arc<CdpClient>) -> PopupTracker {
    let tracker: PopupTracker = Arc::new(Mutex::new(Vec::new()));

    let mut created_rx = client.events().subscribe("Target.targetCreated");
    let t_created = tracker.clone();
    tokio::spawn(async move {
        while let Ok(v) = created_rx.recv().await {
            if let Ok(ev) = serde_json::from_value::<target::TargetCreatedEvent>(v) {
                record_popup_target(&t_created, ev.target_info);
            }
        }
    });

    let mut attached_rx = client.events().subscribe("Target.attachedToTarget");
    let t_attached = tracker.clone();
    tokio::spawn(async move {
        while let Ok(v) = attached_rx.recv().await {
            if let Ok(ev) = serde_json::from_value::<target::AttachedToTargetEvent>(v) {
                record_popup_target(&t_attached, ev.target_info);
            }
        }
    });

    tracker
}

impl BrowserSession {
    pub async fn list_tabs(&self) -> Result<Vec<TargetInfo>, CoreError> {
        let targets = target::get_targets(&self.client).await?;
        Ok(targets
            .target_infos
            .into_iter()
            .filter(|t| t.kind == "page")
            .collect())
    }

    /// Consume the newest `page` target opened by `opener_id` from the
    /// `Target.targetCreated` / `Target.attachedToTarget` event tracker.
    pub fn take_popup_for(
        &self,
        opener_id: &str,
        existing: &HashSet<String>,
    ) -> Option<TargetInfo> {
        take_popup_from_tracker(&self.popups, opener_id, existing)
    }

    pub async fn select_tab(
        &mut self,
        target_id: &str,
        config: &SessionConfig,
    ) -> Result<String, CoreError> {
        if target_id == self.target_id {
            return Ok(self.last_tree.clone());
        }
        let tabs = self.list_tabs().await?;
        if !tabs.iter().any(|t| t.target_id == target_id) {
            return Err(CoreError::NavigationFailed(format!(
                "unknown page target: {target_id}"
            )));
        }
        let ws_url = self.browser_ws_url.clone();
        let _ = target::detach_from_target(&self.client, &self.session_id).await;
        let mut replacement = Self::attach_with_client(
            self.client.clone(),
            ws_url,
            Some(target_id.to_string()),
            config,
        )
        .await?;
        replacement.chrome = self.chrome.take();
        *self = replacement;
        Ok(self.last_tree.clone())
    }

    pub async fn close_tab(
        &mut self,
        target_id: &str,
        config: &SessionConfig,
    ) -> Result<Option<String>, CoreError> {
        let tabs = self.list_tabs().await?;
        if tabs.len() <= 1 {
            return Err(CoreError::NavigationFailed(
                "cannot close the last browser tab".into(),
            ));
        }
        if !tabs.iter().any(|t| t.target_id == target_id) {
            return Err(CoreError::NavigationFailed(format!(
                "unknown page target: {target_id}"
            )));
        }
        if !target::close_target(&self.client, target_id).await? {
            return Err(CoreError::NavigationFailed(format!(
                "Chrome refused to close page target: {target_id}"
            )));
        }
        if target_id == self.target_id {
            let opener_id = tabs
                .iter()
                .find(|t| t.target_id == target_id)
                .and_then(|t| t.opener_id.as_deref());
            let next = opener_id
                .and_then(|oid| tabs.iter().find(|t| t.target_id == oid))
                .or_else(|| {
                    tabs.iter()
                        .find(|t| t.target_id != target_id && !t.url.starts_with("chrome://"))
                })
                .or_else(|| tabs.iter().find(|t| t.target_id != target_id))
                .expect("remaining tab");
            return self.select_tab(&next.target_id, config).await.map(Some);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use flashbrowse_cdp::protocol::target::TargetInfo;

    use super::{record_popup_target, take_popup_from_tracker, PopupTracker};

    fn target(id: &str, kind: &str, opener: Option<&str>) -> TargetInfo {
        TargetInfo {
            target_id: id.to_string(),
            kind: kind.to_string(),
            url: format!("https://example.com/{id}"),
            title: id.to_string(),
            opener_id: opener.map(str::to_string),
        }
    }

    #[test]
    fn records_newest_page_for_opener_and_ignores_non_page_and_existing() {
        let tracker: PopupTracker = Arc::new(Mutex::new(Vec::new()));
        record_popup_target(&tracker, target("sw1", "service_worker", Some("opener1")));
        record_popup_target(&tracker, target("old_tab", "page", Some("opener1")));
        record_popup_target(&tracker, target("other", "page", Some("opener2")));
        record_popup_target(&tracker, target("popup1", "page", Some("opener1")));

        let mut existing = HashSet::new();
        existing.insert("old_tab".to_string());

        let picked = take_popup_from_tracker(&tracker, "opener1", &existing).expect("popup1");
        assert_eq!(picked.target_id, "popup1");
        assert!(take_popup_from_tracker(&tracker, "opener1", &existing).is_none());
        assert!(take_popup_from_tracker(&tracker, "opener2", &existing).is_some());
    }
}
