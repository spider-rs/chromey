//! No Chrome or external endpoints. Public-driver tests use a loopback fake
//! WebSocket. Exact queue saturation and permanently Pending sink feed/flush/
//! close are covered by the companion library tests (independent of OS TCP
//! buffer sizes); those tests use bounded queues of capacity one.
#![cfg(feature = "parallel-handler")]

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::collections::HashSet;
use std::time::Duration;

use chromiumoxide::browser::BrowserConfig;
use chromiumoxide::error::CdpError;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::Browser;
use chromiumoxide_cdp::cdp::js_protocol::runtime::EnableParams;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[test]
fn builder_rejects_zero_and_unrepresentable_request_timeouts() {
    for value in [Duration::ZERO, Duration::MAX] {
        let error = BrowserConfig::builder()
            .request_timeout(value)
            .build()
            .unwrap_err();
        assert!(
            error.contains("request_timeout"),
            "validation must precede executable detection: {error}"
        );
    }
    BrowserConfig::builder()
        .chrome_executable("/not-a-browser")
        .request_timeout(Duration::from_secs(1))
        .build()
        .unwrap();
}

#[tokio::test]
async fn direct_handler_config_rejects_invalid_timeouts_before_connecting() {
    for value in [Duration::ZERO, Duration::MAX] {
        let config = HandlerConfig {
            request_timeout: value,
            ..Default::default()
        };
        let error = Browser::connect_with_config("invalid-url-must-not-be-used", config)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("request_timeout"));
    }
}

