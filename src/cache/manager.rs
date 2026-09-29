pub use http_global_cache::CACACHE_MANAGER;

use crate::http::{convert_headers, HttpRequestLike, HttpResponse, HttpResponseLike, HttpVersion};
use crate::{
    cdp::browser_protocol::{
        fetch::{ContinueRequestParams, EventRequestPaused, FulfillRequestParams, HeaderEntry},
        network::{EnableParams, EventResponseReceived, GetResponseBodyParams, ResourceType},
    },
    page::Page,
    utils::is_data_resource,
};
use base64::{engine::general_purpose, Engine as _};
use chromiumoxide_cdp::cdp::browser_protocol::fetch::{RequestPattern, RequestStage};
use http_cache_reqwest::CacheManager;
use reqwest::StatusCode;
use spider_fingerprint::http;
use std::collections::HashMap;
use std::time::SystemTime;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

lazy_static::lazy_static! {
    /// The streaming chunk size to rewrite the base tag.
    pub(crate) static ref STREAMING_CHUNK_SIZE: usize = {
        let default_streaming_chunk_size: usize = 8192 * 2;
        let min_streaming_chunk_size: usize = default_streaming_chunk_size * 2 / 3;

        std::env::var("STREAMING_CHUNK_SIZE")
            .ok()
            .and_then(|val| val.parse::<usize>().ok())
            .map(|val| {
                if val < min_streaming_chunk_size {
                    min_streaming_chunk_size
                } else {
                    val
                }
            })
            .unwrap_or(default_streaming_chunk_size)
    };
}

