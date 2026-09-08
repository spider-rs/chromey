//! End-to-end tests for the client-side navigation deadline, driven against a
//! real Chrome and a real HTTP origin.
//!
//! `tests/navigate_timeout.rs` pins the same behaviour against a scripted CDP
//! mock. This file checks that the shape the deadline exists for survives
//! contact with Chrome's own lifecycle reporting: a document that is fully
//! delivered and parsed, holding a subresource that never answers, so
//! `DOMContentLoaded` fires and `load` does not.
//!
//! Skipped automatically when no Chrome / Chromium binary is on the box.
//!
//! Run with:
//!   cargo test --test navigate_timeout_e2e
//!   cargo test --features parallel-handler --test navigate_timeout_e2e

#[path = "support/tiny_http.rs"]
mod tiny_http;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig, HeadlessMode};
use chromiumoxide::error::CdpError;
use chromiumoxide::Page;
use chromiumoxide_cdp::cdp::browser_protocol::page::NavigateParams;
use futures_util::StreamExt;
use tokio::time::timeout;

use tiny_http::TestServer;

/// Bound on the whole navigate call, so a hang shows up as a failure with a
/// message rather than as a hung suite.
const HARD_CAP: Duration = Duration::from_secs(60);

/// Which handler loop drives the session. All three read the same
/// `FrameManager` deadline, and the release logic is written out separately in
/// the stream/run driver and in the parallel session loop, so every case runs
/// against each of them.
#[derive(Clone, Copy, Debug)]
enum Driver {
    /// `Handler::run()`, the `tokio::select!` loop.
    Run,
    /// `Handler` polled as a `Stream`.
    Stream,
    #[cfg(feature = "parallel-handler")]
    Parallel,
}

fn drivers() -> Vec<Driver> {
    vec![
        Driver::Run,
        Driver::Stream,
        #[cfg(feature = "parallel-handler")]
        Driver::Parallel,
    ]
}

impl Driver {
    fn tag(self) -> &'static str {
        match self {
            Driver::Run => "run",
            Driver::Stream => "stream",
            #[cfg(feature = "parallel-handler")]
            Driver::Parallel => "parallel",
        }
    }
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

/// `None` when no browser executable can be found on this machine, which is
/// how these tests skip cleanly off a developer box.
fn try_chrome_config(test_name: &str, request_timeout: Duration) -> Option<BrowserConfig> {
    // Probe first: does this box have any Chrome at all?
    let _ = BrowserConfig::builder().build().ok()?;

    let profile_dir = temp_profile_dir(test_name);
    BrowserConfig::builder()
        .user_data_dir(&profile_dir)
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .headless_mode(HeadlessMode::True)
        // Cold CI runners under Xvfb can be slow to print the first
        // `DevTools listening on ws://` line. Normal runs finish far inside
        // this bound.
        .launch_timeout(Duration::from_secs(90))
        .request_timeout(request_timeout)
        .build()
        .ok()
}

/// Launches Chrome and spawns the requested handler loop.
///
/// `Browser::launch` can fail with `LaunchTimeout` on a cold first start under
/// CI, so the launch is retried a bounded number of times; a failure on a later
/// attempt is a real problem and panics.
async fn launch_with_driver(config: BrowserConfig, driver: Driver) -> Browser {
    const MAX_LAUNCH_ATTEMPTS: u32 = 3;
    let mut last_err: Option<CdpError> = None;

    for attempt in 1..=MAX_LAUNCH_ATTEMPTS {
        match Browser::launch(config.clone()).await {
            Ok((browser, handler)) => {
                match driver {
                    Driver::Run => {
                        tokio::spawn(handler.run());
                    }
                    Driver::Stream => {
                        let mut handler = handler;
                        tokio::spawn(async move { while handler.next().await.is_some() {} });
                    }
                    #[cfg(feature = "parallel-handler")]
                    Driver::Parallel => {
                        tokio::spawn(handler.run_parallel());
                    }
                }
                return browser;
            }
            Err(err) => {
                eprintln!(
                    "[chromey test] Browser::launch attempt {attempt}/{MAX_LAUNCH_ATTEMPTS} \
                     failed: {err}"
                );
                last_err = Some(err);
            }
        }
    }

    panic!(
        "launch browser: exhausted {MAX_LAUNCH_ATTEMPTS} attempts, \
         last error: {last_err:?}"
    );
}

