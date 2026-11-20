//! End-to-end test for the remote cache dump → seed round-trip.
//!
//! Spins up a minimal mock cache server (same API as index_cache_server)
//! and verifies that chromey can dump content and retrieve it correctly.
//!
//! Run with:
//!   cargo test --features="cache" --test cache_round_trip

#![cfg(feature = "_cache")]

use base64::engine::general_purpose;
use base64::Engine as _;
use chromey::cache::dump_remote::{enqueue, init_remote_dump_worker, DumpJob};
use chromey::cache::manager::{
    create_cache_key_raw, put_hybrid_cache, site_key_for_target_url,
};
use chromey::cache::remote::{
    dump_to_remote_cache_parts, get_cache_site, get_session_cache_item, LOCAL_SESSION_CACHE,
};
use chromey::http::{HttpResponse, HttpVersion};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Stored entry on the mock server, keyed by site_key.
type MockStore = Arc<Mutex<HashMap<String, Vec<Value>>>>;

/// Start a mock cache server on a random port.
/// Returns (address, join_handle).
async fn start_mock_server() -> (String, MockStore, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{}", addr);

    let store: MockStore = Arc::new(Mutex::new(HashMap::new()));
    let store_clone = store.clone();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };

            let store = store_clone.clone();

            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }

                let request = String::from_utf8_lossy(&buf[..n]).to_string();

                // Parse request line
                let first_line = request.lines().next().unwrap_or("");
                let parts: Vec<&str> = first_line.split_whitespace().collect();
                if parts.len() < 2 {
                    return;
                }

                let method = parts[0];
                let path = parts[1];

                // Extract x-cache-site header
                let cache_site = request
                    .lines()
                    .find(|l| l.to_lowercase().starts_with("x-cache-site:"))
                    .map(|l| l.split_once(':').unwrap().1.trim().to_string())
                    .unwrap_or_default();

                if method == "POST" && path == "/cache/index" {
                    // Find body (after \r\n\r\n)
                    if let Some(body_start) = request.find("\r\n\r\n") {
                        let body = &request[body_start + 4..];

                        if let Ok(payload) = serde_json::from_str::<Value>(body) {
                            let mut s = store.lock().await;
                            s.entry(cache_site).or_default().push(payload);
                        }
                    }

                    let response = "HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: 7\r\nConnection: close\r\n\r\nIndexed";
                    let _ = stream.write_all(response.as_bytes()).await;
                } else if method == "GET" && path.starts_with("/cache/site/") {
                    let site_key = &path["/cache/site/".len()..];

                    let s = store.lock().await;
                    let entries = s.get(site_key).cloned().unwrap_or_default();

                    let body = serde_json::to_string(&entries).unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                } else if method == "GET" && path.starts_with("/cache/resource/") {
                    let resource_key = &path["/cache/resource/".len()..];

                    let s = store.lock().await;

                    // Search all entries for matching resource_key
                    let mut found = None;
                    for entries in s.values() {
                        for entry in entries {
                            if entry.get("resource_key").and_then(|v| v.as_str())
                                == Some(resource_key)
                            {
                                found = Some(entry.clone());
                                break;
                            }
                        }
                        if found.is_some() {
                            break;
                        }
                    }

                    if let Some(entry) = found {
                        let body = serde_json::to_string(&entry).unwrap();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else {
                        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                } else {
                    let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            });
        }
    });

    (base, store, handle)
}

