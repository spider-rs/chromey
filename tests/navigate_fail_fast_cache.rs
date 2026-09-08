#![cfg(feature = "_cache")]

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::time::{Duration, Instant};

use chromiumoxide::error::CdpError;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::{Browser, Page};
use chromiumoxide_cdp::cdp::browser_protocol::page::NavigateParams;
use futures_util::StreamExt;
use tokio::time::timeout;

async fn setup_with_handler(poll_as_stream: bool) -> (cdp_mock::CdpMock, Browser, Page) {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(60),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect to mock");
    if poll_as_stream {
        tokio::spawn(async move {
            let mut handler = handler;
            while handler.next().await.is_some() {}
        });
    } else {
        tokio::spawn(handler.run());
    }
    let page = browser.new_page("about:blank").await.expect("new_page");
    (mock, browser, page)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_intercept_navigate_error_text_resolves_fast() {
    for poll_as_stream in [false, true] {
        let (mock, _browser, page) = setup_with_handler(poll_as_stream).await;
        mock.fail_navigate("net::ERR_NAME_NOT_RESOLVED").await;
        let start = Instant::now();
        let result = timeout(
            Duration::from_secs(10),
            page.http_future_with_cache_intercept_enabled(
                NavigateParams::new("https://nxdomain.test/"),
                None,
                None,
                None,
                Some("http://127.0.0.1:9"),
                None,
            ),
        )
        .await
        .expect("cache intercept must release terminal navigate acks promptly");
        assert!(
            matches!(result, Err(CdpError::ChromeMessage(ref error)) if error == "net::ERR_NAME_NOT_RESOLVED"),
            "{result:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_intercept_navigation_timeout_releases_the_committed_request() {
    for poll_as_stream in [false, true] {
        let (mock, _browser, page) = setup_with_handler(poll_as_stream).await;
        mock.withhold_load();
        let mut params = NavigateParams::new("https://example.test/cache-pending-load");
        params.timeout = Some(3000);
        let start = Instant::now();
        let req = timeout(
            Duration::from_secs(10),
            page.http_future_with_cache_intercept_enabled(
                params,
                None,
                None,
                None,
                Some("http://127.0.0.1:9"),
                None,
            ),
        )
        .await
        .expect("cache intercept must honour the navigation timeout")
        .expect("committed request");
        assert!(req.failure_text.is_none());
        assert_eq!(
            req.response.as_ref().map(|response| response.status),
            Some(200)
        );
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
