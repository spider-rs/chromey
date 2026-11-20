//! End-to-end tests for the parallel handler against a real Chrome.
//!
//! Skipped automatically when no Chrome / Chromium binary is on the box. Run:
//!   cargo test --features parallel-handler --test parallel_handler_e2e

#![cfg(feature = "parallel-handler")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chromey::browser::{Browser, BrowserConfig, HeadlessMode};
use chromey_cdp::cdp::js_protocol::runtime::EvaluateParams;
use futures_util::StreamExt;
use tokio::time::timeout;

fn try_chrome_config(test_name: &str) -> Option<BrowserConfig> {
    // Probe: is *any* Chrome detectable on this box? Returns None when
    // not — the tests then skip cleanly.
    let _ = BrowserConfig::builder().build().ok()?;

    let dir = std::env::temp_dir().join(format!(
        "chromey-{test_name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp profile dir");
    BrowserConfig::builder()
        .user_data_dir(&dir as &PathBuf)
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .headless_mode(HeadlessMode::True)
        .launch_timeout(Duration::from_secs(30))
        .build()
        .ok()
}

/// Sanity baseline: drive the same flow through the *serial* handler so a
/// failure of `e2e_parallel_handler_about_blank_round_trip` can be
/// distinguished from "Chrome misbehaves on this box."
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_serial_handler_about_blank_round_trip() {
    let Some(config) = try_chrome_config("e2e-serial-blank") else {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    };

    let (browser, mut handler) = Browser::launch(config).await.expect("launch chrome");
    let _h = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = timeout(Duration::from_secs(20), browser.new_page("about:blank"))
        .await
        .expect("new_page timeout (serial)")
        .expect("new_page (serial)");

    let url = page.url().await.expect("url");
    assert_eq!(url.as_deref(), Some("about:blank"));

    drop(browser);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_parallel_handler_about_blank_round_trip() {
    let Some(config) = try_chrome_config("e2e-about-blank") else {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    };

    let (browser, handler) = Browser::launch(config).await.expect("launch chrome");
    let _h = tokio::spawn(handler.run_parallel());

    let page = timeout(Duration::from_secs(20), browser.new_page("about:blank"))
        .await
        .expect("new_page timeout")
        .expect("new_page");

    let url = page.url().await.expect("url");
    assert_eq!(url.as_deref(), Some("about:blank"));

    drop(browser);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_parallel_handler_evaluates_js_on_4_pages_concurrently() {
    let Some(config) = try_chrome_config("e2e-eval-4") else {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    };

    let (browser, handler) = Browser::launch(config).await.expect("launch chrome");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    const PAGES: usize = 4;
    let mut create = Vec::with_capacity(PAGES);
    for _ in 0..PAGES {
        let b = browser.clone();
        create.push(tokio::spawn(async move {
            timeout(Duration::from_secs(20), b.new_page("about:blank"))
                .await
                .expect("new_page timeout")
                .expect("new_page")
        }));
    }
    let pages = futures_util::future::join_all(create)
        .await
        .into_iter()
        .map(|r| r.expect("join"))
        .collect::<Vec<_>>();

    // Each page evaluates `1+2` 16 times in parallel.
    let mut tasks = Vec::with_capacity(PAGES * 16);
    for page in &pages {
        for _ in 0..16 {
            let p = page.clone();
            tasks.push(tokio::spawn(async move {
                let resp = p
                    .execute(EvaluateParams::new("1+2"))
                    .await
                    .expect("evaluate");
                let val = resp
                    .result
                    .result
                    .value
                    .as_ref()
                    .and_then(|v| v.as_i64())
                    .expect("number value");
                assert_eq!(val, 3);
            }));
        }
    }
    let results = futures_util::future::join_all(tasks).await;
    for r in results {
        r.expect("join");
    }

    drop(browser);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_parallel_handler_navigates_to_data_url() {
    let Some(config) = try_chrome_config("e2e-nav") else {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    };

    let (browser, handler) = Browser::launch(config).await.expect("launch chrome");
    let _h = tokio::spawn(handler.run_parallel());

    let page = timeout(Duration::from_secs(20), browser.new_page("about:blank"))
        .await
        .expect("new_page timeout")
        .expect("new_page");

    page.goto("data:text/html,<title>parallel-e2e</title><body>hello</body>")
        .await
        .expect("goto");

    let title = page.get_title().await.expect("title");
    assert_eq!(title.as_deref(), Some("parallel-e2e"));

    drop(browser);
}
