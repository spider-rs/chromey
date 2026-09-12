//! Real-website e2e watchdog: drives a handful of public sites through the
//! parallel handler with the same heartbeat methodology as
//! `parallel_handler_stall_watchdog_chrome.rs`. Catches stalls + deadlocks
//! that only surface against actual network I/O + real HTML/JS pipelines.
//!
//! Skipped when:
//! * No Chrome / Chromium binary on the box.
//! * `CHROMEY_NO_NETWORK=1` is set.
//!
//! Run with:
//!   cargo test --features parallel-handler --test parallel_handler_real_sites -- --nocapture

#![cfg(feature = "parallel-handler")]
#![allow(unexpected_cfgs)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig, HeadlessMode};
use chromiumoxide_cdp::cdp::js_protocol::runtime::EvaluateParams;
use tokio::time::timeout;

/// A small mix of public sites: a bare minimal page, a programmable HTML
/// echo, and a real content site. Diverse enough to exercise different
/// network shapes without making the test rely on any one host being
/// healthy.
const SITES: &[&str] = &[
    "https://example.com/",
    "https://httpbin.org/html",
    "https://www.rust-lang.org/",
];

fn try_chrome_config(test_name: &str) -> Option<BrowserConfig> {
    if std::env::var("CHROMEY_NO_NETWORK").is_ok() {
        return None;
    }
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
        .arg("--ignore-certificate-errors")
        .headless_mode(HeadlessMode::True)
        .launch_timeout(Duration::from_secs(30))
        .build()
        .ok()
}

struct Heartbeat {
    stop: Arc<AtomicBool>,
    max_gap_ms: Arc<AtomicU64>,
    handle: tokio::task::JoinHandle<()>,
}

impl Heartbeat {
    fn spawn(tick: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let max_gap_ms = Arc::new(AtomicU64::new(0));
        let stop_clone = stop.clone();
        let max_gap_clone = max_gap_ms.clone();
        let handle = tokio::spawn(async move {
            let mut last = Instant::now();
            while !stop_clone.load(Ordering::Relaxed) {
                tokio::time::sleep(tick).await;
                let now = Instant::now();
                let gap = now.duration_since(last).as_millis() as u64;
                let prev = max_gap_clone.load(Ordering::Relaxed);
                if gap > prev {
                    max_gap_clone.store(gap, Ordering::Relaxed);
                }
                last = now;
            }
        });
        Self {
            stop,
            max_gap_ms,
            handle,
        }
    }

    async fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.await;
        self.max_gap_ms.load(Ordering::Relaxed)
    }
}

/// Drive each site through both code paths:
/// * `Browser::new_page(url)` — exercises `Target.createTarget`'s URL field.
/// * `Browser::new_page("about:blank")` + `Page::goto(url)` — exercises the
///   navigation pipeline (`Page.navigate` + lifecycle event matching).
///
/// Heartbeat watches the runtime throughout so any blocking call surfaces
/// independently of the success/failure of any individual fetch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_parallel_handler_real_websites_no_stall() {
    let Some(config) = try_chrome_config("e2e-real-sites") else {
        eprintln!("skipping: no Chrome or CHROMEY_NO_NETWORK set");
        return;
    };

    let (browser, handler) = Browser::launch(config).await.expect("launch chrome");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    // Burn-in for the launch handshake and process warm-up.
    tokio::time::sleep(Duration::from_millis(300)).await;

    const TICK_FAST: Duration = Duration::from_millis(15);
    const TICK_SLOW: Duration = Duration::from_millis(23);
    // Real network + real Chrome on a public site has measurable jitter
    // (DNS, TLS handshake, JS init). 1500ms is well above that floor while
    // still small enough to catch a sync filesystem read or a lock held
    // across `.await`.
    const MAX_STALL_MS: u64 = 1500;

    let hb_fast = Heartbeat::spawn(TICK_FAST);
    let hb_slow = Heartbeat::spawn(TICK_SLOW);

    let mut tasks = Vec::with_capacity(SITES.len() * 2);

    // Path 1: open + goto.
    for site in SITES {
        let b = browser.clone();
        let url = (*site).to_string();
        tasks.push(tokio::spawn(async move {
            let page = timeout(Duration::from_secs(45), b.new_page("about:blank"))
                .await
                .expect("new_page about:blank timeout")
                .expect("new_page about:blank");
            timeout(Duration::from_secs(45), page.goto(url.as_str()))
                .await
                .unwrap_or_else(|_| panic!("goto({url}) timed out"))
                .unwrap_or_else(|err| panic!("goto({url}) failed: {err}"));
            // Force a JS round-trip post-navigation so we know the session
            // task is responsive after the navigation lifecycle settled.
            let resp = timeout(
                Duration::from_secs(15),
                page.execute(EvaluateParams::new("document.title")),
            )
            .await
            .unwrap_or_else(|_| panic!("title eval timeout for {url}"))
            .unwrap_or_else(|err| panic!("title eval failed for {url}: {err}"));
            let title = resp
                .result
                .result
                .value
                .as_ref()
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_default();
            (url, "goto", title)
        }));
    }

    // Path 2: createTarget direct-to-URL.
    for site in SITES {
        let b = browser.clone();
        let url = (*site).to_string();
        tasks.push(tokio::spawn(async move {
            let page = timeout(Duration::from_secs(45), b.new_page(url.as_str()))
                .await
                .unwrap_or_else(|_| panic!("new_page({url}) timeout"))
                .unwrap_or_else(|err| panic!("new_page({url}) failed: {err}"));
            // querySelectorAll touches the DOM so a parse / paint stall would
            // also show up in the heartbeat.
            let resp = timeout(
                Duration::from_secs(15),
                page.execute(EvaluateParams::new("document.querySelectorAll('*').length")),
            )
            .await
            .unwrap_or_else(|_| panic!("query timeout for {url}"))
            .unwrap_or_else(|err| panic!("query failed for {url}: {err}"));
            let count = resp
                .result
                .result
                .value
                .as_ref()
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            (url, "createTarget", format!("nodes={count}"))
        }));
    }

    let joined = timeout(
        Duration::from_secs(120),
        futures_util::future::join_all(tasks),
    )
    .await
    .expect("real-site workload deadlocked");
    let mut report: Vec<(String, &'static str, String)> = Vec::with_capacity(joined.len());
    for r in joined {
        report.push(r.expect("task join"));
    }

    let max_fast = hb_fast.stop().await;
    let max_slow = hb_slow.stop().await;
    let max_gap = max_fast.max(max_slow);

    eprintln!("real-sites watchdog: fast max {max_fast}ms, slow max {max_slow}ms");
    for (url, path, info) in &report {
        eprintln!("  ok via {path:>13}: {url} → {info}");
    }

    assert!(
        max_gap <= MAX_STALL_MS,
        "real sites: runtime stalled for {max_gap}ms (fast {max_fast}ms / slow {max_slow}ms; \
         limit {MAX_STALL_MS}ms) — possible blocking call in handler",
    );

    drop(browser);
}
