//! Deadlock + CPU-stall watchdog for the parallel handler.
//!
//! Three complementary checks against the in-process CDP mock:
//!
//! * `runtime_stays_responsive_under_load` runs a pair of heartbeat tasks on
//!   the same multi-thread runtime as the Router + SessionTasks. If anything
//!   in the handler holds a worker with a blocking call (sync I/O, busy loop,
//!   lock held across `.await`, `std::thread::sleep`, etc.) the heartbeats
//!   miss ticks — the test fails with the worst observed gap. Workload is
//!   sized to keep all workers busy so a single-worker stall is observable.
//!
//! * `no_deadlock_under_session_churn` opens, uses, and drops pages in a
//!   tight loop so the Router has to recycle slots and prune routing entries
//!   while session traffic is in flight. A wall-clock timeout catches any
//!   path that holds a lock across `.await` or stalls on a closed channel.
//!
//! * `no_long_polls_runtime_metrics` (gated on `--cfg tokio_unstable`) walks
//!   the runtime's poll-time histogram after the workload and asserts no
//!   single poll exceeded `STALL_THRESHOLD`. Tighter than the heartbeat
//!   bound — heartbeats can only see stalls that align with their tick;
//!   the histogram sees every poll. Off by default because enabling
//!   `tokio_unstable` requires a `RUSTFLAGS` opt-in.
//!
//! Run with:
//!   cargo test --features parallel-handler --test parallel_handler_stall_watchdog
//!
//! Run including the runtime-metrics check:
//!   RUSTFLAGS="--cfg tokio_unstable" \
//!     cargo test --features parallel-handler --test parallel_handler_stall_watchdog

#![cfg(feature = "parallel-handler")]
// `tokio_unstable` is opt-in via RUSTFLAGS; tell rustc not to warn when
// the flag is absent.
#![allow(unexpected_cfgs)]

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::Browser;
use chromiumoxide_cdp::cdp::js_protocol::runtime::EvaluateParams;
use tokio::time::timeout;

/// Heartbeat handle: stops the loop on drop and reports the worst gap seen
/// between successive ticks.
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

/// Two workers + saturated workload makes a blocking call on either worker
/// observable: with both workers busy, the heartbeat tasks can't migrate to
/// a free worker and the gap blows out. Two heartbeats with offset cadences
/// (10 ms + 17 ms) so a stall that briefly aligns with one heartbeat's
/// quiet period is still seen by the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_handler_runtime_stays_responsive_under_load() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(10),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect to mock");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    const TICK_FAST: Duration = Duration::from_millis(10);
    const TICK_SLOW: Duration = Duration::from_millis(17);
    // Tightened ceiling: under saturated 2-worker load the heartbeat still
    // round-trips well under 250 ms in practice. A blocking call or sync
    // I/O on either worker pushes one heartbeat past this bound.
    const MAX_STALL_MS: u64 = 250;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let hb_fast = Heartbeat::spawn(TICK_FAST);
    let hb_slow = Heartbeat::spawn(TICK_SLOW);

    const PAGES: usize = 16;
    const CMDS_PER_PAGE: usize = 32;

    let mut create_tasks = Vec::with_capacity(PAGES);
    for _ in 0..PAGES {
        let b = browser.clone();
        create_tasks.push(tokio::spawn(async move {
            timeout(Duration::from_secs(5), b.new_page("about:blank"))
                .await
                .expect("new_page timeout — possible router/session deadlock")
                .expect("new_page")
        }));
    }
    let pages = futures_util::future::join_all(create_tasks)
        .await
        .into_iter()
        .map(|r| r.expect("join"))
        .collect::<Vec<_>>();

    // All commands fired concurrently — saturates the WS writer + every
    // SessionTask + the call-id allocator's DashMap.
    let mut cmd_tasks = Vec::with_capacity(PAGES * CMDS_PER_PAGE);
    for page in &pages {
        for i in 0..CMDS_PER_PAGE {
            let p = page.clone();
            cmd_tasks.push(tokio::spawn(async move {
                timeout(
                    Duration::from_secs(10),
                    p.execute(EvaluateParams::new(format!("'cmd-{i}'"))),
                )
                .await
                .expect("execute timeout — possible deadlock")
                .map(|_| ())
            }));
        }
    }

    let results = timeout(
        Duration::from_secs(30),
        futures_util::future::join_all(cmd_tasks),
    )
    .await
    .expect("workload deadlocked — overall timeout fired");

    let mut ok = 0usize;
    for r in results {
        if r.expect("join").is_ok() {
            ok += 1;
        }
    }
    assert_eq!(
        ok,
        PAGES * CMDS_PER_PAGE,
        "all parallel commands across {PAGES} pages should round-trip"
    );

    let max_fast = hb_fast.stop().await;
    let max_slow = hb_slow.stop().await;
    let max_gap = max_fast.max(max_slow);
    assert!(
        max_gap <= MAX_STALL_MS,
        "runtime stalled for {max_gap}ms (fast tick {}ms / max {max_fast}ms; \
         slow tick {}ms / max {max_slow}ms; limit {MAX_STALL_MS}ms) — \
         a SessionTask/Router future is likely holding a worker with a blocking call \
         or lock contention",
        TICK_FAST.as_millis(),
        TICK_SLOW.as_millis(),
    );

    drop(browser);
}

