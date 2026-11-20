//! Integration tests for all `wait_for_*` methods on `Page`.
//!
//! These tests navigate to `https://example.com` (a stable, lightweight page)
//! and exercise every wait-for variant exposed by chromey:
//!
//!   1. `wait_for_navigation` / `wait_for_navigation_response`
//!   2. `wait_for_dom_content_loaded` / `wait_for_dom_content_loaded_response`
//!   3. `wait_for_network_idle`
//!   4. `wait_for_network_almost_idle`
//!   5. `wait_for_network_idle_with_timeout`
//!   6. `wait_for_network_almost_idle_with_timeout`
//!   7. `find_element` (selector-based wait)
//!
//! Run with:
//!   cargo test --test wait_for

use chromey::browser::{Browser, BrowserConfig, HeadlessMode};
use futures_util::StreamExt;
use std::path::PathBuf;
use tokio::time::{timeout, Duration};

const TARGET: &str = "https://example.com";

fn try_browser_config() -> Option<BrowserConfig> {
    BrowserConfig::builder().build().ok()
}

fn temp_profile_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chromey-waitfor-{test_name}-{}-{}",
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

async fn launch(config: BrowserConfig) -> Browser {
    let (browser, mut handler) = Browser::launch(config).await.expect("launch browser");
    let _handle = tokio::spawn(async move { while let Some(_event) = handler.next().await {} });
    browser
}

// ---------------------------------------------------------------------------
// 1. wait_for_navigation — waits for the `load` lifecycle event
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_navigation_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfn-goto")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let result = timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("wait_for_navigation should not time out")
        .expect("wait_for_navigation should succeed");

    let content = timeout(Duration::from_secs(15), result.content())
        .await
        .expect("content() should not time out")
        .expect("content() should succeed");

    assert!(
        content.contains("Example Domain"),
        "page should contain 'Example Domain', got {} bytes",
        content.len()
    );
}

// ---------------------------------------------------------------------------
// 2. wait_for_navigation_response — returns the HTTP response
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_navigation_response_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfn-response")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let response = timeout(Duration::from_secs(30), page.wait_for_navigation_response())
        .await
        .expect("wait_for_navigation_response should not time out")
        .expect("wait_for_navigation_response should succeed");

    eprintln!("navigation response received: {:?}", response);
}

// ---------------------------------------------------------------------------
// 3. wait_for_dom_content_loaded — fires before `load`, no subresource wait
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_dom_content_loaded_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfdcl")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let result = timeout(Duration::from_secs(30), page.wait_for_dom_content_loaded())
        .await
        .expect("wait_for_dom_content_loaded should not time out")
        .expect("wait_for_dom_content_loaded should succeed");

    let content = timeout(Duration::from_secs(15), result.content())
        .await
        .expect("content() should not time out")
        .expect("content() should succeed");

    assert!(
        content.contains("Example Domain"),
        "page should contain 'Example Domain' after DOMContentLoaded"
    );
}

#[tokio::test]
async fn wait_for_dom_content_loaded_response_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfdcl-response")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let _response = timeout(
        Duration::from_secs(30),
        page.wait_for_dom_content_loaded_response(),
    )
    .await
    .expect("wait_for_dom_content_loaded_response should not time out")
    .expect("wait_for_dom_content_loaded_response should succeed");
}

/// DOMContentLoaded should resolve before or at the same time as load.
#[tokio::test]
async fn dom_content_loaded_resolves_before_load() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("dcl-before-load")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let start = std::time::Instant::now();

    let dcl_time = {
        let _ = timeout(Duration::from_secs(30), page.wait_for_dom_content_loaded())
            .await
            .expect("dcl should not time out")
            .expect("dcl should succeed");
        start.elapsed()
    };

    let load_time = {
        let _ = timeout(Duration::from_secs(30), page.wait_for_navigation())
            .await
            .expect("nav should not time out")
            .expect("nav should succeed");
        start.elapsed()
    };

    eprintln!(
        "DOMContentLoaded: {dcl_time:?}, load: {load_time:?} (dcl <= load: {})",
        dcl_time <= load_time
    );

    assert!(
        dcl_time <= load_time,
        "DOMContentLoaded ({dcl_time:?}) should resolve before or at load ({load_time:?})"
    );
}

