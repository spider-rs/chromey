use crate::cache::manager::create_cache_key_raw;
use crate::http::{convert_headers, HttpRequestLike, HttpResponseLike};
use base64::engine::general_purpose;
use base64::prelude::Engine as _;
use http_cache_reqwest::CacheManager;
use http_cache_semantics::CachePolicy;
use http_global_cache::CACACHE_MANAGER;
use lazy_static::lazy_static;
use reqwest::header::HeaderValue;
use reqwest::Method;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use url::Url;

lazy_static! {
    /// Global HTTP client reused for all remote cache dumps.
    pub static ref HYBRID_CACHE_CLIENT: Client = Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("failed to build HYBRID_CACHE_CLIENT");

    /// Base URL of your remote hybrid cache server.
    ///
    /// Example: "http://127.0.0.1:8080"
    ///
    /// Override via env:
    ///   HYBRID_CACHE_ENDPOINT=http://remote-cache:8080
    pub static ref HYBRID_CACHE_ENDPOINT: String = std::env::var("HYBRID_CACHE_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
}

/// Payload shape for the remote hybrid cache server `/cache/index` endpoint.
///
/// This matches the `CachedEntryPayload` used in your hyper server.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct HybridCachePayload {
    /// Optional website-level key (we’ll derive from URL host if None).
    website_key: Option<String>,
    /// Your unique cache key (same as `cache_key` from put_hybrid_cache).
    resource_key: String,
    url: String,
    method: String,
    status: u16,
    request_headers: std::collections::HashMap<String, String>,
    response_headers: std::collections::HashMap<String, String>,
    /// Base64-encoded HTTP body for JSON transport.
    body_base64: String,
}

/// Best-effort dump of a cached response into the remote hybrid cache server [experimental]
pub async fn dump_to_remote_cache(
    cache_key: &str,
    cache_site: &str, // the cache site from goto
    http_response: &crate::http::HttpResponse,
    method: &str,
    http_request_headers: &std::collections::HashMap<String, String>,
    dump_remote: Option<&str>,
) {
    let website_key = http_response.url.host_str().map(|h| h.to_string());

    let body_base64 = general_purpose::STANDARD.encode(&http_response.body);

    let payload = HybridCachePayload {
        website_key,
        resource_key: cache_key.to_string(),
        url: http_response.url.to_string(),
        method: method.to_string(),
        status: http_response.status,
        request_headers: http_request_headers.clone(),
        response_headers: http_response.headers.clone(),
        body_base64,
    };

    let mut base_url = HYBRID_CACHE_ENDPOINT.as_str();

    if let Some(remote) = dump_remote {
        if remote != "true" {
            base_url = remote.trim_ascii();
        }
    }

    let endpoint = format!("{}/cache/index", &*base_url);

    let result = HYBRID_CACHE_CLIENT
        .post(&endpoint)
        .json(&payload)
        .header(
            "x-cache-site",
            HeaderValue::from_str(&cache_site).unwrap_or(HeaderValue::from_static("")),
        )
        .send()
        .await;

    match result {
        Ok(resp) => {
            if !resp.status().is_success() {
                tracing::warn!(
                    "remote cache dump: non-success status for {}: {}",
                    cache_key,
                    resp.status()
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                "remote cache dump: failed to POST {} to {}: {}",
                cache_key,
                endpoint,
                err
            );
        }
    }
}

/// Get the cache for a website from the remote cache server and seed
/// our local hybrid cache (CACACHE_MANAGER) with **all** entries [experimental].
///
/// `cache_key` here is the `website_key` used by the remote server,
/// e.g. "example.com".
pub async fn get_cache_site(target_url: &str, auth: Option<&str>, remote: Option<&str>) {
    let mut base_url = HYBRID_CACHE_ENDPOINT.as_str();
    let cache_key = create_cache_key_raw(target_url, None, auth.as_deref());

    if let Some(remote) = remote {
        if remote != "true" {
            base_url = remote.trim_ascii();
        }
    }

    let endpoint = format!("{}/cache/site/{}", &*base_url, cache_key);

    // Fetch all entries for this website from the remote cache server.
    let result = HYBRID_CACHE_CLIENT.get(&endpoint).send().await;

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

    // Parse JSON payloads: Vec<HybridCachePayload>
    let payloads: Vec<HybridCachePayload> = match resp.json().await {
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

    // Seed all entries into our local CACACHE_MANAGER.
    for payload in payloads {
        if let Err(err) = seed_payload_into_local_cache(&payload).await {
            tracing::warn!(
                "remote cache get: failed to seed resource {} for website {}: {}",
                payload.resource_key,
                cache_key,
                err
            );
        }
    }
}

/// Seed a single `HybridCachePayload` into the local HTTP cache (CACACHE_MANAGER).
///
/// This mirrors the logic in `put_hybrid_cache`, but uses data coming back
/// from the remote server instead of a live HttpResponse.
async fn seed_payload_into_local_cache(payload: &HybridCachePayload) -> Result<(), String> {
    let body = general_purpose::STANDARD
        .decode(&payload.body_base64)
        .map_err(|e| format!("invalid base64 body for {}: {e}", payload.resource_key))?;

    // Build URI for CachePolicy request side.
    let uri = payload
        .url
        .parse()
        .map_err(|e| format!("invalid URI for {}: {e}", payload.url))?;

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

    // Build the http_cache_reqwest::HttpResponse used by CACACHE_MANAGER.
    let url =
        Url::parse(&payload.url).map_err(|e| format!("invalid Url for {}: {e}", payload.url))?;

    let http_res = http_cache_reqwest::HttpResponse {
        url,
        body,
        headers: payload.response_headers.clone(),
        // When seeding from remote, we assume HTTP/1.1. Adjust if you want.
        version: http_cache::HttpVersion::Http11,
        status: payload.status,
    };

    // Finally seed into local cache.
    let key = payload.resource_key.clone();
    let put_result = CACACHE_MANAGER.put(key.clone(), http_res, policy).await;

    if let Err(e) = put_result {
        return Err(format!("CACACHE_MANAGER.put failed for {}: {e}", key));
    }

    Ok(())
}
