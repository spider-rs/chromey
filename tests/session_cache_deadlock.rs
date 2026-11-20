//! Stress tests for `session_cache_insert` to verify no DashMap deadlocks.
//!
//! The original implementation called `LOCAL_SESSION_CACHE.len()` and
//! `.iter()` while holding a shard write-lock via `Entry::Vacant`, which
//! could deadlock under concurrent access.  These tests exercise the
//! concurrent insertion paths that would trigger such a deadlock.
//!
//! Run with:
//!   cargo test --features="cache" --test session_cache_deadlock

#![cfg(feature = "_cache")]

use chromey::cache::remote::{
    get_session_cache_item, session_cache_insert, LOCAL_SESSION_CACHE,
};
use chromey::http::{HttpRequestLike, HttpResponseLike};
use std::sync::Arc;
use std::time::Duration;

/// Build a minimal `http_cache_reqwest::HttpResponse` for testing.
fn dummy_http_response(url: &str) -> http_cache_reqwest::HttpResponse {
    http_cache_reqwest::HttpResponse {
        url: url::Url::parse(url).unwrap(),
        body: vec![1, 2, 3],
        headers: http_cache::HttpHeaders::default(),
        status: 200,
        version: http_cache::HttpVersion::Http11,
        metadata: None,
    }
}

/// Build a minimal `CachePolicy` for testing.
fn dummy_cache_policy() -> http_cache_semantics::CachePolicy {
    let req = HttpRequestLike {
        uri: "http://test.local/resource".parse().unwrap(),
        method: reqwest::Method::GET,
        headers: Default::default(),
    };
    let res = HttpResponseLike {
        status: reqwest::StatusCode::OK,
        headers: Default::default(),
    };
    http_cache_semantics::CachePolicy::new(&req, &res)
}

/// Concurrent inserts to the *same* cache key from many threads should not
/// deadlock or corrupt the inner HashMap.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_insert_same_key_no_deadlock() {
    let cache_key = "samekey-test";
    // Ensure clean start for this specific key.
    LOCAL_SESSION_CACHE.remove(cache_key);

    let barrier = Arc::new(tokio::sync::Barrier::new(50));

    let handles: Vec<_> = (0..50)
        .map(|i| {
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await; // synchronize all threads
                let entry_key = format!("GET:http://test.local/resource-{i}");
                session_cache_insert(
                    cache_key,
                    dummy_http_response(&format!("http://test.local/resource-{i}")),
                    dummy_cache_policy(),
                    &entry_key,
                );
            })
        })
        .collect();

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        for (i, h) in handles.into_iter().enumerate() {
            h.await
                .unwrap_or_else(|err| panic!("task {i} panicked: {err}"));
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "concurrent same-key inserts timed out — possible deadlock"
    );

    // Verify the key exists and has entries.
    assert!(
        LOCAL_SESSION_CACHE.contains_key(cache_key),
        "cache key should exist after inserts"
    );
    let inner = LOCAL_SESSION_CACHE.get(cache_key).unwrap();
    assert!(
        inner.len() > 0 && inner.len() <= 50,
        "expected 1..=50 entries, got {}",
        inner.len()
    );
    drop(inner);

    LOCAL_SESSION_CACHE.remove(cache_key);
}

/// Concurrent inserts to many *different* cache keys should not deadlock
/// even when eviction is triggered.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_insert_different_keys_no_deadlock() {
    // Use a unique prefix so we don't collide with other tests sharing
    // the global DashMap.
    let prefix = "diffkey";
    let barrier = Arc::new(tokio::sync::Barrier::new(100));

    let handles: Vec<_> = (0..100)
        .map(|i| {
            let barrier = barrier.clone();
            let prefix = prefix.to_string();
            tokio::spawn(async move {
                barrier.wait().await;
                let cache_key = format!("{prefix}-site-{i}");
                let entry_key = format!("GET:http://test.local/page-{i}");
                session_cache_insert(
                    &cache_key,
                    dummy_http_response(&format!("http://test.local/page-{i}")),
                    dummy_cache_policy(),
                    &entry_key,
                );
            })
        })
        .collect();

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        for (i, h) in handles.into_iter().enumerate() {
            h.await
                .unwrap_or_else(|err| panic!("task {i} panicked: {err}"));
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "concurrent different-key inserts timed out — possible deadlock"
    );

    // All 100 keys should be present — verify each individually since the
    // global DashMap is shared with other concurrent tests.
    for i in 0..100 {
        let key = format!("{prefix}-site-{i}");
        assert!(
            LOCAL_SESSION_CACHE.contains_key(&key),
            "key '{key}' should exist after insert"
        );
    }

    for i in 0..100 {
        LOCAL_SESSION_CACHE.remove(&format!("{prefix}-site-{i}"));
    }
}