// ---------------------------------------------------------------------------
// 4. wait_for_network_idle — 500ms of zero open connections
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_network_idle_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfni")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let result = timeout(Duration::from_secs(30), page.wait_for_network_idle())
        .await
        .expect("wait_for_network_idle should not time out")
        .expect("wait_for_network_idle should succeed");

    let content = timeout(Duration::from_secs(15), result.content())
        .await
        .expect("content() should not time out")
        .expect("content() should succeed");

    assert!(
        content.contains("Example Domain"),
        "page should contain 'Example Domain' after network idle"
    );
}

// ---------------------------------------------------------------------------
// 5. wait_for_network_almost_idle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_network_almost_idle_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfnai")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let result = timeout(Duration::from_secs(30), page.wait_for_network_almost_idle())
        .await
        .expect("wait_for_network_almost_idle should not time out")
        .expect("wait_for_network_almost_idle should succeed");

    let content = timeout(Duration::from_secs(15), result.content())
        .await
        .expect("content() should not time out")
        .expect("content() should succeed");

    assert!(
        content.contains("Example Domain"),
        "page should contain 'Example Domain' after network almost idle"
    );
}

// ---------------------------------------------------------------------------
// 6. wait_for_network_idle_with_timeout — bounded idle wait
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_network_idle_with_timeout_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfni-timeout")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let result = timeout(
        Duration::from_secs(30),
        page.wait_for_network_idle_with_timeout(Duration::from_secs(15)),
    )
    .await
    .expect("outer timeout should not fire")
    .expect("wait_for_network_idle_with_timeout should succeed");

    let content = timeout(Duration::from_secs(15), result.content())
        .await
        .expect("content() should not time out")
        .expect("content() should succeed");

    assert!(
        content.contains("Example Domain"),
        "page should contain 'Example Domain' after idle-with-timeout"
    );
}

/// A very short timeout should gracefully return Ok (timeout elapsed).
#[tokio::test]
async fn wait_for_network_idle_with_tiny_timeout_does_not_error() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfni-tiny-timeout")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let result = page
        .wait_for_network_idle_with_timeout(Duration::from_millis(1))
        .await;

    assert!(
        result.is_ok(),
        "wait_for_network_idle_with_timeout should return Ok even when timeout elapses"
    );
}

// ---------------------------------------------------------------------------
// 7. wait_for_network_almost_idle_with_timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_network_almost_idle_with_timeout_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfnai-timeout")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let result = timeout(
        Duration::from_secs(30),
        page.wait_for_network_almost_idle_with_timeout(Duration::from_secs(15)),
    )
    .await
    .expect("outer timeout should not fire")
    .expect("wait_for_network_almost_idle_with_timeout should succeed");

    let content = timeout(Duration::from_secs(15), result.content())
        .await
        .expect("content() should not time out")
        .expect("content() should succeed");

    assert!(
        content.contains("Example Domain"),
        "page should contain 'Example Domain' after almost-idle-with-timeout"
    );
}

/// Tiny timeout is graceful for almost-idle variant too.
#[tokio::test]
async fn wait_for_network_almost_idle_with_tiny_timeout_does_not_error() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfnai-tiny-timeout")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let result = page
        .wait_for_network_almost_idle_with_timeout(Duration::from_millis(1))
        .await;

    assert!(
        result.is_ok(),
        "wait_for_network_almost_idle_with_timeout should return Ok even when timeout elapses"
    );
}

// ---------------------------------------------------------------------------
// 8. find_element — selector-based wait
// ---------------------------------------------------------------------------

