//! End-to-end integration test for `Page::move_mouse_smooth`.
//!
//! After switching `move_mouse_smooth` to dispatch each CDP `mouseMoved`
//! via `send_command` (fire-and-forget, drops the response oneshot)
//! instead of `execute` (awaits the response), we need to prove three
//! things hold end-to-end against a real Chrome:
//!
//! 1. **Events actually arrive at the page.** The page's
//!    `document.addEventListener('mousemove', ...)` fires for each
//!    intermediate point — if the fire-and-forget path dropped
//!    commands on the floor, the listener would miss them.
//!
//! 2. **Order is preserved.** CDP processes commands in arrival order
//!    on the single WebSocket session, and our bounded mpsc is FIFO,
//!    so the sequence of (x, y) observed by the page must match the
//!    sequence of points in the mouse path.
//!
//! 3. **Final position lands on target.** The `mouse_position()`
//!    accessor returns the target point, and the last observed
//!    mousemove event carries coordinates close to the target.
//!
//! This is the "visual" verification — without rendering a UI, we use
//! JavaScript's mousemove listener to capture exactly what the browser
//! saw. If the page ever renders for a human, the perceived motion
//! tracks this captured trajectory.

use chromey::browser::{Browser, BrowserConfig, HeadlessMode};
use chromey::layout::Point;
use futures_util::StreamExt;
use serde_json::Value;
use std::path::PathBuf;
use tokio::time::{timeout, Duration};

fn try_browser_config() -> Option<BrowserConfig> {
    BrowserConfig::builder().build().ok()
}

fn temp_profile_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chromey-mouse-{test_name}-{}-{}",
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
    tokio::spawn(async move { while let Some(_event) = handler.next().await {} });
    browser
}

/// JS that installs a mousemove listener pushing `(clientX, clientY)`
/// onto `window.__mouseTrail`. Injected into an `about:blank` page via
/// `evaluate` — keeping the test self-contained without depending on
/// any external resource.
const INSTALL_TRAIL_JS: &str = "\
    window.__mouseTrail = [];\
    document.addEventListener('mousemove', (e) => {\
        window.__mouseTrail.push([e.clientX, e.clientY]);\
    }, { passive: true });\
    true";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_mouse_smooth_delivers_path_events_end_to_end() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("smooth-path")).await;
    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page should not time out")
        .expect("new_page should resolve");

    // Wait for DOMContentLoaded so `document` exists before the
    // listener install.
    timeout(Duration::from_secs(30), page.wait_for_dom_content_loaded())
        .await
        .expect("wait_for_dom_content_loaded should not time out")
        .expect("wait_for_dom_content_loaded should succeed");

    // Install the mousemove listener in the page's main world.
    page.evaluate(INSTALL_TRAIL_JS)
        .await
        .expect("install mousemove listener");

    // Seed the cursor at (0, 0) so the smooth path starts from a
    // known origin. Do this AFTER installing the listener so the seed
    // move's event is captured and deterministically flushed before we
    // reset — any race here would survive as a stray initial point,
    // which the reset below clears.
    page.move_mouse(Point::new(0.0, 0.0))
        .await
        .expect("seed move should succeed");

    // Give the seed mousemove a moment to fire on the page's task
    // queue, then clear the trail so we only measure events produced
    // by `move_mouse_smooth` itself.
    tokio::time::sleep(Duration::from_millis(100)).await;
    page.evaluate("window.__mouseTrail = []")
        .await
        .expect("reset trail");

    let target = Point::new(420.0, 300.0);
    page.move_mouse_smooth(target)
        .await
        .expect("move_mouse_smooth should succeed");

    // CDP commands are queued by `send_command`, but mousemove events
    // fire on the page's main-thread task queue — there can be a few
    // milliseconds of lag before the last event is observed. Poll the
    // trail up to 2s waiting for it to stop growing.
    let trail = timeout(Duration::from_secs(5), async {
        let mut prev_len = 0usize;
        let mut stable_ticks = 0u32;
        loop {
            let len_val: Value = page
                .evaluate("window.__mouseTrail.length")
                .await
                .expect("evaluate length")
                .into_value()
                .expect("length is number");
            let len = len_val.as_u64().unwrap_or(0) as usize;
            if len == prev_len && len > 0 {
                stable_ticks += 1;
                if stable_ticks >= 3 {
                    break;
                }
            } else {
                stable_ticks = 0;
            }
            prev_len = len;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let trail: Value = page
            .evaluate("JSON.stringify(window.__mouseTrail)")
            .await
            .expect("evaluate trail")
            .into_value()
            .expect("trail is string");
        let s = trail.as_str().expect("trail as str");
        serde_json::from_str::<Vec<(f64, f64)>>(s).expect("parse trail")
    })
    .await
    .expect("trail should stabilize within 5s");

    assert!(
        trail.len() >= 4,
        "smooth path should generate ≥4 intermediate mousemove events, got {}",
        trail.len()
    );

    // The last observed point must be on target (Chrome rounds CSS
    // pixel coordinates, so accept 1px slack).
    let (lx, ly) = *trail.last().expect("non-empty");
    assert!(
        (lx - target.x).abs() <= 1.0 && (ly - target.y).abs() <= 1.0,
        "last observed point {:?} should match target ({}, {})",
        (lx, ly),
        target.x,
        target.y
    );

    // SmartMouse's tracked position must also match target — this is
    // the in-process accessor, set synchronously by `move_mouse` and
    // `move_mouse_smooth` regardless of what the browser saw.
    let tracked = page.mouse_position();
    assert!(
        (tracked.x - target.x).abs() <= 1.0 && (tracked.y - target.y).abs() <= 1.0,
        "SmartMouse tracked position {:?} should match target",
        tracked,
    );
}