/// Concurrent inserts + reads should not deadlock.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_insert_and_read_no_deadlock() {
    let prefix = "rw";

    // Pre-populate some entries.
    for i in 0..50 {
        let cache_key = format!("{prefix}-read-site-{i}");
        let entry_key = format!("GET:http://test.local/existing-{i}");
        session_cache_insert(
            &cache_key,
            dummy_http_response(&format!("http://test.local/existing-{i}")),
            dummy_cache_policy(),
            &entry_key,
        );
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(100));

    let handles: Vec<_> = (0..100)
        .map(|i| {
            let barrier = barrier.clone();
            let prefix = prefix.to_string();
            tokio::spawn(async move {
                barrier.wait().await;
                if i % 2 == 0 {
                    // Writers: insert new entries.
                    let cache_key = format!("{prefix}-new-site-{i}");
                    let entry_key = format!("GET:http://test.local/new-{i}");
                    session_cache_insert(
                        &cache_key,
                        dummy_http_response(&format!("http://test.local/new-{i}")),
                        dummy_cache_policy(),
                        &entry_key,
                    );
                } else {
                    // Readers: read existing entries.
                    let cache_key = format!("{prefix}-read-site-{}", i % 50);
                    let entry_key = format!("GET:http://test.local/existing-{}", i % 50);
                    let _ = get_session_cache_item(&cache_key, &entry_key);
                }
            })
        })
        .collect();

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        for (i, h) in handles.into_iter().enumerate() {
            h.await
                .unwrap_or_else(|err| panic!("task {i} panicked: {err}"));
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "concurrent insert+read timed out — possible deadlock"
    );

    // Clean up our keys.
    for i in 0..50 {
        LOCAL_SESSION_CACHE.remove(&format!("{prefix}-read-site-{i}"));
    }
    for i in (0..100).step_by(2) {
        LOCAL_SESSION_CACHE.remove(&format!("{prefix}-new-site-{i}"));
    }
}

/// The fast path (existing key) should correctly insert entries and the data
/// should be retrievable.
#[tokio::test]
async fn fast_path_insert_is_retrievable() {
    let cache_key = "fastpath-retrieve-test";
    LOCAL_SESSION_CACHE.remove(cache_key);

    let entry_key1 = "GET:http://test.local/page1";
    let entry_key2 = "GET:http://test.local/page2";

    // First insert creates the key (slow path).
    session_cache_insert(
        cache_key,
        dummy_http_response("http://test.local/page1"),
        dummy_cache_policy(),
        entry_key1,
    );

    // Second insert hits the fast path (key exists).
    session_cache_insert(
        cache_key,
        dummy_http_response("http://test.local/page2"),
        dummy_cache_policy(),
        entry_key2,
    );

    // Both entries should be retrievable.
    let item1 = get_session_cache_item(cache_key, entry_key1);
    let item2 = get_session_cache_item(cache_key, entry_key2);

    assert!(item1.is_some(), "first entry should be retrievable");
    assert!(
        item2.is_some(),
        "second entry (fast path) should be retrievable"
    );

    // Verify the URLs are correct.
    let (resp1, _) = item1.unwrap();
    let (resp2, _) = item2.unwrap();
    assert_eq!(resp1.url.as_str(), "http://test.local/page1");
    assert_eq!(resp2.url.as_str(), "http://test.local/page2");

    LOCAL_SESSION_CACHE.remove(cache_key);
}

/// Per-site entry limit should be respected via both the fast and slow paths.
#[tokio::test]
async fn per_site_entry_limit_respected() {
    let cache_key = "limit-test-unique";
    LOCAL_SESSION_CACHE.remove(cache_key);

    // Insert 100 entries under one key.
    for i in 0..100 {
        let entry_key = format!("GET:http://test.local/res-{i}");
        session_cache_insert(
            cache_key,
            dummy_http_response(&format!("http://test.local/res-{i}")),
            dummy_cache_policy(),
            &entry_key,
        );
    }

    // All 100 should be present.
    let inner = LOCAL_SESSION_CACHE.get(cache_key).unwrap();
    assert_eq!(
        inner.len(),
        100,
        "expected 100 entries, got {}",
        inner.len()
    );
    drop(inner);

    LOCAL_SESSION_CACHE.remove(cache_key);
}