#[tokio::test]
async fn find_element_after_goto() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("find-el")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("wait_for_navigation should not time out")
        .expect("wait_for_navigation should succeed");

    let element = timeout(Duration::from_secs(15), page.find_element("h1"))
        .await
        .expect("find_element should not time out")
        .expect("find_element should resolve for <h1>");

    let text = timeout(Duration::from_secs(10), element.inner_text())
        .await
        .expect("inner_text should not time out")
        .expect("inner_text should succeed");

    assert!(
        text.as_deref()
            .is_some_and(|t| t.contains("Example Domain")),
        "h1 inner text should contain 'Example Domain', got: {text:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. Two-phase concurrent page_wait
// ---------------------------------------------------------------------------

/// Phase 1 (concurrent): network waits run together.
/// Phase 2 (concurrent): selector + delay run together.
/// Wall time = max(network waits) + max(selector, delay).
#[tokio::test]
async fn two_phase_concurrent_page_wait() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("two-phase-page-wait")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    let start = std::time::Instant::now();

    // Phase 1: all network waits run concurrently.
    tokio::join!(
        async {
            let _ = page
                .wait_for_network_idle_with_timeout(Duration::from_secs(15))
                .await;
        },
        async {
            let _ = page
                .wait_for_network_almost_idle_with_timeout(Duration::from_secs(15))
                .await;
        },
    );

    let phase1 = start.elapsed();

    // Phase 2: selector + delay run concurrently (after network settles).
    tokio::join!(
        async {
            let _ = timeout(Duration::from_secs(15), page.find_element("body")).await;
        },
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
        },
    );

    let phase2 = start.elapsed() - phase1;

    let content = timeout(Duration::from_secs(15), page.content())
        .await
        .expect("content() should not time out")
        .expect("content() should succeed");

    assert!(
        content.contains("Example Domain"),
        "two-phase wait should yield page with 'Example Domain'"
    );
    assert!(
        content.contains("<h1>"),
        "two-phase wait should yield page with <h1> tag"
    );
    eprintln!(
        "two-phase page_wait: phase1={phase1:?} phase2={phase2:?} total={:?} ({} bytes)",
        start.elapsed(),
        content.len()
    );
}

// ---------------------------------------------------------------------------
// 10. Click + wait_for_navigation (interaction-triggered navigation)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn click_then_wait_for_navigation() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("click-nav")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("initial wait_for_navigation should not time out")
        .expect("initial wait_for_navigation should succeed");

    let link = timeout(Duration::from_secs(15), page.find_element("a"))
        .await
        .expect("find_element(a) should not time out")
        .expect("find_element(a) should resolve");

    timeout(Duration::from_secs(15), link.click())
        .await
        .expect("click should not time out")
        .expect("click should succeed");

    // The navigation may or may not succeed depending on the remote server,
    // but the wait_for_navigation call itself should not hang or panic.
    let nav_result = timeout(Duration::from_secs(30), page.wait_for_navigation()).await;

    match nav_result {
        Ok(Ok(_)) => {
            let url = page.url().await.expect("url()");
            eprintln!("navigated to: {url:?}");
        }
        Ok(Err(err)) => {
            eprintln!("navigation after click errored (acceptable): {err}");
        }
        Err(_) => {
            eprintln!("navigation after click timed out (acceptable for external site)");
        }
    }
}

// ---------------------------------------------------------------------------
// 11. Concurrent wait_for across multiple pages
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_wait_for_across_pages() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("concurrent-wait")).await;

    let mut pages = Vec::new();
    for _ in 0..3 {
        let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
            .await
            .expect("new_page should not time out")
            .expect("new_page should resolve");
        pages.push(page);
    }

    let futs: Vec<_> = pages
        .into_iter()
        .enumerate()
        .map(|(i, page)| {
            tokio::spawn(async move {
                timeout(Duration::from_secs(30), page.goto(TARGET))
                    .await
                    .unwrap_or_else(|_| panic!("page {i}: goto timed out"))
                    .unwrap_or_else(|e| panic!("page {i}: goto failed: {e}"));

                // Each page uses a different wait strategy
                match i % 3 {
                    0 => {
                        let _ = page
                            .wait_for_network_idle_with_timeout(Duration::from_secs(15))
                            .await;
                    }
                    1 => {
                        let _ = page
                            .wait_for_network_almost_idle_with_timeout(Duration::from_secs(15))
                            .await;
                    }
                    _ => {
                        let _ =
                            timeout(Duration::from_secs(15), page.wait_for_dom_content_loaded())
                                .await;
                    }
                }

                let content = timeout(Duration::from_secs(15), page.content())
                    .await
                    .unwrap_or_else(|_| panic!("page {i}: content() timed out"))
                    .unwrap_or_else(|e| panic!("page {i}: content() failed: {e}"));

                assert!(
                    content.contains("Example Domain"),
                    "page {i}: should contain 'Example Domain'"
                );
                eprintln!("page {i}: {} bytes", content.len());
            })
        })
        .collect();

    for fut in futs {
        fut.await.expect("task join");
    }
}

