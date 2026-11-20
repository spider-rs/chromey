<p align="center">
  <img src="https://raw.githubusercontent.com/spider-rs/chromey/main/assets/banner.svg" alt="chromey, Chrome automation for Rust" width="720">
</p>

# chromey

[![Crates.io](https://img.shields.io/crates/v/chromey.svg)](https://crates.io/crates/chromey)
[![Documentation](https://docs.rs/chromey/badge.svg)](https://docs.rs/chromey)
[![License](https://img.shields.io/crates/l/chromey.svg)](#license)

Chrome automation for Rust that moves like an avalanche. It starts with one WebSocket and
carries every page, request and CDP command down the same path.

chromey does the job Puppeteer does, in async Rust on tokio. Launch Chrome or connect to a
running one, then click, type, intercept requests, take screenshots and print PDFs. The
whole Chrome DevTools Protocol is generated as typed Rust, so any command Chrome accepts
is one `page.execute` call away.

## Quick start

```toml
[dependencies]
chromey = "3"
futures-util = "0.3"
tokio = { version = "1", features = ["full"] }
```

```rust
use chromey::{Browser, BrowserConfig};
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut browser, mut handler) = Browser::launch(BrowserConfig::builder().build()?).await?;

    // The handler routes every CDP message. Keep it running.
    let handle = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    });

    let page = browser.new_page("https://en.wikipedia.org").await?;

    page.find_element("input#searchInput")
        .await?
        .click()
        .await?
        .type_str("Rust programming language")
        .await?
        .press_key("Enter")
        .await?;

    let html = page.wait_for_navigation().await?.content().await?;
    println!("{} bytes", html.len());

    browser.close().await?;
    let _ = handle.await;
    Ok(())
}
```

To drive a browser that is already running, swap the launch line for
`Browser::connect("http://localhost:9222").await?`.

## Why it is fast

Most of the time in a CDP client goes to parsing JSON and waiting on a shared loop.
chromey spends its effort there.

- `sonic-rs` parses CDP messages with SIMD. The `simd` feature is on by default.
- `memchr` and `aho-corasick` run the per-request network blocking checks.
- The `parallel-handler` feature gives each page its own task behind one router, so a
  slow page does not hold up the others.
- `Runtime.releaseObject` calls go out in batches from a background worker that takes no
  lock on the hot path.
- A short-lived DNS cache lets a reconnect to a hostname endpoint skip resolution.
- On Linux, the `io_uring` feature moves file I/O and the first TCP connect to io_uring.
- The `cache` features answer repeat requests from disk or memory before they reach the
  network.
- Navigation and browser requests carry deadlines that keep firing under backpressure.

The benches in [`benches/`](benches) cover the handler, parsing and cache paths. Most of
them run without a Chrome process. Run them with `cargo bench`.

## Coming from Puppeteer

| Puppeteer | chromey |
|---|---|
| `puppeteer.launch()` | `Browser::launch(BrowserConfig::builder().build()?)` |
| `puppeteer.connect()` | `Browser::connect(url)` |
| `browser.newPage()` | `browser.new_page(url)` |
| `page.goto(url)` | `page.goto(url)` |
| `page.$(selector)` | `page.find_element(selector)` |
| `page.$$(selector)` | `page.find_elements(selector)` |
| `page.waitForSelector(selector)` | `page.wait_for_selector(selector, timeout)` |
| `page.waitForNavigation()` | `page.wait_for_navigation()` |
| `page.evaluate(js)` | `page.evaluate(js)` |
| `page.type(selector, text)` | `element.type_str(text)` |
| `page.content()` | `page.content()` |
| `page.screenshot()` | `page.screenshot(params)` |
| `page.pdf()` | `page.pdf(params)` |
| `page.setRequestInterception(true)` | `page.set_request_interception(true)` with `page.event_listener::<EventRequestPaused>()` |
| `page.exposeFunction(name, fn)` | `page.expose_function(name, js)` |
| `client.send(method, params)` | `page.execute(params)` |

## Features

Turn these on with Cargo feature flags.

| Flag | What it does |
|---|---|
| `simd` | SIMD parsing of CDP messages. Default. |
| `parallel-handler` | One task per page behind a router. |
| `adblock` | Network and cosmetic ad blocking. |
| `adblock_easylist` | Bundles the EasyList filter lists. |
| `firewall-default` / `firewall-rustls` | Blocks requests by domain or pattern. |
| `cache` / `cache_mem` | HTTP response cache on disk or in memory. |
| `_fetcher-native-tokio` / `_fetcher-rusttls-tokio` | Downloads Chrome for Testing. |
| `io_uring` | io_uring file I/O and connect on Linux. |
| `serde_stacker` | Parses deeply nested CDP payloads. |

## Download Chrome

```rust
use std::path::Path;
use chromey::browser::BrowserConfig;
use chromey::fetcher::{BrowserFetcher, BrowserFetcherOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let download_path = Path::new("./chrome");
    tokio::fs::create_dir_all(&download_path).await?;

    let fetcher = BrowserFetcher::new(
        BrowserFetcherOptions::builder()
            .with_path(&download_path)
            .build()?,
    );
    let info = fetcher.fetch().await?;

    let _config = BrowserConfig::builder()
        .chrome_executable(info.executable_path)
        .build()?;

    Ok(())
}
```

This needs `_fetcher-native-tokio` or `_fetcher-rusttls-tokio`.

## Any CDP command

`Page::execute` runs any command in the protocol. Most built-in methods are thin wrappers
over it.

```rust
pub async fn pdf(&self, params: PrintToPdfParams) -> Result<Vec<u8>> {
    let res = self.execute(params).await?;
    Ok(base64::decode(&res.data)?)
}
```

The generated types live under `chromey::cdp`. Browse the protocol at
[vanilla.aslushnikov.com](https://vanilla.aslushnikov.com/).

## Examples

The [`examples/`](examples) directory has runnable programs for request interception,
screenshots, PDFs, cookies, console logs, incognito contexts, the HTTP cache and
connecting to a remote browser.

```sh
cargo run --example screenshot
```

## Upgrading from 2.x

Version 3 renames the library from `chromiumoxide` to `chromey`, so the import matches the
crate name. Replace `use chromiumoxide::` with `use chromey::` and you are done.

To upgrade without touching your imports, rename the dependency instead.

```toml
[dependencies]
chromiumoxide = { package = "chromey", version = "3" }
```

The four support crates moved too. They are now `chromey_types`, `chromey_pdl`,
`chromey_cdp` and `chromey_fetcher`. See [CHANGELOG.md](CHANGELOG.md) for the full list.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

## Acknowledgments

chromey began as a fork of [chromiumoxide](https://github.com/mattsse/chromiumoxide) by Matthias Seitz.
