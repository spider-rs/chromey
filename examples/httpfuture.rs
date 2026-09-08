use futures_util::StreamExt;
use futures_util::TryFutureExt;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (browser, mut handler) =
        Browser::launch(BrowserConfig::builder().with_head().build()?).await?;

    let handle = tokio::task::spawn(async move {
        loop {
            let _ = handler.next().await.unwrap();
        }
    });

    let target_url = "https://hbo.com";

    let page = browser.new_page(target_url).await?;

    let _response1 = page
        .navigate_http_future(NavigateParams {
            url: target_url.to_string(),
            transition_type: None,
            frame_id: None,
            referrer: None,
            referrer_policy: None,
            timeout: None,
        })?
        .and_then(|request| async { Ok(request.map(|r| r.response.clone())) })
        .await?;

    let _html = page.wait_for_navigation().await?.content().await?;

    // println!("{:?}", _html);

    handle.await?;
    Ok(())
}
