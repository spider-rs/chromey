#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use chromiumoxide::error::CdpError;
use chromiumoxide::handler::httpfuture::navigate_committed;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::{Browser, Page};
use chromiumoxide_cdp::cdp::browser_protocol::page::NavigateParams;
use futures_util::StreamExt;
use tokio::time::timeout;

enum Driver {
    Run,
    Stream,
    #[cfg(feature = "parallel-handler")]
    Parallel,
}

fn drivers() -> Vec<Driver> {
    vec![
        Driver::Run,
        Driver::Stream,
        #[cfg(feature = "parallel-handler")]
        Driver::Parallel,
    ]
}

async fn setup_with_handler(
    request_timeout: Duration,
    poll_as_stream: bool,
) -> (cdp_mock::CdpMock, Browser, Page) {
    setup_with_driver(
        request_timeout,
        if poll_as_stream {
            Driver::Stream
        } else {
            Driver::Run
        },
    )
    .await
}

async fn setup_with_driver(
    request_timeout: Duration,
    driver: Driver,
) -> (cdp_mock::CdpMock, Browser, Page) {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout,
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect to mock");
    match driver {
        Driver::Run => {
            tokio::spawn(handler.run());
        }
        Driver::Stream => {
            tokio::spawn(async move {
                let mut handler = handler;
                while handler.next().await.is_some() {}
            });
        }
        #[cfg(feature = "parallel-handler")]
        Driver::Parallel => {
            tokio::spawn(handler.run_parallel());
        }
    }
    let page = browser.new_page("about:blank").await.expect("new_page");
    (mock, browser, page)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn navigate_params_omit_timeout_when_unset() {
    let params = NavigateParams::new("https://example.test/");
    let serialized = serde_json::to_value(&params).unwrap();
    assert!(serialized.get("timeout").is_none(), "{serialized}");
    assert_eq!(
        serde_json::to_string(&params).unwrap(),
        r#"{"url":"https://example.test/"}"#
    );

    let (mock, _browser, page) = setup_with_handler(Duration::from_secs(60), false).await;
    timeout(
        Duration::from_secs(5),
        page.navigate_http_future(params).unwrap(),
    )
    .await
    .expect("navigate should finish")
    .expect("navigate");
    let params = mock.last_navigate_params().await.expect("captured params");
    assert!(params.get("timeout").is_none(), "{params}");
    let keys: BTreeSet<_> = params
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, BTreeSet::from(["url", "frameId"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn navigate_params_carry_timeout_when_set() {
    let (mock, _browser, page) = setup_with_handler(Duration::from_secs(60), false).await;
    let params = NavigateParams::builder()
        .url("https://example.test/override")
        .timeout(9_000_i64)
        .build()
        .unwrap();
    timeout(
        Duration::from_secs(5),
        page.navigate_http_future_with_timeout(params, Duration::from_secs(3))
            .unwrap(),
    )
    .await
    .expect("navigate should finish")
    .expect("navigate");
    assert_eq!(mock.last_navigate_params().await.unwrap()["timeout"], 3000);

    let params = NavigateParams::builder()
        .url("https://example.test/builder")
        .timeout(3_000_i64)
        .build()
        .unwrap();
    let fut = page.navigate_http_future(params).unwrap();
    assert_eq!(fut.navigation_timeout(), Some(Duration::from_secs(3)));
    timeout(Duration::from_secs(5), fut)
        .await
        .expect("navigate should finish")
        .expect("navigate");
    assert_eq!(mock.last_navigate_params().await.unwrap()["timeout"], 3000);
    for request in mock.navigate_requests().await {
        if request["params"]["url"] != "about:blank" {
            assert_eq!(request["params"]["timeout"], 3000, "{request}");
            assert!(request.get("navigation_timeout").is_none(), "{request}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn navigation_timeout_releases_the_committed_ack_before_load() {
    for driver in drivers() {
        let (mock, _browser, page) = setup_with_driver(Duration::from_secs(60), driver).await;
        mock.withhold_load();
        let fut = page
            .navigate_http_future_with_timeout(
                NavigateParams::new("https://example.test/pending-load"),
                Duration::from_secs(3),
            )
            .unwrap();
        assert!(!fut.committed());
        tokio::pin!(fut);
        let start = Instant::now();
        let req = timeout(Duration::from_secs(10), &mut fut)
            .await
            .expect("navigation deadline must release the held ack")
            .expect("committed navigation")
            .expect("committed request");
        let elapsed = start.elapsed();
        assert_eq!(
            req.response.as_ref().map(|response| response.status),
            Some(200)
        );
        assert!(req.failure_text.is_none());
        assert!(
            elapsed >= Duration::from_millis(2500) && elapsed < Duration::from_secs(6),
            "took {elapsed:?}"
        );
        assert!(fut.committed());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn navigation_timeout_caps_the_post_ack_wait() {
    for driver in drivers() {
        let (mock, _browser, page) = setup_with_driver(Duration::from_secs(60), driver).await;
        mock.fail_navigate("").await;
        let fut = page
            .navigate_http_future_with_timeout(
                NavigateParams::new("https://example.test/no-lifecycle"),
                Duration::from_secs(1),
            )
            .unwrap();
        tokio::pin!(fut);
        let start = Instant::now();
        let result = timeout(Duration::from_secs(5), &mut fut)
            .await
            .expect("post-ack wait must use the navigation cap");
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(CdpError::Timeout)), "{result:?}");
        assert!(
            elapsed >= Duration::from_millis(1800) && elapsed < Duration::from_secs(4),
            "took {elapsed:?}"
        );
        assert!(
            fut.committed(),
            "the released ack must remain available for salvage"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn navigation_timeout_is_capped_by_request_timeout() {
    let (mock, _browser, page) = setup_with_handler(Duration::from_secs(1), false).await;
    mock.fail_navigate("").await;
    let fut = page
        .navigate_http_future_with_timeout(
            NavigateParams::new("https://example.test/capped"),
            Duration::from_secs(60),
        )
        .unwrap();
    assert_eq!(fut.navigation_timeout(), Some(Duration::from_secs(1)));
    let result = timeout(Duration::from_secs(3), fut)
        .await
        .expect("request timeout must remain the outer bound");
    assert!(matches!(result, Err(CdpError::Timeout)), "{result:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_is_false_before_the_ack() {
    let (mock, _browser, page) = setup_with_handler(Duration::from_secs(60), false).await;
    mock.swallow_method("Page.navigate").await;
    let fut = page
        .navigate_http_future(NavigateParams::new("https://example.test/no-ack"))
        .unwrap();
    assert!(!fut.committed());
    tokio::pin!(fut);
    assert!(timeout(Duration::from_millis(200), &mut fut).await.is_err());
    assert!(!fut.committed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_is_true_after_a_clean_ack() {
    for driver in drivers() {
        let (_mock, _browser, page) = setup_with_driver(Duration::from_secs(60), driver).await;
        let fut = page
            .navigate_http_future(NavigateParams::new("https://example.test/clean"))
            .unwrap();
        tokio::pin!(fut);
        timeout(Duration::from_secs(5), &mut fut)
            .await
            .expect("clean ack should finish")
            .expect("navigate");
        assert!(fut.committed());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_is_false_after_a_terminal_error_ack() {
    // Check the public probe independently of the failure path's defensive reset.
    let ack = serde_json::from_value(serde_json::json!({
        "frameId": "frame-test", "loaderId": "loader-test",
        "errorText": "net::ERR_NAME_NOT_RESOLVED"
    }))
    .unwrap();
    assert!(!navigate_committed(&ack));

    for driver in drivers() {
        let (mock, _browser, page) = setup_with_driver(Duration::from_secs(60), driver).await;
        mock.fail_navigate("net::ERR_NAME_NOT_RESOLVED").await;
        let fut = page
            .navigate_http_future(NavigateParams::new("https://nxdomain.test/"))
            .unwrap();
        tokio::pin!(fut);
        let req = timeout(Duration::from_secs(5), &mut fut)
            .await
            .expect("terminal ack should finish")
            .expect("synthetic failure")
            .expect("request");
        assert_eq!(
            req.failure_text.as_deref(),
            Some("net::ERR_NAME_NOT_RESOLVED")
        );
        assert!(!fut.committed());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_holds_for_navigation_continues_carve_outs() {
    for driver in drivers() {
        let (mock, _browser, page) = setup_with_driver(Duration::from_secs(60), driver).await;
        for error in [
            "net::ERR_HTTP_RESPONSE_CODE_FAILURE (403)",
            "net::ERR_ABORTED",
        ] {
            mock.fail_navigate(error).await;
            mock.commit_failed_navigation().await;
            let fut = page
                .navigate_http_future(NavigateParams::new("https://example.test/committed-error"))
                .unwrap();
            tokio::pin!(fut);
            let req = timeout(Duration::from_secs(5), &mut fut)
                .await
                .expect("committed error body should finish")
                .expect("navigation")
                .expect("request");
            assert!(req.failure_text.is_none());
            assert!(fut.committed(), "{error}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn navigation_without_timeout_still_waits_for_request_timeout() {
    for driver in drivers() {
        let (mock, _browser, page) = setup_with_driver(Duration::from_secs(1), driver).await;
        mock.fail_navigate("").await;
        let fut = page
            .navigate_http_future(NavigateParams::new("https://example.test/no-cap"))
            .unwrap();
        assert_eq!(fut.navigation_timeout(), None);
        tokio::pin!(fut);
        let start = Instant::now();
        let result = timeout(Duration::from_secs(3), &mut fut)
            .await
            .expect("request timeout should fire");
        assert!(matches!(result, Err(CdpError::Timeout)), "{result:?}");
        assert!(start.elapsed() >= Duration::from_millis(900));
        assert!(!fut.committed());
    }
}

// Exercise the future's channel boundary without a socket as well, so its
// timeout and commit probes can be verified in network-restricted sandboxes.
mod channels {
    use super::*;
    use chromiumoxide::handler::commandfuture::CommandFuture;
    use chromiumoxide::handler::httpfuture::{navigate_error_text, HttpFuture};
    use chromiumoxide::handler::sender::PageSender;
    use chromiumoxide::handler::target::TargetMessage;
    use chromiumoxide_types::{CallId, Response};
    use tokio::sync::mpsc;

    fn setup() -> (HttpFuture<NavigateParams>, mpsc::Receiver<TargetMessage>) {
        let (tx, rx) = mpsc::channel(4);
        let sender = PageSender::new(tx, None);
        let params = NavigateParams::new("https://example.test/");
        let command =
            CommandFuture::new(params, sender.clone(), None, Duration::from_secs(60)).unwrap();
        let future = HttpFuture::with_failure_check(
            sender,
            command,
            Duration::from_secs(60),
            navigate_error_text,
            None,
        )
        .with_commit_check(navigate_committed);
        (future, rx)
    }

    fn ack(error: Option<&str>) -> Response {
        let mut result = serde_json::json!({"frameId": "frame-test", "loaderId": "loader-test"});
        if let Some(error) = error {
            result["errorText"] = serde_json::json!(error);
        }
        Response {
            id: CallId::new(1),
            result: Some(result),
            error: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn navigate_params_omit_timeout_when_unset() {
        assert_eq!(
            serde_json::to_string(&NavigateParams::new("https://example.test/")).unwrap(),
            r#"{"url":"https://example.test/"}"#,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn navigation_timeout_caps_the_post_ack_wait() {
        let (future, mut rx) = setup();
        let future = future.with_navigation_timeout(Duration::from_secs(1));
        tokio::pin!(future);
        let start = Instant::now();
        assert!(futures_util::poll!(&mut future).is_pending());
        let Some(TargetMessage::Command(command)) = rx.recv().await else {
            panic!("expected navigate command");
        };
        // Let the initial navigation timer expire before releasing the ack.
        // The post-ack wait must reset it using the caller's cap.
        tokio::time::sleep(Duration::from_secs(1)).await;
        command.sender.send(Ok(ack(None))).unwrap();
        let result = timeout(Duration::from_secs(5), &mut future)
            .await
            .expect("post-ack wait must use the navigation cap");
        assert!(matches!(result, Err(CdpError::Timeout)), "{result:?}");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1800) && elapsed < Duration::from_secs(4),
            "took {elapsed:?}"
        );
        assert!(future.committed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn navigation_timeout_reaches_the_handler() {
        let (future, mut rx) = setup();
        let future = future.with_navigation_timeout(Duration::from_secs(3));
        tokio::pin!(future);
        assert!(futures_util::poll!(&mut future).is_pending());
        let Some(TargetMessage::Command(command)) = rx.recv().await else {
            panic!("expected navigate command");
        };
        assert!(command.is_navigation());
        assert_eq!(command.navigation_timeout, Some(Duration::from_secs(3)));
        assert!(serde_json::to_value(&command)
            .unwrap()
            .get("navigation_timeout")
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn committed_is_false_after_a_terminal_error_ack() {
        let response = ack(Some("net::ERR_NAME_NOT_RESOLVED"));
        let returns = serde_json::from_value(response.result.clone().unwrap()).unwrap();
        assert!(!navigate_committed(&returns));
        let (future, mut rx) = setup();
        assert!(!future.committed());
        tokio::pin!(future);
        assert!(futures_util::poll!(&mut future).is_pending());
        let Some(TargetMessage::Command(command)) = rx.recv().await else {
            panic!("expected navigate command");
        };
        command.sender.send(Ok(response)).unwrap();
        let req = timeout(Duration::from_secs(1), &mut future)
            .await
            .expect("terminal ack should finish")
            .expect("synthetic failure")
            .expect("request");
        assert_eq!(
            req.failure_text.as_deref(),
            Some("net::ERR_NAME_NOT_RESOLVED")
        );
        assert!(!future.committed());
    }
}