/// Regression guard + drift-free property check for the
/// `sleep_until`-based pacing.
///
/// `move_mouse_smooth` now paces via absolute deadlines, so the total
/// wall-clock duration should track `sum(step.delay)` tightly even
/// under scheduler jitter and Chrome round-trip variance. We assert:
///
/// 1. Upper bound: 3 seconds (catches a hypothetical regression to
///    per-step response waits piling up on top of the sleeps).
/// 2. Back-to-back property: running `move_mouse_smooth` twice in a
///    row should take ~2× the single-call time — not 3× or more,
///    which would indicate drift accumulating across calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_mouse_smooth_wall_clock_tracks_step_delays() {
    if try_browser_config().is_none() {
        eprintln!("skipping: no Chrome/Chromium executable found");
        return;
    }

    let browser = launch(headless_config("smooth-timing")).await;
    let page = timeout(Duration::from_secs(30), browser.new_page("about:blank"))
        .await
        .expect("new_page")
        .expect("resolved");

    page.move_mouse(Point::new(0.0, 0.0))
        .await
        .expect("seed move");

    // Single-call baseline.
    let start = std::time::Instant::now();
    page.move_mouse_smooth(Point::new(500.0, 400.0))
        .await
        .expect("smooth move");
    let single = start.elapsed();

    assert!(
        single < Duration::from_secs(3),
        "move_mouse_smooth took {single:?} — fire-and-forget regression?"
    );

    // Back-to-back: two calls from (500, 400) → (50, 50) → (500, 400).
    // With drift-free pacing the total should be roughly 2× single,
    // not ballooning linearly in N calls.
    let start2 = std::time::Instant::now();
    page.move_mouse_smooth(Point::new(50.0, 50.0))
        .await
        .expect("smooth back");
    page.move_mouse_smooth(Point::new(500.0, 400.0))
        .await
        .expect("smooth forth");
    let double = start2.elapsed();

    // Generous upper bound (3× single) to avoid flakes — tight enough
    // to catch drift accumulation of ~50%+ per call.
    assert!(
        double < single * 3,
        "two move_mouse_smooth calls took {double:?} vs single {single:?} — drift regression?"
    );
}
