use crate::cache::manager::site_key_for_target_url;
use crate::http::{convert_headers, HttpRequestLike, HttpResponseLike, HttpVersion};
use base64::engine::general_purpose;
use base64::prelude::Engine as _;
use hashbrown::HashMap;
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

    /// The local session cache per run cleared.
    pub static ref LOCAL_SESSION_CACHE: dashmap::DashMap<String, HashMap<String, (http_cache_reqwest::HttpResponse, CachePolicy)>> = dashmap::DashMap::new();
}

/// Payload shape for the remote hybrid cache server `/cache/index` endpoint.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct HybridCachePayload {
    /// Optional website-level key (defaults to URL host if None).
    #[serde(default)]
    website_key: Option<String>,
    resource_key: String,
    url: String,
    method: String,
    status: u16,
    request_headers: std::collections::HashMap<String, String>,
    response_headers: std::collections::HashMap<String, String>,
    http_version: HttpVersion,
    /// Base64-encoded HTTP body for JSON transport.
    body_base64: String,
}

pub async fn dump_to_remote_cache_parts(
    cache_key: &str,
    cache_site: &str,
    url_str: &str,
    body: &[u8],
    method: &str,
    status: u16,
    http_request_headers: &std::collections::HashMap<String, String>,
    response_headers: &std::collections::HashMap<String, String>,
    http_version: &HttpVersion,
    dump_remote: Option<&str>,
) {
    // If URL parsing fails, website_key becomes None (same as your original intent).
    let website_key = url::Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()));

    let body_base64 = general_purpose::STANDARD.encode(body);

    let payload = HybridCachePayload {
        website_key,
        resource_key: cache_key.to_string(),
        url: url_str.to_string(),
        method: method.to_string(),
        status,
        http_version: *http_version,
        request_headers: http_request_headers.clone(),
        response_headers: response_headers.clone(),
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
            HeaderValue::from_str(cache_site).unwrap_or(HeaderValue::from_static("")),
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
            } else {
                tracing::info!(
                    "remote cache dump: success status for {}: {}",
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

/// Best-effort dump of a cached response into the remote hybrid cache server [experimental]
pub async fn dump_to_remote_cache(
    cache_key: &str,
    cache_site: &str,
    http_response: &crate::http::HttpResponse,
    method: &str,
    http_request_headers: &std::collections::HashMap<String, String>,
    dump_remote: Option<&str>,
) {
    dump_to_remote_cache_parts(
        cache_key,
        cache_site,
        http_response.url.as_str(),
        &http_response.body,
        method,
        http_response.status,
        http_request_headers,
        &http_response.headers,
        &http_response.version,
        dump_remote,
    )
    .await
}

/// Get the cache for a website from the remote cache server and seed
/// our local hybrid cache (CACACHE_MANAGER) with **all** entries [experimental].
///
/// `cache_key` here is the `website_key` used by the remote server,
/// e.g. "example.com".
pub async fn get_cache_site(target_url: &str, auth: Option<&str>, remote: Option<&str>) {
    let mut base_url = HYBRID_CACHE_ENDPOINT.as_str();

    if let Some(remote) = remote {
        if remote != "true" {
            base_url = remote.trim_ascii();
        }
    }

    let cache_key = site_key_for_target_url(target_url, auth.as_deref());

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

    // Seed all entries into our local CACACHE_MANAGER.
    for payload in payloads {
        if let Err(err) = seed_payload_into_local_cache(&cache_key, &payload).await {
            tracing::warn!(
                "remote cache get: failed to seed resource {} for website {}: {}",
                payload.resource_key,
                cache_key,
                err
            );
        }
    }
}

/// Remove item from local session cache.
pub async fn clear_local_session_cache(cache_key: &str) {
    LOCAL_SESSION_CACHE.remove(cache_key);
}

/// Insert the item into the dashmap
pub fn session_cache_insert(
    cache_key: &str,
    http_res: http_cache_reqwest::HttpResponse,
    cache_policy: CachePolicy,
) {
    use dashmap::mapref::entry::Entry;
    let entry_key = http_res.url.to_string();

    match LOCAL_SESSION_CACHE.entry(cache_key.to_string()) {
        Entry::Occupied(mut occ) => {
            occ.get_mut().insert(entry_key, (http_res, cache_policy));
        }
        Entry::Vacant(vac) => {
            let mut m: HashMap<String, (http_cache_reqwest::HttpResponse, CachePolicy)> =
                HashMap::new();
            m.insert(entry_key, (http_res, cache_policy));
            vac.insert(m);
        }
    }
}

/// Seed a single `HybridCachePayload` into the local HTTP cache (CACACHE_MANAGER).
async fn seed_payload_into_local_cache(
    cache_key: &str,
    payload: &HybridCachePayload,
) -> Result<(), String> {
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
        version: payload.http_version.into(),
        status: payload.status,
    };

    let key = payload.resource_key.clone();

    session_cache_insert(cache_key, http_res.clone(), policy.clone());

    let put_result = CACACHE_MANAGER.put(key.clone(), http_res, policy).await;

    if let Err(e) = put_result {
        return Err(format!("CACACHE_MANAGER.put failed for {}: {e}", key));
    }

    Ok(())
}

/// Get the resource from the cache. TODO: add different methods.
pub fn get_session_cache_item(
    target_url: &str,
    cache_key: &str,
) -> Option<(http_cache_reqwest::HttpResponse, CachePolicy)> {
    LOCAL_SESSION_CACHE
        .get(target_url)
        .and_then(|local_cache| local_cache.get(cache_key).cloned())
}
