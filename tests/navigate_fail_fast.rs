#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::time::{Duration, Instant};

use chromiumoxide::error::CdpError;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::{Browser, Page};
use chromiumoxide_cdp::cdp::browser_protocol::page::NavigateParams;
use futures_util::StreamExt;
use tokio::time::timeout;

async fn setup(request_timeout: Duration) -> (cdp_mock::CdpMock, Browser, Page) {
    setup_with_handler(request_timeout, false).await
}

async fn setup_with_handler(
    request_timeout: Duration,
    poll_as_stream: bool,
) -> (cdp_mock::CdpMock, Browser, Page) {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout,
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
async fn navigate_error_text_resolves_fast() {
    let (mock, _browser, page) = setup(Duration::from_secs(3)).await;
    mock.fail_navigate("net::ERR_NAME_NOT_RESOLVED").await;
    let fut = page
        .navigate_http_future(NavigateParams::new("https://nxdomain.test/"))
        .expect("navigate future");
    let start = Instant::now();
    let req = timeout(Duration::from_secs(2), fut)
        .await
        .expect("failed navigate ack should resolve before the two-second timeout")
        .expect("synthetic failed request")
        .expect("navigation request");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "failed navigate ack should resolve in under one second, took {elapsed:?}"
    );
    assert_eq!(
        req.failure_text.as_deref(),
        Some("net::ERR_NAME_NOT_RESOLVED")
    );
    assert!(req.response.is_none());
    assert_eq!(req.url.as_deref(), Some("https://nxdomain.test/"));
    assert!(req.is_navigation_request);
    assert!(req.method.is_none());
    assert!(req.headers.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goto_error_text_resolves_fast_and_allows_next_navigation() {
    for poll_as_stream in [false, true] {
        let (mock, _browser, page) =
            setup_with_handler(Duration::from_secs(3), poll_as_stream).await;
        mock.fail_navigate("net::ERR_NAME_NOT_RESOLVED").await;
        let start = Instant::now();
        let result = timeout(Duration::from_secs(1), page.goto("https://nxdomain.test/"))
            .await
            .expect("goto must release a failed ack without lifecycle in under one second");
        assert!(
            matches!(result, Err(CdpError::ChromeMessage(ref err)) if err == "net::ERR_NAME_NOT_RESOLVED"),
            "goto should return the navigate errorText, got {result:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(1));

        mock.clear_fail_navigate().await;
        timeout(Duration::from_secs(1), page.goto("https://recovered.test/"))
            .await
            .expect("abandoned navigation must not block the next goto")
            .expect("next goto should succeed on the reused page");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn err_aborted_keeps_waiting_for_the_replacement_navigation() {
    let (mock, _browser, page) = setup(Duration::from_secs(3)).await;
    mock.fail_navigate("net::ERR_ABORTED").await;
    let fut = page
        .navigate_http_future(NavigateParams::new("https://aborted.test/"))
        .expect("navigate future");
    tokio::pin!(fut);
    let pending = timeout(Duration::from_millis(300), &mut fut).await;
    assert!(
        pending.is_err(),
        "ERR_ABORTED must keep waiting for replacement navigation, got {pending:?}"
    );

    mock.clear_fail_navigate().await;
    mock.emit_navigation(page.session_id().as_ref(), "https://replacement.test/")
        .await;
    let req = timeout(Duration::from_secs(2), &mut fut)
        .await
        .expect("replacement navigation should resolve within two seconds")
        .expect("replacement navigation")
        .expect("replacement navigation request");
    assert!(req.failure_text.is_none());
    assert!(req.response.is_some());
    assert_committed_url(&req, "https://replacement.test/");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn success_path_unchanged() {
    for poll_as_stream in [false, true] {
        let (_mock, _browser, page) =
            setup_with_handler(Duration::from_secs(3), poll_as_stream).await;
        let fut = page
            .navigate_http_future(NavigateParams::new("https://example.test/"))
            .expect("navigate future");
        let req = timeout(Duration::from_secs(1), fut)
            .await
            .expect("successful navigation should resolve within one second")
            .expect("successful navigation")
            .expect("navigation request");
        assert!(req.failure_text.is_none());
        assert_eq!(req.response.as_ref().map(|r| r.status), Some(200));
        assert_committed_url(&req, "https://example.test/");
    }
}

fn assert_committed_url(req: &chromiumoxide::handler::http::HttpRequest, url: &str) {
    // Real requests never populate HttpRequest::url; only the synthetic failed
    // request does. Read response.url for the committed URL instead.
    assert_eq!(req.response.as_ref().map(|r| r.url.as_str()), Some(url));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_response_code_failure_keeps_waiting_for_the_committed_body() {
    let (mock, _browser, page) = setup(Duration::from_secs(3)).await;
    mock.fail_navigate("net::ERR_HTTP_RESPONSE_CODE_FAILURE (403)")
        .await;
    mock.commit_failed_navigation().await;
    let fut = page
        .navigate_http_future(NavigateParams::new("https://forbidden.test/"))
        .expect("navigate future");
    let req = timeout(Duration::from_secs(2), fut)
        .await
        .expect("committed error body should resolve within two seconds")
        .expect("committed error navigation")
        .expect("committed navigation request");
    assert!(
        req.failure_text.is_none(),
        "HTTP response code failure must wait for the committed body, got {:?}",
        req.failure_text
    );
    assert!(req.response.is_some());
    assert_committed_url(&req, "https://forbidden.test/");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_error_text_is_treated_as_absent() {
    let (mock, _browser, page) = setup(Duration::from_secs(3)).await;
    mock.fail_navigate("").await;
    let fut = page
        .navigate_http_future(NavigateParams::new("https://empty-error.test/"))
        .expect("navigate future");
    tokio::pin!(fut);
    let pending = timeout(Duration::from_millis(300), &mut fut).await;
    assert!(
        pending.is_err(),
        "empty errorText must keep waiting for replacement navigation, got {pending:?}"
    );

    mock.clear_fail_navigate().await;
    mock.emit_navigation(page.session_id().as_ref(), "https://replacement.test/")
        .await;
    let req = timeout(Duration::from_secs(2), &mut fut)
        .await
        .expect("replacement navigation should resolve within two seconds")
        .expect("replacement navigation")
        .expect("replacement navigation request");
    assert!(req.failure_text.is_none());
    assert!(req.response.is_some());
    assert_committed_url(&req, "https://replacement.test/");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generic_http_future_still_waits() {
    let (mock, _browser, page) = setup(Duration::from_secs(1)).await;
    mock.fail_navigate("net::ERR_NAME_NOT_RESOLVED").await;
    let fut = page
        .http_future(NavigateParams::new("https://nxdomain.test/"))
        .expect("generic HTTP future");
    let start = Instant::now();
    let result = timeout(Duration::from_secs(2), fut)
        .await
        .expect("generic HTTP future should reach its own request timeout");
    assert!(
        matches!(result, Err(CdpError::Timeout)),
        "generic HTTP future should time out, got {result:?}"
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(900),
        "generic HTTP future should wait for its request timeout, took {elapsed:?}"
    );

    // An empty errorText keeps the handler's navigation watcher armed. Detach
    // its frame without lifecycle completion to distinguish the watcher's
    // FrameNotFound error from the target waiter's generic Timeout.
    let (mock, _browser, page) = setup(Duration::from_secs(3)).await;
    mock.fail_navigate("").await;
    let fut = page
        .http_future(NavigateParams::new("https://pending.test/"))
        .expect("generic HTTP future");
    tokio::pin!(fut);
    assert!(timeout(Duration::from_millis(300), &mut fut).await.is_err());
    mock.detach_main_frame(page.session_id().as_ref()).await;
    let result = timeout(Duration::from_secs(1), &mut fut)
        .await
        .expect("generic path must retain the frame manager's navigation watcher");
    assert!(
        matches!(result, Err(CdpError::FrameNotFound(_))),
        "expected the navigation watcher's FrameNotFound, got {result:?}"
    );
}