#[tokio::test]
async fn test_dump_and_seed_round_trip() {
    let (base_url, store, _handle) = start_mock_server().await;

    let test_url = "https://dump-seed.example.com/page";
    let auth: Option<&str> = None;
    let method = "GET";

    let cache_key = create_cache_key_raw(test_url, Some(method), auth);
    let cache_site = site_key_for_target_url(test_url, auth, None);

    let body = b"<html><head><title>Test</title></head><body><h1>Hello World</h1></body></html>";
    let status: u16 = 200;

    let mut request_headers = HashMap::new();
    request_headers.insert("accept".to_string(), "text/html".to_string());

    let mut response_headers = HashMap::new();
    response_headers.insert(
        "content-type".to_string(),
        "text/html; charset=utf-8".to_string(),
    );

    let http_version = spider_remote_cache::HttpVersion::Http11;

    // --- Step 1: Dump to the mock server ---
    dump_to_remote_cache_parts(
        &cache_key,
        &cache_site,
        test_url,
        body,
        method,
        status,
        &request_headers,
        &response_headers,
        &http_version,
        Some(&base_url),
    )
    .await;

    // Give the server a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // --- Step 2: Verify the mock server received the payload ---
    {
        let s = store.lock().await;
        let entries = s.get(&cache_site).expect("no entries for cache_site");
        assert_eq!(entries.len(), 1, "expected exactly 1 entry");

        let entry = &entries[0];

        assert_eq!(entry["resource_key"].as_str().unwrap(), cache_key);
        assert_eq!(entry["url"].as_str().unwrap(), test_url);
        assert_eq!(entry["method"].as_str().unwrap(), method);
        assert_eq!(entry["status"].as_u64().unwrap(), status as u64);
        assert_eq!(entry["http_version"].as_str().unwrap(), "Http11");

        // Verify body is correctly base64 encoded
        let body_base64 = entry["body_base64"].as_str().unwrap();
        let decoded_body = general_purpose::STANDARD.decode(body_base64).unwrap();
        assert_eq!(decoded_body, body, "body round-trip failed through base64");

        // Verify headers
        let req_h = entry["request_headers"].as_object().unwrap();
        assert_eq!(req_h["accept"].as_str().unwrap(), "text/html");

        let resp_h = entry["response_headers"].as_object().unwrap();
        assert_eq!(
            resp_h["content-type"].as_str().unwrap(),
            "text/html; charset=utf-8"
        );
    }

    // --- Step 3: Clear this test's session cache entries and seed from the mock server ---
    LOCAL_SESSION_CACHE.remove(&cache_site);

    get_cache_site(test_url, auth, Some(&base_url), None).await;

    // --- Step 4: Verify the session cache was populated ---
    let session_key = format!("{}:{}", method, test_url);
    let cached = get_session_cache_item(&cache_site, &session_key);

    assert!(
        cached.is_some(),
        "session cache should have the entry after seeding"
    );

    let (http_response, _cache_policy) = cached.unwrap();

    assert_eq!(
        http_response.body, body,
        "body should match after seed round-trip"
    );
    assert_eq!(http_response.status, status);
    assert_eq!(
        http_response.headers.get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    assert_eq!(http_response.url.as_str(), test_url);
}

#[tokio::test]
async fn test_dump_multiple_resources_same_site() {
    let (base_url, store, _handle) = start_mock_server().await;

    let page_url = "https://multi-res.example.com/index";
    let css_url = "https://multi-res.example.com/style.css";
    let js_url = "https://multi-res.example.com/app.js";
    let auth: Option<&str> = None;
    let method = "GET";

    let cache_site = site_key_for_target_url(page_url, auth, None);

    // Dump the HTML page
    let html_body = b"<html><body>Page content here</body></html>";
    let html_key = create_cache_key_raw(page_url, Some(method), auth);
    let mut html_resp_headers = HashMap::new();
    html_resp_headers.insert("content-type".to_string(), "text/html".to_string());

    dump_to_remote_cache_parts(
        &html_key,
        &cache_site,
        page_url,
        html_body,
        method,
        200,
        &HashMap::new(),
        &html_resp_headers,
        &spider_remote_cache::HttpVersion::Http11,
        Some(&base_url),
    )
    .await;

    // Dump the CSS resource
    let css_body = b"body { color: red; }";
    let css_key = create_cache_key_raw(css_url, Some(method), auth);
    let mut css_resp_headers = HashMap::new();
    css_resp_headers.insert("content-type".to_string(), "text/css".to_string());

    dump_to_remote_cache_parts(
        &css_key,
        &cache_site,
        css_url,
        css_body,
        method,
        200,
        &HashMap::new(),
        &css_resp_headers,
        &spider_remote_cache::HttpVersion::Http11,
        Some(&base_url),
    )
    .await;

    // Dump the JS resource
    let js_body = b"console.log('hello');";
    let js_key = create_cache_key_raw(js_url, Some(method), auth);
    let mut js_resp_headers = HashMap::new();
    js_resp_headers.insert(
        "content-type".to_string(),
        "application/javascript".to_string(),
    );

    dump_to_remote_cache_parts(
        &js_key,
        &cache_site,
        js_url,
        js_body,
        method,
        200,
        &HashMap::new(),
        &js_resp_headers,
        &spider_remote_cache::HttpVersion::Http11,
        Some(&base_url),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify all 3 resources are stored under the same site key
    {
        let s = store.lock().await;
        let entries = s.get(&cache_site).expect("no entries for cache_site");
        assert_eq!(entries.len(), 3, "expected 3 entries for the site");
    }

    // Seed from remote and verify all 3 are in session cache
    LOCAL_SESSION_CACHE.remove(&cache_site);
    get_cache_site(page_url, auth, Some(&base_url), None).await;

    // The page document should be in both CACACHE and session cache
    let page_session_key = format!("{}:{}", method, page_url);
    let page_cached = get_session_cache_item(&cache_site, &page_session_key);
    assert!(page_cached.is_some(), "page should be in session cache");
    assert_eq!(page_cached.unwrap().0.body, html_body);

    // Sub-resources should be in session cache
    let css_session_key = format!("{}:{}", method, css_url);
    let css_cached = get_session_cache_item(&cache_site, &css_session_key);
    assert!(css_cached.is_some(), "CSS should be in session cache");
    assert_eq!(css_cached.unwrap().0.body, css_body);

    let js_session_key = format!("{}:{}", method, js_url);
    let js_cached = get_session_cache_item(&cache_site, &js_session_key);
    assert!(js_cached.is_some(), "JS should be in session cache");
    assert_eq!(js_cached.unwrap().0.body, js_body);
}

#[tokio::test]
async fn test_dump_with_auth() {
    let (base_url, store, _handle) = start_mock_server().await;

    let test_url = "https://auth-test.example.com/protected";
    let auth = Some("bearer_token_123");
    let method = "GET";

    let cache_key = create_cache_key_raw(test_url, Some(method), auth);
    let cache_site = site_key_for_target_url(test_url, auth, None);

    let body = b"protected content here";
    let mut response_headers = HashMap::new();
    response_headers.insert("content-type".to_string(), "text/plain".to_string());

    dump_to_remote_cache_parts(
        &cache_key,
        &cache_site,
        test_url,
        body,
        method,
        200,
        &HashMap::new(),
        &response_headers,
        &spider_remote_cache::HttpVersion::H2,
        Some(&base_url),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify the payload includes auth in the cache_key
    {
        let s = store.lock().await;
        let entries = s.get(&cache_site).expect("no entries");
        assert_eq!(entries.len(), 1);

        let entry = &entries[0];
        // resource_key should contain auth
        let resource_key = entry["resource_key"].as_str().unwrap();
        assert!(
            resource_key.contains("bearer_token_123"),
            "resource_key should include auth: {}",
            resource_key
        );
        assert_eq!(entry["http_version"].as_str().unwrap(), "H2");
    }

    // Seed and verify
    LOCAL_SESSION_CACHE.remove(&cache_site);
    get_cache_site(test_url, auth, Some(&base_url), None).await;

    let session_key = format!("{}:{}", method, test_url);
    let cached = get_session_cache_item(&cache_site, &session_key);
    assert!(cached.is_some(), "session cache should have the auth entry");
    assert_eq!(cached.unwrap().0.body, body);
}

#[tokio::test]
async fn test_dump_empty_body_skipped() {
    let (base_url, store, _handle) = start_mock_server().await;

    let test_url = "https://empty-body.example.com/empty";
    let cache_key = create_cache_key_raw(test_url, Some("GET"), None);
    let cache_site = site_key_for_target_url(test_url, None, None);

    // Empty body should be skipped by the server-side check
    // but we test that the dump still works (the server will store it,
    // the client-side put_hybrid_cache would skip it)
    dump_to_remote_cache_parts(
        &cache_key,
        &cache_site,
        test_url,
        b"",
        "GET",
        200,
        &HashMap::new(),
        &HashMap::new(),
        &spider_remote_cache::HttpVersion::Http11,
        Some(&base_url),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The dump function itself doesn't skip empty bodies — it sends them.
    // The filtering happens at the caller level (put_hybrid_cache / handle_single_response).
    // Verify the server still received it (the server stores whatever it gets).
    let s = store.lock().await;
    let empty = vec![];
    let entries = s.get(&cache_site).unwrap_or(&empty);
    // The server should have received it since dump_to_remote_cache_parts doesn't filter
    assert_eq!(
        entries.len(),
        1,
        "dump_to_remote_cache_parts should send even empty bodies"
    );
}

#[tokio::test]
async fn test_cache_key_consistency() {
    // Verify that cache keys generated for dump and retrieval match
    let url = "https://key-test.example.com/path?q=1#frag";
    let auth = Some("token");

    let dump_key = create_cache_key_raw(url, Some("GET"), auth);
    let retrieve_key = create_cache_key_raw(url, None, auth);

    // Both should produce "GET:url:auth" since None defaults to GET
    assert_eq!(
        dump_key, retrieve_key,
        "dump and retrieve cache keys must match"
    );

    // Site keys should be stable
    let site1 = site_key_for_target_url(url, auth, None);
    let site2 = site_key_for_target_url(url, auth, None);
    assert_eq!(site1, site2, "site keys must be deterministic");

    // Different auth produces different keys
    let site_no_auth = site_key_for_target_url(url, None, None);
    assert_ne!(
        site1, site_no_auth,
        "different auth should produce different site keys"
    );
}

#[tokio::test]
async fn test_binary_body_round_trip() {
    let (base_url, _store, _handle) = start_mock_server().await;

    let test_url = "https://binary-test.example.com/image.png";
    let cache_key = create_cache_key_raw(test_url, Some("GET"), None);
    let cache_site = site_key_for_target_url(test_url, None, None);

    // Simulate a binary body (PNG header + random bytes)
    let body: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
        0xFF, 0xFE, 0xFD, 0x00, 0x01, 0x02, 0x03, 0x04, // arbitrary bytes
    ];

    let mut response_headers = HashMap::new();
    response_headers.insert("content-type".to_string(), "image/png".to_string());

    dump_to_remote_cache_parts(
        &cache_key,
        &cache_site,
        test_url,
        &body,
        "GET",
        200,
        &HashMap::new(),
        &response_headers,
        &spider_remote_cache::HttpVersion::Http11,
        Some(&base_url),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Seed back and verify binary content is preserved
    LOCAL_SESSION_CACHE.remove(&cache_site);
    get_cache_site(test_url, None, Some(&base_url), None).await;

    let session_key = format!("GET:{}", test_url);
    let cached = get_session_cache_item(&cache_site, &session_key);
    assert!(
        cached.is_some(),
        "binary content should be in session cache"
    );
    assert_eq!(
        cached.unwrap().0.body,
        body,
        "binary body should survive base64 round-trip"
    );
}

#[tokio::test]
async fn test_dump_worker_queue_end_to_end() {
    let (base_url, store, _handle) = start_mock_server().await;

    // Initialize the dump worker (this is idempotent via OnceCell)
    init_remote_dump_worker(100, 50, 5000).await;

    let test_url = "https://worker-test.example.com/page";
    let cache_key = create_cache_key_raw(test_url, Some("GET"), None);
    let cache_site = site_key_for_target_url(test_url, None, None);

    let body = b"<html><body>Worker test content</body></html>".to_vec();
    let mut response_headers = HashMap::new();
    response_headers.insert("content-type".to_string(), "text/html".to_string());

    let job = DumpJob {
        cache_key: cache_key.clone(),
        cache_site: cache_site.clone(),
        url: test_url.to_string(),
        method: "GET".to_string(),
        status: 200,
        request_headers: HashMap::new(),
        response_headers,
        body: body.clone(),
        http_version: spider_remote_cache::HttpVersion::Http11,
        dump_remote: Some(base_url.clone()),
    };

    // Enqueue the job via the worker
    enqueue(job).await.expect("enqueue should succeed");

    // Wait for the worker to process the job
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the mock server received the payload via the worker
    {
        let s = store.lock().await;
        let entries = s
            .get(&cache_site)
            .expect("worker should have dumped to server");
        assert_eq!(entries.len(), 1, "expected 1 entry from worker");

        let entry = &entries[0];
        assert_eq!(entry["url"].as_str().unwrap(), test_url);
        assert_eq!(entry["resource_key"].as_str().unwrap(), cache_key);

        let body_base64 = entry["body_base64"].as_str().unwrap();
        let decoded = general_purpose::STANDARD.decode(body_base64).unwrap();
        assert_eq!(decoded, body, "worker-dumped body should match");
    }

    // Now seed from the server and verify round-trip through the worker
    LOCAL_SESSION_CACHE.remove(&cache_site);
    get_cache_site(test_url, None, Some(&base_url), None).await;

    let session_key = format!("GET:{}", test_url);
    let cached = get_session_cache_item(&cache_site, &session_key);
    assert!(cached.is_some(), "seeded entry should be in session cache");
    assert_eq!(
        cached.unwrap().0.body,
        body,
        "worker → server → seed round-trip should preserve body"
    );
}

/// Regression test: CachePolicy must use request headers for the request and
/// response headers for the response (not swapped).  Validates that
/// `Cache-Control: max-age=3600` on the *response* is correctly interpreted,
/// producing a non-stale policy.
#[tokio::test]
async fn test_cache_policy_uses_correct_headers() {
    use chromey::cache::manager::put_hybrid_cache;
    use chromey::cache::remote::{
        dump_to_remote_cache_parts, get_session_cache_item, LOCAL_SESSION_CACHE,
    };
    use chromey::http::HttpResponse;

    let (base_url, store, _handle) = start_mock_server().await;

    let test_url = "https://policy-header-test.example.com/page";
    let cache_key = create_cache_key_raw(test_url, Some("GET"), None);
    let cache_site = site_key_for_target_url(test_url, None, None);

    let body = b"<html><body>Cache-Control test</body></html>".to_vec();

    // Response headers include cache-control — this must end up on the
    // *response* side of the CachePolicy, not the request side.
    let mut response_headers = HashMap::new();
    response_headers.insert("content-type".to_string(), "text/html".to_string());
    response_headers.insert("cache-control".to_string(), "max-age=3600".to_string());

    // Request headers include accept — must end up on the *request* side.
    let mut request_headers = HashMap::new();
    request_headers.insert("accept".to_string(), "text/html".to_string());

    let http_response = HttpResponse {
        body: body.clone(),
        headers: response_headers.clone(),
        status: 200,
        url: url::Url::parse(test_url).unwrap(),
        version: HttpVersion::Http11,
    };

    LOCAL_SESSION_CACHE.remove(&cache_site);

    // Use put_hybrid_cache WITHOUT remote dump to test CachePolicy construction.
    // This avoids worker timing issues.
    put_hybrid_cache(
        &cache_key,
        &cache_site,
        http_response,
        "GET",
        request_headers.clone(),
        None, // no remote dump — just local + session cache
    )
    .await;

    // Verify the session cache entry exists and has a non-stale policy
    // (would be stale if cache-control ended up on the wrong side).
    let session_key = format!("GET:{}", test_url);
    let cached = get_session_cache_item(&cache_site, &session_key);
    assert!(
        cached.is_some(),
        "session cache should have the entry after put_hybrid_cache"
    );

    let (_http_res, policy) = cached.unwrap();
    assert!(
        !policy.is_stale(std::time::SystemTime::now()),
        "policy must not be stale — max-age=3600 response header must be on the response side"
    );

    // Separately verify that dump_to_remote_cache_parts preserves headers
    // correctly (direct call, no worker involved).
    dump_to_remote_cache_parts(
        &cache_key,
        &cache_site,
        test_url,
        &body,
        "GET",
        200,
        &request_headers,
        &response_headers,
        &spider_remote_cache::HttpVersion::Http11,
        Some(&base_url),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    {
        let s = store.lock().await;
        let entries = s
            .get(&cache_site)
            .expect("remote server should have the entry");
        assert!(!entries.is_empty());

        let entry = &entries[0];
        let resp_h = entry["response_headers"].as_object().unwrap();
        assert_eq!(
            resp_h.get("cache-control").and_then(|v| v.as_str()),
            Some("max-age=3600"),
            "response headers must be preserved in remote dump"
        );

        let req_h = entry["request_headers"].as_object().unwrap();
        assert_eq!(
            req_h.get("accept").and_then(|v| v.as_str()),
            Some("text/html"),
            "request headers must be preserved in remote dump"
        );
    }
}

/// Regression test: `put_hybrid_cache` on a cold local cache (no existing entry)
/// must still dump to the remote server.  Before the fix, `Ok(None)` from
/// `CACACHE_MANAGER.get()` was treated as "fresh entry" and the dump was skipped.
#[tokio::test]
async fn test_put_hybrid_cache_cold_cache_dumps_to_remote() {
    let (base_url, store, _handle) = start_mock_server().await;

    // Initialize the dump worker so put_hybrid_cache can enqueue
    init_remote_dump_worker(100, 50, 5000).await;

    let test_url = "https://put-cold.example.com/page";
    let cache_key = create_cache_key_raw(test_url, Some("GET"), None);
    let cache_site = site_key_for_target_url(test_url, None, None);

    let body = b"<html><body>Cold cache test content</body></html>".to_vec();
    let mut response_headers = HashMap::new();
    response_headers.insert("content-type".to_string(), "text/html".to_string());

    let http_response = HttpResponse {
        body: body.clone(),
        headers: response_headers.clone(),
        status: 200,
        url: url::Url::parse(test_url).unwrap(),
        version: HttpVersion::Http11,
    };

    // Ensure clean state for this test's cache_site only
    LOCAL_SESSION_CACHE.remove(&cache_site);

    // Call put_hybrid_cache with dump_remote pointing to our mock server
    put_hybrid_cache(
        &cache_key,
        &cache_site,
        http_response,
        "GET",
        HashMap::new(),
        Some(&base_url),
    )
    .await;

    // Wait for the async dump worker to process the job
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the mock server received the payload
    {
        let s = store.lock().await;
        let entries = s.get(&cache_site);
        assert!(
            entries.is_some() && !entries.unwrap().is_empty(),
            "put_hybrid_cache on cold cache MUST dump to remote server"
        );

        let entry = &entries.unwrap()[0];
        assert_eq!(entry["url"].as_str().unwrap(), test_url);

        let body_b64 = entry["body_base64"].as_str().unwrap();
        let decoded = general_purpose::STANDARD.decode(body_b64).unwrap();
        assert_eq!(decoded, body, "dumped body must match original");
    }

    // Also verify session cache was populated by put_hybrid_cache
    let session_key = format!("GET:{}", test_url);
    let cached = get_session_cache_item(&cache_site, &session_key);
    assert!(
        cached.is_some(),
        "put_hybrid_cache should populate session cache"
    );
    assert_eq!(cached.unwrap().0.body, body);
}

/// Test the full end-to-end flow: put_hybrid_cache dumps → seed retrieves → session cache serves.
#[tokio::test]
async fn test_put_hybrid_cache_full_round_trip() {
    let (base_url, _store, _handle) = start_mock_server().await;

    init_remote_dump_worker(100, 50, 5000).await;

    let page_url = "https://roundtrip.example.com/index";
    let cache_key = create_cache_key_raw(page_url, Some("GET"), None);
    let cache_site = site_key_for_target_url(page_url, None, None);

    let body = b"<html><body>Full round trip content</body></html>".to_vec();
    let mut resp_headers = HashMap::new();
    resp_headers.insert("content-type".to_string(), "text/html".to_string());
    resp_headers.insert("x-custom".to_string(), "preserved".to_string());

    let http_response = HttpResponse {
        body: body.clone(),
        headers: resp_headers.clone(),
        status: 200,
        url: url::Url::parse(page_url).unwrap(),
        version: HttpVersion::H2,
    };

    // Clean state for this test
    LOCAL_SESSION_CACHE.remove(&cache_site);

    // Step 1: Dump via put_hybrid_cache
    put_hybrid_cache(
        &cache_key,
        &cache_site,
        http_response,
        "GET",
        HashMap::new(),
        Some(&base_url),
    )
    .await;

    // Wait for dump to complete
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Step 2: Clear this test's session cache to simulate a fresh session
    LOCAL_SESSION_CACHE.remove(&cache_site);

    // Step 3: Seed from remote
    get_cache_site(page_url, None, Some(&base_url), None).await;

    // Step 4: Verify session cache has the entry with correct data
    let session_key = format!("GET:{}", page_url);
    let cached = get_session_cache_item(&cache_site, &session_key);
    assert!(cached.is_some(), "seeded entry should be in session cache");

    let (http_res, _policy) = cached.unwrap();
    assert_eq!(
        http_res.body, body,
        "body must survive dump → remote → seed"
    );
    assert_eq!(http_res.status, 200);
    assert_eq!(http_res.headers.get("content-type").unwrap(), "text/html");
    assert_eq!(
        http_res.headers.get("x-custom").unwrap(),
        "preserved",
        "custom headers must survive round-trip"
    );
}