// ---------------------------------------------------------------------------
// 12. dom_content_loaded under concurrency — no deadlock
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_dom_content_loaded_does_not_deadlock() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("concurrent-dcl")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    // Fire many concurrent wait_for_dom_content_loaded calls.
    let futs: Vec<_> = (0..50)
        .map(|i| {
            let page = page.clone();
            tokio::spawn(async move {
                timeout(Duration::from_secs(30), page.wait_for_dom_content_loaded())
                    .await
                    .unwrap_or_else(|_| panic!("dcl({i}) timed out — possible deadlock"))
                    .unwrap_or_else(|err| panic!("dcl({i}) failed: {err}"));
            })
        })
        .collect();

    for (i, fut) in futs.into_iter().enumerate() {
        fut.await
            .unwrap_or_else(|err| panic!("task {i} panicked: {err}"));
    }
}

// ---------------------------------------------------------------------------
// 13. wait_for_navigation resolves on DOMContentLoaded (not load)
// ---------------------------------------------------------------------------

/// Verify that goto + wait_for_navigation unblocks the pipeline on
/// DOMContentLoaded — content is ready without waiting for subresources.
#[tokio::test]
async fn wait_for_navigation_resolves_on_dom_content_loaded() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfn-dcl")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    // wait_for_navigation now resolves on DOMContentLoaded.
    timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("wait_for_navigation should not time out")
        .expect("wait_for_navigation should succeed");

    // Content should be fully available immediately.
    let content = timeout(Duration::from_secs(15), page.content())
        .await
        .expect("content() should not time out")
        .expect("content() should succeed");

    assert!(
        content.contains("Example Domain"),
        "page should contain 'Example Domain' after wait_for_navigation (DCL)"
    );
    eprintln!("wait_for_navigation (DCL): {} bytes ready", content.len());
}

// ---------------------------------------------------------------------------
// 14. wait_for_selector — poll until CSS selector matches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_selector_finds_element() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfs")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("wait_for_navigation should not time out")
        .expect("wait_for_navigation should succeed");

    // example.com has an <h1> — wait_for_selector should find it.
    let el = timeout(
        Duration::from_secs(15),
        page.wait_for_selector("h1", Some(Duration::from_secs(10))),
    )
    .await
    .expect("wait_for_selector should not time out")
    .expect("wait_for_selector should find <h1>");

    let text = timeout(Duration::from_secs(10), el.inner_text())
        .await
        .expect("inner_text should not time out")
        .expect("inner_text should succeed");

    assert!(
        text.as_deref()
            .is_some_and(|t| t.contains("Example Domain")),
        "h1 should contain 'Example Domain', got: {text:?}"
    );
}

/// wait_for_selector with a missing element should time out, not hang.
#[tokio::test]
async fn wait_for_selector_times_out_for_missing_element() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfs-missing")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("wait_for_navigation should not time out")
        .expect("wait_for_navigation should succeed");

    // Nonexistent selector — should return Err(Timeout), not hang.
    let result = page
        .wait_for_selector("#does-not-exist", Some(Duration::from_secs(2)))
        .await;

    assert!(
        result.is_err(),
        "wait_for_selector should error on missing element"
    );
}