/// Open / use / drop pages in a tight loop so the Router cycles through
/// `alloc_slot` → `remove_session` → `ids.drop_slot` while traffic is in
/// flight. A wall-clock timeout catches any deadlock on the lifecycle
/// channel, the per-session inbox, or the call-id allocator's DashMap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_handler_no_deadlock_under_session_churn() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    const ROUNDS: usize = 32;
    const CMDS_PER_ROUND: usize = 4;

    let work = async {
        for round in 0..ROUNDS {
            let page = browser
                .new_page("about:blank")
                .await
                .unwrap_or_else(|err| panic!("new_page failed at round {round}: {err}"));
            for i in 0..CMDS_PER_ROUND {
                page.execute(EvaluateParams::new(format!("'churn-{round}-{i}'")))
                    .await
                    .unwrap_or_else(|err| panic!("execute failed at {round}/{i}: {err}"));
            }
            // Drop the Page handle so the Target/Session lifecycle teardown
            // runs while the next iteration starts a fresh attach.
            drop(page);
        }
    };

    timeout(Duration::from_secs(30), work)
        .await
        .expect("session churn deadlocked — open/use/drop loop did not complete");

    drop(browser);
}

/// Concurrent churn: many tasks each opening + dropping pages in parallel.
/// Stresses the Router's `pending_initiators` parking path, the lifecycle
/// channel (`SessionAttached` / `Detached` interleaving), and the shared
/// call-id allocator. Wall-clock timeout is the deadlock detector.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_handler_no_deadlock_under_concurrent_churn() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect");
    let _h = tokio::spawn(handler.run_parallel());
    let browser = Arc::new(browser);

    const WORKERS: usize = 8;
    const ROUNDS_PER_WORKER: usize = 8;

    let mut tasks = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS {
        let b = browser.clone();
        tasks.push(tokio::spawn(async move {
            for round in 0..ROUNDS_PER_WORKER {
                let page = b
                    .new_page("about:blank")
                    .await
                    .unwrap_or_else(|err| panic!("worker {w} new_page round {round}: {err}"));
                let _ = page
                    .execute(EvaluateParams::new(format!("'w{w}-r{round}'")))
                    .await
                    .unwrap_or_else(|err| panic!("worker {w} execute round {round}: {err}"));
                drop(page);
            }
        }));
    }

    let joined = timeout(
        Duration::from_secs(30),
        futures_util::future::join_all(tasks),
    )
    .await
    .expect("concurrent churn deadlocked — overall timeout fired");
    for r in joined {
        r.expect("worker task join");
    }

    drop(browser);
}

/// Runtime poll-time histogram check. Builds a runtime with the per-worker
/// poll-time histogram enabled, runs the same load profile as the heartbeat
/// test, then asserts that no worker recorded a poll above the stall
/// threshold.
///
/// Why this is sharper than the heartbeat: the heartbeat only sees a stall
/// when its own poll lands on the same worker that's blocked. The histogram
/// records every poll, so a sub-tick stall on a future the heartbeat never
/// shares a worker with still shows up.
///
/// Gated on `--cfg tokio_unstable` because tokio's runtime metrics are
/// unstable; without the flag this whole test is compiled out.
#[cfg(tokio_unstable)]
#[test]
fn parallel_handler_no_long_polls_runtime_metrics() {
    use tokio::runtime::Builder;

    // Threshold is the upper bound for "this poll didn't block". Any poll
    // landing in a histogram bucket whose lower edge is at or above this
    // value definitely took at least this long — that's a blocking call.
    const STALL_THRESHOLD: Duration = Duration::from_millis(50);

    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .enable_metrics_poll_time_histogram()
        .build()
        .expect("build runtime with poll-time histogram");

    runtime.block_on(async {
        let mock = cdp_mock::CdpMock::spawn().await;
        let cfg = HandlerConfig {
            request_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
            .await
            .expect("connect to mock");
        let _h = tokio::spawn(handler.run_parallel());
        let browser = Arc::new(browser);

        const PAGES: usize = 16;
        const CMDS_PER_PAGE: usize = 32;

        let mut create_tasks = Vec::with_capacity(PAGES);
        for _ in 0..PAGES {
            let b = browser.clone();
            create_tasks.push(tokio::spawn(async move {
                timeout(Duration::from_secs(5), b.new_page("about:blank"))
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
            for i in 0..CMDS_PER_PAGE {
                let p = page.clone();
                cmd_tasks.push(tokio::spawn(async move {
                    timeout(
                        Duration::from_secs(10),
                        p.execute(EvaluateParams::new(format!("'cmd-{i}'"))),
                    )
                    .await
                    .expect("execute timeout")
                    .map(|_| ())
                }));
            }
        }
        let _ = timeout(
            Duration::from_secs(30),
            futures_util::future::join_all(cmd_tasks),
        )
        .await
        .expect("workload deadlocked");

        drop(browser);
        // Brief settle so straggler polls (handler shutdown) hit the histogram
        // before we sample it.
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

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
            // Bucket's lower edge is the floor for any poll counted here:
            // if `range.start >= threshold`, every poll in this bucket
            // took at least `threshold`.
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
        over_threshold, 0,
        "poll-time histogram: {over_threshold} polls took ≥ {}ms (worst bucket lower bound: {}µs) — \
         a future is doing blocking work without yielding",
        STALL_THRESHOLD.as_millis(),
        worst_lower_us,
    );
}
