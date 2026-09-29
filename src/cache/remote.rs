use base64::engine::general_purpose;
use base64::prelude::Engine as _;
use hashbrown::HashMap;
use http_cache_reqwest::CacheManager;
use http_cache_semantics::CachePolicy;
use http_global_cache::CACACHE_MANAGER;
use lazy_static::lazy_static;
use reqwest::Method;
use reqwest::StatusCode;
use url::Url;

use crate::cache::manager::site_key_for_target_url;
use crate::http::{convert_headers, HttpRequestLike, HttpResponseLike};

// Re-export from the shared crate so existing call sites keep working.
pub use spider_remote_cache::{
    build_payload, dump_batch_to_remote as dump_batch_to_remote_cache,
    dump_to_remote as dump_to_remote_cache_parts, get_client, get_endpoint,
    resolve_base_url as resolve_remote_base_url, set_client, set_endpoint, HybridCachePayload,
};

lazy_static! {
    /// The local session cache per run cleared.
    pub static ref LOCAL_SESSION_CACHE: dashmap::DashMap<String, HashMap<String, (http_cache_reqwest::HttpResponse, CachePolicy)>> = dashmap::DashMap::new();
    /// URLs currently being streamed via `Fetch.takeResponseBodyAsStream`.
    /// Checked by the `Network.responseReceived` listener to avoid a
    /// redundant `getResponseBody` call for the same resource.
    pub(crate) static ref PENDING_STREAM_URLS: dashmap::DashSet<String> = dashmap::DashSet::new();
}

/// Install chromey's configured `reqwest::Client` as the remote cache
/// client. Idempotent; only the first call does anything.
///
/// `spider_remote_cache` keeps the first client it sees for the life of the
/// process and otherwise builds its own default, which in 0.3 has no
/// timeout. This runs at browser construction and at the top of every
/// remote cache read, so a read can no longer build that default before
/// chromey's client is in place. A host that wants its own client must
/// call `spider_remote_cache::set_client` before creating a browser.
pub fn install_cache_client() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        spider_remote_cache::set_client(crate::browser::request_client().clone());
    });
}

/// Default bound on the remote seed GET, in milliseconds.
pub const DEFAULT_REMOTE_SEED_TIMEOUT_MS: u64 = 1_500;

const SEED_UNSET: u64 = u64::MAX;
static SEED_TIMEOUT_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(SEED_UNSET);
/// 0 = unset (use env), 1 = do not skip, 2 = skip.
static SEED_SKIP_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn env_seed_timeout_ms() -> u64 {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("CHROMEY_REMOTE_CACHE_SEED_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_REMOTE_SEED_TIMEOUT_MS)
    })
}

fn env_seed_skip() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        matches!(
            std::env::var("CHROMEY_REMOTE_CACHE_SKIP_SEED")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("TRUE") | Some("yes")
        )
    })
}

