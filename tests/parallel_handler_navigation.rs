//! Phase 3: navigation flow round-trip on the parallel handler.

#![cfg(feature = "parallel-handler")]

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::sync::Arc;
use std::time::Duration;

use chromey::handler::HandlerConfig;
use chromey::Browser;
use chromey_cdp::cdp::browser_protocol::page::NavigateParams;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_handler_completes_navigation() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect to mock");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    let page = browser.new_page("about:blank").await.expect("new_page");

    // `Page.navigate` against the mock — completes when the lifecycle
    // events fire (`load` etc. emitted by the mock immediately after the
    // response).
    let resp = page
        .execute(NavigateParams::new("https://example.test/"))
        .await
        .expect("navigate");
    assert!(
        !resp.frame_id.as_ref().is_empty(),
        "navigate response should carry a frame_id"
    );

    drop(browser);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_handler_supports_concurrent_navigations_across_pages() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect to mock");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    const PAGES: usize = 4;
    let mut tasks = Vec::with_capacity(PAGES);
    for i in 0..PAGES {
        let b = browser.clone();
        tasks.push(tokio::spawn(async move {
            let page = b.new_page("about:blank").await.expect("new_page");
            page.execute(NavigateParams::new(format!("https://example.test/{i}")))
                .await
                .map(|_| ())
        }));
    }

    let results = futures_util::future::join_all(tasks).await;
    for r in results {
        r.expect("join").expect("navigate");
    }

    drop(browser);
}
