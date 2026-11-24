//! WebSocket Transport & Correlation Engine.
//!
//! One `CdpClient` per browser WebSocket. Multiplexes any number of
//! `sessionId` (flatten-mode) RPCs over the same socket:
//! - Outgoing frames serialize to a single writer task via `mpsc::UnboundedSender`.
//! - Incoming frames with `id` resolve the matching `oneshot` pending entry.
//! - Incoming frames with `method` (no `id`) are routed to [`EventEmitter`].
//!
//! Every [`CdpClient::send`] call is bounded by `CALL_TIMEOUT` and returns a
//! typed [`CdpError::Timeout`] instead of hanging a dead socket forever.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::error::CdpError;
use crate::events::EventEmitter;

/// Per-RPC timeout.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Max accepted WebSocket text frame (32 MiB - screenshots/AXTree batches).
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// `connect_async` budget before socket is usable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

type PendingMap = DashMap<u64, (String, oneshot::Sender<Result<Value, CdpError>>)>;

/// Raw CDP client over a single browser WebSocket.
///
/// Clone is not provided intentionally - share via `Arc<CdpClient>`.
pub struct CdpClient {
    next_id: AtomicU64,
    pending: Arc<PendingMap>,
    sender: Mutex<Option<mpsc::UnboundedSender<String>>>,
    events: Arc<EventEmitter>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    writer_alive: Arc<AtomicBool>,
}

#[cfg(unix)]
fn fail_pending_oversize(pending: &PendingMap) {
    let keys: Vec<u64> = pending.iter().map(|e| *e.key()).collect();
    for key in keys {
        if let Some((_, (method, sender))) = pending.remove(&key) {
            let _ = sender.send(Err(CdpError::ProtocolError {
                method,
                code: -1,
                message: format!("CDP response exceeded {MAX_FRAME_BYTES} byte limit"),
            }));
        }
    }
}

#[cfg(unix)]
fn fail_pending_disconnected(pending: &PendingMap) {
    let keys: Vec<u64> = pending.iter().map(|e| *e.key()).collect();
    for key in keys {
        if let Some((_, (method, sender))) = pending.remove(&key) {
            let _ = sender.send(Err(CdpError::Disconnected { method }));
        }
    }
}