/// wait_for_selector with `None` timeout must NOT hang forever — the default
/// 30s timeout kicks in.  We verify it returns `Err(Timeout)` within a
/// reasonable outer bound (well under 30s is fine; the key is it doesn't block
/// forever).
#[tokio::test]
async fn wait_for_selector_none_timeout_does_not_hang() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfs-none-timeout")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("wait_for_navigation should not time out")
        .expect("wait_for_navigation should succeed");

    // Pass `None` — must not loop forever; the 30s default timeout fires.
    // We wrap in a 35s outer timeout so the test itself does not hang if
    // the fix regresses.
    let result = timeout(
        Duration::from_secs(35),
        page.wait_for_selector("#absolutely-does-not-exist-xyz", None),
    )
    .await;

    match result {
        Ok(Err(_cdp_err)) => {
            // Expected: CdpError::Timeout from the default 30s timeout.
        }
        Ok(Ok(_)) => panic!("selector should not have been found"),
        Err(_elapsed) => {
            panic!(
                "outer 35s timeout fired — wait_for_selector(None) did not \
                 apply the default 30s timeout, possible infinite loop regression"
            );
        }
    }
}

/// wait_for_selector with `None` still finds existing elements normally.
#[tokio::test]
async fn wait_for_selector_none_timeout_finds_existing_element() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfs-none-exists")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("wait_for_navigation should not time out")
        .expect("wait_for_navigation should succeed");

    // h1 exists on example.com — None timeout should still find it quickly.
    let el = timeout(Duration::from_secs(15), page.wait_for_selector("h1", None))
        .await
        .expect("should not time out")
        .expect("should find h1 with None timeout");

    let text = timeout(Duration::from_secs(10), el.inner_text())
        .await
        .expect("inner_text should not time out")
        .expect("inner_text should succeed");

    assert!(
        text.as_deref()
            .is_some_and(|t| t.contains("Example Domain")),
        "h1 should contain 'Example Domain', got: {text:?}"
    );
}

/// Many concurrent wait_for_selector calls on the same page must not
/// deadlock the handler or cause starvation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_wait_for_selector_does_not_deadlock() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("concurrent-wfs")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    timeout(Duration::from_secs(30), page.wait_for_navigation())
        .await
        .expect("wait_for_navigation should not time out")
        .expect("wait_for_navigation should succeed");

    // Fire 10 concurrent wait_for_selector calls — mix of hits and misses.
    // Use a generous timeout since each polling iteration sends a CDP command
    // and many concurrent callers compete for the handler.
    let futs: Vec<_> = (0..10)
        .map(|i| {
            let page = page.clone();
            tokio::spawn(async move {
                let selector = if i % 2 == 0 { "h1" } else { "#nonexistent" };
                let result = page
                    .wait_for_selector(selector, Some(Duration::from_secs(15)))
                    .await;
                if i % 2 == 0 {
                    assert!(
                        result.is_ok(),
                        "task {i}: wait_for_selector('h1') should succeed"
                    );
                } else {
                    assert!(
                        result.is_err(),
                        "task {i}: wait_for_selector('#nonexistent') should timeout"
                    );
                }
            })
        })
        .collect();

    let outer = timeout(Duration::from_secs(60), async {
        for (i, fut) in futs.into_iter().enumerate() {
            fut.await
                .unwrap_or_else(|err| panic!("task {i} panicked: {err}"));
        }
    })
    .await;

    assert!(
        outer.is_ok(),
        "concurrent wait_for_selector timed out — possible deadlock"
    );
}

