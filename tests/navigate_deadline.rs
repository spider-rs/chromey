//! Wire-level checks for the navigation deadline binding.
//!
//! Every assertion reads the raw `Page.navigate` params the mock server saw, so
//! it fails if the client stops serializing what it is supposed to.
//!
//! One `navigate_http_future` call produces two `Page.navigate` requests on the
//! wire: the raw submit, plus the frame manager's copy with `frameId` filled
//! in. That is pre-existing behaviour, so these tests assert over every
//! captured request rather than pinning a count.

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::time::Duration;

use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::{Browser, Page};
use chromiumoxide_cdp::cdp::browser_protocol::page::NavigateParams;
use tokio::time::timeout;

async fn setup() -> (cdp_mock::CdpMock, Browser, Page) {
    let mut mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(3),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect to mock");
    tokio::spawn(handler.run());
    let page = browser.new_page("about:blank").await.expect("new_page");
    mock.clear_navigate_params();
    (mock, browser, page)
}

fn keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("params object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unarmed_navigate_sends_no_timeout_key() {
    let (mut mock, _browser, page) = setup().await;
    let fut = page
        .navigate_http_future(NavigateParams::new("https://plain.test/"))
        .expect("navigate future");
    let _ = timeout(Duration::from_secs(3), fut)
        .await
        .expect("navigate");

    let captured = mock.drain_navigate_params();
    assert!(
        !captured.is_empty(),
        "mock recorded no Page.navigate params"
    );
    for params in &captured {
        assert_eq!(
            params.get("url").and_then(|v| v.as_str()),
            Some("https://plain.test/")
        );
        assert!(
            params.get("timeout").is_none(),
            "the un-armed path must not send a timeout key, got {params}"
        );
        // `url`, plus `frameId` on the frame manager's copy — nothing else.
        let keys = keys(params);
        assert!(
            keys == vec!["url".to_string()]
                || keys == vec!["frameId".to_string(), "url".to_string()],
            "unexpected keys on the un-armed path: {keys:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_navigate_sends_integer_millisecond_timeout() {
    let (mut mock, _browser, page) = setup().await;
    let fut = page
        .navigate_http_future_with_timeout(
            NavigateParams::new("https://armed.test/"),
            Duration::from_secs(3),
        )
        .expect("navigate future");
    let _ = timeout(Duration::from_secs(3), fut)
        .await
        .expect("navigate");

    let captured = mock.drain_navigate_params();
    assert!(
        !captured.is_empty(),
        "mock recorded no Page.navigate params"
    );
    for params in &captured {
        assert_eq!(
            params.get("url").and_then(|v| v.as_str()),
            Some("https://armed.test/")
        );
        assert_eq!(
            params.get("timeout"),
            Some(&serde_json::json!(3000)),
            "armed navigate must carry the deadline in milliseconds, got {params}"
        );
        assert!(
            params["timeout"].is_i64() || params["timeout"].is_u64(),
            "the deadline must serialize as an integer, got {}",
            params["timeout"]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_navigate_uses_the_page_navigate_method_string() {
    let mut mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(3),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect to mock");
    tokio::spawn(handler.run());
    let page = browser.new_page("about:blank").await.expect("new_page");
    mock.clear_navigate_params();

    // The mock only records params under the literal `Page.navigate` method, and
    // only that method emits the navigation burst the future waits on. A
    // different method string would return `{}` and the navigate would hang.
    let fut = page
        .navigate_http_future_with_timeout(
            NavigateParams::new("https://method.test/"),
            Duration::from_millis(250),
        )
        .expect("navigate future");
    timeout(Duration::from_secs(3), fut)
        .await
        .expect("armed navigate must be dispatched as Page.navigate")
        .expect("navigate result");

    assert!(
        !mock.drain_navigate_params().is_empty(),
        "armed navigate did not arrive under the Page.navigate method string"
    );
    drop(browser);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_navigate_preserves_every_standard_field() {
    use chromiumoxide_cdp::cdp::browser_protocol::page::{FrameId, ReferrerPolicy, TransitionType};

    let base = NavigateParams {
        url: "https://fields.test/page".into(),
        referrer: Some("https://referrer.test/".into()),
        transition_type: Some(TransitionType::Link),
        frame_id: Some(FrameId::from("frame-fixed".to_string())),
        referrer_policy: Some(ReferrerPolicy::NoReferrer),
    };

    let (mut plain_mock, plain_browser, plain_page) = setup().await;
    let fut = plain_page
        .navigate_http_future(base.clone())
        .expect("navigate future");
    let _ = timeout(Duration::from_secs(3), fut).await;
    let plain = plain_mock.drain_navigate_params();
    drop(plain_browser);

    let (mut armed_mock, armed_browser, armed_page) = setup().await;
    let fut = armed_page
        .navigate_http_future_with_timeout(base, Duration::from_millis(1500))
        .expect("navigate future");
    let _ = timeout(Duration::from_secs(3), fut).await;
    let armed = armed_mock.drain_navigate_params();
    drop(armed_browser);

    let plain = plain.first().expect("plain navigate params");
    let armed = armed.first().expect("armed navigate params");

    for (key, value) in plain.as_object().expect("plain object") {
        assert_eq!(
            armed.get(key),
            Some(value),
            "key `{key}` diverged between the plain and armed paths"
        );
    }
    let mut extra = keys(armed);
    extra.retain(|k| plain.get(k).is_none());
    assert_eq!(extra, vec!["timeout".to_string()]);
    assert_eq!(armed["timeout"], serde_json::json!(1500));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_deadline_saturates_on_the_wire() {
    let (mut mock, _browser, page) = setup().await;
    let fut = page
        .navigate_http_future_with_timeout(
            NavigateParams::new("https://saturate.test/"),
            Duration::from_secs(u64::MAX),
        )
        .expect("navigate future");
    let _ = timeout(Duration::from_secs(3), fut)
        .await
        .expect("navigate");

    let captured = mock.drain_navigate_params();
    assert!(
        !captured.is_empty(),
        "mock recorded no Page.navigate params"
    );
    for params in &captured {
        assert_eq!(
            params.get("timeout"),
            Some(&serde_json::json!(i64::MAX)),
            "an oversized deadline must saturate at i64::MAX, got {params}"
        );
        assert!(
            params["timeout"].as_i64().is_some_and(|ms| ms > 0),
            "the deadline must never go out negative, got {}",
            params["timeout"]
        );
    }
}