#[tokio::test]
async fn blocked_websocket_peer_leaves_browser_timeouts_and_close_serviceable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (ready, handshake) = oneshot::channel();
    let (stop, stopped) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        ready.send(()).unwrap();
        // Hold a real WebSocket open without reading or responding. No
        // timing-based assumption about when the OS send buffer fills.
        let _ = stopped.await;
        drop(socket);
    });
    let config = HandlerConfig {
        request_timeout: Duration::from_millis(50),
        connection_retries: 0,
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(url, config).await.unwrap();
    handshake.await.unwrap();
    let driver = tokio::spawn(handler.run_parallel());
    let (command, page) = timeout(Duration::from_secs(3), async {
        tokio::join!(
            browser.execute(EnableParams::default()),
            browser.new_page("about:blank")
        )
    })
    .await
    .expect("driver stalled with an unresponsive peer");
    assert!(
        matches!(command, Err(CdpError::Timeout)),
        "expected command timeout, got {:?}",
        command.err()
    );
    assert!(matches!(page, Err(CdpError::Timeout)), "{page:?}");
    assert!(
        matches!(
            timeout(Duration::from_secs(1), browser.close())
                .await
                .unwrap(),
            Err(CdpError::Timeout)
        ),
        "an unresponsive peer must not be reported as successfully closed"
    );
    timeout(Duration::from_secs(1), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stop.send(()).unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn close_reaches_websocket_peer_and_waits_for_acknowledgement() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (observed, received_close) = oneshot::channel();
    let (acknowledge, allow_response) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(frame) = socket.next().await {
            let frame = frame.unwrap();
            let request: serde_json::Value =
                serde_json::from_str(frame.to_text().unwrap()).unwrap();
            let close = request["method"] == "Browser.close";
            if close {
                assert!(request
                    .get("sessionId")
                    .map_or(true, serde_json::Value::is_null));
                observed.send(()).unwrap();
                allow_response.await.unwrap();
                socket
                    .send(WsMessage::Text(
                        serde_json::json!({"id": request["id"], "result": {}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                return;
            }
            assert_eq!(request["method"], "Target.setDiscoverTargets");
            socket
                .send(WsMessage::Text(
                    serde_json::json!({"id": request["id"], "result": {}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
        panic!("driver closed the socket before sending Browser.close");
    });

    timeout(Duration::from_secs(3), async {
        let (browser, handler) = Browser::connect(url).await.unwrap();
        let driver = tokio::spawn(handler.run_parallel());
        let close = browser.close();
        tokio::pin!(close);
        tokio::select! {
            result = &mut close => panic!("close completed before peer acknowledgement: {result:?}"),
            observed = received_close => observed.unwrap(),
        }
        assert!(!driver.is_finished());
        acknowledge.send(()).unwrap();
        close.await.unwrap();
        driver.await.unwrap().unwrap();
        peer.await.unwrap();
    }).await.expect("CDP close or shutdown stalled");
}

#[tokio::test]
async fn accepted_commands_keep_their_responses_across_sessions_and_browser() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let (browser, handler) = Browser::connect(mock.ws_url()).await.unwrap();
    let driver = tokio::spawn(handler.run_parallel());
    timeout(Duration::from_secs(5), async {
        let page_a = browser.new_page("about:blank").await.unwrap();
        let page_b = browser.new_page("about:blank").await.unwrap();
        let commands = (0..128).map(|i| {
            let page = if i % 2 == 0 { &page_a } else { &page_b };
            page.execute(EnableParams::default())
        });
        let responses = futures_util::future::join_all(commands).await;
        let mut ids = HashSet::new();
        for response in responses {
            assert!(ids.insert(response.unwrap().id));
        }
        let response = browser.execute(EnableParams::default()).await.unwrap();
        assert!(ids.insert(response.id));
        assert_eq!(ids.len(), 129);
        browser.close().await.unwrap();
    })
    .await
    .expect("accepted command or shutdown was lost");
    timeout(Duration::from_secs(1), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn network_event_burst_preserves_events_and_command_responses() {
    use chromiumoxide_cdp::cdp::browser_protocol::network::EventDataReceived;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let mock = cdp_mock::CdpMock::spawn().await;
    let upstream_url = mock.ws_url();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (burst, mut trigger) = oneshot::channel::<String>();
    let (injected, injection_done) = oneshot::channel();
    // A loopback proxy adds a network burst to the existing fake CDP server
    // without changing the shared test support or starting a browser.
    let proxy = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let client = tokio_tungstenite::accept_async(socket).await.unwrap();
        let (upstream, _) = tokio_tungstenite::connect_async(upstream_url)
            .await
            .unwrap();
        let (mut client_tx, mut client_rx) = client.split();
        let (mut upstream_tx, mut upstream_rx) = upstream.split();
        let mut injected = Some(injected);
        loop {
            tokio::select! {
                session = &mut trigger, if injected.is_some() => {
                    let session = session.unwrap();
                    for index in 0..4096 {
                        let event = serde_json::json!({
                            "method": "Network.dataReceived", "sessionId": session,
                            "params": {"requestId": index.to_string(), "timestamp": index,
                                "dataLength": 1024, "encodedDataLength": 512}
                        });
                        client_tx.feed(WsMessage::Text(event.to_string().into())).await.unwrap();
                    }
                    client_tx.flush().await.unwrap();
                    injected.take().unwrap().send(()).unwrap();
                }
                frame = client_rx.next() => {
                    let Some(Ok(frame)) = frame else { break };
                    if frame.is_close() || upstream_tx.send(frame).await.is_err() { break; }
                }
                frame = upstream_rx.next() => {
                    let Some(Ok(frame)) = frame else { break };
                    if frame.is_close() || client_tx.send(frame).await.is_err() { break; }
                }
            }
        }
    });
    let (browser, handler) = Browser::connect(url).await.unwrap();
    let driver = tokio::spawn(handler.run_parallel());
    timeout(Duration::from_secs(10), async {
        let page = browser.new_page("about:blank").await.unwrap();
        let mut events = page.event_listener::<EventDataReceived>().await.unwrap();
        // Round-trip after listener registration is a barrier before injection.
        page.execute(EnableParams::default()).await.unwrap();
        burst.send(page.session_id().as_ref().to_owned()).unwrap();
        injection_done.await.unwrap();
        // Delay the event consumer until the entire burst is sent and commands
        // complete. Exact SessionTask starvation is covered by the unit test.
        let (page_response, browser_response) = tokio::join!(
            page.execute(EnableParams::default()),
            browser.execute(EnableParams::default())
        );
        assert_ne!(page_response.unwrap().id, browser_response.unwrap().id);
        for index in 0..4096 {
            let event = events
                .next()
                .await
                .expect("event stream closed during burst");
            assert_eq!(event.request_id.as_ref(), index.to_string());
        }
        page.execute(EnableParams::default()).await.unwrap();
        browser.close().await.unwrap();
    })
    .await
    .expect("burst lost an event, response, or shutdown");
    timeout(Duration::from_secs(1), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(1), proxy)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn normal_single_handler_still_round_trips_and_shuts_down() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let (browser, handler) = Browser::connect(mock.ws_url()).await.unwrap();
    let driver = tokio::spawn(handler.run());
    timeout(Duration::from_secs(5), async {
        let page = browser.new_page("about:blank").await.unwrap();
        page.execute(EnableParams::default()).await.unwrap();
        browser.execute(EnableParams::default()).await.unwrap();
    })
    .await
    .unwrap();
    drop(browser);
    timeout(Duration::from_secs(2), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
