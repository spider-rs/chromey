//! Phase 5: per-session eviction + browser-level event listeners.

#![cfg(feature = "parallel-handler")]

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::sync::Arc;
use std::time::Duration;

use chromey::handler::HandlerConfig;
use chromey::Browser;
use chromey_cdp::cdp::browser_protocol::target::EventTargetCreated;
use chromey_cdp::cdp::js_protocol::runtime::EvaluateParams;
use futures_util::StreamExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_handler_evicts_stalled_command() {
    let mock = cdp_mock::CdpMock::spawn().await;

    // Make the mock silently swallow the next Runtime.evaluate so the
    // SessionTask's pending entry never gets a response — the eviction
    // tick should fire CdpError::Timeout within ~request_timeout.
    mock.swallow_method("Runtime.evaluate").await;

    let cfg = HandlerConfig {
        // Tight timeout so the test runs fast.
        request_timeout: Duration::from_millis(300),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    let page = browser.new_page("about:blank").await.expect("new_page");

    let start = std::time::Instant::now();
    let result = page.execute(EvaluateParams::new("'never'")).await;
    let elapsed = start.elapsed();

    let err = match result {
        Ok(_) => panic!("expected eviction Timeout, got Ok"),
        Err(e) => e,
    };
    let kind = format!("{err:?}");
    assert!(
        kind.contains("Timeout"),
        "expected Timeout error, got: {kind}"
    );
    // Should fire within roughly request_timeout + one tick (≤2x).
    assert!(
        elapsed < Duration::from_millis(1500),
        "eviction should fire within ~request_timeout, took {elapsed:?}"
    );

    drop(browser);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_handler_browser_event_listener_receives_target_created() {
    let mock = cdp_mock::CdpMock::spawn().await;

    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    // Subscribe BEFORE opening a page so we don't race the event.
    let mut events = browser
        .event_listener::<EventTargetCreated>()
        .await
        .expect("event_listener");

    let _page = browser.new_page("about:blank").await.expect("new_page");

    // Wait up to 2s for the browser-level subscriber to receive the
    // event the Router fanned out.
    let got = tokio::time::timeout(Duration::from_secs(2), events.next()).await;
    let evt = got.expect("listener timed out").expect("stream closed");
    assert!(
        !evt.target_info.target_id.as_ref().is_empty(),
        "Target.targetCreated event should carry a non-empty target id"
    );

    drop(browser);
}
