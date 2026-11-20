//! Phase 1 baseline benchmark for the chromey handler under multi-page load.
//!
//! Drives the real `Handler::run()` against an in-process CDP mock so the
//! measurement isolates the handler's command-dispatch + event-routing path
//! (no Chrome process, no network). The number this produces is the baseline
//! that the Phase 2 per-session-task redesign must beat.
//!
//! Two metrics, parameterised on page count P ∈ {1, 2, 4, 8, 16, 32}:
//!
//! * `parallel_pages/throughput/<P>` — total commands/sec when P pages each
//!   issue K = 64 `Runtime.evaluate` calls *sequentially*, with all P page
//!   tasks driven in parallel. Sequential per page keeps in-flight depth
//!   bounded so the global WS command channel (2048 slots) is never the
//!   bottleneck — the limit is handler-loop dispatch overhead, which is
//!   exactly what the per-session redesign is meant to relieve.
//! * `parallel_pages/per_cmd_latency/<P>` — wall time for one round-trip on
//!   the first page while P-1 sibling pages are *idle* (i.e., handler is
//!   tracking them but they are not issuing commands). Reveals constant-
//!   factor cost the handler pays for each tracked page.

#[path = "../tests/support/cdp_mock.rs"]
mod cdp_mock;

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use chromey::handler::HandlerConfig;
use chromey::Browser;
use chromey::Page;
use chromey_cdp::cdp::js_protocol::runtime::EvaluateParams;

const PAGE_COUNTS: &[usize] = &[1, 2, 4, 8, 16, 32];
const CMDS_PER_PAGE: usize = 64;

/// One harness instance: mock + browser + N pages, ready to receive commands.
struct Harness {
    _mock: cdp_mock::CdpMock,
    _browser: Arc<Browser>,
    pages: Vec<Page>,
    _handler: tokio::task::JoinHandle<chromey::error::Result<()>>,
}

/// Which handler driver to use for a given setup.
#[derive(Copy, Clone, Debug)]
enum Driver {
    Serial,
    #[cfg(feature = "parallel-handler")]
    Parallel,
}

impl Driver {
    fn label(self) -> &'static str {
        match self {
            Driver::Serial => "serial",
            #[cfg(feature = "parallel-handler")]
            Driver::Parallel => "parallel",
        }
    }
}

async fn setup(rt: &tokio::runtime::Handle, pages: usize, driver: Driver) -> Harness {
    let mock = cdp_mock::CdpMock::spawn().await;
    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(10),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(mock.ws_url(), cfg)
        .await
        .expect("connect to mock");
    let handle = match driver {
        Driver::Serial => rt.spawn(handler.run()),
        #[cfg(feature = "parallel-handler")]
        Driver::Parallel => rt.spawn(handler.run_parallel()),
    };
    let browser = Arc::new(browser);

    let mut create_tasks = Vec::with_capacity(pages);
    for _ in 0..pages {
        let b = browser.clone();
        create_tasks.push(rt.spawn(async move { b.new_page("about:blank").await }));
    }
    let pages = futures_util::future::join_all(create_tasks)
        .await
        .into_iter()
        .map(|r| r.expect("join").expect("new_page"))
        .collect::<Vec<_>>();

    Harness {
        _mock: mock,
        _browser: browser,
        pages,
        _handler: handle,
    }
}

fn drivers() -> Vec<Driver> {
    #[cfg_attr(not(feature = "parallel-handler"), allow(unused_mut))]
    let mut v = vec![Driver::Serial];
    #[cfg(feature = "parallel-handler")]
    v.push(Driver::Parallel);
    v
}

fn bench_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("rt");

    let mut group = c.benchmark_group("parallel_pages/throughput");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(8));

    for driver in drivers() {
        for &pages in PAGE_COUNTS {
            let harness = rt.block_on(setup(rt.handle(), pages, driver));
            group.throughput(Throughput::Elements((pages * CMDS_PER_PAGE) as u64));

            let id = BenchmarkId::new(driver.label(), pages);
            group.bench_with_input(id, &pages, |b, _| {
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let start = Instant::now();
                        for _ in 0..iters {
                            // One task per page. Each task issues K commands
                            // *sequentially* (await each round-trip before the
                            // next), but page tasks run in parallel.
                            let mut tasks = Vec::with_capacity(pages);
                            for page in &harness.pages {
                                let p = page.clone();
                                tasks.push(tokio::spawn(async move {
                                    for i in 0..CMDS_PER_PAGE {
                                        p.execute(EvaluateParams::new(format!("'p{i}'")))
                                            .await
                                            .expect("evaluate");
                                    }
                                }));
                            }
                            for t in tasks {
                                t.await.expect("join");
                            }
                        }
                        start.elapsed()
                    })
                });
            });

            // Drop the harness explicitly to free the mock port between page sizes.
            drop(harness);
        }
    }
    group.finish();
}

fn bench_single_cmd_latency_under_load(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("rt");

    let mut group = c.benchmark_group("parallel_pages/per_cmd_latency");
    group.sample_size(40);
    group.measurement_time(Duration::from_secs(6));

    for driver in drivers() {
        for &pages in PAGE_COUNTS {
            let harness = rt.block_on(setup(rt.handle(), pages, driver));

            let id = BenchmarkId::new(driver.label(), pages);
            group.bench_with_input(id, &pages, |b, _| {
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let page = harness.pages[0].clone();
                        let start = Instant::now();
                        for _ in 0..iters {
                            page.execute(EvaluateParams::new("'x'"))
                                .await
                                .expect("evaluate");
                        }
                        start.elapsed()
                    })
                });
            });

            drop(harness);
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_throughput,
    bench_single_cmd_latency_under_load
);
criterion_main!(benches);
