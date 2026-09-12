//! Real-Chrome companion to `parallel_handler_stall_watchdog.rs`.
//!
//! Same heartbeat methodology, but driven against a real headless Chrome.
//! Skipped automatically when no Chrome / Chromium binary is on the box.
//!
//! Catches stalls that the mock can't reproduce: real CDP timing (variable
//! `Target.attachToTarget` latency, late `targetCreated` events, navigation
//! lifecycle ordering) plus the WS over the loopback socket. The ceiling
//! is more generous than the mock test because Chrome IPC has legitimate
//! pauses; bumping the ceiling further would only mask real regressions.
//!
//! Run with:
//!   cargo test --features parallel-handler --test parallel_handler_stall_watchdog_chrome
//!
//! With runtime poll-time histogram check (Chrome variant):
//!   RUSTFLAGS="--cfg tokio_unstable" \
//!     cargo test --features parallel-handler \
//!       --test parallel_handler_stall_watchdog_chrome

#![cfg(feature = "parallel-handler")]
#![allow(unexpected_cfgs)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig, HeadlessMode};
use chromiumoxide_cdp::cdp::js_protocol::runtime::EvaluateParams;
use tokio::time::timeout;

fn try_chrome_config(test_name: &str) -> Option<BrowserConfig> {
    // Probe: any Chrome detectable on this box?
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_parallel_handler_runtime_stays_responsive_real_chrome() {
    let Some(config) = try_chrome_config("e2e-stall-watchdog") else {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    };

    let (browser, handler) = Browser::launch(config).await.expect("launch chrome");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    // Real Chrome IPC has more jitter than the in-process mock; widen the
    // ceiling but keep it tight enough that a sync filesystem read or a
    // lock-across-`.await` regression still trips.
    const TICK_FAST: Duration = Duration::from_millis(15);
    const TICK_SLOW: Duration = Duration::from_millis(23);
    const MAX_STALL_MS: u64 = 750;

    // Burn-in: let the launch handshake settle so it doesn't dominate the
    // first heartbeat sample.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let hb_fast = Heartbeat::spawn(TICK_FAST);
    let hb_slow = Heartbeat::spawn(TICK_SLOW);

    // Real Chrome on macOS struggles past ~16 concurrent tabs in CI; keep
    // the workload modest but enough to keep both workers busy.
    const PAGES: usize = 4;
    const CMDS_PER_PAGE: usize = 16;

    let mut create_tasks = Vec::with_capacity(PAGES);
    for _ in 0..PAGES {
        let b = browser.clone();
        create_tasks.push(tokio::spawn(async move {
            timeout(Duration::from_secs(20), b.new_page("about:blank"))
                .await
                .expect("new_page timeout — possible deadlock")
                .expect("new_page")
        }));
    }
    let pages = futures_util::future::join_all(create_tasks)
        .await
        .into_iter()
        .map(|r| r.expect("join"))
        .collect::<Vec<_>>();

    let mut cmd_tasks = Vec::with_capacity(PAGES * CMDS_PER_PAGE);
    for page in &pages {
        for i in 0..CMDS_PER_PAGE {
            let p = page.clone();
            cmd_tasks.push(tokio::spawn(async move {
                let resp = timeout(
                    Duration::from_secs(15),
                    p.execute(EvaluateParams::new("1+2")),
                )
                .await
                .expect("execute timeout — possible deadlock")
                .expect("evaluate");
                let val = resp
                    .result
                    .result
                    .value
                    .as_ref()
                    .and_then(|v| v.as_i64())
                    .expect("number");
                let _ = i;
                assert_eq!(val, 3);
            }));
        }
    }

    let _ = timeout(
        Duration::from_secs(60),
        futures_util::future::join_all(cmd_tasks),
    )
    .await
    .expect("real-Chrome workload deadlocked");

    let max_fast = hb_fast.stop().await;
    let max_slow = hb_slow.stop().await;
    let max_gap = max_fast.max(max_slow);
    assert!(
        max_gap <= MAX_STALL_MS,
        "real Chrome: runtime stalled for {max_gap}ms (fast {}ms / max {max_fast}ms; \
         slow {}ms / max {max_slow}ms; limit {MAX_STALL_MS}ms)",
        TICK_FAST.as_millis(),
        TICK_SLOW.as_millis(),
    );

    drop(browser);
}

/// Real-Chrome runtime poll-time histogram check. Builds a runtime with
/// the per-worker poll-time histogram enabled, drives a real Chrome
/// workload, and asserts no poll exceeded the threshold.
///
/// More generous than the mock variant: real Chrome's CDP responses are
/// processed on the runtime, and JSON deserialization of larger payloads
/// can legitimately take a few ms. 100 ms is well above that floor while
/// still catching anything that would pass for a "blocking call."
#[cfg(tokio_unstable)]
#[test]
fn e2e_parallel_handler_no_long_polls_real_chrome() {
    use tokio::runtime::Builder;

    let runtime = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .enable_metrics_poll_time_histogram()
        .build()
        .expect("build runtime with poll-time histogram");

    let chrome_available = runtime.block_on(async {
        let Some(config) = try_chrome_config("e2e-stall-histogram") else {
            eprintln!("skipping: no Chrome/Chromium executable found");
            return false;
        };

        let (browser, handler) = Browser::launch(config).await.expect("launch chrome");
        let _h = tokio::spawn(handler.run_parallel());
        let browser = Arc::new(browser);

        const PAGES: usize = 4;
        const CMDS_PER_PAGE: usize = 16;

        let mut create_tasks = Vec::with_capacity(PAGES);
        for _ in 0..PAGES {
            let b = browser.clone();
            create_tasks.push(tokio::spawn(async move {
                timeout(Duration::from_secs(20), b.new_page("about:blank"))
                    .await
                    .expect("new_page timeout")
                    .expect("new_page")
            }));
        }
        let pages = futures_util::future::join_all(create_tasks)
            .await
            .into_iter()
            .map(|r| r.expect("join"))
            .collect::<Vec<_>>();

        let mut cmd_tasks = Vec::with_capacity(PAGES * CMDS_PER_PAGE);
        for page in &pages {
            for _ in 0..CMDS_PER_PAGE {
                let p = page.clone();
                cmd_tasks.push(tokio::spawn(async move {
                    let _ = timeout(
                        Duration::from_secs(15),
                        p.execute(EvaluateParams::new("1+2")),
                    )
                    .await
                    .expect("execute timeout")
                    .expect("evaluate");
                }));
            }
        }
        let _ = timeout(
            Duration::from_secs(60),
            futures_util::future::join_all(cmd_tasks),
        )
        .await
        .expect("workload deadlocked");

        drop(browser);
        // Settle so straggler polls land in the histogram before we sample.
        tokio::time::sleep(Duration::from_millis(100)).await;
        true
    });

    if !chrome_available {
        return;
    }

    const STALL_THRESHOLD: Duration = Duration::from_millis(100);
    let metrics = runtime.metrics();
    let num_workers = metrics.num_workers();
    let num_buckets = metrics.poll_time_histogram_num_buckets();
    assert!(num_workers > 0 && num_buckets > 0);

    let mut over_threshold: u64 = 0;
    let mut worst_lower_us: u128 = 0;
    for w in 0..num_workers {
        for b in 0..num_buckets {
            let count = metrics.poll_time_histogram_bucket_count(w, b);
            if count == 0 {
                continue;
            }
            let range = metrics.poll_time_histogram_bucket_range(b);
            if range.start >= STALL_THRESHOLD {
                over_threshold += count;
                let lo = range.start.as_micros();
                if lo > worst_lower_us {
                    worst_lower_us = lo;
                }
            }
        }
    }

    assert_eq!(
        over_threshold,
        0,
        "real Chrome poll-time histogram: {over_threshold} polls took ≥ {}ms \
         (worst bucket lower bound: {}µs)",
        STALL_THRESHOLD.as_millis(),
        worst_lower_us,
    );
}
