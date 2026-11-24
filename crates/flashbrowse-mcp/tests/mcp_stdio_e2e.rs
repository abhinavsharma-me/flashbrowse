//! End-to-end MCP JSON-RPC validation over duplex pipes.
//! Drives `serve()` in-process (no binary needed until Phase 4 CLI wiring).
//! Requires system Chromium - skipped when absent.

use flashbrowse_core::SessionConfig;
use flashbrowse_mcp::server::serve;
use serde_json::Value;
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Send one JSON-RPC line and await the next response line as JSON.
async fn roundtrip<W, R>(client_out: &mut W, client_lines: &mut R, frame: Value) -> Value
where
    W: AsyncWriteExt + Unpin,
    R: AsyncBufReadExt + Unpin,
{
    client_out
        .write_all(frame.to_string().as_bytes())
        .await
        .unwrap();
    client_out.write_all(b"\n").await.unwrap();
    client_out.flush().await.unwrap();
    let mut line = String::new();
    client_lines.read_line(&mut line).await.unwrap();
    assert!(!line.is_empty(), "empty response (EOF?)");
    // Stdout purity: every line must be valid JSON.
    let v: Value = serde_json::from_str(line.trim()).expect("stdout line must be JSON");
    assert_eq!(v.get("jsonrpc").and_then(|x| x.as_str()), Some("2.0"));
    v
}

#[tokio::test]
async fn test_mcp_stdio_e2e() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);

    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve")
    });

    // initialize
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e"}} }),
    )
    .await;
    assert!(resp.get("error").is_none(), "init error: {resp}");
    let result = resp.get("result").expect("init result");
    assert!(
        result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .is_some(),
        "missing protocolVersion: {result}"
    );
    assert_eq!(
        result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str()),
        Some("flashbrowse")
    );

    // tools/list
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("tools list");
    assert_eq!(tools.len(), 10);
    let names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    for want in [
        "browser_open",
        "browser_act",
        "browser_fill_form",
        "browser_extract_markdown",
        "browser_evaluate",
        "browser_screenshot",
        "browser_tabs",
        "browser_storage_state",
        "browser_upload",
        "browser_downloads",
    ] {
        assert!(names.contains(&want.to_string()), "missing tool {want}");
    }

    // browser_open → numbered tree contains [1] Button: "Submit"
    let url = "data:text/html,<html><body><button>Submit</button></body></html>";
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "browser_open", "arguments": {"url": url}}}),
    )
    .await;
    let content = tool_text(&resp);
    assert!(
        content.contains("[1] Button: \"Submit\""),
        "browser_open tree missing [1] Button: {content}"
    );

    // browser_act click [1] → success
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "browser_act", "arguments": {"action_id": 1, "action": "click"}}}),
    )
    .await;
    assert_tool_ok(&resp);

    // browser_act hover + scroll + press_key should also succeed (no error).
    for (id, action, value) in [
        (5, "hover", serde_json::json!(null)),
        (6, "scroll", serde_json::json!("down")),
        (7, "press_key", serde_json::json!("Enter")),
    ] {
        let args = serde_json::json!({"action_id": 1, "action": action, "value": value});
        let resp = roundtrip(
            &mut client_out,
            &mut client_lines,
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "browser_act", "arguments": args}}),
        )
        .await;
        assert_tool_ok(&resp);
    }

    // browser_extract_markdown → text
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "browser_extract_markdown", "arguments": {}}}),
    )
    .await;
    assert_tool_ok(&resp);

    // browser_screenshot → image content block
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "browser_screenshot", "arguments": {"full_page": false}}}),
    )
    .await;
    let first = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .expect("image content");
    assert_eq!(
        first.get("type").and_then(|v| v.as_str()),
        Some("image"),
        "screenshot should be image content: {first}"
    );

    // Opening a popup by user gesture focuses it and leaves the opener selectable.
    let popup_page = "data:text/html,<html><body><button onclick=\"const w=window.open('about:blank');w.document.body.innerHTML='<h1>Popup content</h1>'\">Open popup</button></body></html>";
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":popup_page}}}),
    ).await;
    assert!(tool_text(&resp).contains("Open popup"));
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("Popup content"),
        "popup was not focused: {resp}"
    );

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    ).await;
    let tabs: Value = serde_json::from_str(&tool_text(&resp)).expect("tab list JSON");
    let tabs = tabs.as_array().expect("tab list array");
    assert!(tabs.len() >= 2, "expected opener and popup: {tabs:?}");
    let popup_id = tabs
        .iter()
        .find(|t| t.get("active").and_then(Value::as_bool) == Some(true))
        .and_then(|t| t.get("target_id"))
        .and_then(Value::as_str)
        .expect("active popup target id")
        .to_string();
    let opener_id = tabs
        .iter()
        .find(|t| {
            t.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.contains("Open popup"))
        })
        .and_then(|t| t.get("target_id"))
        .and_then(Value::as_str)
        .expect("opener target id")
        .to_string();
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"select","target_id":opener_id}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("Open popup"),
        "failed to return to opener: {resp}"
    );
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"close","target_id":popup_id}}}),
    ).await;
    assert_tool_ok(&resp);

    let dialog_page = "data:text/html,<html><body><h1>Dialog Test</h1><button onclick=\"alert('alert-note')\">Alert</button><button onclick=\"document.getElementById('result').textContent=confirm('confirm-note')?'yes':'no'\">Confirm</button><button onclick=\"document.getElementById('result').textContent=prompt('prompt-note')\">Prompt</button><p id='result'></p></body></html>";
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":dialog_page}}}),
    ).await;
    assert!(tool_text(&resp).contains("Dialog Test"), "{resp}");

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("alert dialog: accepted (alert-note)"),
        "alert hung or was not accepted: {resp}"
    );

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":2,"action":"click"}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("confirm dialog: dismissed (confirm-note)"),
        "confirm did not use safe default: {resp}"
    );
    assert!(
        tool_text(&resp).contains("no"),
        "confirm result did not reflect dismissal: {resp}"
    );

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":3,"action":"click","dialog_action":"accept","prompt_text":"provided-value"}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("prompt dialog: accepted (prompt-note)"),
        "prompt did not use explicit acceptance: {resp}"
    );
    assert!(
        tool_text(&resp).contains("provided-value"),
        "prompt text was not delivered: {resp}"
    );

    // ping → result
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 19, "method": "ping", "params": {}}),
    )
    .await;
    assert!(resp.get("error").is_none());

    // Drop client → server loop EOF → clean teardown.
    drop(client_out);
    drop(client_lines);
    server.await.expect("server task");
}

