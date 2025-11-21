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
pub use http_global_cache::CACACHE_MANAGER;
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
    if let Some(authentication) = auth {
        format!(
            "{}:{}:{}",
            override_method.unwrap_or_else(|| "GET".into()),
            uri,
            authentication
        )
    } else {
        format!(
            "{}:{}",
            override_method.unwrap_or_else(|| "GET".into()),
            uri
        )
    }
}

/// Get a cached url.
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
pub async fn put_hybrid_cache(
    cache_key: &str,
    http_response: HttpResponse,
    method: &str,
    http_request_headers: std::collections::HashMap<String, String>,
) {
    use http_cache_reqwest::CacheManager;
    use http_cache_semantics::CachePolicy;

    match http_response.url.as_str().parse::<http::uri::Uri>() {
        Ok(u) => {
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

            let _ = CACACHE_MANAGER
                .put(
                    cache_key.into(),
                    http_cache_reqwest::HttpResponse {
                        url: http_response.url,
                        body: http_response.body,
                        headers: http_response.headers,
                        version: match http_response.version {
                            HttpVersion::H2 => http_cache::HttpVersion::H2,
                            HttpVersion::Http10 => http_cache::HttpVersion::Http10,
                            HttpVersion::H3 => http_cache::HttpVersion::H3,
                            HttpVersion::Http09 => http_cache::HttpVersion::Http09,
                            HttpVersion::Http11 => http_cache::HttpVersion::Http11,
                        },
                        status: http_response.status,
                    },
                    policy,
                )
                .await;
        }
        _ => (),
    }
}

/// Spawn a background task that listens to *all* Network.responseReceived
/// events for this page and stores them in your cache.
pub async fn spawn_response_cache_listener(
    page: Page,
    auth: Option<String>,
    cache_strategy: Option<CacheStrategy>,
) -> Result<JoinHandle<()>, crate::error::CdpError> {
    page.execute(EnableParams::default()).await?;
    let mut events = page.event_listener::<EventResponseReceived>().await?;

    let handle = tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            if let Err(err) =
                handle_single_response(&page, ev, auth.as_deref(), cache_strategy).await
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

/// Handle single response from network and store it in the cache.
async fn handle_single_response(
    page: &Page,
    ev: std::sync::Arc<EventResponseReceived>,
    auth: Option<&str>,
    cache_strategy: Option<CacheStrategy>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !ev.response.url.starts_with("http") {
        return Ok(());
    }

    let document_resource = ev.r#type == ResourceType::Document;

    let eligible_for_cache =
        document_resource || allow_cache_response(&ev.r#type, cache_strategy.as_ref());

    if !eligible_for_cache {
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

        let url = ev.response.url.parse::<url::Url>()?;
        let status = ev.response.status as u16;

        let version = match ev.response.protocol.as_deref() {
            Some("h2") | Some("HTTP/2") | Some("HTTP/2.0") => HttpVersion::H2,
            Some("h3") | Some("HTTP/3") | Some("HTTP/3.0") => HttpVersion::H3,
            Some("HTTP/1.0") => HttpVersion::Http10,
            Some("HTTP/0.9") => HttpVersion::Http09,
            _ => HttpVersion::Http11,
        };

        let method = "GET";

        let cache_key = create_cache_key_raw(url.as_str(), Some(method), auth);

        let http_response = HttpResponse {
            body: body_bytes,
            headers: resp_headers,
            status,
            url,
            version,
        };

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            put_hybrid_cache(&cache_key, http_response, method, req_headers).await
        })
        .await;

        if let Err(result) = result {
            tracing::debug!("Storing cache timeout {}", result);
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
    page.execute(crate::cdp::browser_protocol::fetch::EnableParams {
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
