// RUST_LOG=debug cargo run --example http_cache --features="cache"
use chromiumoxide::{
    browser::Browser,
    cache::{BasicCachePolicy, CacheStrategy},
    handler::HandlerConfig,
};
use futures::StreamExt;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let cache_strat = CacheStrategy::Scraping;

    let (browser, mut handler) = Browser::connect_with_config(
        "http://localhost:9223",
        HandlerConfig {
            // todo: the handler configs from intercept need to move over to prevent conflicts.
            request_intercept: false,
            // rely only on the global memory cache.
            cache_enabled: false,
            ..Default::default()
        },
    )
    .await?;

    // Drive the CDP handler
    let handle = tokio::task::spawn(async move {
        loop {
            let _ = handler.next().await.unwrap();
        }
    });

    let page = browser.new_page("about:blank").await?;

    // setup response → cache listener.
    page.spawn_cache_listener("spider.cloud", None, Some(cache_strat), Some("true".into()))
        .await?;

    let test_url = "https://spider.cloud";

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

    // allow allow even resources that should not be cached.
    let cache_policy = BasicCachePolicy::AllowStale;

    page.spawn_cache_intercepter(None, Some(cache_policy), Some(cache_strat))
        .await?;

    tokio::time::sleep(Duration::from_secs(4)).await;

    // ---- Second run (warm, via cache interceptor) ----

    // enable fetch → cache interceptor before second navigation

    let start_second = Instant::now();

    println!("Attempting second navigation");

    page.goto_with_cache_fast_seed(test_url, None, None).await?;

    let html2 = page.wait_for_navigation().await?.content().await?;

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
    //     dur_second * 3 / 2 < dur_first,
    //     "Warm run was not at least 1.5x faster (first: {:?}, second: {:?})",
    //     dur_first,
    //     dur_second
    // );

    println!("Main size: {:?}", html.len());
    println!("Cached size: {:?}", html2.len());

    handle.await?;
    Ok(())
}