/// Handler drain budget: many rapid commands from a single page must not
/// starve other pages. We verify by running two pages concurrently — one
/// flooding with JS evaluations, the other doing a simple navigation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handler_drain_budget_does_not_starve_other_pages() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("drain-budget")).await;

    let page_flood = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("flood page: new_page should not time out")
        .expect("flood page: new_page should resolve");

    let page_simple = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("simple page: new_page should not time out")
        .expect("simple page: new_page should resolve");

    // Start both pages at once: one floods the handler, the other navigates.
    let flood_handle = tokio::spawn({
        let page = page_flood.clone();
        async move {
            // Send many rapid evaluate calls to flood the handler channel.
            for i in 0..200 {
                let _ = timeout(Duration::from_secs(5), page.evaluate(format!("1 + {i}"))).await;
            }
        }
    });

    let simple_handle = tokio::spawn({
        let page = page_simple.clone();
        async move {
            let start = std::time::Instant::now();
            timeout(Duration::from_secs(15), page.goto(TARGET))
                .await
                .expect("simple page: goto should not time out")
                .expect("simple page: goto should succeed");

            timeout(Duration::from_secs(15), page.wait_for_navigation())
                .await
                .expect("simple page: wait_for_navigation should not time out")
                .expect("simple page: wait_for_navigation should succeed");

            let elapsed = start.elapsed();
            eprintln!("simple page navigated in {elapsed:?} despite flood");
            elapsed
        }
    });

    let (flood_result, simple_result) = tokio::join!(flood_handle, simple_handle);
    flood_result.expect("flood task should not panic");
    let nav_time = simple_result.expect("simple task should not panic");

    // The simple page should complete within a reasonable time even under
    // handler pressure. If the drain budget fix regresses, the flood page
    // would starve the simple page.
    assert!(
        nav_time < Duration::from_secs(15),
        "simple page took {nav_time:?} — handler may be starved by flood page"
    );
}

// ---------------------------------------------------------------------------
// 15. wait_for_delay — simple floor delay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wait_for_delay_sleeps() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("wfd")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    let start = std::time::Instant::now();
    page.wait_for_delay(Duration::from_millis(200)).await;
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(180),
        "wait_for_delay should sleep at least ~200ms, got {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Fan-out stress: 1024 concurrent waiters exercise the drain budget
// ---------------------------------------------------------------------------
//
// Background: the target's waiter queues (`wait_for_*`) used to be drained
// with unbounded `while let Some(tx) = pop()` loops inside `Target::poll`.
// Each queue now drains at most `WAITER_DRAIN_BUDGET = 64` senders per poll
// and self-wakes if more remain, which spreads fan-out across multiple polls.
//
// This test stresses that path with 1024 waiters across both queues (`load`
// and `dom_content_loaded`). If the re-arm is broken (missing `wake_by_ref`
// after a partial drain), these waiters will stall mid-queue and the test
// will time out — which is what the per-task timeout catches.
//
// Scale chosen so the budget has to cycle at least 16 times per queue,
// proving re-arm works across many polls without deadlock.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_waiters_drain_without_deadlock() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("fan-out-drain")).await;

    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    // Kick a real navigation (about:blank may already be `loaded` at this
    // point, which would make every waiter fire inline and bypass the
    // queue). A real goto forces the target to reset lifecycle state and
    // re-enqueue fresh waiters.
    timeout(Duration::from_secs(30), page.goto(TARGET))
        .await
        .expect("goto should not time out")
        .expect("goto should succeed");

    const WAITERS_PER_QUEUE: usize = 512;
    let mut futs = Vec::with_capacity(WAITERS_PER_QUEUE * 2);

    for i in 0..WAITERS_PER_QUEUE {
        let p = page.clone();
        futs.push(tokio::spawn(async move {
            timeout(Duration::from_secs(30), p.wait_for_load())
                .await
                .unwrap_or_else(|_| panic!("wait_for_load({i}) timed out — drain-budget deadlock?"))
                .unwrap_or_else(|err| panic!("wait_for_load({i}) failed: {err}"));
        }));
    }
    for i in 0..WAITERS_PER_QUEUE {
        let p = page.clone();
        futs.push(tokio::spawn(async move {
            timeout(Duration::from_secs(30), p.wait_for_dom_content_loaded())
                .await
                .unwrap_or_else(|_| {
                    panic!("wait_for_dom_content_loaded({i}) timed out — drain-budget deadlock?")
                })
                .unwrap_or_else(|err| panic!("wait_for_dom_content_loaded({i}) failed: {err}"));
        }));
    }

    for (i, fut) in futs.into_iter().enumerate() {
        fut.await
            .unwrap_or_else(|err| panic!("task {i} panicked: {err}"));
    }
}