/// Override the bound on remote cache reads (the seed GET in
/// [`get_cache_site`] and [`get_cache_resource`]). `None` restores the env
/// value `CHROMEY_REMOTE_CACHE_SEED_TIMEOUT_MS` (default 1500 ms).
/// `Some(Duration::ZERO)` removes the bound.
pub fn set_remote_seed_timeout(timeout: Option<std::time::Duration>) {
    let v = match timeout {
        Some(d) => (d.as_millis() as u64).min(SEED_UNSET - 1),
        None => SEED_UNSET,
    };
    SEED_TIMEOUT_OVERRIDE_MS.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// The bound applied to remote cache reads, or `None` when unbounded.
pub fn remote_seed_timeout() -> Option<std::time::Duration> {
    let ms = match SEED_TIMEOUT_OVERRIDE_MS.load(std::sync::atomic::Ordering::Relaxed) {
        SEED_UNSET => env_seed_timeout_ms(),
        v => v,
    };
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// Skip the remote seed GET entirely. `None` restores the env value
/// `CHROMEY_REMOTE_CACHE_SKIP_SEED` (`1` skips; default off). Skipping
/// leaves local caching and remote dumps unchanged.
pub fn set_skip_remote_seed(skip: Option<bool>) {
    let v = match skip {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    SEED_SKIP_OVERRIDE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the remote seed GET is skipped.
pub fn skip_remote_seed() -> bool {
    match SEED_SKIP_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => env_seed_skip(),
    }
}

/// Run a remote cache read under [`remote_seed_timeout`]. Returns `false`
/// when the bound fired; the in-flight request is dropped with the future.
async fn bounded_read<F: std::future::Future<Output = ()>>(what: &str, key: &str, fut: F) -> bool {
    match remote_seed_timeout() {
        Some(limit) => {
            if tokio::time::timeout(limit, fut).await.is_err() {
                tracing::warn!(
                    "remote cache {what}: timed out after {}ms for {key}, continuing without it",
                    limit.as_millis()
                );
                return false;
            }
            true
        }
        None => {
            fut.await;
            true
        }
    }
}

/// Convert a `spider_remote_cache::HttpVersion` to an `http_cache::HttpVersion`.
fn remote_version_to_http_cache(v: spider_remote_cache::HttpVersion) -> http_cache::HttpVersion {
    match v {
        spider_remote_cache::HttpVersion::H2 => http_cache::HttpVersion::H2,
        spider_remote_cache::HttpVersion::H3 => http_cache::HttpVersion::H3,
        spider_remote_cache::HttpVersion::Http09 => http_cache::HttpVersion::Http09,
        spider_remote_cache::HttpVersion::Http10 => http_cache::HttpVersion::Http10,
        spider_remote_cache::HttpVersion::Http11 | _ => http_cache::HttpVersion::Http11,
    }
}

/// Best-effort dump of a cached response into the remote hybrid cache server [experimental]
pub async fn dump_to_remote_cache(
    cache_key: &str,
    cache_site: &str,
    http_response: &crate::http::HttpResponse,
    method: &str,
    http_request_headers: &std::collections::HashMap<String, String>,
    dump_remote: Option<&str>,
) {
    // Convert chromey HttpVersion to spider_remote_cache HttpVersion.
    let version = match http_response.version {
        crate::http::HttpVersion::Http09 => spider_remote_cache::HttpVersion::Http09,
        crate::http::HttpVersion::Http10 => spider_remote_cache::HttpVersion::Http10,
        crate::http::HttpVersion::H2 => spider_remote_cache::HttpVersion::H2,
        crate::http::HttpVersion::H3 => spider_remote_cache::HttpVersion::H3,
        _ => spider_remote_cache::HttpVersion::Http11,
    };

    dump_to_remote_cache_parts(
        cache_key,
        cache_site,
        http_response.url.as_str(),
        &http_response.body,
        method,
        http_response.status,
        http_request_headers,
        &http_response.headers,
        &version,
        dump_remote,
    )
    .await
}

/// Get the cache for a website from the remote cache server and seed
/// our local hybrid cache (CACACHE_MANAGER) with **all** entries [experimental].
///
/// Bounded by [`remote_seed_timeout`] (env
/// `CHROMEY_REMOTE_CACHE_SEED_TIMEOUT_MS`, default 1500 ms) and skipped
/// when [`skip_remote_seed`] is set (env `CHROMEY_REMOTE_CACHE_SKIP_SEED=1`).
/// On timeout it returns with whatever entries were already seeded, so a
/// slow or blackholed cache server can no longer hold up navigation.
pub async fn get_cache_site(
    target_url: &str,
    auth: Option<&str>,
    remote: Option<&str>,
    namespace: Option<&str>,
) {
    if skip_remote_seed() {
        return;
    }
    install_cache_client();
    let cache_key = site_key_for_target_url(target_url, auth, namespace);
    bounded_read(
        "get",
        &cache_key,
        get_cache_site_inner(&cache_key, target_url, remote),
    )
    .await;
}

async fn get_cache_site_inner(cache_key: &str, target_url: &str, remote: Option<&str>) {
    let cache_key = cache_key.to_string();
    let base_url = spider_remote_cache::resolve_base_url(remote);

    let endpoint = format!("{}/cache/site/{}", base_url, cache_key);

    let result = get_client().get(&endpoint).send().await;

    let resp = match result {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(
                "remote cache get: failed to GET {} from {}: {}",
                cache_key,
                endpoint,
                err
            );
            return;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!(
            "remote cache get: non-success status for {}: {}",
            cache_key,
            resp.status()
        );
        return;
    }

    let payloads: Vec<Box<HybridCachePayload>> = match resp.json().await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(
                "remote cache get: failed to parse JSON for {} from {}: {}",
                cache_key,
                endpoint,
                err
            );
            return;
        }
    };

    tracing::debug!(
        "remote cache get: seeding {} entries locally for website {}",
        payloads.len(),
        cache_key
    );

    for payload in payloads {
        if let Err(err) = seed_payload_into_local_cache(&cache_key, &payload, target_url).await {
            tracing::warn!(
                "remote cache get: failed to seed resource {} for website {}: {}",
                payload.resource_key,
                cache_key,
                err
            );
        }
    }
}

/// Get the cache for a resource from the remote cache server and seed
/// our local hybrid cache (CACACHE_MANAGER) with **all** entries [experimental].
///
/// Bounded by [`remote_seed_timeout`] like [`get_cache_site`].
pub async fn get_cache_resource(
    target_url: &str,
    auth: Option<&str>,
    remote: Option<&str>,
    namespace: Option<&str>,
) {
    install_cache_client();
    let cache_key = site_key_for_target_url(target_url, auth, namespace);
    bounded_read(
        "get resource",
        &cache_key,
        get_cache_resource_inner(&cache_key, target_url, remote),
    )
    .await;
}

async fn get_cache_resource_inner(cache_key: &str, target_url: &str, remote: Option<&str>) {
    let cache_key = cache_key.to_string();
    let base_url = spider_remote_cache::resolve_base_url(remote);

    let endpoint = format!("{}/cache/resource/{}", base_url, cache_key);

    let result = get_client().get(&endpoint).send().await;

    let resp = match result {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(
                "remote cache get: failed to GET {} from {}: {}",
                cache_key,
                endpoint,
                err
            );
            return;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!(
            "remote cache get: non-success status for {}: {}",
            cache_key,
            resp.status()
        );
        return;
    }

    let payload: Box<HybridCachePayload> = match resp.json().await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(
                "remote cache get: failed to parse JSON for {} from {}: {}",
                cache_key,
                endpoint,
                err
            );
            return;
        }
    };

    tracing::debug!(
        "remote cache get: seeding 1 entrie locally for website {}",
        cache_key
    );

    if let Err(err) = seed_payload_into_local_cache(&cache_key, &payload, target_url).await {
        tracing::warn!(
            "remote cache get: failed to seed resource {} for website {}: {}",
            payload.resource_key,
            cache_key,
            err
        );
    }
}

