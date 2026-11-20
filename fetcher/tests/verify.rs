use chromey_fetcher::{BrowserKind, Platform, Revision, CURRENT_REVISION};

// Check if the chosen chromium revision has a build available for all platforms.
#[test]
fn verify_chromium_revision_available() {
    let kind = BrowserKind::Chromium;
    let build_info = chromey_fetcher::BuildInfo::revision(CURRENT_REVISION);
    let host = chromey_fetcher::BrowserHost::current(BrowserKind::Chromium);

    for platform in Platform::all() {
        let url = kind.download_url(*platform, &build_info, &host);
        let res = ureq::head(&url).call();

        if res.is_err() {
            panic!(
                "Revision {} is not available for {:?}",
                CURRENT_REVISION, platform
            );
        }
    }
}

// Check if Chrome for Testing Stable has builds available for all platforms.
#[test]
fn verify_chrome_for_testing_available() {
    let kind = BrowserKind::Chrome;
    // Use a known stable version
    let build_info = chromey_fetcher::BuildInfo::version("133.0.6943.126".to_string());
    let host = chromey_fetcher::BrowserHost::current(BrowserKind::Chrome);

    for platform in Platform::all() {
        let url = kind.download_url(*platform, &build_info, &host);
        let res = ureq::head(&url).call();

        if res.is_err() {
            panic!(
                "Chrome for Testing version {} is not available for {:?}: url={}",
                build_info.id, platform, url
            );
        }
    }
}

// Check if Chrome Headless Shell has builds available for all platforms.
#[test]
fn verify_chrome_headless_shell_available() {
    let kind = BrowserKind::ChromeHeadlessShell;
    let build_info = chromey_fetcher::BuildInfo::version("133.0.6943.126".to_string());
    let host = chromey_fetcher::BrowserHost::current(BrowserKind::ChromeHeadlessShell);

    for platform in Platform::all() {
        let url = kind.download_url(*platform, &build_info, &host);
        let res = ureq::head(&url).call();

        if res.is_err() {
            panic!(
                "Chrome Headless Shell version {} is not available for {:?}: url={}",
                build_info.id, platform, url
            );
        }
    }
}

// Verify the Chrome for Testing JSON API is reachable and returns valid data.
#[tokio::test]
async fn verify_stable_channel_resolution() {
    let host = chromey_fetcher::BrowserHost::current(BrowserKind::Chrome);
    let url = format!(
        "{}chrome-for-testing/last-known-good-versions.json",
        host.metadata
    );
    let res: serde_json::Value = reqwest::get(&url)
        .await
        .expect("Failed to fetch last-known-good-versions.json")
        .json()
        .await
        .expect("Failed to parse JSON");

    assert!(
        res["channels"]["Stable"]["version"].as_str().is_some(),
        "Stable channel version not found in API response"
    );
}

#[ignore]
#[test]
fn find_revision_available() {
    let min = 1355000; // Enter the minimum revision
    let max = 1458586; // Enter the maximum revision

    let kind = BrowserKind::Chromium;
    let host = chromey_fetcher::BrowserHost::current(BrowserKind::Chromium);

    'outer: for revision in (min..max).rev() {
        println!("Checking revision {}", revision);
        let build_info = chromey_fetcher::BuildInfo::revision(Revision::from(revision));

        for platform in Platform::all() {
            let url = kind.download_url(*platform, &build_info, &host);
            let res = ureq::head(&url).call();

            if res.is_err() {
                println!("Revision {} is not available for {:?}", revision, platform);
                continue 'outer;
            }
        }

        println!("Found revision {}", revision);
        break;
    }
}