/// Returns `None` when there is no browser to test against.
async fn session(
    test_name: &str,
    driver: Driver,
    request_timeout: Duration,
) -> Option<(Browser, Page)> {
    let config = try_chrome_config(&format!("{test_name}-{}", driver.tag()), request_timeout)?;
    let browser = launch_with_driver(config, driver).await;
    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page(about:blank) timed out")
        .expect("new_page(about:blank)");
    Some((browser, page))
}

fn skip(test_name: &str) {
    eprintln!("[chromey test] skipping {test_name}: no Chrome/Chromium executable found");
}

/// Case 1 — control. No deadline, a page that loads normally. This is the path
/// every existing caller is on; it must behave exactly as it did before the
/// deadline arm existed.
///
/// Breaks if: `navigate_http_future` stops resolving on a normal load, or the
/// deadline arm leaks into the un-armed path and cuts the wait short with no
/// document in hand.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_control_no_deadline_fast_page_succeeds() {
    let request_timeout = Duration::from_secs(20);

    for driver in drivers() {
        let Some((browser, page)) = session("nav-e2e-control", driver, request_timeout).await
        else {
            skip("e2e_control_no_deadline_fast_page_succeeds");
            return;
        };
        let server = TestServer::start().await;

        let fut = page
            .navigate_http_future(NavigateParams::new(server.url("/fast")))
            .expect("build navigate future");
        assert!(!fut.committed(), "[{}] nothing acked yet", driver.tag());
        tokio::pin!(fut);

        let start = Instant::now();
        let request = timeout(HARD_CAP, &mut fut)
            .await
            .unwrap_or_else(|_| panic!("[{}] navigate hung past the hard cap", driver.tag()))
            .unwrap_or_else(|err| panic!("[{}] navigate failed: {err:?}", driver.tag()));
        let elapsed = start.elapsed();

        assert!(
            fut.committed(),
            "[{}] a normally loaded page must report committed",
            driver.tag()
        );
        assert!(
            elapsed < request_timeout,
            "[{}] a fast page must not spend the request timeout, took {elapsed:?}",
            driver.tag()
        );
        if let Some(request) = request {
            assert!(
                request.failure_text.is_none(),
                "[{}] unexpected failure text: {:?}",
                driver.tag(),
                request.failure_text
            );
        }

        let title = page.get_title().await.expect("title");
        assert_eq!(title.as_deref(), Some("fast"), "[{}]", driver.tag());

        eprintln!("[chromey test] control/{} returned in {elapsed:?}", driver.tag());
        drop(browser);
    }
}

/// Case 2 — the reason the feature exists. `/slow` commits and fires
/// `DOMContentLoaded`, but its `<img src="/hang">` keeps `load` pending
/// forever. With a 2s deadline against a 20s request timeout the caller gets
/// the committed document back at roughly the deadline.
///
/// Breaks if: the handler drops the parked ack instead of releasing it (the
/// call then ends `Err(Timeout)` with `committed()` false), or the deadline is
/// never armed (the call then spends the full 20s request timeout and fails
/// the upper bound), or the post-ack lifecycle wait stops resolving on
/// `DOMContentLoaded`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_deadline_above_commit_returns_the_committed_document() {
    let request_timeout = Duration::from_secs(20);
    let deadline = Duration::from_secs(2);

    for driver in drivers() {
        let Some((browser, page)) = session("nav-e2e-release", driver, request_timeout).await
        else {
            skip("e2e_deadline_above_commit_returns_the_committed_document");
            return;
        };
        let server = TestServer::start().await;

        let fut = page
            .navigate_http_future_with_timeout(NavigateParams::new(server.url("/slow")), deadline)
            .expect("build navigate future");
        assert_eq!(fut.navigation_timeout(), Some(deadline));
        tokio::pin!(fut);

        let start = Instant::now();
        let result = timeout(HARD_CAP, &mut fut)
            .await
            .unwrap_or_else(|_| panic!("[{}] navigate hung past the hard cap", driver.tag()));
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "[{}] the deadline must hand back the committed document, got {result:?}",
            driver.tag()
        );
        assert!(
            fut.committed(),
            "[{}] the released ack must report committed",
            driver.tag()
        );
        // Lower bound: the deadline is a deadline, not an early return. Upper
        // bound: it returned on the deadline's account, not the request
        // timeout's. Both are loose enough to survive a cold shared runner.
        assert!(
            elapsed >= deadline,
            "[{}] returned before the deadline, took {elapsed:?}",
            driver.tag()
        );
        assert!(
            elapsed < Duration::from_secs(12),
            "[{}] returned on the request timeout, not the deadline, took {elapsed:?}",
            driver.tag()
        );

        // The document really is usable, which is the whole point of releasing
        // the ack rather than dropping it.
        let title = page.get_title().await.expect("title");
        assert_eq!(title.as_deref(), Some("slow"), "[{}]", driver.tag());

        eprintln!(
            "[chromey test] release/{} returned in {elapsed:?} (deadline {deadline:?}, \
             request timeout {request_timeout:?})",
            driver.tag()
        );
        drop(browser);
    }
}