#[tokio::test]
async fn test_mcp_delayed_http_popup_and_opener_fallback() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local test server");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}/");

    let http_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path == "/slow-popup" {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    "<html><head><title>Slow Popup</title></head><body><h1>Delayed HTTP Popup</h1><button onclick=\"document.body.append('popup-ok')\">Popup Btn</button><script>setTimeout(() => { const p = document.createElement('p'); p.textContent = 'Late Hydrated Popup Content'; document.body.append(p); }, 800)</script></body></html>"
                } else {
                    "<html><head><title>Opener Page</title></head><body><h1>Main Opener</h1><a href=\"/slow-popup\" target=\"_blank\">Open Delayed Popup</a></body></html>"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);

    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve");
    });

    let _ = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0.1"}}
        }),
    )
    .await;
    client_out
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":base_url}}}),
    )
    .await;
    assert!(tool_text(&resp).contains("Main Opener"), "{resp}");

    // Click <a target="_blank"> pointing to a 400ms-delayed HTTP endpoint.
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
    )
    .await;
    assert!(
        tool_text(&resp).contains("Delayed HTTP Popup"),
        "delayed HTTP popup was not focused/settled: {resp}"
    );
    assert!(
        tool_text(&resp).contains("Late Hydrated Popup Content"),
        "popup tree was returned before late hydration completed: {resp}"
    );

    // Act inside the newly-focused HTTP popup.
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
    )
    .await;
    assert!(
        tool_text(&resp).contains("popup-ok"),
        "act inside delayed popup failed: {resp}"
    );

    // Closing the active popup must deterministically return to its opener (not chrome://newtab).
    let list_resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    )
    .await;
    let tabs: Value = serde_json::from_str(&tool_text(&list_resp)).expect("tab list");
    let popup_id = tabs
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t.get("active").and_then(Value::as_bool) == Some(true))
        .and_then(|t| t.get("target_id"))
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let close_resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"close","target_id":popup_id}}}),
    )
    .await;
    assert!(
        tool_text(&close_resp).contains("Main Opener"),
        "closing active popup did not return to opener: {close_resp}"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("server task");
    http_task.abort();
}

