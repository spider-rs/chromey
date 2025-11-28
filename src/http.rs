use http_cache_semantics::{RequestLike, ResponseLike};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use spider_fingerprint::http;

/// Represents an HTTP version
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum HttpVersion {
    /// HTTP Version 0.9
    Http09,
    /// HTTP Version 1.0
    Http10,
    #[default]
    /// HTTP Version 1.1
    Http11,
    /// HTTP Version 2.0
    H2,
    /// HTTP Version 3.0
    H3,
}

impl HttpVersion {
    #[inline]
    pub fn parse_protocol(s: &str) -> Self {
        // normalize for matching
        let t = s.trim();

        // fast-path common exact strings without allocations
        match t {
            "h2" | "H2" | "HTTP/2" | "HTTP/2.0" => return Self::H2,
            "h3" | "H3" | "HTTP/3" | "HTTP/3.0" => return Self::H3,
            "HTTP/1.0" => return Self::Http10,
            "HTTP/0.9" => return Self::Http09,
            "HTTP/1.1" => return Self::Http11,
            _ => {}
        }

        if t.eq_ignore_ascii_case("h2")
            || t.eq_ignore_ascii_case("http/2")
            || t.eq_ignore_ascii_case("http/2.0")
        {
            Self::H2
        } else if t.eq_ignore_ascii_case("h3")
            || t.eq_ignore_ascii_case("http/3")
            || t.eq_ignore_ascii_case("http/3.0")
        {
            Self::H3
        } else if t.eq_ignore_ascii_case("http/1.0") {
            Self::Http10
        } else if t.eq_ignore_ascii_case("http/0.9") {
            Self::Http09
        } else {
            Self::Http11
        }
    }
}

impl From<&str> for HttpVersion {
    #[inline]
    fn from(s: &str) -> Self {
        Self::parse_protocol(s)
    }
}

impl From<Option<&str>> for HttpVersion {
    #[inline]
    fn from(s: Option<&str>) -> Self {
        s.map(HttpVersion::from).unwrap_or(HttpVersion::Http11)
    }
}

#[cfg(feature = "cache")]
impl From<HttpVersion> for http_cache::HttpVersion {
    fn from(v: HttpVersion) -> Self {
        match v {
            HttpVersion::H2 => http_cache::HttpVersion::H2,
            HttpVersion::H3 => http_cache::HttpVersion::H3,
            HttpVersion::Http09 => http_cache::HttpVersion::Http09,
            HttpVersion::Http10 => http_cache::HttpVersion::Http10,
            HttpVersion::Http11 => http_cache::HttpVersion::Http11,
        }
    }
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
#[derive(Debug, Default)]
pub struct HttpRequestLike {
    ///  The URI component of a request.
    pub uri: http::uri::Uri,
    /// The http method.
    pub method: reqwest::Method,
    /// The http headers.
    pub headers: http::HeaderMap,
}

/// A HTTP response type for caching.
#[derive(Debug, Default)]
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
