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
pub async fn rewrite_base_tag(html: &[u8], base_url: &Option<&str>) -> String {
    use lol_html::{element, html_content::ContentType};
    use std::sync::OnceLock;

    if html.is_empty() {
        return Default::default();
    }

    let base_tag_inserted = OnceLock::new();
    let already_present = OnceLock::new();

    let base_url_len = base_url.map(|s| s.len());

    let rewriter_settings: lol_html::Settings<'_, '_, lol_html::send::SendHandlerTypes> =
        lol_html::send::Settings {
            element_content_handlers: vec![
                // Handler for <base> to mark if it is present with href
                element!("base", {
                    |el| {
                        // check base tags that do not exist yet.
                        if base_tag_inserted.get().is_none() {
                            // Check if a <base> with href already exists
                            if let Some(attr) = el.get_attribute("href") {
                                let valid_http =
                                    attr.starts_with("http://") || attr.starts_with("https://");

                                // we can validate if the domain is the same if not to remove it.
                                if valid_http {
                                    let _ = base_tag_inserted.set(true);
                                    let _ = already_present.set(true);
                                } else {
                                    el.remove();
                                }
                            } else {
                                el.remove();
                            }
                        }

                        Ok(())
                    }
                }),
                // Handler for <head> to insert <base> tag if not present
                element!("head", {
                    |el: &mut lol_html::send::Element<'_, '_>| {
                        if let Some(handlers) = el.end_tag_handlers() {
                            let base_tag_inserted = base_tag_inserted.clone();
                            let base_url =
                                format!(r#"<base href="{}">"#, base_url.unwrap_or_default());

                            handlers.push(Box::new(move |end| {
                                if base_tag_inserted.get().is_none() {
                                    let _ = base_tag_inserted.set(true);
                                    end.before(&base_url, ContentType::Html);
                                }
                                Ok(())
                            }))
                        }
                        Ok(())
                    }
                }),
                // Handler for html if <head> not present to insert <head><base></head> tag if not present
                element!("html", {
                    |el: &mut lol_html::send::Element<'_, '_>| {
                        if let Some(handlers) = el.end_tag_handlers() {
                            let base_tag_inserted = base_tag_inserted.clone();
                            let base_url = format!(
                                r#"<head><base href="{}"></head>"#,
                                base_url.unwrap_or_default()
                            );

                            handlers.push(Box::new(move |end| {
                                if base_tag_inserted.get().is_none() {
                                    let _ = base_tag_inserted.set(true);
                                    end.before(&base_url, ContentType::Html);
                                }
                                Ok(())
                            }))
                        }
                        Ok(())
                    }
                }),
            ],
            ..lol_html::send::Settings::new_for_handler_types()
        };

    let mut buffer = Vec::with_capacity(
        html.len()
            + match base_url_len {
                Some(l) => l + 29,
                _ => 0,
            },
    );

    let mut rewriter = lol_html::send::HtmlRewriter::new(rewriter_settings, |c: &[u8]| {
        buffer.extend_from_slice(c);
    });

    let mut stream = tokio_stream::iter(html.chunks(*STREAMING_CHUNK_SIZE));

    let mut wrote_error = false;

    while let Some(chunk) = stream.next().await {
        // early exist
        if already_present.get().is_some() {
            break;
        }
        if rewriter.write(chunk).is_err() {
            wrote_error = true;
            break;
        }
    }

    if !wrote_error {
        let _ = rewriter.end();
    }

    if already_present.get().is_some() {
        std::str::from_utf8(&html).unwrap_or_default().into()
    } else {
        auto_encoder::auto_encode_bytes(&buffer)
    }
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
    let cache_url = create_cache_key_raw(target_url, None, auth_opt.as_deref());

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
    let cache_key = create_cache_key_raw(target_url, None, auth_opt.as_deref());

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
                return Some((http_response.body, http_response.headers));
            }
        }
    }

    None
}

