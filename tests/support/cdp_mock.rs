//! Minimal in-process CDP mock server used by integration tests and benches.
//!
//! Speaks just enough Chrome DevTools Protocol to let `Browser::connect` attach
//! and to drive `Browser::new_page` to a usable `Page` handle. Every supported
//! method returns a canned shape; unknown methods return `{}` so unrelated CDP
//! calls don't error out.
//!
//! The mock binds to `127.0.0.1:0`, so each test gets its own ephemeral port.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// One thread-safe entry point per active connection so tests can inject
/// out-of-band events (e.g. force-detach a session).
type ConnectionInjector = tokio::sync::mpsc::Sender<String>;

struct MockShared {
    /// Senders into each connection's writer task; tests `inject(...)`
    /// arbitrary CDP frames into a connection by pushing JSON strings.
    connections: Mutex<Vec<ConnectionInjector>>,
    /// CDP method names whose requests the mock should silently swallow
    /// (no response). Lets eviction tests exercise the timeout path.
    swallow: Mutex<hashbrown::HashSet<String>>,
    navigate_error: Mutex<Option<NavigateError>>,
    /// Raw `params` object of every `Page.navigate` request the mock received,
    /// exactly as it arrived on the wire. Tests drain the matching receiver to
    /// assert which keys the client actually serialized. Unbounded, so a
    /// connection task never blocks on a test that stops draining.
    navigate_params: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    injected_loader: AtomicU64,
}

#[derive(Clone)]
struct NavigateError {
    text: String,
    commits: bool,
}

/// Handle to a running mock server. Drop it to shut the server down.
pub struct CdpMock {
    addr: SocketAddr,
    accept_task: Option<JoinHandle<()>>,
    shutdown: Arc<tokio::sync::Notify>,
    shared: Arc<MockShared>,
    /// Receiving half of `MockShared::navigate_params`. Owned by the handle, so
    /// draining it needs `&mut self` instead of a shared lock.
    navigate_params: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
}

impl CdpMock {
    /// Bind to a free local port and start accepting WebSocket connections.
    pub async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let shutdown_clone = shutdown.clone();
        let (navigate_tx, navigate_rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(MockShared {
            connections: Default::default(),
            swallow: Default::default(),
            navigate_error: Default::default(),
            navigate_params: navigate_tx,
            injected_loader: AtomicU64::new(0),
        });
        let shared_clone = shared.clone();

        let accept_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_clone.notified() => break,
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, _)) => {
                                tokio::spawn(handle_connection(stream, shared_clone.clone()));
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Self {
            addr,
            accept_task: Some(accept_task),
            shutdown,
            shared,
            navigate_params: navigate_rx,
        }
    }

    /// `ws://…` URL that `Browser::connect` understands.
    pub fn ws_url(&self) -> String {
        format!("ws://{}/devtools/browser/mock", self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Swallow any subsequent request whose `method` matches — the mock
    /// will not respond. Test harness uses this to drive the parallel
    /// handler's per-session eviction path.
    pub async fn swallow_method(&self, method: &str) {
        self.shared.swallow.lock().await.insert(method.to_string());
    }

    /// Return this `errorText` in subsequent navigate acks without completion events.
    pub async fn fail_navigate(&self, error_text: &str) {
        *self.shared.navigate_error.lock().await = Some(NavigateError {
            text: error_text.to_string(),
            commits: false,
        });
    }

    /// Emit the full navigation burst even when the ack carries `errorText`.
    pub async fn commit_failed_navigation(&self) {
        if let Some(error) = self.shared.navigate_error.lock().await.as_mut() {
            error.commits = true;
        }
    }

    /// Take every `Page.navigate` params object recorded since the last drain,
    /// in arrival order. Non-blocking: it returns what has already arrived and
    /// never waits for more.
    pub fn drain_navigate_params(&mut self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(params) = self.navigate_params.try_recv() {
            out.push(params);
        }
        out
    }

    /// Drop the recorded `Page.navigate` params, so a test can ignore the
    /// navigations `Browser::new_page` performs during setup.
    pub fn clear_navigate_params(&mut self) {
        let _ = self.drain_navigate_params();
    }

    /// Restore successful navigate acks and completion events.
    pub async fn clear_fail_navigate(&self) {
        *self.shared.navigate_error.lock().await = None;
    }

    /// Emit a complete replacement navigation on every active connection.
    pub async fn emit_navigation(&self, session_id: &str, url: &str) {
        let frame_id = format!("frame-{session_id}");
        let n = self.shared.injected_loader.fetch_add(1, Ordering::Relaxed);
        let loader_id = format!("loader-injected-{n}");
        let burst = navigation_burst(session_id, &frame_id, &loader_id, url);
        let conns = self.shared.connections.lock().await;
        for tx in conns.iter() {
            for payload in &burst {
                let _ = tx.send(payload.clone()).await;
            }
        }
    }

    /// Remove the main frame without completing its navigation lifecycle.
    pub async fn detach_main_frame(&self, session_id: &str) {
        let payload = serde_json::json!({
            "method": "Page.frameDetached",
            "sessionId": session_id,
            "params": { "frameId": format!("frame-{session_id}"), "reason": "remove" }
        })
        .to_string();
        let conns = self.shared.connections.lock().await;
        for tx in conns.iter() {
            let _ = tx.send(payload.clone()).await;
        }
    }

    /// Force-emit a `Target.detachedFromTarget` event for `session_id` on
    /// every active connection, simulating a tab close from the browser.
    pub async fn detach_session(&self, session_id: &str, target_id: &str) {
        let payload = serde_json::json!({
            "method": "Target.detachedFromTarget",
            "params": {
                "sessionId": session_id,
                "targetId": target_id,
            }
        })
        .to_string();
        let conns = self.shared.connections.lock().await;
        for tx in conns.iter() {
            let _ = tx.send(payload.clone()).await;
        }
    }
}

impl Drop for CdpMock {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
        if let Some(t) = self.accept_task.take() {
            t.abort();
        }
    }
}