/// Case 3 — a deadline that expires before the ack can land. `/hang` never
/// answers at all, so the navigation cannot commit inside any deadline and
/// there is nothing parked for the handler to release. The caller gets
/// `Err(Timeout)` with `committed()` false, on the deadline rather than on the
/// request timeout.
///
/// The route is what makes this deterministic. A deadline racing a *fast*
/// commit is genuinely racy — see
/// `e2e_sub_commit_deadline_bounds_the_wait` below for that shape and what it
/// can honestly assert.
///
/// Breaks if: the timeout path reports a document it never received, or the
/// deadline stops bounding the wait (the call then spends the 20s request
/// timeout and fails the upper bound).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_deadline_below_commit_times_out_uncommitted() {
    let request_timeout = Duration::from_secs(20);
    let deadline = Duration::from_millis(300);

    for driver in drivers() {
        let Some((browser, page)) = session("nav-e2e-early", driver, request_timeout).await else {
            skip("e2e_deadline_below_commit_times_out_uncommitted");
            return;
        };
        let server = TestServer::start().await;

        let fut = page
            .navigate_http_future_with_timeout(NavigateParams::new(server.url("/hang")), deadline)
            .expect("build navigate future");
        tokio::pin!(fut);

        let start = Instant::now();
        let result = timeout(HARD_CAP, &mut fut)
            .await
            .unwrap_or_else(|_| panic!("[{}] navigate hung past the hard cap", driver.tag()));
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(CdpError::Timeout)),
            "[{}] a navigation that never commits must time out, got {result:?}",
            driver.tag()
        );
        assert!(
            !fut.committed(),
            "[{}] a timeout with no released ack must not claim a commit",
            driver.tag()
        );
        assert!(
            elapsed >= deadline,
            "[{}] returned before the deadline, took {elapsed:?}",
            driver.tag()
        );
        assert!(
            elapsed < Duration::from_secs(12),
            "[{}] the deadline did not bound the wait, took {elapsed:?}",
            driver.tag()
        );

        eprintln!(
            "[chromey test] early/{} returned in {elapsed:?}, committed={}",
            driver.tag(),
            fut.committed()
        );
        drop(browser);
    }
}

/// Case 3b — the same idea with a single-digit-millisecond deadline against a
/// page that *can* commit. On loopback the ack sometimes beats the deadline
/// and sometimes does not, so this asserts only what the feature actually
/// guarantees:
///
/// - the call returns on the deadline's account, far short of the 20s request
///   timeout;
/// - a success carries a commit;
/// - a timeout after a released ack still leaves `committed()` true, which is
///   the salvage signal, and a timeout with nothing released leaves it false.
///
/// All three outcomes were observed on this repo. Ok/committed happens when
/// the ack lands and `DOMContentLoaded` has already fired; Err/committed
/// happens when the ack is released but the post-ack wait then spends its own
/// `cap` (the documented `2 * cap` worst case); Err/uncommitted happens when
/// no ack arrived. Pinning any one of them would pin the scheduler, not the
/// feature.
///
/// Breaks if: the deadline stops being armed (the call then spends the 20s
/// request timeout), or a success is reported without a commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_sub_commit_deadline_bounds_the_wait() {
    let request_timeout = Duration::from_secs(20);
    let deadline = Duration::from_millis(5);

    for driver in drivers() {
        let Some((browser, page)) = session("nav-e2e-tiny", driver, request_timeout).await else {
            skip("e2e_sub_commit_deadline_bounds_the_wait");
            return;
        };
        let server = TestServer::start().await;

        let fut = page
            .navigate_http_future_with_timeout(NavigateParams::new(server.url("/slow")), deadline)
            .expect("build navigate future");
        tokio::pin!(fut);

        let start = Instant::now();
        let result = timeout(HARD_CAP, &mut fut)
            .await
            .unwrap_or_else(|_| panic!("[{}] navigate hung past the hard cap", driver.tag()));
        let elapsed = start.elapsed();

        match result {
            Ok(_) => assert!(
                fut.committed(),
                "[{}] a success must carry a commit",
                driver.tag()
            ),
            Err(CdpError::Timeout) => {}
            other => panic!("[{}] unexpected navigate result: {other:?}", driver.tag()),
        }
        assert!(
            elapsed < Duration::from_secs(12),
            "[{}] the deadline did not bound the wait, took {elapsed:?}",
            driver.tag()
        );

        eprintln!(
            "[chromey test] tiny/{} returned in {elapsed:?}, committed={}",
            driver.tag(),
            fut.committed()
        );
        drop(browser);
    }
}