/// Rewrite the initial base-tag.
pub async fn rewrite_base_tag(html: &[u8], base_url: Option<&str>) -> String {
    use lol_html::{element, html_content::ContentType};
    use std::sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    };

    #[inline]
    fn bytes_to_string(b: &[u8]) -> String {
        match std::str::from_utf8(b) {
            Ok(s) => s.to_owned(),
            Err(_) => String::from_utf8_lossy(b).into_owned(),
        }
    }

    if html.is_empty() {
        return String::new();
    }

    let base_href = match base_url {
        Some(s) if !s.is_empty() => s,
        _ => return bytes_to_string(html),
    };

    const UNSET: u8 = 0;
    const INSERTED: u8 = 1;
    const PRESENT: u8 = 2;

    let state = Arc::new(AtomicU8::new(UNSET));
    let saw_head = Arc::new(AtomicBool::new(false));

    let base_tag = format!(r#"<base href="{}">"#, base_href);
    let head_with_base = format!(r#"<head>{}</head>"#, base_tag);

    let mut buffer = Vec::with_capacity(html.len() + base_href.len() + 64);

    let state_for_base = state.clone();
    let state_for_head = state.clone();
    let state_for_body = state.clone();
    let saw_head_for_head = saw_head.clone();
    let saw_head_for_body = saw_head.clone();

    let settings: lol_html::Settings<'_, '_, lol_html::send::SendHandlerTypes> =
        lol_html::send::Settings {
            element_content_handlers: vec![
                element!("base", move |el| {
                    if state_for_base.load(Ordering::Relaxed) == PRESENT {
                        el.remove();
                        return Ok(());
                    }

                    match el.get_attribute("href") {
                        Some(href)
                            if href.starts_with("http://") || href.starts_with("https://") =>
                        {
                            state_for_base.store(PRESENT, Ordering::Relaxed);
                        }
                        _ => el.remove(),
                    }

                    Ok(())
                }),
                element!("head", move |el: &mut lol_html::send::Element<'_, '_>| {
                    saw_head_for_head.store(true, Ordering::Relaxed);

                    if let Some(handlers) = el.end_tag_handlers() {
                        let state = state_for_head.clone();
                        let base_tag = base_tag.clone();

                        handlers.push(Box::new(move |end| {
                            if state
                                .compare_exchange(
                                    UNSET,
                                    INSERTED,
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                )
                                .is_ok()
                            {
                                end.before(&base_tag, ContentType::Html);
                            }
                            Ok(())
                        }));
                    }

                    Ok(())
                }),
                element!("body", move |el: &mut lol_html::send::Element<'_, '_>| {
                    if !saw_head_for_body.load(Ordering::Relaxed)
                        && state_for_body
                            .compare_exchange(UNSET, INSERTED, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        el.before(&head_with_base, ContentType::Html);
                    }
                    Ok(())
                }),
            ],
            ..lol_html::send::Settings::new_for_handler_types()
        };

    let mut rewriter = lol_html::send::HtmlRewriter::new(settings, |c: &[u8]| {
        buffer.extend_from_slice(c);
    });

    for chunk in html.chunks(*STREAMING_CHUNK_SIZE) {
        // If a valid absolute <base href> already exists, keep original HTML exactly.
        if state.load(Ordering::Relaxed) == PRESENT {
            return bytes_to_string(html);
        }
        if rewriter.write(chunk).is_err() {
            return bytes_to_string(html);
        }
    }

    if rewriter.end().is_err() {
        return bytes_to_string(html);
    }

    // Your existing helper returned String in the original signature.
    auto_encoder::auto_encode_bytes(&buffer)
}

/// Create the cache key from string.
pub fn create_cache_key_raw(
    uri: &str,
    override_method: Option<&str>,
    auth: Option<&str>,
) -> String {
    let method = override_method.unwrap_or("GET");
    match auth {
        Some(a) => format!("{method}:{uri}:{a}"),
        None => format!("{method}:{uri}"),
    }
}

/// Hash the key.
fn hash_key_v1(s: &str) -> String {
    hex::encode(blake3::hash(s.as_bytes()).as_bytes())
}

/// Site/page grouping key (used for site:{site_key}::{resource_key})
pub fn create_site_key(target_url: &str, auth: Option<&str>, method: Option<&str>) -> String {
    let normalized = url::Url::parse(target_url)
        .map(|mut u| {
            u.set_fragment(None);
            u.to_string()
        })
        .unwrap_or_else(|_| target_url.to_string());

    // If you want method-specific site groups, set method=Some("POST") etc.
    // If you don't, pass None and it won't fragment.
    let method_part = method.unwrap_or("GET"); // or "ANY" if you prefer

    let raw = match auth {
        Some(a) => format!("site|v1|m={method_part}|url={normalized}|auth={a}"),
        None => format!("site|v1|m={method_part}|url={normalized}|auth="),
    };

    hash_key_v1(&raw)
}

/// Get a cached url from the hybrid cache.
pub async fn get_cached_url(target_url: &str, auth_opt: Option<&str>) -> Option<Vec<u8>> {
    let cache_url = create_cache_key_raw(target_url, None, auth_opt);

    let result = tokio::time::timeout(std::time::Duration::from_millis(60), async {
        CACACHE_MANAGER.get(&cache_url).await
    })
    .await;

    if let Ok(cached) = result {
        if let Ok(Some((http_response, cache_policy))) = cached {
            if !cache_policy.is_stale(SystemTime::now()) {
                return Some(http_response.body);
            }
        }
    }

    None
}

/// Basic cache policy.
#[derive(Debug, Default, Clone)]
pub enum BasicCachePolicy {
    /// Allow stale caches – responses may be used even if they *should* be revalidated.
    AllowStale,
    /// Use this `SystemTime` as the reference "now" for staleness checks.
    Period(SystemTime),
    #[default]
    /// Use the default system time.
    Normal,
}

impl BasicCachePolicy {
    /// Decide whether a cached entry is usable right now.
    #[inline]
    pub fn allows_cached(&self, cache_policy: &http_cache_semantics::CachePolicy) -> bool {
        match self {
            // caller accepts staleness
            BasicCachePolicy::AllowStale => true,
            // use injected time for determinism/testing
            BasicCachePolicy::Period(now) => !cache_policy.is_stale(*now),
            // default behavior: must not be stale at real "now"
            BasicCachePolicy::Normal => !cache_policy.is_stale(SystemTime::now()),
        }
    }
}

/// Get a cached url with headers.
pub async fn get_cached_url_with_metadata(
    target_url: &str,
    auth_opt: Option<&str>,
    policy: Option<&BasicCachePolicy>,
) -> Option<(Vec<u8>, HashMap<String, String>)> {
    let cache_key = create_cache_key_raw(target_url, None, auth_opt);

    let result = tokio::time::timeout(std::time::Duration::from_millis(250), async {
        CACACHE_MANAGER.get(&cache_key).await
    })
    .await;

    if let Ok(cached) = result {
        if let Ok(Some((http_response, stored_policy))) = cached {
            let allow = match policy {
                Some(BasicCachePolicy::AllowStale) => true,
                Some(BasicCachePolicy::Period(now)) => !stored_policy.is_stale(*now),
                _ => !stored_policy.is_stale(SystemTime::now()),
            };

            if allow {
                let headers = crate::http::headers_from_multi(&http_response.headers);
                return Some((http_response.body, headers));
            }
        }
    }

    None
}

/// Store the page to cache to be re-used across HTTP request.
/// Store the page to the local HTTP cache (CACACHE_MANAGER) and,
/// optionally, dump it to the remote hybrid cache server.
///
/// Write semantics:
/// * `dump_remote = None`             => local cache only.
/// * `dump_remote = Some(url)` and
///   `dump_readonly = false` (default) => local + remote (async enqueue).
/// * `dump_remote = Some(url)` and
///   `dump_readonly = true`            => local only; the URL is
///   accepted for future read-through use but **no remote dump is
///   performed**. Use when an upstream proxy handles uploads.
pub async fn put_hybrid_cache(
    cache_key: &str,
    cache_site: &str,
    http_response: HttpResponse,
    method: &str,
    http_request_headers: std::collections::HashMap<String, String>,
    dump_remote: Option<&str>,
    dump_readonly: bool,
) {
    use http_cache_reqwest::CacheManager;
    use http_cache_semantics::CachePolicy;

    // Never cache empty or near-empty HTML responses.
    if is_body_empty_for_cache(&http_response.body) {
        return;
    }

    // Compute all derived values while borrowing http_response, then move
    // fields into their final owners to avoid cloning headers and body.
    if let Ok(u) = http_response.url.as_str().parse::<http::uri::Uri>() {
        let req = HttpRequestLike {
            uri: u,
            method: http::method::Method::from_bytes(method.as_bytes())
                .unwrap_or(http::method::Method::GET),
            headers: convert_headers(&http_request_headers),
        };

        let res = HttpResponseLike {
            status: StatusCode::from_u16(http_response.status)
                .unwrap_or(StatusCode::EXPECTATION_FAILED),
            headers: convert_headers(&http_response.headers),
        };

        let policy = CachePolicy::new(&req, &res);

        tracing::debug!("Storing cache {:?}", http_response.url.as_str());

        // Pre-compute values that borrow http_response before we move fields out.
        let multi_headers = crate::http::headers_to_multi(&http_response.headers);
        let session_key = format!("{}:{}", method, http_response.url);
        let status = http_response.status;
        let version = http_response.version;

        // Determine whether we need a DumpJob (and thus must clone the body).
        // Read-only mode short-circuits regardless of whether `dump_remote`
        // is set — callers opt in to this when an external proxy handles
        // uploads on their behalf.
        let mut need_dump = false;

        if dump_remote.is_some() && !dump_readonly {
            let result = tokio::time::timeout(std::time::Duration::from_millis(250), async {
                CACACHE_MANAGER.get(cache_key).await
            })
            .await;

            let already_fresh = matches!(
                result,
                Ok(Ok(Some((_, ref stored_policy)))) if !stored_policy.is_stale(SystemTime::now())
            );

            if !already_fresh {
                let cached =
                    crate::cache::remote::check_session_cache_item(cache_site, &session_key);
                need_dump = !cached;
            }
        }

        // When a dump is needed, clone the body once before moving it into
        // cached_response. The header HashMaps are moved (not cloned) into DumpJob.
        let dump_body = if need_dump {
            Some(http_response.body.clone())
        } else {
            None
        };

        // Build cached_response, moving url, body, and using pre-computed multi_headers.
        let cached_response = http_cache_reqwest::HttpResponse {
            url: http_response.url,
            body: http_response.body,
            headers: http_cache::HttpHeaders::Modern(multi_headers),
            version: version.into(),
            status,
            metadata: None,
        };

        // Enqueue the remote dump job, moving (not cloning) the header HashMaps.
        if let Some(body) = dump_body {
            let job = super::dump_remote::DumpJob {
                cache_key: cache_key.to_string(),
                cache_site: cache_site.to_string(),
                url: cached_response.url.to_string(),
                method: method.to_string(),
                status,
                request_headers: http_request_headers,
                response_headers: http_response.headers,
                body,
                http_version: version.into(),
                dump_remote: dump_remote.map(|s| s.to_string()),
            };

            if super::dump_remote::worke_inited() {
                if !super::dump_remote::try_enqueue(job) {
                    tracing::debug!("remote dump skipped (queue full)");
                }
            } else {
                // Worker should already be inited by spawn_cache_listener,
                // but enqueue() auto-inits as a fallback.
                if let Err(err) = super::dump_remote::enqueue(job).await {
                    tracing::debug!("remote dump skipped (queue full) - {:?}", err);
                }
            }
        }

        // Populate the session cache so the handler-level interceptor can
        // serve this resource on subsequent requests in the same session.
        crate::cache::remote::session_cache_insert(
            cache_site,
            cached_response.clone(),
            policy.clone(),
            &session_key,
        );

        // Finally, store in your existing local cache.
        let _ = CACACHE_MANAGER
            .put(cache_key.into(), cached_response, policy)
            .await;
    }
}

/// Spawn a background task that listens to *all* Network.responseReceived
/// events for this page and stores them in your cache.
///
/// Set `dump_readonly = true` to keep local caching active but suppress
/// outbound writes to the remote cache server — useful when an upstream
/// proxy is responsible for remote uploads.
pub async fn spawn_response_cache_listener(
    page: Page,
    cache_site: String,
    auth: Option<String>,
    cache_strategy: Option<CacheStrategy>,
    dump_remote: Option<String>,
    dump_readonly: bool,
) -> Result<JoinHandle<()>, crate::error::CdpError> {
    page.execute(EnableParams::default()).await?;
    let mut events = page.event_listener::<EventResponseReceived>().await?;

    let handle = tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            if let Err(err) = handle_single_response(
                &page,
                &cache_site,
                ev,
                auth.as_deref(),
                cache_strategy,
                dump_remote.as_deref(),
                dump_readonly,
            )
            .await
            {
                tracing::debug!("failed to cache response: {err:?}");
            }
        }
    });

    Ok(handle)
}

