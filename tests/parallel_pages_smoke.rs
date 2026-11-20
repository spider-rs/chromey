//! Smoke test that the CDP mock + chromey `Browser::connect` round-trip works
//! and that N pages can issue commands concurrently. Used to validate the
//! bench harness — does *not* assert performance.

#[path = "support/cdp_mock.rs"]
mod cdp_mock;

use std::sync::Arc;
use std::time::Duration;

use chromey::handler::HandlerConfig;
use chromey::Browser;
use chromey_cdp::cdp::js_protocol::runtime::EvaluateParams;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mock_round_trip_and_n_pages_issue_commands() {
    let mock = cdp_mock::CdpMock::spawn().await;
    let url = mock.ws_url();

    let cfg = HandlerConfig {
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let (browser, handler) = Browser::connect_with_config(url, cfg)
        .await
        .expect("connect to mock");
    let _h = tokio::spawn(handler.run());
    let browser = Arc::new(browser);

    const PAGES: usize = 4;
    const CMDS_PER_PAGE: usize = 8;

    // Create N pages in parallel.
    let mut create_tasks = Vec::with_capacity(PAGES);
    for _ in 0..PAGES {
        let b = browser.clone();
        create_tasks.push(tokio::spawn(async move {
            b.new_page("about:blank").await.expect("new_page")
        }));
    }
    let pages = futures_util::future::join_all(create_tasks)
        .await
        .into_iter()
        .map(|r| r.expect("join"))
        .collect::<Vec<_>>();

    // Each page issues CMDS_PER_PAGE Runtime.evaluate calls in parallel.
    let mut cmd_tasks = Vec::with_capacity(PAGES * CMDS_PER_PAGE);
    for page in &pages {
        for i in 0..CMDS_PER_PAGE {
            let p = page.clone();
            cmd_tasks.push(tokio::spawn(async move {
                p.execute(EvaluateParams::new(format!("'cmd-{i}'")))
                    .await
                    .map(|_| ())
            }));
        }
    }

    let results = futures_util::future::join_all(cmd_tasks).await;
    let mut ok = 0usize;
    for r in results {
        if r.expect("join").is_ok() {
            ok += 1;
        }
    }
    assert_eq!(
        ok,
        PAGES * CMDS_PER_PAGE,
        "all parallel commands across {PAGES} pages should succeed"
    );

    drop(browser);
}
