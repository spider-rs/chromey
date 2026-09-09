//! Wire-level checks for headers on the CDP WebSocket upgrade request.
//!
//! The mock records every upgrade request it receives, so each test reads
//! what actually went over the socket rather than what the config holds.

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::time::Duration;

use chromiumoxide::cdp::CdpEventMessage;
use chromiumoxide::conn::{ConnectHeaderError, ConnectHeaders, Connection};
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::{Browser, BrowserConfig};
use tokio_tungstenite::tungstenite::http::HeaderMap;

const NAME: &str = "x-spider-request-id";
const VALUE: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

/// The header names tungstenite writes on its own for a bare URL. Anything
/// outside this set on an unconfigured connect is a behaviour change.
const HANDSHAKE_HEADERS: [&str; 5] = [
    "host",
    "connection",
    "upgrade",
    "sec-websocket-version",
    "sec-websocket-key",
];

fn tagged() -> ConnectHeaders {
    ConnectHeaders::new()
        .with(NAME, VALUE)
        .expect("valid header")
}

fn config_with(headers: ConnectHeaders) -> HandlerConfig {
    HandlerConfig {
        request_timeout: Duration::from_secs(3),
        connect_headers: headers,
        ..Default::default()
    }
}

fn values<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a str> {
    headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().expect("ascii"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_rides_on_the_upgrade_exactly_once() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let (_browser, _handler) = Browser::connect_with_config(mock.ws_url(), config_with(tagged()))
        .await
        .expect("connect to mock");

    let seen = mock.upgrade_requests();
    assert_eq!(seen.len(), 1, "one upgrade request, got {seen:?}");
    assert!(seen[0].completed);
    assert_eq!(values(&seen[0].headers, NAME), vec![VALUE]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unset_leaves_the_upgrade_untouched() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let (_browser, _handler) =
        Browser::connect_with_config(mock.ws_url(), config_with(ConnectHeaders::new()))
            .await
            .expect("connect to mock");

    let seen = mock.upgrade_requests();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].headers.get(NAME).is_none());
    let mut names: Vec<&str> = seen[0].headers.keys().map(|k| k.as_str()).collect();
    names.sort_unstable();
    let mut expected = HANDSHAKE_HEADERS.to_vec();
    expected.sort_unstable();
    assert_eq!(names, expected, "bare connect grew extra upgrade headers");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_resends_the_header() {
    let mock = cdp_mock::CdpMock::spawn().await;
    mock.drop_next_handshakes(1);

    let mut cfg = config_with(tagged());
    cfg.connection_retries = 2;
    let (_browser, _handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("second attempt connects");

    let seen = mock.upgrade_requests();
    assert_eq!(
        seen.len(),
        2,
        "one dropped attempt plus one success, got {seen:?}"
    );
    assert!(!seen[0].completed, "first attempt should have been dropped");
    assert!(seen[1].completed);
    for attempt in &seen {
        assert_eq!(
            values(&attempt.headers, NAME),
            vec![VALUE],
            "every attempt carries the header once"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_connection_with_headers() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let _conn = Connection::<CdpEventMessage>::connect_with_headers(mock.ws_url(), &tagged())
        .await
        .expect("raw connect");

    let seen = mock.upgrade_requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(values(&seen[0].headers, NAME), vec![VALUE]);
}

/// A hostname URL takes the DNS-cache branch of `connect_default`, which
/// hands tungstenite a pre-built request rather than the URL string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_rides_the_hostname_branch_too() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let url = format!(
        "ws://localhost:{}/devtools/browser/mock",
        mock.addr().port()
    );
    let (_browser, _handler) = Browser::connect_with_config(url, config_with(tagged()))
        .await
        .expect("connect via hostname");

    let seen = mock.upgrade_requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(values(&seen[0].headers, NAME), vec![VALUE]);
}

#[test]
fn builder_rejects_bad_headers_without_panicking() {
    let builder = || BrowserConfig::builder().chrome_executable("/nonexistent/chrome");

    assert!(matches!(
        builder().connect_header(NAME, "caf\u{e9}").err(),
        Some(ConnectHeaderError::NonAscii(_))
    ));
    assert!(matches!(
        builder().connect_header(NAME, "a\r\nb").err(),
        Some(ConnectHeaderError::NonAscii(_))
    ));
    assert!(matches!(
        builder().connect_header("not a name", VALUE).err(),
        Some(ConnectHeaderError::InvalidName(_))
    ));
    assert!(matches!(
        builder().connect_header("host", "evil").err(),
        Some(ConnectHeaderError::Reserved(_))
    ));
    assert!(matches!(
        builder().connect_header("sec-websocket-key", "x").err(),
        Some(ConnectHeaderError::Reserved(_))
    ));
    let big = "x".repeat(ConnectHeaders::MAX_TOTAL_BYTES);
    assert!(matches!(
        builder().connect_header(NAME, big.as_str()).err(),
        Some(ConnectHeaderError::TooLarge { .. })
    ));
}

#[test]
fn builder_carries_headers_into_the_config() {
    let mut map = HeaderMap::new();
    map.insert("x-first", "1".parse().unwrap());
    let cfg = BrowserConfig::builder()
        .chrome_executable("/nonexistent/chrome")
        .connect_headers(map)
        .expect("valid map")
        .connect_header(NAME, "old")
        .expect("valid")
        .connect_header(NAME, VALUE)
        .expect("replace keeps one entry")
        .build()
        .expect("build with explicit executable");

    let got: Vec<(&str, &str)> = cfg
        .connect_headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.to_str().unwrap()))
        .collect();
    assert_eq!(got, vec![("x-first", "1"), (NAME, VALUE)]);
}