#[cfg(unix)]
fn set_nonblocking(fd: std::os::unix::io::BorrowedFd<'_>) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let raw = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

impl CdpClient {
    /// Connect to a freshly launched [`crate::ChromeProcess`], preferring POSIX
    /// pipe transport when available and falling back to WebSocket.
    pub async fn connect_chrome(chrome: &mut crate::ChromeProcess) -> Result<Self, CdpError> {
        #[cfg(unix)]
        if let Some(pipes) = chrome.take_pipe_handles() {
            return Self::connect_pipe(pipes.read_fd, pipes.write_fd).await;
        }
        Self::connect(chrome.browser_ws_url()).await
    }

    /// Connect over `\0`-delimited POSIX DevTools pipes (`--remote-debugging-pipe`).
    #[cfg(unix)]
    pub async fn connect_pipe(
        read_fd: std::os::unix::io::OwnedFd,
        write_fd: std::os::unix::io::OwnedFd,
    ) -> Result<Self, CdpError> {
        use std::os::unix::io::AsFd;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::unix::pipe::{Receiver, Sender};

        set_nonblocking(read_fd.as_fd()).map_err(|e| CdpError::WsConnectFailed {
            url: "pipe://fd".to_string(),
            detail: e.to_string(),
        })?;
        set_nonblocking(write_fd.as_fd()).map_err(|e| CdpError::WsConnectFailed {
            url: "pipe://fd".to_string(),
            detail: e.to_string(),
        })?;

        let mut pipe_rx =
            Receiver::from_owned_fd(read_fd).map_err(|e| CdpError::WsConnectFailed {
                url: "pipe://fd".to_string(),
                detail: e.to_string(),
            })?;
        let mut pipe_tx =
            Sender::from_owned_fd(write_fd).map_err(|e| CdpError::WsConnectFailed {
                url: "pipe://fd".to_string(),
                detail: e.to_string(),
            })?;

        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = Arc::new(EventEmitter::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        let writer_alive = Arc::new(AtomicBool::new(true));
        let writer_flag = writer_alive.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(text) = out_rx.recv().await {
                if pipe_tx.write_all(text.as_bytes()).await.is_err()
                    || pipe_tx.write_all(b"\0").await.is_err()
                    || pipe_tx.flush().await.is_err()
                {
                    break;
                }
            }
            writer_flag.store(false, Ordering::SeqCst);
        }));

        let pending_reader = pending.clone();
        let events_rx = events.clone();
        tasks.push(tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match pipe_rx.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        let mut start = 0usize;
                        while let Some(rel) = buf[start..].iter().position(|&b| b == 0) {
                            let end = start + rel;
                            let frame = &buf[start..end];
                            if frame.len() > MAX_FRAME_BYTES {
                                tracing::warn!(
                                    bytes = frame.len(),
                                    "oversize CDP pipe frame - failing pending calls"
                                );
                                fail_pending_oversize(&pending_reader);
                                fail_pending_disconnected(&pending_reader);
                                return;
                            }
                            if !frame.is_empty() {
                                if let Ok(value) = serde_json::from_slice::<Value>(frame) {
                                    dispatch_frame(&value, &pending_reader, &events_rx);
                                }
                            }
                            start = end + 1;
                        }
                        if start > 0 {
                            buf.drain(..start);
                        }
                        if buf.len() > MAX_FRAME_BYTES {
                            tracing::warn!(
                                bytes = buf.len(),
                                "oversize CDP pipe frame buffer - failing pending calls"
                            );
                            fail_pending_oversize(&pending_reader);
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            fail_pending_disconnected(&pending_reader);
        }));

        Ok(Self {
            next_id: AtomicU64::new(1),
            pending,
            sender: Mutex::new(Some(out_tx)),
            events,
            tasks: Mutex::new(tasks),
            writer_alive,
        })
    }

    /// Connect to a browser-level DevTools WebSocket URL and spawn
    /// reader/writer background tasks.
    pub async fn connect(ws_url: &str) -> Result<Self, CdpError> {
        const ATTEMPTS: u32 = 6;
        let mut last_err = CdpError::WsConnectFailed {
            url: ws_url.to_string(),
            detail: "connect failed".to_string(),
        };
        let mut socket = None;
        for attempt in 0..ATTEMPTS {
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(ws_url))
                .await
            {
                Ok(Ok(pair)) => {
                    socket = Some(pair.0);
                    break;
                }
                Ok(Err(e)) => {
                    last_err = CdpError::WsConnectFailed {
                        url: ws_url.to_string(),
                        detail: e.to_string(),
                    };
                }
                Err(_) => {
                    last_err = CdpError::WsConnectFailed {
                        url: ws_url.to_string(),
                        detail: "connect timed out".to_string(),
                    };
                }
            }
            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
            }
        }
        let socket = socket.ok_or(last_err)?;

        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = Arc::new(EventEmitter::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

        let (mut write, mut read) = socket.split();
        let pending_writer = pending.clone();
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        // Writer loop
        let writer_alive = Arc::new(AtomicBool::new(true));
        let writer_flag = writer_alive.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(text) = out_rx.recv().await {
                if write.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            writer_flag.store(false, Ordering::SeqCst);
        }));

        // Reader loop
        let pending_reader = pending.clone();
        let events_rx = events.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(frame) = read.next().await {
                let text = match frame {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                };
                // Max-frame cap (M1): oversize frames are dropped, not
                // parsed - protects the reader task from OOM per page.
                if text.len() > MAX_FRAME_BYTES {
                    tracing::warn!(
                        bytes = text.len(),
                        "oversize CDP frame - failing pending calls"
                    );
                    // Fail every pending call now (a silent drop would only
                    // surface later as a 15s timeout).
                    let keys: Vec<u64> = pending_reader.iter().map(|e| *e.key()).collect();
                    for key in keys {
                        if let Some((_, (method, sender))) = pending_reader.remove(&key) {
                            let _ = sender.send(Err(CdpError::ProtocolError {
                                method,
                                code: -1,
                                message: format!(
                                    "CDP response exceeded {MAX_FRAME_BYTES} byte limit"
                                ),
                            }));
                        }
                    }
                    break;
                }
                let value: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                dispatch_frame(&value, &pending_reader, &events_rx);
            }
            // Socket dead: fail every still-pending call with its real method.
            let pending_keys: Vec<u64> = pending_writer.iter().map(|e| *e.key()).collect();
            for key in pending_keys {
                if let Some((_, (method, sender))) = pending_writer.remove(&key) {
                    let _ = sender.send(Err(CdpError::Disconnected { method }));
                }
            }
        }));

        Ok(Self {
            next_id: AtomicU64::new(1),
            pending,
            sender: Mutex::new(Some(out_tx)),
            events,
            tasks: Mutex::new(tasks),
            writer_alive,
        })
    }

    /// Shared event router for `connect`ed transports.
    pub fn events(&self) -> Arc<EventEmitter> {
        self.events.clone()
    }

    /// Fail all still-pending calls, close writer, abort I/O tasks.
    ///
    /// Socket halves close once the writer task exits and this client is
    /// dropped. Pair with `ChromeProcess::shutdown()` in `BrowserSession`.
    pub fn shutdown(&self) {
        // Take the sender so writer loop sees channel close and exits.
        if let Ok(mut slot) = self.sender.lock() {
            let _ = slot.take();
        }
        let keys: Vec<u64> = self.pending.iter().map(|e| *e.key()).collect();
        for key in keys {
            if let Some((_, (method, sender))) = self.pending.remove(&key) {
                let _ = sender.send(Err(CdpError::Disconnected { method }));
            }
        }
        if let Ok(mut tasks) = self.tasks.lock() {
            for h in tasks.drain(..) {
                h.abort();
            }
        }
        tracing::debug!("CdpClient shutdown: pending calls drained");
    }

    /// Send a JSON-RPC `value` payload and await its `result`.
    ///
    /// `session_id` is included as top-level `sessionId` when `Some` (flatten
    /// mode). `method` is e.g. `"Page.navigate"`.
    pub async fn send(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, (method.to_string(), tx));

        // Guard the pending entry: if the caller cancels `send` mid-flight
        // (select/timeout), the entry is dropped - no unbounded growth (M2).
        struct PendingGuard<'a> {
            pending: &'a PendingMap,
            id: u64,
            armed: bool,
        }
        impl Drop for PendingGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.pending.remove(&self.id);
                }
            }
        }
        let mut guard = PendingGuard {
            pending: &self.pending,
            id,
            armed: true,
        };

        let mut frame = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(sid) = session_id {
            frame["sessionId"] = json!(sid);
        }

        if !self.writer_alive.load(Ordering::SeqCst) {
            return Err(CdpError::Disconnected {
                method: method.to_string(),
            });
        }
        let sent: Result<(), ()> = match self.sender.lock() {
            Ok(slot) => match slot.as_ref() {
                Some(tx) => tx.send(frame.to_string()).map_err(|_| ()),
                None => Err(()),
            },
            Err(_) => Err(()),
        };
        if sent.is_err() {
            return Err(CdpError::Disconnected {
                method: method.to_string(),
            });
        }

        let rx_result = tokio::time::timeout(CALL_TIMEOUT, rx).await;
        match rx_result {
            Err(_) => Err(CdpError::Timeout {
                method: method.to_string(),
            }),
            Ok(Err(_)) => Err(CdpError::Disconnected {
                method: method.to_string(),
            }),
            Ok(Ok(inner)) => {
                guard.armed = false;
                inner.map_err(|e| match e {
                    CdpError::ProtocolError { code, message, .. } => CdpError::ProtocolError {
                        method: method.to_string(),
                        code,
                        message,
                    },
                    CdpError::Disconnected { .. } => CdpError::Disconnected {
                        method: method.to_string(),
                    },
                    other => other,
                })
            }
        }
    }

    /// Typed variant of [`send`]: serializes `P` to params, deserializes result to `R`.
    pub async fn send_typed<P, R>(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: &P,
    ) -> Result<R, CdpError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let params_value = serde_json::to_value(params)?;
        let result = self.send(session_id, method, params_value).await?;
        Ok(serde_json::from_value(result)?)
    }
}