// ---------------------------------------------------------------------------
//  Per-connection state machine
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ConnState {
    next_target: u64,
    next_session: u64,
    next_loader: u64,
    /// session_id → (target_id, frame_id, loader_id)
    sessions: hashbrown::HashMap<String, (String, String, String)>,
}

async fn handle_connection(stream: tokio::net::TcpStream, shared: Arc<MockShared>) {
    let _ = stream.set_nodelay(true);
    let ws = match tokio_tungstenite::accept_async(tokio_tungstenite::MaybeTlsStream::Plain(stream))
        .await
    {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let (mut sink, mut stream) = ws.split();

    // Out-of-band injection channel — the test harness pushes raw JSON
    // here (e.g. `Target.detachedFromTarget`) and we forward it to the
    // connected client.
    let (inject_tx, mut inject_rx) = tokio::sync::mpsc::channel::<String>(64);
    {
        let mut conns = shared.connections.lock().await;
        conns.push(inject_tx);
    }

    let mut state = ConnState::default();

    loop {
        tokio::select! {
            biased;

            injected = inject_rx.recv() => {
                let Some(line) = injected else { break };
                if sink.send(WsMessage::Text(line.into())).await.is_err() {
                    break;
                }
            }

            msg = stream.next() => {
                let Some(msg) = msg else { break };
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let text = match msg {
                    WsMessage::Text(t) => t,
                    WsMessage::Binary(b) => match std::str::from_utf8(&b) {
                        Ok(s) => s.to_string().into(),
                        Err(_) => continue,
                    },
                    WsMessage::Close(_) => break,
                    WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
                };

                let req: serde_json::Value = match serde_json::from_str(text.as_str()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = req.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let method = req
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let session_id = req
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let params = req.get("params").cloned().unwrap_or(serde_json::Value::Null);

                // Record before the swallow check, so a swallowed navigate is
                // still visible to the test that dropped it.
                if method == "Page.navigate" {
                    let _ = shared.navigate_params.send(params.clone());
                }

                if shared.swallow.lock().await.contains(&method) {
                    // Test asked us to drop this method — emit nothing.
                    continue;
                }
                let navigate_error = shared.navigate_error.lock().await.clone();
                let outbox = handle_method(
                    &mut state,
                    id,
                    &method,
                    session_id.as_deref(),
                    &params,
                    navigate_error.as_ref(),
                );
                for line in outbox {
                    if sink.send(WsMessage::Text(line.into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Returns the wire-format JSON strings the mock should emit for this request,
/// in send order. Events come before the response when the handler relies on
/// event-driven state (e.g. `Target.targetCreated` must arrive before the
/// `Target.createTarget` response).
fn handle_method(
    state: &mut ConnState,
    id: u64,
    method: &str,
    session_id: Option<&str>,
    params: &serde_json::Value,
    navigate_error: Option<&NavigateError>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(2);

    match method {
        "Target.setDiscoverTargets" => {
            out.push(empty_response(id));
        }

        "Target.createTarget" => {
            state.next_target += 1;
            let target_id = format!("target-{:08x}", state.next_target);
            let url = params
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank")
                .to_string();
            // `Target.targetCreated` event before the response so the handler
            // has the target in its map by the time `on_response` runs.
            out.push(target_created_event(&target_id, &url));
            out.push(json_response(
                id,
                serde_json::json!({ "targetId": target_id }),
            ));
        }

        "Target.attachToTarget" => {
            let target_id = params
                .get("targetId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            state.next_session += 1;
            state.next_loader += 1;
            let sid = format!("session-{:08x}", state.next_session);
            let frame_id = format!("frame-{}", &sid);
            let loader_id = format!("loader-{:08x}", state.next_loader);
            state.sessions.insert(
                sid.clone(),
                (target_id.clone(), frame_id.clone(), loader_id),
            );
            out.push(attached_to_target_event(&sid, &target_id));
            out.push(json_response(id, serde_json::json!({ "sessionId": sid })));
        }

        "Page.navigate" => {
            let frame_id = session_id
                .and_then(|s| state.sessions.get(s))
                .map(|(_t, f, _l)| f.clone())
                .unwrap_or_else(|| "frame-unknown".into());
            state.next_loader += 1;
            let new_loader = format!("loader-{:08x}", state.next_loader);
            // Update the session record so subsequent commands see the new loader.
            if let Some(sid) = session_id {
                if let Some(entry) = state.sessions.get_mut(sid) {
                    entry.2 = new_loader.clone();
                }
            }
            let url = params
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank")
                .to_string();
            let mut result = serde_json::json!({
                "frameId": frame_id,
                "loaderId": new_loader,
            });
            if let Some(error) = navigate_error {
                result["errorText"] = serde_json::json!(error.text);
            }
            if navigate_error.is_some_and(|error| !error.commits) {
                if let Some(sid) = session_id {
                    out.push(frame_started_loading(sid, &frame_id));
                }
                out.push(json_response(id, result));
            } else if let Some(sid) = session_id {
                out = navigation_burst(sid, &frame_id, &new_loader, &url);
                // Chrome starts loading and sends the document request before
                // the ack, clearing any stale navigation state first.
                out.insert(2, json_response(id, result));
            } else {
                out.push(json_response(id, result));
            }
        }

        "Page.getFrameTree" => {
            let (frame_id, loader_id) = session_id
                .and_then(|s| state.sessions.get(s))
                .map(|(_t, f, l)| (f.clone(), l.clone()))
                .unwrap_or_else(|| ("frame-unknown".into(), "loader-unknown".into()));
            out.push(json_response(
                id,
                serde_json::json!({
                    "frameTree": {
                        "frame": frame_tree_frame(&frame_id, &loader_id),
                        "childFrames": []
                    }
                }),
            ));
            // Mark the main frame as loaded so the target's `Initialized`
            // state can deliver the Page back to the caller of `new_page`.
            if let Some(sid) = session_id {
                out.push(lifecycle_event(sid, &frame_id, &loader_id, "init"));
                out.push(lifecycle_event(
                    sid,
                    &frame_id,
                    &loader_id,
                    "DOMContentLoaded",
                ));
                out.push(lifecycle_event(sid, &frame_id, &loader_id, "load"));
            }
        }

        // All other CDP commands the chromey init chain issues — return `{}`.
        // This is enough because the handler only inspects responses for the
        // specific methods above; everything else just clears its pending entry.
        _ => {
            out.push(empty_response(id));
        }
    }

    out
}

// ---------------------------------------------------------------------------
//  Wire helpers
// ---------------------------------------------------------------------------

fn navigation_burst(session_id: &str, frame_id: &str, loader_id: &str, url: &str) -> Vec<String> {
    let mut out = vec![
        frame_started_loading(session_id, frame_id),
        serde_json::json!({
            "method": "Network.requestWillBeSent",
            "sessionId": session_id,
            "params": {
                "requestId": loader_id,
                "loaderId": loader_id,
                "documentURL": url,
                "frameId": frame_id,
                "type": "Document",
                "request": {
                    "url": url,
                    "method": "GET",
                    "headers": {},
                    "initialPriority": "VeryHigh",
                    "referrerPolicy": "no-referrer-when-downgrade"
                },
                "timestamp": 0.0,
                "wallTime": 0.0,
                "initiator": { "type": "other" },
                "redirectHasExtraInfo": false
            }
        })
        .to_string(),
        serde_json::json!({
            "method": "Network.responseReceived",
            "sessionId": session_id,
            "params": {
                "requestId": loader_id,
                "loaderId": loader_id,
                "frameId": frame_id,
                "type": "Document",
                "timestamp": 0.0,
                "hasExtraInfo": false,
                "response": {
                    "url": url,
                    "status": 200,
                    "statusText": "OK",
                    "headers": { "content-type": "text/html" },
                    "mimeType": "text/html",
                    "charset": "utf-8",
                    "connectionReused": false,
                    "connectionId": 1.0,
                    "encodedDataLength": 0.0,
                    "securityState": "secure",
                    "protocol": "http/1.1"
                }
            }
        })
        .to_string(),
        serde_json::json!({
            "method": "Network.loadingFinished",
            "sessionId": session_id,
            "params": {
                "requestId": loader_id,
                "timestamp": 0.0,
                "encodedDataLength": 0.0
            }
        })
        .to_string(),
        frame_navigated(session_id, frame_id, url, loader_id),
    ];
    for name in [
        "init",
        "DOMContentLoaded",
        "load",
        "networkAlmostIdle",
        "networkIdle",
    ] {
        out.push(lifecycle_event(session_id, frame_id, loader_id, name));
    }
    out.push(frame_stopped_loading(session_id, frame_id));
    out
}

fn empty_response(id: u64) -> String {
    format!(r#"{{"id":{},"result":{{}}}}"#, id)
}

fn json_response(id: u64, result: serde_json::Value) -> String {
    serde_json::json!({ "id": id, "result": result }).to_string()
}

fn target_created_event(target_id: &str, url: &str) -> String {
    serde_json::json!({
        "method": "Target.targetCreated",
        "params": {
            "targetInfo": target_info(target_id, url, false)
        }
    })
    .to_string()
}

fn attached_to_target_event(session_id: &str, target_id: &str) -> String {
    serde_json::json!({
        "method": "Target.attachedToTarget",
        "params": {
            "sessionId": session_id,
            "targetInfo": target_info(target_id, "about:blank", true),
            "waitingForDebugger": false
        }
    })
    .to_string()
}

fn target_info(target_id: &str, url: &str, attached: bool) -> serde_json::Value {
    serde_json::json!({
        "targetId": target_id,
        "type": "page",
        "title": "",
        "url": url,
        "attached": attached,
        "canAccessOpener": false
    })
}

fn frame_tree_frame(frame_id: &str, loader_id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": frame_id,
        "loaderId": loader_id,
        "url": "about:blank",
        "domainAndRegistry": "",
        "securityOrigin": "://",
        "mimeType": "text/html",
        "secureContextType": "Secure",
        "crossOriginIsolatedContextType": "NotIsolated",
        "gatedAPIFeatures": []
    })
}

fn frame_started_loading(session_id: &str, frame_id: &str) -> String {
    serde_json::json!({
        "method": "Page.frameStartedLoading",
        "sessionId": session_id,
        "params": { "frameId": frame_id }
    })
    .to_string()
}

fn frame_stopped_loading(session_id: &str, frame_id: &str) -> String {
    serde_json::json!({
        "method": "Page.frameStoppedLoading",
        "sessionId": session_id,
        "params": { "frameId": frame_id }
    })
    .to_string()
}

fn frame_navigated(session_id: &str, frame_id: &str, url: &str, loader_id: &str) -> String {
    serde_json::json!({
        "method": "Page.frameNavigated",
        "sessionId": session_id,
        "params": {
            "frame": {
                "id": frame_id,
                "loaderId": loader_id,
                "url": url,
                "domainAndRegistry": "",
                "securityOrigin": "://",
                "mimeType": "text/html",
                "secureContextType": "Secure",
                "crossOriginIsolatedContextType": "NotIsolated",
                "gatedAPIFeatures": []
            },
            "type": "Navigation"
        }
    })
    .to_string()
}

fn lifecycle_event(session_id: &str, frame_id: &str, loader_id: &str, name: &str) -> String {
    serde_json::json!({
        "method": "Page.lifecycleEvent",
        "sessionId": session_id,
        "params": {
            "frameId": frame_id,
            "loaderId": loader_id,
            "name": name,
            "timestamp": 0.0
        }
    })
    .to_string()
}

#[allow(unused)]
pub static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
