//! Remote cache seed bound and the listener's site key contract.
//!
//! No Chrome and no cache server needed: the seed tests use a local socket
//! that accepts and never answers, which is what a blackholed cache node
//! looks like to the client.
//!
//!   cargo test --test remote_cache_seed --features cache
#![cfg(feature = "_cache")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::cache::manager::{is_site_key, listener_site_key, site_key_for_target_url};
use chromiumoxide::cache::remote::{
    get_cache_site, remote_seed_timeout, set_remote_seed_timeout, set_skip_remote_seed,
    skip_remote_seed, DEFAULT_REMOTE_SEED_TIMEOUT_MS,
};

#[test]
fn listener_key_hashes_a_url_once() {
    let url = "https://example.com/a?b=1#frag";
    let key = site_key_for_target_url(url, Some("user:pass"), Some("us"));
    assert_eq!(listener_site_key(url, Some("user:pass"), Some("us")), key);
}

#[test]
fn listener_key_passes_a_site_key_through() {
    let key = site_key_for_target_url("https://example.com/", None, Some("ns"));
    assert!(is_site_key(&key));
    // Same key whatever auth/namespace ride along: they are already in it.
    assert_eq!(listener_site_key(&key, None, None), key);
    assert_eq!(
        listener_site_key(&key, Some("user:pass"), Some("other")),
        key
    );
    // And it is the key seed_cache reads for the same URL.
    assert_ne!(
        site_key_for_target_url(&key, None, Some("ns")),
        key,
        "rehashing changes the key, which was the bug"
    );
}

#[test]
fn is_site_key_is_strict() {
    let key = "0123456789abcdef".repeat(4);
    assert!(is_site_key(&key));
    assert!(!is_site_key(&key.to_uppercase()));
    assert!(!is_site_key(&key[..63]));
    assert!(!is_site_key(&format!("{key}0")));
    assert!(!is_site_key("https://example.com/"));
    assert!(!is_site_key(&"g".repeat(64)));
}

/// A server that accepts connections and never writes a byte.
async fn blackhole() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(s);
        }
    });
    (format!("http://{addr}"), accepted)
}

// One test owns the process-wide seed overrides so parallel tests cannot
// race on them.
#[tokio::test]
async fn seed_is_bounded_and_skippable() {
    // Default when nothing is set (the env vars are not set under test).
    if std::env::var("CHROMEY_REMOTE_CACHE_SEED_TIMEOUT_MS").is_err() {
        assert_eq!(
            remote_seed_timeout(),
            Some(Duration::from_millis(DEFAULT_REMOTE_SEED_TIMEOUT_MS))
        );
    }
    if std::env::var("CHROMEY_REMOTE_CACHE_SKIP_SEED").is_err() {
        assert!(!skip_remote_seed());
    }

    let (base, accepted) = blackhole().await;

    // Bounded: a server that never answers costs the timeout, not forever.
    set_remote_seed_timeout(Some(Duration::from_millis(200)));
    let started = Instant::now();
    get_cache_site("https://example.com/", None, Some(&base), None).await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(150),
        "returned early: {took:?}"
    );
    assert!(
        took < Duration::from_secs(3),
        "seed was not bounded: {took:?}"
    );
    assert!(
        accepted.load(Ordering::SeqCst) >= 1,
        "seed never reached the server"
    );

    // Skipped: no request at all.
    let before = accepted.load(Ordering::SeqCst);
    set_skip_remote_seed(Some(true));
    assert!(skip_remote_seed());
    let started = Instant::now();
    get_cache_site("https://example.com/other", None, Some(&base), None).await;
    assert!(started.elapsed() < Duration::from_millis(100));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(accepted.load(Ordering::SeqCst), before);

    // Zero removes the bound; None goes back to the env default.
    set_remote_seed_timeout(Some(Duration::ZERO));
    assert_eq!(remote_seed_timeout(), None);
    set_remote_seed_timeout(None);
    set_skip_remote_seed(None);
}
