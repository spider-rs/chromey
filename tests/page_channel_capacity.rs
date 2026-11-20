//! Integration tests for the tunable `page_channel_capacity` config.
//!
//! These tests spin up a real headless Chrome and exercise the
//! `BrowserConfigBuilder::page_channel_capacity(N)` knob threaded through
//! `HandlerConfig` → `TargetConfig` → `PageHandle::with_capacity`.
//!
//! They cover three properties that must hold regardless of the chosen
//! capacity — no regressions, no deadlocks, no panics:
//!
//! 1. **Small capacity still works.** Setting `page_channel_capacity(1)`
//!    forces every burst of concurrent commands into the `CommandFuture`
//!    async-send fallback path. If the fallback is broken, the many
//!    concurrent `goto` / `evaluate` calls would hang forever; the
//!    per-task 30s timeouts catch that.
//!
//! 2. **Default behaves identically to the legacy hard-coded 2048.** A
//!    smoke test confirms the un-overridden builder produces a working
//!    page — if the default ever regresses to 0 (panic) or some broken
//!    value, this will fail at `new_page`.
//!
//! 3. **Large capacity is accepted and stable.** Set `page_channel_capacity
//!    (65_536)` and verify basic page ops still work — catches any
//!    integer-overflow or allocation failure in the channel factory.
//!
//! Each test uses a fresh profile dir so they can run in parallel without
//! stomping each other's Chrome state. Skips with a note if no Chrome is
//! on the PATH.

use chromey::browser::{Browser, BrowserConfig, HeadlessMode};
use futures_util::StreamExt;
use std::path::PathBuf;
use tokio::time::{timeout, Duration};

const TARGET: &str = "about:blank";

fn try_browser_config() -> Option<BrowserConfig> {
    BrowserConfig::builder().build().ok()
}

fn temp_profile_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chromey-pagecap-{test_name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp profile dir");
    dir
}

/// Build a headless config with a custom `page_channel_capacity`.
/// When `capacity` is `None`, the builder default (2048) is used.
fn headless_config(test_name: &str, capacity: Option<usize>) -> BrowserConfig {
    let profile_dir = temp_profile_dir(test_name);
    let mut b = BrowserConfig::builder()
        .user_data_dir(&profile_dir)
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .headless_mode(HeadlessMode::True)
        .launch_timeout(Duration::from_secs(30));
    if let Some(n) = capacity {
        b = b.page_channel_capacity(n);
    }
    b.build().expect("headless browser config")
}

async fn launch(config: BrowserConfig) -> Browser {
    let (browser, mut handler) = Browser::launch(config).await.expect("launch browser");
    tokio::spawn(async move { while let Some(_event) = handler.next().await {} });
    browser
}

// ---------------------------------------------------------------------------
// 1. Tiny capacity: forces CommandFuture async-send fallback every time.
// ---------------------------------------------------------------------------
//
// With capacity = 1, only a single `TargetMessage` can sit in the page
// channel at a time. Any concurrent command from the page lands on
// `TrySendError::Full` and has to take the slow path. If that path ever
// deadlocks (e.g. missing waker), these timeouts catch it.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiny_capacity_does_not_deadlock_under_concurrent_commands() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("tiny-cap", Some(1))).await;

    let page = timeout(Duration::from_secs(30), browser.new_page(TARGET))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    // Fire many concurrent `evaluate` commands — each one sends at least
    // one `TargetMessage::Command` through the capacity-1 channel. With
    // 32 concurrent tasks, the async-send fallback runs on ~31 of them.
    let futs: Vec<_> = (0..32)
        .map(|i| {
            let p = page.clone();
            tokio::spawn(async move {
                let v = timeout(Duration::from_secs(30), p.evaluate(format!("{i}+1")))
                    .await
                    .unwrap_or_else(|_| panic!("evaluate({i}) timed out — capacity-1 deadlock?"))
                    .unwrap_or_else(|err| panic!("evaluate({i}) failed: {err}"));
                let n: i64 = v
                    .into_value()
                    .unwrap_or_else(|err| panic!("evaluate({i}) result not an integer: {err}"));
                assert_eq!(n, (i + 1) as i64);
            })
        })
        .collect();

    for (i, fut) in futs.into_iter().enumerate() {
        fut.await
            .unwrap_or_else(|err| panic!("task {i} panicked: {err}"));
    }
}

// ---------------------------------------------------------------------------
// 2. Default capacity: builder with no override must still work.
// ---------------------------------------------------------------------------
//
// Regression guard. If `BrowserConfigBuilder::default` ever produced a
// broken `page_channel_capacity` (0 used to panic in tokio before our
// clamp), this catches it at `new_page` before any command runs.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_capacity_still_works() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("default-cap", None)).await;
    let page = timeout(Duration::from_secs(30), browser.new_page(TARGET))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    let v = timeout(Duration::from_secs(30), page.evaluate("1+2"))
        .await
        .expect("evaluate should not time out")
        .expect("evaluate should succeed");
    let n: i64 = v.into_value().expect("evaluate result should be int");
    assert_eq!(n, 3);
}

// ---------------------------------------------------------------------------
// 3. Large capacity: numbers far above the old 2048 default are accepted.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_capacity_works() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("large-cap", Some(65_536))).await;
    let page = timeout(Duration::from_secs(30), browser.new_page(TARGET))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    // Basic sanity — page ops work with the oversized channel.
    let v = timeout(Duration::from_secs(30), page.evaluate("1+41"))
        .await
        .expect("evaluate should not time out")
        .expect("evaluate should succeed");
    let n: i64 = v.into_value().expect("evaluate result should be int");
    assert_eq!(n, 42);
}
