//! Validates stealth evasions against a live headless instance.
//! Requires system Chromium - skipped when absent.

use flashbrowse_cdp::protocol::runtime;
use flashbrowse_core::{BrowserSession, SessionConfig};

#[tokio::test]
async fn test_stealth_fingerprint() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch session");
    session
        .open("data:text/html,<html><body><h1>stealth</h1></body></html>")
        .await
        .expect("open data url");

    let sid = session.session_id().to_string();
    let client = session.client();

    // navigator.webdriver === undefined
    let webdriver = runtime::evaluate(&client, &sid, "(navigator.webdriver === undefined)")
        .await
        .expect("evaluate")
        .result
        .value;
    assert_eq!(
        webdriver,
        Some(serde_json::Value::Bool(true)),
        "navigator.webdriver should be undefined"
    );

    // window.chrome present with stubs
    let chrome = runtime::evaluate(
        &client,
        &sid,
        "window.chrome && typeof window.chrome === 'object'",
    )
    .await
    .expect("evaluate")
    .result
    .value;
    assert_eq!(
        chrome,
        Some(serde_json::Value::Bool(true)),
        "window.chrome stub missing"
    );

    // plugins.length > 0
    let plugins = runtime::evaluate(&client, &sid, "(navigator.plugins.length > 0)")
        .await
        .expect("evaluate")
        .result
        .value;
    assert_eq!(
        plugins,
        Some(serde_json::Value::Bool(true)),
        "navigator.plugins should be mocked non-empty"
    );

    session.shutdown().await;
}

#[tokio::test]
async fn test_stealth_profiles_coherent_and_no_headless_chrome() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            let body = "<html><body><h1>profile</h1></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });

    for profile in [
        flashbrowse_core::StealthProfile::HostNative,
        flashbrowse_core::StealthProfile::Windows,
    ] {
        let mut session = BrowserSession::launch(SessionConfig {
            stealth_profile: profile,
            ..Default::default()
        })
        .await
        .expect("launch session");
        session
            .open(&format!("http://127.0.0.1:{port}/"))
            .await
            .expect("open http url");

        let sid = session.session_id().to_string();
        let client = session.client();
        let info = runtime::evaluate(
            &client,
            &sid,
            "JSON.stringify({ ua: navigator.userAgent, platform: navigator.platform, chPlatform: navigator.userAgentData ? navigator.userAgentData.platform : '', brands: navigator.userAgentData ? navigator.userAgentData.brands.map(b => b.brand) : [] })",
        )
        .await
        .expect("evaluate")
        .result
        .value
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("json string");
        let parsed: serde_json::Value = serde_json::from_str(&info).expect("parse profile JSON");
        let ua = parsed["ua"].as_str().unwrap();
        assert!(
            !ua.contains("HeadlessChrome"),
            "UA leaked HeadlessChrome for {profile}: {ua}"
        );
        assert_eq!(
            parsed["platform"].as_str().unwrap(),
            profile.navigator_platform(),
            "navigator.platform mismatch for {profile}"
        );
        let brands = parsed["brands"].to_string();
        assert!(
            !brands.contains("HeadlessChrome"),
            "Client Hints brands leaked HeadlessChrome for {profile}: {brands}"
        );
        if profile == flashbrowse_core::StealthProfile::Windows {
            assert!(ua.contains("Windows NT 10.0"), "Windows UA missing: {ua}");
            assert_eq!(parsed["chPlatform"], "Windows");
        }
        session.shutdown().await;
    }
}