/// Convert CDP Headers into a plain HashMap<String, String>
fn headers_to_string_map(
    headers: &crate::cdp::browser_protocol::network::Headers,
) -> HashMap<String, String> {
    let obj = headers.inner().as_object();
    let mut out = HashMap::with_capacity(obj.map(|o| o.len()).unwrap_or(0));

    if let Some(obj) = obj {
        for (k, v) in obj {
            // CDP normally uses strings, but be safe:
            let val = if let Some(s) = v.as_str() {
                s.to_string()
            } else {
                v.to_string()
            };
            out.insert(k.clone(), val);
        }
    }

    out
}

/// The default cache control handling.
#[derive(Debug, Default, Clone, PartialEq, Copy)]
pub enum CacheStrategy {
    #[default]
    /// General caching for data collecting.
    Scraping,
    /// Caching for screenshots.
    Screenshots,
}

/// Allow the resource to be cached?
pub fn allow_cache_response(
    resource_type: &ResourceType,
    cache_strategy: Option<&CacheStrategy>,
) -> bool {
    let is_data = is_data_resource(resource_type);
    let strategy = cache_strategy.copied().unwrap_or(CacheStrategy::Scraping);

    // Treat these as “media-like” heavy assets
    let is_media_like = matches!(
        resource_type,
        ResourceType::Image | ResourceType::Media | ResourceType::Font
    );

    // Only cache real network responses, and under Scraping skip media-like assets.
    if strategy == CacheStrategy::Scraping {
        !is_data && !is_media_like
    } else {
        !is_data
    }
}

