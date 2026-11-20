//! End-to-end benchmark: serial vs parallel handler against a real Chrome.
//!
//! Run with:
//!     cargo bench --features parallel-handler --bench parallel_e2e_bench
//!
//! Skips when no Chrome / Chromium binary is available. The mock-based bench
//! (`parallel_pages_bench`) isolates handler-loop dispatch overhead; this
//! one measures the *delivered* throughput a real user sees, including
//! Chrome's own scheduling. The mock peaks at ~140k cmds/s; real Chrome
//! adds non-trivial per-round-trip cost (~hundreds of µs per evaluate),
//! so absolute numbers are an order of magnitude lower — the *shape* of
//! the curve is what matters.

#![cfg(feature = "parallel-handler")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromey::browser::{Browser, BrowserConfig, HeadlessMode};
use chromey::Page;
use chromey_cdp::cdp::js_protocol::runtime::EvaluateParams;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use futures_util::StreamExt;

const PAGE_COUNTS: &[usize] = &[1, 2, 4, 8];
const CMDS_PER_PAGE: usize = 16;

fn try_chrome_config(test_name: &str) -> Option<BrowserConfig> {
    BrowserConfig::builder().build().ok()?;
    let dir = std::env::temp_dir().join(format!(
        "chromey-bench-{test_name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
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

#[derive(Copy, Clone, Debug)]
enum Driver {
    Serial,
    Parallel,
}

impl Driver {
    fn label(self) -> &'static str {
        match self {
            Driver::Serial => "serial",
            Driver::Parallel => "parallel",
        }
    }
}

struct Harness {
    _browser: Arc<Browser>,
    pages: Vec<Page>,
    _handler: tokio::task::JoinHandle<()>,
}

async fn setup(rt: &tokio::runtime::Handle, pages: usize, driver: Driver) -> Option<Harness> {
    let config = try_chrome_config(&format!("e2e-{:?}-{pages}", driver))?;
    let (browser, handler) = Browser::launch(config).await.ok()?;

    let join = match driver {
        Driver::Serial => {
            let mut h = handler;
            rt.spawn(async move { while h.next().await.is_some() {} })
        }
        Driver::Parallel => rt.spawn(async move {
            let _ = handler.run_parallel().await;
        }),
    };

    let browser = Arc::new(browser);
    let mut create = Vec::with_capacity(pages);
    for _ in 0..pages {
        let b = browser.clone();
        create.push(rt.spawn(async move {
            tokio::time::timeout(Duration::from_secs(20), b.new_page("about:blank"))
                .await
                .ok()?
                .ok()
        }));
    }
    let pages_vec = futures_util::future::join_all(create)
        .await
        .into_iter()
        .filter_map(|r| r.ok().flatten())
        .collect::<Vec<_>>();

    if pages_vec.len() != pages {
        return None;
    }

    Some(Harness {
        _browser: browser,
        pages: pages_vec,
        _handler: join,
    })
}

fn bench_e2e_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("rt");

    // Probe: skip cleanly if no Chrome.
    if try_chrome_config("probe").is_none() {
        eprintln!("skipping parallel_e2e_bench: no Chrome/Chromium executable found");
        return;
    }

    let mut group = c.benchmark_group("parallel_e2e/throughput");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(8));

    for driver in [Driver::Serial, Driver::Parallel] {
        for &pages in PAGE_COUNTS {
            let Some(harness) = rt.block_on(setup(rt.handle(), pages, driver)) else {
                eprintln!("skipping {}/{pages}: setup failed", driver.label());
                continue;
            };
            group.throughput(Throughput::Elements((pages * CMDS_PER_PAGE) as u64));

            let id = BenchmarkId::new(driver.label(), pages);
            group.bench_with_input(id, &pages, |b, _| {
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let start = Instant::now();
                        for _ in 0..iters {
                            let mut tasks = Vec::with_capacity(pages);
                            for page in &harness.pages {
                                let p = page.clone();
                                tasks.push(tokio::spawn(async move {
                                    for i in 0..CMDS_PER_PAGE {
                                        p.execute(EvaluateParams::new(format!("{i}+1")))
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

            drop(harness);
        }
    }
    group.finish();
}

criterion_group!(benches, bench_e2e_throughput);
criterion_main!(benches);
