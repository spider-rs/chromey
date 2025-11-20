use chromiumoxide::browser::Browser;
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (browser, mut handler) = Browser::connect("http://localhost:9222").await?;

    let handle = tokio::task::spawn(async move {
        loop {
            let _ = handler.next().await.unwrap();
        }
    });

    let target_url = "https://en.wikipedia.org";
    let page = browser.new_page(target_url).await?;

    page.goto(target_url).await?;

    let html = page.wait_for_navigation().await?.content().await?;
    println!("{:?}", html);

    handle.await?;
    Ok(())
}