/// Get the site key for target url.
///
/// `namespace` is an opaque partition string supplied by the caller so logically
/// distinct variants of the same URL (country, proxy pool, tenant, A/B bucket,
/// device profile, …) never collide on the same cached bytes. `None` is treated
/// as the empty namespace.
pub fn site_key_for_target_url(
    target_url: &str,
    auth: Option<&str>,
    namespace: Option<&str>,
) -> String {
    let normalized = match url::Url::parse(target_url) {
        Ok(mut u) => {
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => target_url.to_string(),
    };
    let input = format!(
        "v1|url={}|auth={}|ns={}",
        normalized,
        auth.unwrap_or(""),
        namespace.unwrap_or(""),
    );
    hex::encode(blake3::hash(input.as_bytes()).as_bytes()) // 64 hex chars, path-safe
}

/// True when `s` is already a site key as produced by
/// [`site_key_for_target_url`]: exactly 64 lowercase hex characters (a hex
/// blake3 digest). No URL has that shape, since a URL needs a scheme.
#[inline]
pub fn is_site_key(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Site key for the response listener.
///
/// `target` may be either the navigation URL or a site key that the caller
/// already computed with [`site_key_for_target_url`]. A site key is used as
/// is; anything else is hashed with `auth` and `namespace`. Hashing a site
/// key a second time filed every listener dump under a key no read looks
/// up, and split the per-session cache from the one the interceptor reads.
pub fn listener_site_key(target: &str, auth: Option<&str>, namespace: Option<&str>) -> String {
    if is_site_key(target) {
        target.to_string()
    } else {
        site_key_for_target_url(target, auth, namespace)
    }
}

/// Returns true if the body should NOT be cached (empty, near-empty, or known-bad HTML).
///
/// HTML-specific heuristics (empty `<body>`, skeleton pages) are only applied
/// when the content looks like HTML (starts with `<`).  Non-HTML assets such as
/// JSON, images, CSS, JS, fonts, etc. short-circuit after the basic
/// empty / whitespace check.
#[inline]
fn is_body_empty_for_cache(body: &[u8]) -> bool {
    if body.is_empty() {
        return true;
    }
    let trimmed = body.trim_ascii();
    if trimmed.is_empty() {
        return true;
    }
    // Non-HTML content: if it doesn't start with '<' it's not markup —
    // skip the HTML-specific heuristics entirely.
    if trimmed[0] != b'<' {
        return false;
    }
    // --- HTML-specific checks ---
    if trimmed == b"<html><head></head><body></body></html>" || trimmed == b"<html></html>" {
        return true;
    }
    // Detect pages with HTML structure but empty <body> (small pages only)
    if trimmed.len() <= 2048 {
        let lower: Vec<u8> = trimmed.iter().map(|c| c.to_ascii_lowercase()).collect();
        if let Some(body_open) = memchr::memmem::find(&lower, b"<body") {
            if let Some(gt) = memchr::memchr(b'>', &lower[body_open..]) {
                let content_start = body_open + gt + 1;
                if let Some(close) = memchr::memmem::find(&lower[content_start..], b"</body>") {
                    let content_end = content_start + close;
                    if trimmed[content_start..content_end]
                        .iter()
                        .all(|c| c.is_ascii_whitespace())
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Default method for responses.
const DEFAULT_METHOD: &str = "GET";

/// Handle single response from network and store it in the cache.
async fn handle_single_response(
    page: &Page,
    cache_site: &str,
    ev: std::sync::Arc<EventResponseReceived>,
    auth: Option<&str>,
    cache_strategy: Option<CacheStrategy>,
    dump_remote: Option<&str>,
    dump_readonly: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !ev.response.url.starts_with("http") {
        return Ok(());
    }

    let document_resource = ev.r#type == ResourceType::Document;

    let eligible_for_cache =
        document_resource || allow_cache_response(&ev.r#type, cache_strategy.as_ref());

    if !eligible_for_cache || ev.response.encoded_data_length == 0.0 {
        return Ok(());
    }

    let method = DEFAULT_METHOD.to_string();
    let current_url = format!("{}:{}", &method, &ev.response.url);

    if crate::cache::remote::check_session_cache_item(cache_site, &current_url) {
        return Ok(());
    }

    // Skip if the Fetch streaming path is already handling this URL.
    if crate::cache::remote::is_stream_pending(&current_url) {
        return Ok(());
    }

    let body_ret = page
        .execute(GetResponseBodyParams::new(ev.request_id.clone()))
        .await;

    if let Ok(body_ret) = body_ret {
        let body_bytes = if body_ret.base64_encoded {
            general_purpose::STANDARD.decode(&body_ret.body)?
        } else {
            body_ret.body.clone().into_bytes()
        };

        if is_body_empty_for_cache(&body_bytes) {
            return Ok(());
        }

        let resp_headers: HashMap<String, String> = headers_to_string_map(&ev.response.headers);

        let req_headers: HashMap<String, String> = ev
            .response
            .request_headers
            .as_ref()
            .map(headers_to_string_map)
            .unwrap_or_default();

        let url = &ev.response.url;
        let status = ev.response.status as u16;

        let version = match ev.response.protocol.as_deref() {
            Some(v) => v.into(),
            _ => HttpVersion::Http11,
        };

        let cache_key = create_cache_key_raw(url.as_str(), Some(DEFAULT_METHOD), auth);

        // Populate the session cache so the handler-level interceptor can
        // serve this resource on subsequent navigations in the same session,
        // and so the dedup check at the top of this function works.
        {
            let parsed_url = match url::Url::parse(url.as_str()) {
                Ok(u) => u,
                Err(_) => {
                    // SAFETY: "http://localhost" is always a valid URL.
                    // Use a match to avoid nested unwrap.
                    match url::Url::parse("http://localhost") {
                        Ok(u) => u,
                        Err(_) => return Ok(()),
                    }
                }
            };

            let uri: http::uri::Uri = url.as_str().parse().unwrap_or_default();

            let req = HttpRequestLike {
                uri,
                method: http::method::Method::GET,
                headers: convert_headers(&req_headers),
            };
            let res = HttpResponseLike {
                status: StatusCode::from_u16(status).unwrap_or(StatusCode::EXPECTATION_FAILED),
                headers: convert_headers(&resp_headers),
            };
            let policy = http_cache_semantics::CachePolicy::new(&req, &res);

            let http_res = http_cache_reqwest::HttpResponse {
                url: parsed_url,
                body: body_bytes.clone(),
                headers: http_cache::HttpHeaders::Modern(crate::http::headers_to_multi(
                    &resp_headers,
                )),
                version: version.into(),
                status,
                metadata: None,
            };

            crate::cache::remote::session_cache_insert(cache_site, http_res, policy, &current_url);
        }

        // Read-only mode: skip the remote-cache dump entirely. The
        // local cache and per-session cache have already been
        // populated above, so lookups continue to work — only the
        // outbound POST to the remote cache server is suppressed.
        // Used when an upstream proxy is responsible for uploads.
        if dump_readonly {
            return Ok(());
        }

        let job = super::dump_remote::DumpJob {
            cache_key,
            cache_site: cache_site.to_string(),
            url: url.to_string(),
            method: method.to_string(),
            status,
            request_headers: req_headers,
            response_headers: resp_headers,
            body: body_bytes,
            http_version: version.into(),
            dump_remote: dump_remote.map(|s| s.to_string()),
        };

        if super::dump_remote::worke_inited() {
            if !super::dump_remote::try_enqueue(job) {
                tracing::debug!("remote dump skipped (queue full)");
            }
        } else {
            if let Err(err) = super::dump_remote::enqueue(job).await {
                tracing::debug!("remote dump skipped (queue full) - {:?}", err);
            }
        }
    }

    Ok(())
}

/// Spawn a background task that listens to Fetch.requestPaused and
/// either serves from cache or lets the request proceed.
///
/// - If cache hit: send Fetch.fulfillRequest with cached body
/// - If miss:      send Fetch.continueRequest so Chromium hits the network
pub async fn spawn_fetch_cache_interceptor(
    page: Page,
    auth: Option<String>,
    policy: Option<BasicCachePolicy>,
    cache_strategy: Option<CacheStrategy>,
) -> Result<JoinHandle<()>, crate::error::CdpError> {
    page.send_command(crate::cdp::browser_protocol::fetch::EnableParams {
        handle_auth_requests: Some(false),
        patterns: Some(vec![
            // Request-stage: serve from cache or continue.
            RequestPattern {
                resource_type: Some(ResourceType::Document),
                request_stage: Some(RequestStage::Request),
                url_pattern: Some("*".into()),
            },
            RequestPattern {
                resource_type: Some(ResourceType::Script),
                request_stage: Some(RequestStage::Request),
                url_pattern: Some("*".into()),
            },
            // Response-stage: stream large bodies into cache.
            RequestPattern {
                resource_type: Some(ResourceType::Document),
                request_stage: Some(RequestStage::Response),
                url_pattern: Some("*".into()),
            },
            RequestPattern {
                resource_type: Some(ResourceType::Script),
                request_stage: Some(RequestStage::Response),
                url_pattern: Some("*".into()),
            },
        ]),
    })
    .await?;

    let mut events = page.event_listener::<EventRequestPaused>().await?;

    // Ensure the shared fetch-response background worker is running.
    // One `OnceLock` load per listener-start — no allocation after
    // the first call. Submits from the loop below are wait-free.
    super::fetch_worker::init_worker();

    let handle = tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            // Response-stage events (body available for streaming)
            // are dispatched to the shared fetch-response worker via
            // a lock-free `OnceLock::get + mpsc::send`, instead of
            // spawning a dedicated task per event. This keeps the
            // event-listener loop responsive for fast request-stage
            // decisions and amortises spawn overhead across up to
            // `DISPATCH_BATCH` jobs per batch worker.
            let is_response_stage = ev.response_status_code.is_some();

            if is_response_stage {
                super::fetch_worker::submit(super::fetch_worker::FetchResponseJob {
                    page: page.clone(),
                    ev,
                    auth: auth.clone(),
                    cache_strategy,
                });
            } else if let Err(err) = handle_fetch_paused(
                &page,
                &ev,
                auth.as_deref(),
                policy.as_ref(),
                cache_strategy.as_ref(),
            )
            .await
            {
                tracing::debug!("cache interceptor error: {err:?} - {:?}", ev.request.url);
            }
        }
    });

    Ok(handle)
}

/// Async handler for a single Fetch.requestPaused event (request-stage).
/// Serves from cache on hit, continues on miss.
async fn handle_fetch_paused(
    page: &Page,
    ev: &std::sync::Arc<EventRequestPaused>,
    auth: Option<&str>,
    policy: Option<&BasicCachePolicy>,
    cache_strategy: Option<&CacheStrategy>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let current_url = ev.request.url.as_str();

    let eligible_for_cache = allow_cache_response(&ev.resource_type, cache_strategy);

    if !eligible_for_cache || !current_url.starts_with("http") {
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
        return Ok(());
    }

    // Response-stage events with errors: continue without caching.
    if ev.response_error_reason.is_some() {
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
        return Ok(());
    }

    if let Some((body, metadata)) = get_cached_url_with_metadata(current_url, auth, policy).await {
        tracing::debug!("Cache HIT: {}", current_url);
        let mut resp_headers = Vec::<HeaderEntry>::with_capacity(metadata.len());

        for (key, val) in metadata.iter() {
            resp_headers.push(HeaderEntry {
                name: key.into(),
                value: val.into(),
            });
        }

        let mut params = FulfillRequestParams::new(ev.request_id.clone(), 200);

        params.body = Some(general_purpose::STANDARD.encode(&body).into());
        params.response_headers = Some(resp_headers);

        page.send_command(params).await?;
    } else {
        tracing::debug!("Cache MISS: {}, continuing request", current_url);
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
    }

    Ok(())
}

/// Handle a response-stage `Fetch.requestPaused` event.
///
/// Tries to stream the body via `Fetch.takeResponseBodyAsStream` + `IO.read`.
/// If streaming can't start, falls back to `Fetch.getResponseBody` (one-shot,
/// identical to the original `Network.getResponseBody` approach).
/// If streaming started but failed mid-way, fulfills with the partial body
/// to avoid hanging the request.
///
/// In every case the request is either fulfilled or continued — never left
/// paused, never panics, never deadlocks.
pub(crate) async fn handle_fetch_response_stage(
    page: &Page,
    ev: &std::sync::Arc<EventRequestPaused>,
    auth: Option<&str>,
    cache_strategy: Option<&CacheStrategy>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let current_url = ev.request.url.as_str();

    // Error responses: nothing to cache.
    if ev.response_error_reason.is_some() {
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
        return Ok(());
    }

    let eligible_for_cache = allow_cache_response(&ev.resource_type, cache_strategy);

    if !eligible_for_cache || !current_url.starts_with("http") {
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
        return Ok(());
    }

    // Only cache successful responses.
    let status = ev.response_status_code.unwrap_or(0);
    if !(200..400).contains(&status) {
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
        return Ok(());
    }

    // Extract Content-Length from response headers.
    let resp_headers = response_headers_from_event(ev);
    let content_length = super::stream::content_length_from_headers(&resp_headers);
    let threshold = super::stream::streaming_threshold_bytes();

    // Small responses: let the Network listener handle via getResponseBody.
    // When Content-Length is missing (chunked encoding) skip streaming —
    // most dynamic/chunked responses are small and the one-shot path is
    // faster.  Only stream when we *know* the body is large.
    let should_stream = match content_length {
        Some(len) => len >= threshold,
        None => false,
    };

    if !should_stream {
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
        return Ok(());
    }

    // Build a dedup key matching the format used by the Network listener.
    let dedup_key = format!("{}:{}", DEFAULT_METHOD, current_url);

    // Mark as in-flight so the Network listener skips this URL.
    crate::cache::remote::mark_stream_pending(&dedup_key);

    // ---------------------------------------------------------------
    // Try streaming first.
    // ---------------------------------------------------------------
    let body_bytes = match super::stream::read_response_body_as_stream(
        page,
        ev.request_id.clone(),
        content_length,
    )
    .await
    {
        super::stream::StreamResult::Ok(body) => body,

        super::stream::StreamResult::NotStarted(err) => {
            // Body was NOT consumed — fall back to the one-shot
            // `Fetch.getResponseBody` (identical to the original
            // `Network.getResponseBody` path).
            tracing::debug!(
                "Stream not started for {}, falling back to Fetch.getResponseBody: {err}",
                current_url
            );
            match fetch_response_body_oneshot(page, ev, &dedup_key).await {
                Some(body) => body,
                None => {
                    // Both paths failed — continue the request so Chrome
                    // processes it normally.  The Network listener may
                    // still cache it via the original getResponseBody.
                    crate::cache::remote::clear_stream_pending(&dedup_key);
                    let params = ContinueRequestParams::new(ev.request_id.clone());
                    let _ = page.send_command(params).await;
                    return Ok(());
                }
            }
        }

        super::stream::StreamResult::PartialBody { body, error } => {
            // Body IS consumed — we MUST fulfill.  Use whatever was
            // recovered (may be empty).
            tracing::debug!(
                "Stream failed mid-read for {} ({} bytes recovered): {error}",
                current_url,
                body.len(),
            );
            // Fulfill with the partial body so the request isn't left
            // hanging.  Don't cache partial data.
            crate::cache::remote::clear_stream_pending(&dedup_key);
            let mut params = FulfillRequestParams::new(ev.request_id.clone(), status);
            params.body = Some(general_purpose::STANDARD.encode(&body).into());
            params.response_headers = Some(headers_to_header_entries(&resp_headers));
            let _ = page.send_command(params).await;
            return Ok(());
        }
    };

    // ---------------------------------------------------------------
    // We have the full body — cache and fulfill.
    // ---------------------------------------------------------------
    crate::cache::remote::clear_stream_pending(&dedup_key);

    if is_body_empty_for_cache(&body_bytes) {
        let mut params = FulfillRequestParams::new(ev.request_id.clone(), status);
        params.body = Some(general_purpose::STANDARD.encode(&body_bytes).into());
        params.response_headers = Some(headers_to_header_entries(&resp_headers));
        page.send_command(params).await?;
        return Ok(());
    }

    // Populate session cache.
    let req_headers = request_headers_from_event(ev);
    let cache_key_url = current_url;

    let uri: http::uri::Uri = cache_key_url.parse().unwrap_or_default();
    let req = crate::http::HttpRequestLike {
        uri,
        method: http::method::Method::GET,
        headers: crate::http::convert_headers(&req_headers),
    };
    let res = crate::http::HttpResponseLike {
        status: StatusCode::from_u16(status as u16).unwrap_or(StatusCode::EXPECTATION_FAILED),
        headers: crate::http::convert_headers(&resp_headers),
    };
    let policy = http_cache_semantics::CachePolicy::new(&req, &res);

    let parsed_url = url::Url::parse(cache_key_url).unwrap_or_else(|_| {
        // "http://localhost" is a known-valid static URL.
        #[allow(clippy::expect_used)]
        url::Url::parse("http://localhost").expect("static URL")
    });

    let http_res = http_cache_reqwest::HttpResponse {
        url: parsed_url,
        body: body_bytes.clone(),
        headers: http_cache::HttpHeaders::Modern(crate::http::headers_to_multi(&resp_headers)),
        version: crate::http::HttpVersion::Http11.into(),
        status: status as u16,
        metadata: None,
    };

    let cache_site = site_key_for_target_url(cache_key_url, auth, None);

    crate::cache::remote::session_cache_insert(&cache_site, http_res, policy, &dedup_key);

    // Fulfill the paused request with the full body.
    let mut params = FulfillRequestParams::new(ev.request_id.clone(), status);
    params.body = Some(general_purpose::STANDARD.encode(&body_bytes).into());
    params.response_headers = Some(headers_to_header_entries(&resp_headers));
    page.send_command(params).await?;

    tracing::debug!(
        "Stream cached: {} ({} bytes)",
        current_url,
        body_bytes.len()
    );

    Ok(())
}

/// Fallback: use `Fetch.getResponseBody` (one-shot, same as the original
/// `Network.getResponseBody` approach) when streaming couldn't start.
///
/// Returns `Some(body)` on success, `None` if the fallback also fails.
async fn fetch_response_body_oneshot(
    page: &Page,
    ev: &std::sync::Arc<EventRequestPaused>,
    dedup_key: &str,
) -> Option<Vec<u8>> {
    use chromiumoxide_cdp::cdp::browser_protocol::fetch::GetResponseBodyParams;

    let result = page
        .execute(GetResponseBodyParams::new(ev.request_id.clone()))
        .await;

    match result {
        Ok(ret) => {
            let body_bytes = if ret.result.base64_encoded {
                match general_purpose::STANDARD.decode(&ret.result.body) {
                    Ok(b) => b,
                    Err(err) => {
                        tracing::debug!("Fetch.getResponseBody base64 decode failed: {err}");
                        crate::cache::remote::clear_stream_pending(dedup_key);
                        return None;
                    }
                }
            } else {
                ret.result.body.into_bytes()
            };
            Some(body_bytes)
        }
        Err(err) => {
            tracing::debug!("Fetch.getResponseBody failed: {err}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Header helpers for response-stage events
// ---------------------------------------------------------------------------

/// Extract response headers from a `requestPaused` event into a HashMap.
fn response_headers_from_event(ev: &EventRequestPaused) -> HashMap<String, String> {
    let headers = ev.response_headers.as_ref();
    let mut map = HashMap::with_capacity(headers.map(|h| h.len()).unwrap_or(0));
    if let Some(headers) = headers {
        for entry in headers {
            map.insert(entry.name.clone(), entry.value.clone());
        }
    }
    map
}

/// Extract request headers from a `requestPaused` event into a HashMap.
fn request_headers_from_event(ev: &EventRequestPaused) -> HashMap<String, String> {
    let obj = ev.request.headers.inner().as_object();
    let mut map = HashMap::with_capacity(obj.map(|o| o.len()).unwrap_or(0));
    if let Some(obj) = obj {
        for (k, v) in obj {
            let val = if let Some(s) = v.as_str() {
                s.to_string()
            } else {
                v.to_string()
            };
            map.insert(k.clone(), val);
        }
    }
    map
}

/// Convert a `HashMap<String, String>` to a `Vec<HeaderEntry>`.
fn headers_to_header_entries(headers: &HashMap<String, String>) -> Vec<HeaderEntry> {
    headers
        .iter()
        .map(|(k, v)| HeaderEntry {
            name: k.clone(),
            value: v.clone(),
        })
        .collect()
}