/// Case 4 — `Duration::ZERO` opts out. The wait reverts to `request_timeout`,
/// so `/slow` does not return early and ends `Err(Timeout)`.
///
/// Breaks if: a zero duration is treated as a 0ms deadline (the call then
/// returns almost immediately and fails the lower bound).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_zero_duration_opts_out_of_the_deadline() {
    let request_timeout = Duration::from_secs(6);

    for driver in drivers() {
        let Some((browser, page)) = session("nav-e2e-zero", driver, request_timeout).await else {
            skip("e2e_zero_duration_opts_out_of_the_deadline");
            return;
        };
        let server = TestServer::start().await;

        let fut = page
            .navigate_http_future_with_timeout(
                NavigateParams::new(server.url("/slow")),
                Duration::ZERO,
            )
            .expect("build navigate future");
        assert_eq!(
            fut.navigation_timeout(),
            None,
            "[{}] a zero duration must not arm a deadline",
            driver.tag()
        );
        tokio::pin!(fut);

        let start = Instant::now();
        let result = timeout(HARD_CAP, &mut fut)
            .await
            .unwrap_or_else(|_| panic!("[{}] navigate hung past the hard cap", driver.tag()));
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(CdpError::Timeout)),
            "[{}] opting out must end in the request timeout, got {result:?}",
            driver.tag()
        );
        assert!(
            elapsed >= request_timeout.mul_f32(0.75),
            "[{}] returned early, so the zero duration armed a deadline after all, \
             took {elapsed:?}",
            driver.tag()
        );

        eprintln!(
            "[chromey test] zero/{} returned in {elapsed:?} (request timeout \
             {request_timeout:?})",
            driver.tag()
        );
        drop(browser);
    }
}

/// Case 5 — the before-picture that makes case 2 mean something. The same
/// `/slow` page with no deadline waits out `request_timeout` and returns
/// `Err(Timeout)`, with no document handed back.
///
/// Breaks if: the release path fires for a caller that never armed a deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_no_deadline_slow_page_spends_the_request_timeout() {
    let request_timeout = Duration::from_secs(6);

    for driver in drivers() {
        let Some((browser, page)) = session("nav-e2e-baseline", driver, request_timeout).await
        else {
            skip("e2e_no_deadline_slow_page_spends_the_request_timeout");
            return;
        };
        let server = TestServer::start().await;

        let fut = page
            .navigate_http_future(NavigateParams::new(server.url("/slow")))
            .expect("build navigate future");
        assert_eq!(fut.navigation_timeout(), None);
        tokio::pin!(fut);

        let start = Instant::now();
        let result = timeout(HARD_CAP, &mut fut)
            .await
            .unwrap_or_else(|_| panic!("[{}] navigate hung past the hard cap", driver.tag()));
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(CdpError::Timeout)),
            "[{}] an unheld `load` must end in the request timeout, got {result:?}",
            driver.tag()
        );
        assert!(
            elapsed >= request_timeout.mul_f32(0.75),
            "[{}] returned before the request timeout, took {elapsed:?}",
            driver.tag()
        );

        eprintln!(
            "[chromey test] baseline/{} returned in {elapsed:?} (request timeout \
             {request_timeout:?})",
            driver.tag()
        );
        drop(browser);
    }
}