/// Remove item from local session cache.
pub async fn clear_local_session_cache(cache_key: &str) {
    LOCAL_SESSION_CACHE.remove(cache_key);
}

/// Maximum number of top-level site keys in the local session cache.
const SESSION_CACHE_MAX_SITES: usize = 2_000;

/// Maximum number of resource entries per site key.
const SESSION_CACHE_MAX_PER_SITE: usize = 10_000;

/// Insert the item into the dashmap
pub fn session_cache_insert(
    cache_key: &str,
    http_res: http_cache_reqwest::HttpResponse,
    cache_policy: CachePolicy,
    entry_key: &str,
) {
    // Fast path: key already exists — no eviction needed.
    if let Some(mut inner) = LOCAL_SESSION_CACHE.get_mut(cache_key) {
        if inner.len() < SESSION_CACHE_MAX_PER_SITE {
            inner.insert(entry_key.into(), (http_res, cache_policy));
        }
        return;
    }

    // Slow path: new site key — evict oldest entries *before* acquiring
    // the entry lock so we never call `.len()` / `.iter()` / `.remove()`
    // while a shard write-lock is held (which would deadlock DashMap).
    if LOCAL_SESSION_CACHE.len() >= SESSION_CACHE_MAX_SITES {
        let to_remove: Vec<String> = LOCAL_SESSION_CACHE
            .iter()
            .take(SESSION_CACHE_MAX_SITES / 4)
            .map(|r| r.key().clone())
            .collect();
        for key in to_remove {
            LOCAL_SESSION_CACHE.remove(&key);
        }
    }

    // Insert the new site key.  Another thread may have raced us, so use
    // `entry()` to handle the occupied case gracefully.
    use dashmap::mapref::entry::Entry;
    match LOCAL_SESSION_CACHE.entry(cache_key.to_string()) {
        Entry::Occupied(mut occ) => {
            let inner = occ.get_mut();
            if inner.len() < SESSION_CACHE_MAX_PER_SITE {
                inner.insert(entry_key.into(), (http_res, cache_policy));
            }
        }
        Entry::Vacant(vac) => {
            let mut m: HashMap<String, (http_cache_reqwest::HttpResponse, CachePolicy)> =
                HashMap::new();
            m.insert(entry_key.into(), (http_res, cache_policy));
            vac.insert(m);
        }
    }
}