#[tokio::test]
async fn test_mcp_fill_form_and_partial_failure() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);

    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve");
    });

    let _ = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0.1"}}
        }),
    )
    .await;
    client_out
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();

    let form_page =
        "data:text/html,<form><input aria-label='First' /><input aria-label='Second' /></form>";
    let open = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":form_page}}}),
    )
    .await;
    assert!(tool_text(&open).contains("First"));

    let filled = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"browser_fill_form","arguments":{"fields":[
                {"action_id": 1, "value": "Ada"},
                {"action_id": 2, "value": "Lovelace"}
            ]}}
        }),
    )
    .await;
    assert_tool_ok(&filled);
    let tree = tool_text(&filled);
    assert!(tree.contains("Ada") && tree.contains("Lovelace"), "{tree}");

    // Partial failure: field 1 succeeds ("Grace"), field 99 does not exist.
    let partial = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"browser_fill_form","arguments":{"fields":[
                {"action_id": 1, "value": "Grace"},
                {"action_id": 99, "value": "Hopper"}
            ]}}
        }),
    )
    .await;
    assert_eq!(
        partial
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(Value::as_bool),
        Some(true),
        "expected partial failure error: {partial}"
    );
    let partial_text = tool_text(&partial);
    assert!(
        partial_text.contains("completed fields [1]"),
        "{partial_text}"
    );
    assert!(partial_text.contains("Grace"), "{partial_text}");

    drop(client_out);
    drop(client_lines);
    server.await.expect("server task");
}

