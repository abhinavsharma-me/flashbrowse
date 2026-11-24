use flashbrowse_cdp::{protocol::browser, CdpClient, LaunchOptions};
use flashbrowse_core::{BrowserSession, SessionConfig};

#[tokio::test]
async fn custom_profile_and_external_cdp_attach_preserve_browser_lifecycle() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let profile =
        std::env::temp_dir().join(format!("flashbrowse-profile-test-{}", std::process::id()));
    let mut chrome = flashbrowse_cdp::launch_chrome(LaunchOptions {
        user_data_dir: Some(profile.clone()),
        no_sandbox: Some(true),
        persistent: true,
        ..Default::default()
    })
    .await
    .expect("launch custom profile");
    let ws_url = chrome.browser_ws_url().to_string();
    let http_endpoint = ws_url
        .split("/devtools/browser/")
        .next()
        .expect("browser websocket endpoint")
        .replacen("ws://", "http://", 1);
    let monitor = CdpClient::connect(&ws_url)
        .await
        .expect("monitor connection");

    let attached = BrowserSession::launch(SessionConfig {
        cdp_url: Some(http_endpoint),
        no_sandbox: Some(true),
        ..Default::default()
    })
    .await
    .expect("attach through HTTP debugging endpoint");
    assert!(
        attached.pid().is_none(),
        "attached browser must remain externally owned"
    );
    assert!(!attached.last_tree().is_empty());
    attached.shutdown().await;

    let version = browser::get_version(&monitor)
        .await
        .expect("external browser remains alive after attached session closes");
    assert!(!version.product.is_empty());
    assert!(profile.is_dir(), "user supplied profile must be preserved");
    monitor.shutdown();
    chrome.shutdown().await;
    let _ = std::fs::remove_dir_all(profile);
}
