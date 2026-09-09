//! Wire-level checks for the navigation deadline on the cache-intercept path.
//!
//! The sibling `navigate_deadline` suite pins the plain navigate call site.
//! This one pins the other one spider calls: the cache path, where a miss
//! falls through to `Page.navigate` and a hit sets the document content
//! instead. Every assertion reads the raw params the mock server saw.
//!
//! One navigate produces two `Page.navigate` requests on the wire: the raw
//! submit, plus the frame manager's copy with `frameId` filled in. That is
//! pre-existing, so these tests assert over every captured request rather
//! than pinning a count.

#![cfg(feature = "_cache")]

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::collections::HashMap;
use std::time::Duration;

use chromiumoxide::cache::manager::{create_cache_key_raw, put_hybrid_cache};
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::http::{HttpResponse, HttpVersion};
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

/// Seed the local hybrid cache so `get_cached_url` reports a hit for `url`.
async fn seed_local_cache(url: &str) {
    let cache_key = create_cache_key_raw(url, None, None);
    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), "text/html".to_string());
    headers.insert("cache-control".to_string(), "max-age=600".to_string());

    let body = b"<html><head><title>cached</title></head><body><h1>cached body</h1></body></html>"
        .to_vec();

    put_hybrid_cache(
        &cache_key,
        &cache_key,
        HttpResponse {
            body,
            headers,
            status: 200,
            url: url::Url::parse(url).expect("seed url"),
            version: HttpVersion::Http11,
        },
        "GET",
        HashMap::new(),
        None,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_miss_armed_sends_integer_millisecond_timeout() {
    let (mut mock, _browser, page) = setup().await;
    let url = "https://cache-miss-armed.test/";

    let _ = timeout(
        Duration::from_secs(5),
        page.goto_with_cache_http_future_with_timeout(
            NavigateParams::new(url),
            None,
            Some(Duration::from_millis(1500)),
        ),
    )
    .await
    .expect("cache navigate");

    let captured = mock.drain_navigate_params();
    assert!(
        !captured.is_empty(),
        "a cache miss must fall through to Page.navigate"
    );
    for params in &captured {
        assert_eq!(params.get("url").and_then(|v| v.as_str()), Some(url));
        assert_eq!(
            params.get("timeout"),
            Some(&serde_json::json!(1500)),
            "the armed cache miss must carry the deadline in milliseconds, got {params}"
        );
        assert!(
            params["timeout"].is_i64() || params["timeout"].is_u64(),
            "the deadline must serialize as an integer, got {}",
            params["timeout"]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_miss_unarmed_sends_no_timeout_key() {
    let (mut mock, _browser, page) = setup().await;
    let url = "https://cache-miss-plain.test/";

    let _ = timeout(
        Duration::from_secs(5),
        page.goto_with_cache_http_future_with_timeout(NavigateParams::new(url), None, None),
    )
    .await
    .expect("cache navigate");

    let captured = mock.drain_navigate_params();
    assert!(
        !captured.is_empty(),
        "a cache miss must fall through to Page.navigate"
    );
    for params in &captured {
        assert_eq!(params.get("url").and_then(|v| v.as_str()), Some(url));
        assert!(
            params.get("timeout").is_none(),
            "an un-armed cache miss must not send a timeout key, got {params}"
        );
    }
}

/// The plain entry point must stay byte-identical to the un-armed variant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_plain_cache_entry_point_still_sends_no_timeout_key() {
    let (mut mock, _browser, page) = setup().await;
    let url = "https://cache-miss-legacy.test/";

    let _ = timeout(
        Duration::from_secs(5),
        page.goto_with_cache_http_future(NavigateParams::new(url), None),
    )
    .await
    .expect("cache navigate");

    let captured = mock.drain_navigate_params();
    assert!(
        !captured.is_empty(),
        "a cache miss must fall through to Page.navigate"
    );
    for params in &captured {
        assert!(
            params.get("timeout").is_none(),
            "the plain cache entry point must not send a timeout key, got {params}"
        );
    }
}

/// A hit sets the document content and never navigates, so the deadline has
/// nothing to apply to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cache_hit_never_navigates() {
    let url = "https://cache-hit-armed.test/";
    seed_local_cache(url).await;

    let (mut mock, _browser, page) = setup().await;
    mock.commit_set_document_content();

    let result = timeout(
        Duration::from_secs(5),
        page.goto_with_cache_http_future_with_timeout(
            NavigateParams::new(url),
            None,
            Some(Duration::from_millis(1500)),
        ),
    )
    .await
    .expect("cache navigate");

    assert!(
        result.is_ok(),
        "the cache hit must resolve off set document content, got {result:?}"
    );
    assert!(
        mock.drain_navigate_params().is_empty(),
        "a cache hit must not send Page.navigate at all"
    );
}

/// The public intercept-enabled entry point carries the deadline all the way
/// down to the wire. `remote` points at a closed local port so the cache
/// listener and seeder resolve without leaving the machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_intercept_entry_point_plumbs_the_deadline_to_the_wire() {
    let (mut mock, _browser, page) = setup().await;
    let url = "https://cache-intercept-armed.test/";
    let dead_remote = closed_local_endpoint().await;

    let _ = timeout(
        Duration::from_secs(20),
        page.http_future_with_cache_intercept_enabled_with_timeout(
            NavigateParams::new(url),
            None,
            None,
            None,
            Some(&dead_remote),
            None,
            Some(Duration::from_millis(1500)),
        ),
    )
    .await
    .expect("cache navigate");

    let captured = mock.drain_navigate_params();
    assert!(
        !captured.is_empty(),
        "a cache miss must fall through to Page.navigate"
    );
    for params in &captured {
        assert_eq!(params.get("url").and_then(|v| v.as_str()), Some(url));
        assert_eq!(
            params.get("timeout"),
            Some(&serde_json::json!(1500)),
            "the intercept entry point dropped the deadline, got {params}"
        );
    }
}

/// The same entry point without a deadline sends what it always sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_intercept_entry_point_unarmed_sends_no_timeout_key() {
    let (mut mock, _browser, page) = setup().await;
    let url = "https://cache-intercept-plain.test/";
    let dead_remote = closed_local_endpoint().await;

    let _ = timeout(
        Duration::from_secs(20),
        page.http_future_with_cache_intercept_enabled(
            NavigateParams::new(url),
            None,
            None,
            None,
            Some(&dead_remote),
            None,
        ),
    )
    .await
    .expect("cache navigate");

    let captured = mock.drain_navigate_params();
    assert!(
        !captured.is_empty(),
        "a cache miss must fall through to Page.navigate"
    );
    for params in &captured {
        assert!(
            params.get("timeout").is_none(),
            "the un-armed intercept entry point must not send a timeout key, got {params}"
        );
    }
}

/// Bind a port, read it, drop the listener. Anything sent there is refused
/// locally instead of reaching the default remote cache endpoint.
async fn closed_local_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    format!("http://{addr}")
}