#[tokio::test]
async fn test_mcp_storage_state_import_export() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = std::env::temp_dir().join(format!(
        "flashbrowse-storage-state-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create storage-state root");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local test server");
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/state");
    let http_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let response = if path == "/download" {
                    let body = "download contents";
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=download.txt\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else {
                    let body = "<html><body><h1>Storage State</h1><label>Upload<input id='upload' type='file' aria-label='Upload a file'></label><a id='download' href='/download'>Download file</a></body></html>";
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                };
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    let state = serde_json::json!({
        "cookies": [{"name": "auth", "value": "cookie-value", "domain": "127.0.0.1", "path": "/", "expires": -1, "httpOnly": false, "secure": false, "sameSite": "Lax"}],
        "origins": [{"origin": format!("http://127.0.0.1:{port}"), "localStorage": [{"name": "token", "value": "storage-value"}]}]
    });
    std::fs::write(root.join("state.json"), serde_json::to_vec(&state).unwrap())
        .expect("write fixture state");
    std::fs::write(root.join("upload.txt"), "upload contents").expect("write upload fixture");

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let config = SessionConfig {
        storage_state_root: Some(root.clone()),
        mcp_file_root: Some(root.clone()),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        serve(server_in, server_out, config).await.expect("serve");
    });
    let _ = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}),
    )
    .await;
    client_out
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    let opened = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":url}}}),
    )
    .await;
    assert!(tool_text(&opened).contains("Storage State"), "{opened}");
    let imported = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_storage_state","arguments":{"action":"import","name":"state"}}}),
    )
    .await;
    assert!(tool_text(&imported).contains("Storage State"), "{imported}");
    let evaluated = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"[document.cookie, localStorage.getItem('token')]"}}}),
    )
    .await;
    assert!(
        tool_text(&evaluated).contains("cookie-value"),
        "cookie was not imported: {evaluated}"
    );
    assert!(
        tool_text(&evaluated).contains("storage-value"),
        "localStorage was not imported before page scripts: {evaluated}"
    );
    let exported = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_storage_state","arguments":{"action":"export","name":"roundtrip"}}}),
    )
    .await;
    assert!(tool_text(&exported).contains("exported"), "{exported}");
    let exported_state: Value =
        serde_json::from_slice(&std::fs::read(root.join("roundtrip.json")).unwrap()).unwrap();
    assert!(exported_state["cookies"]
        .as_array()
        .is_some_and(|cookies| cookies.iter().any(|cookie| cookie["name"] == "auth")));
    assert!(exported_state["origins"]
        .as_array()
        .is_some_and(|origins| origins.iter().any(|origin| origin["localStorage"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item["name"] == "token" && item["value"] == "storage-value")))));

    let upload = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_upload","arguments":{"action_id":1,"files":["upload.txt"]}}}),
    ).await;
    assert_tool_ok(&upload);
    let uploaded_name = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.querySelector('#upload').files[0]?.name"}}}),
    ).await;
    assert!(
        tool_text(&uploaded_name).contains("upload.txt"),
        "file input did not receive the selected file: {uploaded_name}"
    );

    let start_download = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.querySelector('#download').click()"}}}),
    ).await;
    assert_tool_ok(&start_download);
    let mut download_list = Value::Null;
    for _ in 0..40 {
        let response = roundtrip(
            &mut client_out,
            &mut client_lines,
            serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"browser_downloads","arguments":{"action":"list"}}}),
        ).await;
        download_list = serde_json::from_str(&tool_text(&response)).unwrap_or(Value::Null);
        if download_list["downloads"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        download_list["downloads"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "download event was not recorded: {download_list}"
    );
    let guid = download_list["downloads"][0]["guid"]
        .as_str()
        .expect("download guid");
    let completed = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_downloads","arguments":{"action":"wait","guid":guid,"timeout_ms":5000}}}),
    ).await;
    let completed: Value =
        serde_json::from_str(&tool_text(&completed)).expect("download status JSON");
    assert_eq!(
        completed["state"], "completed",
        "download did not complete: {completed}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("downloads/download.txt")).unwrap(),
        "download contents"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
    http_task.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn test_mcp_live_policy_switch_preserves_browser_session() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve")
    });
    let init = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    ).await;
    assert!(init.get("error").is_none(), "initialize failed: {init}");

    let open1 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":"data:text/html,<h1>First</h1>","resource_policy":"agent-default"}}}),
    ).await;
    assert_tool_ok(&open1);

    let tabs1 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    ).await;
    assert_tool_ok(&tabs1);
    let tabs1_json: Value = serde_json::from_str(&tool_text(&tabs1)).expect("tabs1 json");
    let first_tid = tabs1_json
        .as_array()
        .and_then(|arr| arr.iter().find(|t| t["active"] == true))
        .and_then(|t| t["target_id"].as_str())
        .expect("active target_id")
        .to_string();

    let open2 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":"data:text/html,<h1>Second</h1>","resource_policy":"full-render"}}}),
    ).await;
    assert_tool_ok(&open2);
    assert!(tool_text(&open2).contains("Second"));

    let tabs2 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    ).await;
    assert_tool_ok(&tabs2);
    let tabs2_json: Value = serde_json::from_str(&tool_text(&tabs2)).expect("tabs2 json");
    let second_tid = tabs2_json
        .as_array()
        .and_then(|arr| arr.iter().find(|t| t["active"] == true))
        .and_then(|t| t["target_id"].as_str())
        .expect("active target_id")
        .to_string();

    assert_eq!(
        first_tid, second_tid,
        "switching resource_policy must update live session without restarting Chrome"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
}

fn tool_text(resp: &Value) -> String {
    resp.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn assert_tool_ok(resp: &Value) {
    assert!(resp.get("error").is_none(), "JSON-RPC error: {resp}");
    let is_error = resp
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(|v| v.as_bool());
    assert_ne!(
        is_error,
        Some(true),
        "tool returned isError: {}",
        tool_text(resp)
    );
}
