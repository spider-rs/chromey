// RUST_LOG=debug cargo run --example connect
use chromiumoxide::{
    browser::Browser,
    cdp::browser_protocol::{page::NavigateParams, target::CreateTargetParams},
    handler::HandlerConfig,
};
use futures::StreamExt;

async fn test_connect(target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut conf = HandlerConfig::default();

    conf.request_intercept = true;

    let (mut browser, mut handler) =
        Browser::connect_with_config("http://localhost:9222", conf).await?;

    let handle = tokio::task::spawn(async move {
        loop {
            let _ = handler.next().await.unwrap();
        }
    });

    let browser_context_id = browser
        .start_incognito_context()
        .await?
        .create_browser_context(Default::default())
        .await?;

    let page = browser
        .new_page(CreateTargetParams {
            browser_context_id: Some(browser_context_id),
            url: "about:blank".into(),
            ..Default::default()
        })
        .await?;

    let frame_id = page.mainframe().await?;
    let page = page
        .goto(NavigateParams {
            url: target.into(),
            frame_id: frame_id,
            ..Default::default()
        })
        .await?;
    let html = page.wait_for_navigation().await?.content().await?;

    println!("{:?}", html);

    handle.await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let _ = tokio::join!(
        test_connect("https://www.example.com"),
        test_connect("https://jeffmendez.com"),
    );

    Ok(())
}
