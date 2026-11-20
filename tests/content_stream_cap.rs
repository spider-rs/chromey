//! DoS guardrail test for `content_bytes_streaming`.
//!
//! Lives in its own integration-test binary so the process-wide env var
//! `CHROMEY_CONTENT_STREAM_MAX_BYTES` can't race with other streaming tests
//! running in parallel.  Each `tests/*.rs` file compiles to a separate test
//! executable, so this test runs in isolation.
//!
//! Run with:
//!   cargo test --test content_stream_cap

use chromey::browser::{Browser, BrowserConfig, HeadlessMode};
use futures_util::StreamExt;
use std::path::PathBuf;
use tokio::time::{timeout, Duration};

fn try_browser_config() -> Option<BrowserConfig> {
    BrowserConfig::builder().build().ok()
}

fn temp_profile_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chromey-{test_name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp profile dir");
    dir
}

fn headless_config(test_name: &str) -> BrowserConfig {
    let profile_dir = temp_profile_dir(test_name);
    BrowserConfig::builder()
        .user_data_dir(&profile_dir)
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .headless_mode(HeadlessMode::True)
        .launch_timeout(Duration::from_secs(30))
        .build()
        .expect("headless browser config")
}

async fn launch_with_handler(config: BrowserConfig) -> Browser {
    let (browser, mut handler) = Browser::launch(config).await.expect("launch browser");
    let _handle = tokio::spawn(async move { while let Some(_event) = handler.next().await {} });
    browser
}

/// When `CHROMEY_CONTENT_STREAM_MAX_BYTES` is set below the actual document
/// size, `content_bytes_streaming` must return an error instead of
/// allocating the full document.  The pump API (`content_bytes_stream`)
/// does not enforce the cap and must still complete on the same page.
#[tokio::test]
async fn accumulated_bytes_cap_errors_then_pump_succeeds() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    };

    // Set the cap for this test binary's entire process.  No other tests
    // run in this binary, so there's nothing to race with.
    //
    // SAFETY: `set_var` is unsafe on Rust 2024+; this is a single-threaded
    // setup before any other thread reads env.
    unsafe {
        std::env::set_var("CHROMEY_CONTENT_STREAM_MAX_BYTES", "1048576");
    }

    let browser = launch_with_handler(headless_config("content-stream-cap")).await;
    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page timeout")
        .expect("new_page failed");

    // >2 MiB document so the accumulating streamer will blow past 1 MiB.
    let mut body = String::with_capacity(3 * 1024 * 1024);
    body.push_str("<html><body>");
    for i in 0..40_000_u32 {
        body.push_str("<p>line-");
        body.push_str(&format!("{i:08}"));
        body.push_str(" lorem ipsum dolor sit amet consectetur adipis</p>");
    }
    body.push_str("</body></html>");

    timeout(Duration::from_secs(30), page.set_content(&body))
        .await
        .expect("set_content timeout")
        .expect("set_content failed");

    let err = timeout(Duration::from_secs(60), page.content_bytes_streaming())
        .await
        .expect("content_bytes_streaming should not hang")
        .expect_err("should error at the accumulated-bytes cap");
    let msg = format!("{err}");
    assert!(
        msg.contains("accumulated bytes exceeded cap"),
        "unexpected error message: {msg}"
    );

    // Pump API is not capped — must still complete.
    let mut stream = Box::pin(page.content_bytes_stream(None));
    let mut total: usize = 0;
    while let Some(item) = timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("pump next timeout")
    {
        total += item.expect("chunk error").len();
    }
    assert!(
        total > 2 * 1024 * 1024,
        "pump should complete despite cap (total={total})"
    );
}
