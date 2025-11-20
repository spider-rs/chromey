use std::time::SystemTime;

use http_cache_reqwest::CacheManager;
use http_cache_semantics::{RequestLike, ResponseLike};
/// The http global cache.
pub use http_global_cache::CACACHE_MANAGER;
use reqwest::StatusCode;
use spider_fingerprint::http;
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
                    |el: &mut lol_html::send::Element| {
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
                    |el: &mut lol_html::send::Element| {
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

/// Represents an HTTP version
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpVersion {
    /// HTTP Version 0.9
    Http09,
    /// HTTP Version 1.0
    Http10,
    /// HTTP Version 1.1
    Http11,
    /// HTTP Version 2.0
    H2,
    /// HTTP Version 3.0
    H3,
}

/// A basic generic type that represents an HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP response body
    pub body: Vec<u8>,
    /// HTTP response headers
    pub headers: std::collections::HashMap<String, String>,
    /// HTTP response status code
    pub status: u16,
    /// HTTP response url
    pub url: url::Url,
    /// HTTP response version
    pub version: HttpVersion,
}

/// A HTTP request type for caching.
pub struct HttpRequestLike {
    ///  The URI component of a request.
    pub uri: http::uri::Uri,
    /// The http method.
    pub method: reqwest::Method,
    /// The http headers.
    pub headers: http::HeaderMap,
}

/// A HTTP response type for caching.
pub struct HttpResponseLike {
    /// The http status code.
    pub status: StatusCode,
    /// The http headers.
    pub headers: http::HeaderMap,
}

impl RequestLike for HttpRequestLike {
    fn uri(&self) -> http::uri::Uri {
        self.uri.clone()
    }
    fn is_same_uri(&self, other: &http::Uri) -> bool {
        &self.uri == other
    }
    fn method(&self) -> &reqwest::Method {
        &self.method
    }
    fn headers(&self) -> &http::HeaderMap {
        &self.headers
    }
}

impl ResponseLike for HttpResponseLike {
    fn status(&self) -> StatusCode {
        self.status
    }
    fn headers(&self) -> &http::HeaderMap {
        &self.headers
    }
}

/// Convert headers to header map
pub fn convert_headers(
    headers: &std::collections::HashMap<String, String>,
) -> reqwest::header::HeaderMap {
    let mut header_map = reqwest::header::HeaderMap::new();

    for (index, items) in headers.iter().enumerate() {
        if let Ok(head) = reqwest::header::HeaderValue::from_str(items.1) {
            use std::str::FromStr;
            if let Ok(key) = reqwest::header::HeaderName::from_str(items.0) {
                header_map.insert(key, head);
            }
        }
        // mal headers
        if index > 1000 {
            break;
        }
    }

    header_map
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