/// Store the page to cache to be re-used across HTTP request.
/// Store the page to the local HTTP cache (CACACHE_MANAGER) and,
/// optionally, dump it to the remote hybrid cache server.
///
/// `dump_remote == true` => local cache + remote cache
/// `dump_remote == false` => local cache only
pub async fn put_hybrid_cache(
    cache_key: &str,
    cache_site: &str,
    http_response: HttpResponse,
    method: &str,
    http_request_headers: std::collections::HashMap<String, String>,
    dump_remote: Option<&str>,
) {
    use http_cache_reqwest::CacheManager;
    use http_cache_semantics::CachePolicy;

    // We need to do everything that only borrows `http_response` *before*
    // we move it into CACACHE_MANAGER::put.
    if let Ok(u) = http_response.url.as_str().parse::<http::uri::Uri>() {
        let req = HttpRequestLike {
            uri: u,
            method: http::method::Method::from_bytes(method.as_bytes())
                .unwrap_or(http::method::Method::GET),
            headers: convert_headers(&http_response.headers),
        };

        let res = HttpResponseLike {
            status: StatusCode::from_u16(http_response.status)
                .unwrap_or(StatusCode::EXPECTATION_FAILED),
            headers: convert_headers(&http_request_headers),
        };

        let policy = CachePolicy::new(&req, &res);

        tracing::debug!("Storing cache {:?}", http_response.url.as_str());

        if dump_remote.is_some() {
            // check if the value is in the cache and not stale to dump it
            let result = tokio::time::timeout(std::time::Duration::from_millis(250), async {
                CACACHE_MANAGER.get(&cache_key).await
            })
            .await;

            let mut put_cache = false;

            if let Ok(cached) = result {
                if let Ok(Some((_http_response, stored_policy))) = cached {
                    if stored_policy.is_stale(SystemTime::now()) {
                        put_cache = true;
                    }
                }
            } else {
                put_cache = true;
            }

            if put_cache {
                let url = http_response.url.to_string();
                let method = method.to_string();
                let current_url = format!("{}:{}", &method, &url);
                let cached =
                    crate::cache::remote::check_session_cache_item(cache_site, &current_url);

                // insert the item into the cache.
                if !cached {
                    let job = super::dump_remote::DumpJob {
                        cache_key: cache_key.to_string(),
                        cache_site: cache_site.to_string(),
                        url: url,
                        method: method,
                        status: http_response.status,
                        request_headers: http_request_headers.clone(),
                        response_headers: http_response.headers.clone(),
                        body: http_response.body.clone(),
                        http_version: http_response.version.clone(),
                        dump_remote: dump_remote.map(|s| s.to_string()),
                    };

                    if super::dump_remote::worke_inited() {
                        if !super::dump_remote::try_enqueue(job) {
                            tracing::debug!(
                                "remote dump skipped (worker not initialized or queue full)"
                            );
                        }
                    } else {
                        if let Err(err) = super::dump_remote::enqueue(job).await {
                            tracing::debug!(
                                "remote dump skipped (worker not initialized or queue full) - {:?}",
                                err
                            );
                        }
                    }
                }
            }
        }

        // Finally, store in your existing local cache.
        let _ = CACACHE_MANAGER
            .put(
                cache_key.into(),
                http_cache_reqwest::HttpResponse {
                    url: http_response.url,
                    body: http_response.body,
                    headers: http_response.headers,
                    version: http_response.version.into(),
                    status: http_response.status,
                },
                policy,
            )
            .await;
    }
}

/// Spawn a background task that listens to *all* Network.responseReceived
/// events for this page and stores them in your cache.
pub async fn spawn_response_cache_listener(
    page: Page,
    cache_site: String,
    auth: Option<String>,
    cache_strategy: Option<CacheStrategy>,
    dump_remote: Option<String>,
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
    let mut out = HashMap::new();

    if let Some(obj) = headers.inner().as_object() {
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
pub fn site_key_for_target_url(target_url: &str, auth: Option<&str>) -> String {
    let normalized = match url::Url::parse(target_url) {
        Ok(mut u) => {
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => target_url.to_string(),
    };
    let input = format!("v1|url={}|auth={}", normalized, auth.unwrap_or(""));
    hex::encode(blake3::hash(input.as_bytes()).as_bytes()) // 64 hex chars, path-safe
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

    let body_ret = page
        .execute(GetResponseBodyParams::new(ev.request_id.clone()))
        .await;

    if let Ok(body_ret) = body_ret {
        let body_bytes = if body_ret.base64_encoded {
            general_purpose::STANDARD.decode(&body_ret.body)?
        } else {
            body_ret.body.clone().into_bytes()
        };

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

        let job = super::dump_remote::DumpJob {
            cache_key: cache_key,
            cache_site: cache_site.to_string(),
            url: url.to_string(),
            method: method.to_string(),
            status: status,
            request_headers: req_headers,
            response_headers: resp_headers,
            body: body_bytes,
            http_version: version,
            dump_remote: dump_remote.map(|s| s.to_string()),
        };

        if super::dump_remote::worke_inited() {
            if !super::dump_remote::try_enqueue(job) {
                tracing::debug!("remote dump skipped (worker not initialized or queue full)");
            }
        } else {
            if let Err(err) = super::dump_remote::enqueue(job).await {
                tracing::debug!(
                    "remote dump skipped (worker not initialized or queue full) - {:?}",
                    err
                );
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
        ]),
    })
    .await?;

    let mut events = page.event_listener::<EventRequestPaused>().await?;

    let handle = tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            if let Err(err) = handle_fetch_paused(
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

/// Async handler for a single Fetch.requestPaused event. If we have the internal listener this will be the first layer.
/// [experimental].
async fn handle_fetch_paused(
    page: &Page,
    ev: &std::sync::Arc<EventRequestPaused>,
    auth: Option<&str>,
    policy: Option<&BasicCachePolicy>,
    cache_strategy: Option<&CacheStrategy>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let current_url = ev.request.url.as_str();

    let eligible_for_cache = allow_cache_response(&ev.resource_type, cache_strategy.as_deref());

    if !eligible_for_cache || !current_url.starts_with("http") {
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
        return Ok(());
    }

    if ev.response_status_code.is_some() || ev.response_error_reason.is_some() {
        let params = ContinueRequestParams::new(ev.request_id.clone());
        page.send_command(params).await?;
        return Ok(());
    }

    if let Some((body, metadata)) =
        get_cached_url_with_metadata(&current_url, auth.as_deref(), policy).await
    {
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
