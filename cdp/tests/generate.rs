use chromey_cdp::CURRENT_REVISION;
use chromey_pdl::build::Generator;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Resolve `include domains/X.pdl` directives by fetching each referenced file
/// and inlining its contents. Newer versions of the devtools-protocol repository
/// split browser_protocol.pdl into per-domain files under `pdl/domains/`.
fn resolve_pdl_includes(content: &str, base_url: &str) -> String {
    let mut resolved = String::with_capacity(content.len() * 2);
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(path) = trimmed.strip_prefix("include ") {
            let url = format!("{}/{}", base_url, path.trim());
            match ureq::get(&url).call() {
                Ok(resp) => {
                    let domain_content = resp.into_string().unwrap();
                    resolved.push_str(&domain_content);
                    resolved.push('\n');
                }
                Err(e) => {
                    eprintln!("warning: skipping missing include {path}: {e}");
                }
            }
        } else {
            resolved.push_str(line);
            resolved.push('\n');
        }
    }
    resolved
}

/// Check that the generated files are up to date and if not perform the updates.
#[ignore]
#[test]
fn generated_code_is_fresh() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR", "./"));
    let js_proto = env::var("CDP_JS_PROTOCOL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dir.join("js_protocol.pdl"));

    let browser_proto = env::var("CDP_BROWSER_PROTOCOL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dir.join("browser_protocol.pdl"));

    let tmp = tempfile::tempdir().unwrap();
    Generator::default()
        .out_dir(tmp.path())
        .experimental(env::var("CDP_NO_EXPERIMENTAL").is_err())
        .deprecated(env::var("CDP_DEPRECATED").is_ok())
        .compile_pdls(&[js_proto, browser_proto])
        .unwrap();

    let new = fs::read_to_string(tmp.path().join("cdp.rs")).unwrap();
    let src = dir.join("src/cdp.rs");
    let old = fs::read_to_string(&src).unwrap();

    if new != old {
        fs::write(src, new).unwrap();
        println!("generated code in the repository is outdated, updating...");
    }
}

/// Check that the PDL files are up to date
#[test]
fn pdl_is_fresh() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let js_proto = env::var("CDP_JS_PROTOCOL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dir.join("js_protocol.pdl"));

    let browser_proto = env::var("CDP_BROWSER_PROTOCOL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dir.join("browser_protocol.pdl"));

    let base_url = format!(
        "https://raw.githubusercontent.com/ChromeDevTools/devtools-protocol/{}/pdl",
        CURRENT_REVISION
    );

    let js_proto_old = fs::read_to_string(&js_proto).unwrap();
    let js_proto_new = ureq::get(&format!("{}/js_protocol.pdl", base_url))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(js_proto_new.contains("The Chromium Authors"));

    let browser_proto_old = fs::read_to_string(&browser_proto).unwrap();
    let browser_proto_raw = ureq::get(&format!("{}/browser_protocol.pdl", base_url))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(browser_proto_raw.contains("The Chromium Authors"));
    // Resolve `include domains/X.pdl` directives into a single flat file
    let browser_proto_new = resolve_pdl_includes(&browser_proto_raw, &base_url);

    if js_proto_new != js_proto_old || browser_proto_new != browser_proto_old {
        fs::write(js_proto, js_proto_new).unwrap();
        fs::write(browser_proto, browser_proto_new).unwrap();
        println!("pdl in the repository are outdated, updating...");
    }
}