pub(crate) fn dispatch_frame(value: &Value, pending: &PendingMap, events: &EventEmitter) {
    if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
        if let Some((_, (method, sender))) = pending.remove(&id) {
            if let Some(err) = value.get("error") {
                let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown CDP error")
                    .to_string();
                let _ = sender.send(Err(CdpError::ProtocolError {
                    method,
                    code,
                    message,
                }));
            } else if let Some(result) = value.get("result") {
                let _ = sender.send(Ok(result.clone()));
            } else {
                let _ = sender.send(Err(CdpError::ProtocolError {
                    method,
                    code: -1,
                    message: "response missing result and error".to_string(),
                }));
            }
            return;
        }
        // Unknown id: ignore (late response after timeout).
        return;
    }

    if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        events.emit(method, params);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::oneshot;

    #[test]
    fn dispatch_resolves_pending_rpc_with_result() {
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = EventEmitter::new();
        let (tx, mut rx) = oneshot::channel();
        pending.insert(7, ("Test.method".to_string(), tx));
        dispatch_frame(&json!({"id": 7, "result": {"ok": true}}), &pending, &events);
        let got = rx.try_recv().expect("sender fired");
        assert!(got.is_ok());
        assert_eq!(got.unwrap()["ok"], true);
    }

    #[test]
    fn dispatch_maps_protocol_error() {
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = EventEmitter::new();
        let (tx, mut rx) = oneshot::channel();
        pending.insert(1, ("Page.navigate".to_string(), tx));
        dispatch_frame(
            &json!({"id": 1, "error": {"code": -32000, "message": "boom"}}),
            &pending,
            &events,
        );
        let err = rx.try_recv().unwrap().unwrap_err();
        assert!(matches!(err, CdpError::ProtocolError { message, .. } if message == "boom"));
    }

    #[test]
    fn dispatch_ignores_unknown_id_without_panicking() {
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = EventEmitter::new();
        dispatch_frame(&json!({"id": 99999, "result": {}}), &pending, &events);
    }

    #[test]
    fn dispatch_emits_events_without_id() {
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let events = EventEmitter::new();
        let mut sub = events.subscribe("Network.loadingFinished");
        dispatch_frame(
            &json!({"method": "Network.loadingFinished", "params": {"requestId": "x"}}),
            &pending,
            &events,
        );
        let ev = sub.try_recv().expect("event emitted");
        assert_eq!(ev["requestId"], "x");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_transport_handles_framing_events_and_shutdown() {
        use std::os::unix::io::{AsFd, FromRawFd, OwnedFd, RawFd};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::unix::pipe::{Receiver, Sender};

        fn pipe_pair() -> (OwnedFd, OwnedFd) {
            let mut fds: [RawFd; 2] = [0; 2];
            unsafe {
                assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
                (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
            }
        }

        let (peer_rx_fd, client_tx_fd) = pipe_pair();
        let (client_rx_fd, peer_tx_fd) = pipe_pair();
        set_nonblocking(peer_rx_fd.as_fd()).unwrap();
        set_nonblocking(peer_tx_fd.as_fd()).unwrap();
        let mut peer_rx = Receiver::from_owned_fd(peer_rx_fd).unwrap();
        let mut peer_tx = Sender::from_owned_fd(peer_tx_fd).unwrap();

        let client = CdpClient::connect_pipe(client_rx_fd, client_tx_fd)
            .await
            .expect("connect_pipe");
        let mut ev_rx = client.events().subscribe("Target.targetCreated");

        let peer_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = peer_rx.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.iter().position(|&b| b == 0) {
                    let req: Value = serde_json::from_slice(&buf[..pos]).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let ev = json!({"method": "Target.targetCreated", "params": {"targetInfo": {"targetId": "t1"}}});
                    let reply = json!({"id": id, "result": {"echo": req["method"]}});
                    // Send event + split reply across two writes to test framing.
                    let mut payload = serde_json::to_vec(&ev).unwrap();
                    payload.push(0);
                    let reply_bytes = serde_json::to_vec(&reply).unwrap();
                    let mid = reply_bytes.len() / 2;
                    peer_tx.write_all(&payload).await.unwrap();
                    peer_tx.write_all(&reply_bytes[..mid]).await.unwrap();
                    peer_tx.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    peer_tx.write_all(&reply_bytes[mid..]).await.unwrap();
                    peer_tx.write_all(b"\0").await.unwrap();
                    peer_tx.flush().await.unwrap();
                    break;
                }
            }
        });

        let res = client
            .send(None, "Browser.getVersion", json!({}))
            .await
            .expect("pipe rpc");
        assert_eq!(res["echo"], "Browser.getVersion");
        let ev = ev_rx.recv().await.expect("event delivered");
        assert_eq!(ev["targetInfo"]["targetId"], "t1");

        client.shutdown();
        peer_task.await.unwrap();
    }
}
