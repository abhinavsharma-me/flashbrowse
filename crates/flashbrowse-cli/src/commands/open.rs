//! `flashbrowse open <url>` - print the compressed action tree.

use std::path::PathBuf;

use flashbrowse_core::{
    normalize_url, BrowserSession, ResourcePolicy, SessionConfig, StealthProfile,
};

use super::session_file::write_session;
use flashbrowse_core::observability;

/// Ephemeral `open` or persistent (`--session-id` detach).
pub async fn run(
    url: String,
    session_id: Option<String>,
    chrome_path: Option<PathBuf>,
    resource_policy: ResourcePolicy,
    stealth_profile: StealthProfile,
    cdp_url: Option<String>,
    user_data_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    // Resolve before launching: a typo must not cost a browser, and the raw
    // CDP error for a scheme-less host ("invalid URL") says nothing useful.
    let url = match normalize_url(&url) {
        Ok(u) => u,
        Err(e) => {
            observability::record_op("open", &url, started, None, false, Some(e.to_string()));
            return Err(anyhow::anyhow!("{e}"));
        }
    };
    // Validate the session id BEFORE launching Chrome: an invalid id must not
    // spawn a persistent browser with no session file to stop it.
    if let Some(id) = session_id.as_deref() {
        crate::commands::session_file::validate_session_id(id)?;
    }
    let config = SessionConfig {
        chrome_path,
        resource_policy,
        stealth_profile,
        persistent: session_id.is_some(),
        cdp_url,
        user_data_dir,
        ..Default::default()
    };
    let mut session = BrowserSession::launch(config).await?;
    let result = session.open(&url).await;
    // Capture the settle breakdown before the session is consumed below.
    let settle = session.last_settle().cloned();
    match result {
        Err(e) => {
            session.shutdown().await;
            let msg = e.to_string();
            observability::record_op_with_settle(
                "open",
                &url,
                started,
                None,
                false,
                Some(msg.clone()),
                settle,
            );
            return Err(e.into());
        }
        Ok(tree) => {
            let payload = (tree.len(), observability::count_actions(&tree));
            match session_id {
                Some(id) => {
                    let detached = session.detach();
                    write_session(&id, &detached)?;
                    println!("{tree}");
                    println!(
                        "(persistent session '{id}' pid {} - 'flashbrowse close --session-id {id}' to stop)",
                        detached.pid
                    );
                }
                None => {
                    println!("{tree}");
                    session.shutdown().await;
                }
            }
            observability::record_op_with_settle(
                "open",
                &url,
                started,
                Some(payload),
                true,
                None,
                settle,
            );
        }
    }
    Ok(())
}
