// RUST_LOG=debug cargo run --example http_cache --features="cache"
// startup the [hybrid_cache_server](https://github.com/spider-rs/hybrid_cache_server)
use chromey::{
    browser::Browser,
    cache::{BasicCachePolicy, CacheStrategy},
    handler::HandlerConfig,
    init_default_cache_worker,
};
use futures_util::StreamExt;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let cache_strat = CacheStrategy::Scraping;

    let (browser, mut handler) = Browser::connect_with_config(
        "http://localhost:9223",
        HandlerConfig {
            // todo: the handler configs from intercept need to move over to prevent conflicts.
            request_intercept: true,
            // rely only on the global memory cache.
            cache_enabled: false,
            ..Default::default()
        },
    )
    .await?;

    init_default_cache_worker().await;

    // Drive the CDP handler
    let handle = tokio::task::spawn(async move {
        loop {
            if let Some(Err(e)) = handler.next().await {
                eprintln!("{:?}", e);
            }
        }
    });

    let page = browser.new_page("about:blank").await?;

    let test_url = "https://spider.cloud/about";

    // setup response → cache listener.
    page.spawn_cache_listener(test_url, None, Some(cache_strat), Some("true".into()), None)
        .await?;

    // ---- First run (cold) ----
    let start_first = Instant::now();

    println!("Attempting first navigation");

    page.goto(test_url).await?;

    let html = page.wait_for_navigation().await?.content().await?;

    let dur_first = start_first.elapsed();

    println!(
        "First (cold) navigation to {} took: {:?}",
        test_url, dur_first
    );

    tokio::time::sleep(Duration::from_secs(5)).await;

    // allow allow even resources that should not be cached.
    let cache_policy = BasicCachePolicy::AllowStale;

    // ---- Second run (warm, via cache interceptor) ----
    // enable fetch → cache interceptor before second navigation

    let start_second = Instant::now();

    println!("Attempting second navigation");

    let html2 = page
        .goto_with_cache_remote_intercept_enabled(
            test_url,
            None,
            Some(cache_policy.clone()),
            Some(cache_strat),
            None,
            None,
        )
        .await?
        .wait_for_navigation()
        .await?
        .content()
        .await?;

    let dur_second = start_second.elapsed();

    println!(
        "Second (warm) navigation to {} took: {:?}",
        test_url, dur_second
    );

    // Compare HTML to ensure we served the same content
    // assert_eq!(html, html2, "HTML from cold vs warm run differ!");

    // Print speedup ratio
    if dur_second.as_millis() > 0 {
        let speedup = dur_first.as_secs_f64() / dur_second.as_secs_f64();
        println!(
            "Warm run speedup: {:.2}x (first: {:?}, second: {:?})",
            speedup, dur_first, dur_second
        );
    } else {
        println!(
            "Second run duration was extremely small (<= 1ms); \
             treat as effectively instant. First: {:?}",
            dur_first
        );
    }

    // assert!(
    //     dur_second < dur_first,
    //     "Warm run was not faster (first: {:?}, second: {:?})",
    //     dur_first,
    //     dur_second
    // );

    println!("Main size: {:?}", html.len());
    println!("Cached size: {:?}", html2.len());

    handle.await?;
    Ok(())
}