/// Seed a single `HybridCachePayload` into the local HTTP cache (CACACHE_MANAGER).
async fn seed_payload_into_local_cache(
    cache_key: &str,
    payload: &HybridCachePayload,
    target_url: &str,
) -> Result<(), String> {
    if payload.body_base64.is_empty() {
        return Ok(());
    }

    let same_document = payload.url == target_url;

    let uri = payload
        .url
        .parse()
        .map_err(|e| format!("invalid URI for {}: {e}", payload.url))?;

    let body = general_purpose::STANDARD
        .decode(&payload.body_base64)
        .map_err(|e| format!("invalid base64 body for {}: {e}", payload.resource_key))?;

    let req = HttpRequestLike {
        uri,
        method: Method::from_bytes(payload.method.as_bytes()).unwrap_or(Method::GET),
        headers: convert_headers(&payload.request_headers),
    };

    let res = HttpResponseLike {
        status: StatusCode::from_u16(payload.status).unwrap_or(StatusCode::EXPECTATION_FAILED),
        headers: convert_headers(&payload.response_headers),
    };

    let policy = CachePolicy::new(&req, &res);

    let url =
        Url::parse(&payload.url).map_err(|e| format!("invalid Url for {}: {e}", payload.url))?;

    let http_res = http_cache_reqwest::HttpResponse {
        url,
        headers: http_cache::HttpHeaders::Modern(crate::http::headers_to_multi(
            &payload.response_headers,
        )),
        version: remote_version_to_http_cache(payload.http_version),
        status: payload.status,
        body,
        metadata: None,
    };

    let key = payload.resource_key.clone();
    let session_key = format!("{}:{}", payload.method, http_res.url);

    if same_document {
        let put_result = CACACHE_MANAGER
            .put(key.clone(), http_res.clone(), policy.clone())
            .await;
        if let Err(e) = put_result {
            return Err(format!("CACACHE_MANAGER.put failed for {}: {e}", key));
        }
    }

    session_cache_insert(cache_key, http_res, policy, &session_key);

    Ok(())
}

/// Get the resource from the cache.
pub fn get_session_cache_item(
    cache_key: &str,
    target_url: &str,
) -> Option<(http_cache_reqwest::HttpResponse, CachePolicy)> {
    LOCAL_SESSION_CACHE
        .get(cache_key)
        .and_then(|local_cache| local_cache.get(target_url).cloned())
}

/// Check the resource from the cache.
pub fn check_session_cache_item(cache_key: &str, target_url: &str) -> bool {
    LOCAL_SESSION_CACHE
        .get(cache_key)
        .is_some_and(|local_cache| local_cache.contains_key(target_url))
}

/// Mark a URL as "stream in-flight" so the Network listener skips it.
pub fn mark_stream_pending(key: &str) {
    PENDING_STREAM_URLS.insert(key.to_string());
}

/// Remove the in-flight marker (called on success *and* failure).
pub fn clear_stream_pending(key: &str) {
    PENDING_STREAM_URLS.remove(key);
}

/// Returns `true` when the URL is currently being body-streamed.
pub fn is_stream_pending(key: &str) -> bool {
    PENDING_STREAM_URLS.contains(key)
}
