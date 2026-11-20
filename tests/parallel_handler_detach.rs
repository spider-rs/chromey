//! Phase 3: target detach / lifecycle handling for the parallel handler.

#![cfg(feature = "parallel-handler")]

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::sync::Arc;
use std::time::Duration;

use chromey::handler::HandlerConfig;
use chromey::Browser;
use chromey_cdp::cdp::js_protocol::runtime::EvaluateParams;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_handler_errors_on_detached_target() {
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

    let page = browser.new_page("about:blank").await.expect("new_page");
    let session_id = page.session_id().as_ref().to_string();
    let target_id: String = page.target_id().clone().into();

    // Sanity: a normal command works before detach.
    page.execute(EvaluateParams::new("'pre'"))
        .await
        .expect("pre");

    // Force-detach the session.
    mock.detach_session(&session_id, &target_id).await;

    // Allow the detach event to propagate to the Router and through to the
    // SessionTask. A short loop polls until subsequent commands start failing
    // — we don't care about the exact timing, only that the page is no longer
    // serviceable after detach.
    let mut last_err = None;
    for _ in 0..50 {
        match page.execute(EvaluateParams::new("'post'")).await {
            Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }

    assert!(
        last_err.is_some(),
        "expected post-detach command to error within ~500 ms"
    );

    drop(browser);
}
