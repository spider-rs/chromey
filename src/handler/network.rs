use super::blockers::{
    block_websites::block_xhr, ignore_script_embedded, ignore_script_xhr, ignore_script_xhr_media,
    xhr::IGNORE_XHR_ASSETS,
};
use crate::auth::Credentials;
use crate::cmd::CommandChain;
use crate::handler::http::HttpRequest;
use aho_corasick::AhoCorasick;
use case_insensitive_string::CaseInsensitiveString;
use chromiumoxide_cdp::cdp::browser_protocol::network::{
    EmulateNetworkConditionsParams, EventLoadingFailed, EventLoadingFinished,
    EventRequestServedFromCache, EventRequestWillBeSent, EventResponseReceived, Headers,
    InterceptionId, RequestId, ResourceType, Response, SetCacheDisabledParams,
    SetExtraHttpHeadersParams,
};
use chromiumoxide_cdp::cdp::browser_protocol::{
    fetch::{
        self, AuthChallengeResponse, AuthChallengeResponseResponse, ContinueRequestParams,
        ContinueWithAuthParams, DisableParams, EventAuthRequired, EventRequestPaused,
        RequestPattern,
    },
    network::SetBypassServiceWorkerParams,
};
use chromiumoxide_cdp::cdp::browser_protocol::{
    network::EnableParams, security::SetIgnoreCertificateErrorsParams,
};
use chromiumoxide_types::{Command, Method, MethodId};
use hashbrown::{HashMap, HashSet};
use lazy_static::lazy_static;
use reqwest::header::PROXY_AUTHORIZATION;
use spider_network_blocker::intercept_manager::NetworkInterceptManager;
pub use spider_network_blocker::scripts::{
    URL_IGNORE_SCRIPT_BASE_PATHS, URL_IGNORE_SCRIPT_STYLES_PATHS, URL_IGNORE_TRIE_PATHS,
};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::Duration;

lazy_static! {
    /// General patterns for popular libraries and resources
    static ref JS_FRAMEWORK_ALLOW: Vec<&'static str> = vec![
        "jquery",           // Covers jquery.min.js, jquery.js, etc.
        "angular",
        "react",            // Covers all React-related patterns
        "vue",              // Covers all Vue-related patterns
        "bootstrap",
        "d3",
        "lodash",
        "ajax",
        "application",
        "app",              // Covers general app scripts like app.js
        "main",
        "index",
        "bundle",
        "vendor",
        "runtime",
        "polyfill",
        "scripts",
        "es2015.",
        "es2020.",
        "webpack",
        "/wp-content/js/",  // Covers Wordpress content
        // Verified 3rd parties for request
        "https://m.stripe.network/",
        "https://challenges.cloudflare.com/",
        "https://www.google.com/recaptcha/api.js",
        "https://google.com/recaptcha/api.js",
        "https://js.stripe.com/",
        "https://cdn.prod.website-files.com/", // webflow cdn scripts
        "https://cdnjs.cloudflare.com/",        // cloudflare cdn scripts
        "https://code.jquery.com/jquery-"
    ];

    /// Determine if a script should be rendered in the browser by name.
    pub static ref ALLOWED_MATCHER: AhoCorasick = AhoCorasick::new(JS_FRAMEWORK_ALLOW.iter()).expect("matcher to build");

    /// path of a js framework
    pub static ref JS_FRAMEWORK_PATH: phf::Set<&'static str> = {
        phf::phf_set! {
            // Add allowed assets from JS_FRAMEWORK_ASSETS except the excluded ones
            "_astro/", "_app/immutable"
        }
    };

    /// Ignore the content types.
    pub static ref IGNORE_CONTENT_TYPES: phf::Set<&'static str> = phf::phf_set! {
        "application/pdf",
        "application/zip",
        "application/x-rar-compressed",
        "application/x-tar",
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/bmp",
        "image/svg+xml",
        "video/mp4",
        "video/x-msvideo",
        "video/x-matroska",
        "video/webm",
        "audio/mpeg",
        "audio/ogg",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.ms-excel",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "application/vnd.ms-powerpoint",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "application/x-7z-compressed",
        "application/x-rpm",
        "application/x-shockwave-flash",
        "application/rtf",
    };

    /// Ignore the resources for visual content types.
    pub static ref IGNORE_VISUAL_RESOURCE_MAP: phf::Set<&'static str> = phf::phf_set! {
        "Image",
        "Media",
        "Font"
    };

    /// Ignore the resources for visual content types.
    pub static ref IGNORE_NETWORKING_RESOURCE_MAP: phf::Set<&'static str> = phf::phf_set! {
        "CspViolationReport",
        "Manifest",
        "Other",
        "Prefetch",
        "Ping",
    };

    /// Case insenstive css matching
    pub static ref CSS_EXTENSION: CaseInsensitiveString = CaseInsensitiveString::from("css");

    /// The command chain.
    pub static ref INIT_CHAIN: Vec<(std::borrow::Cow<'static, str>, serde_json::Value)>  = {
        let enable = EnableParams::default();

        if let Ok(c) = serde_json::to_value(&enable) {
            vec![(enable.identifier(), c)]
        } else {
            vec![]
        }
    };

    /// The command chain with https ignore.
    pub static ref INIT_CHAIN_IGNORE_HTTP_ERRORS: Vec<(std::borrow::Cow<'static, str>, serde_json::Value)>  = {
        let enable = EnableParams::default();
        let mut v = vec![];
        if let Ok(c) = serde_json::to_value(&enable) {
            v.push((enable.identifier(), c));
        }
        let ignore = SetIgnoreCertificateErrorsParams::new(true);
        if let Ok(ignored) = serde_json::to_value(&ignore) {
            v.push((ignore.identifier(), ignored));
        }

        v
    };

    /// Enable the fetch intercept command
    pub static ref ENABLE_FETCH: chromiumoxide_cdp::cdp::browser_protocol::fetch::EnableParams = {
        fetch::EnableParams::builder()
        .handle_auth_requests(true)
        .pattern(RequestPattern::builder().url_pattern("*").build())
        .build()
    };
}

/// Determine if a redirect is true.
pub(crate) fn is_redirect_status(status: i64) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

#[derive(Debug)]
/// The base network manager.
pub struct NetworkManager {
    queued_events: VecDeque<NetworkEvent>,
    ignore_httpserrors: bool,
    requests: HashMap<RequestId, HttpRequest>,
    // TODO put event in an Arc?
    requests_will_be_sent: HashMap<RequestId, EventRequestWillBeSent>,
    extra_headers: std::collections::HashMap<String, String>,
    request_id_to_interception_id: HashMap<RequestId, InterceptionId>,
    user_cache_disabled: bool,
    attempted_authentications: HashSet<RequestId>,
    credentials: Option<Credentials>,
    pub(crate) user_request_interception_enabled: bool,
    block_all: bool,
    pub(crate) protocol_request_interception_enabled: bool,
    /// The network is offline.
    offline: bool,
    /// The page request timeout.
    pub request_timeout: Duration,
    // made_request: bool,
    /// Ignore visuals (no pings, prefetching, and etc).
    pub ignore_visuals: bool,
    /// Block CSS stylesheets.
    pub block_stylesheets: bool,
    /// Block javascript that is not critical to rendering.
    pub block_javascript: bool,
    /// Block analytics from rendering
    pub block_analytics: bool,
    /// Only html from loading.
    pub only_html: bool,
    /// Is xml document?
    pub xml_document: bool,
    /// The custom intercept handle logic to run on the website.
    pub intercept_manager: NetworkInterceptManager,
    /// Track the amount of times the document reloaded.
    pub document_reload_tracker: u8,
    /// The initial target domain. We want to use a new page on every navigation to prevent re-using the old domain.
    pub document_target_domain: String,
    /// The max bytes to receive.
    pub max_bytes_allowed: Option<u64>,
    /// The cache site_key to use.
    pub cache_site_key: Option<String>,
}

impl NetworkManager {
    pub fn new(ignore_httpserrors: bool, request_timeout: Duration) -> Self {
        Self {
            queued_events: Default::default(),
            ignore_httpserrors,
            requests: Default::default(),
            requests_will_be_sent: Default::default(),
            extra_headers: Default::default(),
            request_id_to_interception_id: Default::default(),
            user_cache_disabled: false,
            attempted_authentications: Default::default(),
            credentials: None,
            block_all: false,
            user_request_interception_enabled: false,
            protocol_request_interception_enabled: false,
            offline: false,
            request_timeout,
            ignore_visuals: false,
            block_javascript: false,
            block_stylesheets: false,
            block_analytics: true,
            only_html: false,
            xml_document: false,
            intercept_manager: NetworkInterceptManager::Unknown,
            document_reload_tracker: 0,
            document_target_domain: String::new(),
            max_bytes_allowed: None,
            cache_site_key: None,
        }
    }

    pub fn init_commands(&self) -> CommandChain {
        let cmds = if self.ignore_httpserrors {
            INIT_CHAIN_IGNORE_HTTP_ERRORS.clone()
        } else {
            INIT_CHAIN.clone()
        };
        CommandChain::new(cmds, self.request_timeout)
    }

    pub(crate) fn push_cdp_request<T: Command>(&mut self, cmd: T) {
        let method = cmd.identifier();
        if let Ok(params) = serde_json::to_value(cmd) {
            self.queued_events
                .push_back(NetworkEvent::SendCdpRequest((method, params)));
        }
    }

    /// The next event to handle
    pub fn poll(&mut self) -> Option<NetworkEvent> {
        self.queued_events.pop_front()
    }

    pub fn extra_headers(&self) -> &std::collections::HashMap<String, String> {
        &self.extra_headers
    }

    pub fn set_extra_headers(&mut self, headers: std::collections::HashMap<String, String>) {
        self.extra_headers = headers;
        self.extra_headers.remove(PROXY_AUTHORIZATION.as_str());
        self.extra_headers.remove("Proxy-Authorization");
        if let Ok(headers) = serde_json::to_value(&self.extra_headers) {
            self.push_cdp_request(SetExtraHttpHeadersParams::new(Headers::new(headers)));
        }
    }

    pub fn set_service_worker_enabled(&mut self, bypass: bool) {
        self.push_cdp_request(SetBypassServiceWorkerParams::new(bypass));
    }

    pub fn set_block_all(&mut self, block_all: bool) {
        self.block_all = block_all;
    }

    pub fn set_request_interception(&mut self, enabled: bool) {
        self.user_request_interception_enabled = enabled;
        self.update_protocol_request_interception();
    }

    pub fn set_cache_enabled(&mut self, enabled: bool) {
        let run = self.user_cache_disabled != !enabled;
        self.user_cache_disabled = !enabled;
        if run {
            self.update_protocol_cache_disabled();
        }
    }

    pub fn disable_request_intercept(&mut self) {
        self.protocol_request_interception_enabled = true;
    }

    /// Set the cache site key.
    pub fn set_cache_site_key(&mut self, cache_site_key: Option<String>) {
        self.cache_site_key = cache_site_key;
    }

    pub fn update_protocol_cache_disabled(&mut self) {
        self.push_cdp_request(SetCacheDisabledParams::new(self.user_cache_disabled));
    }

    pub fn authenticate(&mut self, credentials: Credentials) {
        self.credentials = Some(credentials);
        self.update_protocol_request_interception();
        self.protocol_request_interception_enabled = true;
    }

    fn update_protocol_request_interception(&mut self) {
        let enabled = self.user_request_interception_enabled || self.credentials.is_some();

        if enabled == self.protocol_request_interception_enabled {
            return;
        }

        if enabled {
            self.push_cdp_request(ENABLE_FETCH.clone())
        } else {
            self.push_cdp_request(DisableParams::default())
        }
    }

    /// Url matches analytics that we want to ignore or trackers.
    pub(crate) fn ignore_script(
        &self,
        url: &str,
        block_analytics: bool,
        intercept_manager: NetworkInterceptManager,
    ) -> bool {
        let mut ignore_script = block_analytics
            && spider_network_blocker::scripts::URL_IGNORE_TRIE.contains_prefix(url);

        if !ignore_script {
            if let Some(index) = url.find("//") {
                let pos = index + 2;

                // Ensure there is something after `//`
                if pos < url.len() {
                    // Find the first slash after the `//`
                    if let Some(slash_index) = url[pos..].find('/') {
                        let base_path_index = pos + slash_index + 1;

                        if url.len() > base_path_index {
                            let new_url: &str = &url[base_path_index..];

                            // ignore assets we do not need for frameworks
                            if !ignore_script
                                && intercept_manager == NetworkInterceptManager::Unknown
                            {
                                let hydration_file =
                                    JS_FRAMEWORK_PATH.iter().any(|p| new_url.starts_with(p));

                                // ignore astro paths
                                if hydration_file && new_url.ends_with(".js") {
                                    ignore_script = true;
                                }
                            }

                            if !ignore_script
                                && URL_IGNORE_SCRIPT_BASE_PATHS.contains_prefix(new_url)
                            {
                                ignore_script = true;
                            }

                            if !ignore_script
                                && self.ignore_visuals
                                && URL_IGNORE_SCRIPT_STYLES_PATHS.contains_prefix(new_url)
                            {
                                ignore_script = true;
                            }
                        }
                    }
                }
            }
        }

        // fallback for file ending in analytics.js
        if !ignore_script && block_analytics {
            ignore_script = URL_IGNORE_TRIE_PATHS.contains_prefix(url);
        }

        ignore_script
    }

    /// Determine if the request should be skipped.
    fn skip_xhr(
        &self,
        skip_networking: bool,
        event: &EventRequestPaused,
        network_event: bool,
    ) -> bool {
        // XHR check
        if !skip_networking && network_event {
            let request_url = event.request.url.as_str();

            // check if part of ignore scripts.
            let skip_analytics =
                self.block_analytics && (ignore_script_xhr(request_url) || block_xhr(request_url));

            if skip_analytics {
                true
            } else if self.block_stylesheets || self.ignore_visuals {
                let block_css = self.block_stylesheets;
                let block_media = self.ignore_visuals;

                let mut block_request = false;

                if let Some(position) = request_url.rfind('.') {
                    let hlen = request_url.len();
                    let has_asset = hlen - position;

                    if has_asset >= 3 {
                        let next_position = position + 1;

                        if block_media
                            && IGNORE_XHR_ASSETS.contains::<CaseInsensitiveString>(
                                &request_url[next_position..].into(),
                            )
                        {
                            block_request = true;
                        } else if block_css {
                            block_request =
                                CaseInsensitiveString::from(request_url[next_position..].as_bytes())
                                    .contains(&**CSS_EXTENSION)
                        }
                    }
                }

                if !block_request {
                    block_request = ignore_script_xhr_media(request_url);
                }

                block_request
            } else {
                skip_networking
            }
        } else {
            skip_networking
        }
    }

    #[cfg(feature = "adblock")]
    #[inline]
    /// Detect if ad enabled.
    fn detect_ad_if_enabled(&mut self, event: &EventRequestPaused, skip_networking: bool) -> bool {
        if skip_networking {
            true
        } else {
            self.detect_ad(event)
        }
    }

    /// When adblock feature is disabled, this is a no-op.
    #[cfg(not(feature = "adblock"))]
    #[inline]
    fn detect_ad_if_enabled(&mut self, _event: &EventRequestPaused, skip_networking: bool) -> bool {
        skip_networking
    }

    #[inline]
    /// Fail request
    fn fail_request_blocked(
        &mut self,
        request_id: &chromiumoxide_cdp::cdp::browser_protocol::fetch::RequestId,
    ) {
        let params = chromiumoxide_cdp::cdp::browser_protocol::fetch::FailRequestParams::new(
            request_id.clone(),
            chromiumoxide_cdp::cdp::browser_protocol::network::ErrorReason::BlockedByClient,
        );
        self.push_cdp_request(params);
    }

    #[inline]
    /// Fulfill request
    fn fulfill_request_empty_200(
        &mut self,
        request_id: &chromiumoxide_cdp::cdp::browser_protocol::fetch::RequestId,
    ) {
        let params = chromiumoxide_cdp::cdp::browser_protocol::fetch::FulfillRequestParams::new(
            request_id.clone(),
            200,
        );
        self.push_cdp_request(params);
    }

    // Optional: when cache feature is OFF, keep callsites compiling if you
    // accidentally call it (it becomes a no-op).
    #[cfg(not(feature = "cache"))]
    #[inline]
    fn fulfill_request_from_cache(
        &mut self,
        _request_id: &chromiumoxide_cdp::cdp::browser_protocol::fetch::RequestId,
        _body: &[u8],
        _headers: &std::collections::HashMap<String, String>,
        _status: i64,
    ) {
        // cache feature disabled
    }

    #[cfg(feature = "cache")]
    #[inline]
    /// Fulfill a paused Fetch request from cached bytes + header map.
    ///
    /// `headers` should be response headers (e.g. Content-Type, Cache-Control, etc).
    fn fulfill_request_from_cache(
        &mut self,
        request_id: &chromiumoxide_cdp::cdp::browser_protocol::fetch::RequestId,
        body: &[u8],
        headers: &std::collections::HashMap<String, String>,
        status: i64,
    ) {
        use crate::cdp::browser_protocol::fetch::HeaderEntry;
        use crate::handler::network::fetch::FulfillRequestParams;
        use base64::Engine;
        let mut resp_headers = Vec::<HeaderEntry>::with_capacity(headers.len());
        for (k, v) in headers.iter() {
            resp_headers.push(HeaderEntry {
                name: k.clone().into(),
                value: v.clone().into(),
            });
        }

        let mut params = FulfillRequestParams::new(request_id.clone(), status);

        params.body = Some(
            base64::engine::general_purpose::STANDARD
                .encode(body)
                .into(),
        );
        params.response_headers = Some(resp_headers);

        self.push_cdp_request(params);
    }

    #[inline]
    /// Continue the request url.
    fn continue_request_with_url(
        &mut self,
        request_id: &chromiumoxide_cdp::cdp::browser_protocol::fetch::RequestId,
        url: Option<&str>,
        intercept_response: bool,
    ) {
        let mut params = ContinueRequestParams::new(request_id.clone());
        if let Some(url) = url {
            params.url = Some(url.to_string());
            params.intercept_response = Some(intercept_response);
        }
        self.push_cdp_request(params);
    }

    /// On fetch requesdt paused interception.
    #[inline]
    pub fn on_fetch_request_paused(&mut self, event: &EventRequestPaused) {
        // If both interceptions are enabled, do nothing.
        if self.user_request_interception_enabled && self.protocol_request_interception_enabled {
            return;
        }

        if self.block_all {
            tracing::debug!(
                "Blocked (block_all): {:?} - {}",
                event.resource_type,
                event.request.url
            );
            return self.fail_request_blocked(&event.request_id);
        }

        // If this paused request corresponds to a "request_will_be_sent", hand it off and exit.
        if let Some(network_id) = event.network_id.as_ref() {
            if let Some(request_will_be_sent) =
                self.requests_will_be_sent.remove(network_id.as_ref())
            {
                return self
                    .on_request(&request_will_be_sent, Some(event.request_id.clone().into()));
            }
        } else {
            // No network_id, just continue.
            return self.push_cdp_request(ContinueRequestParams::new(event.request_id.clone()));
        }

        // From here on, we handle the full decision tree.
        let resource_type = &event.resource_type;
        let javascript_resource = *resource_type == ResourceType::Script;
        let document_resource = *resource_type == ResourceType::Document;
        let network_resource = !document_resource && crate::utils::is_data_resource(resource_type);

        // Start with static / cheap skip checks.
        let mut skip_networking =
            self.block_all || IGNORE_NETWORKING_RESOURCE_MAP.contains(resource_type.as_ref());

        // Also short-circuit if we've reloaded this document too many times.
        if !skip_networking {
            skip_networking = self.document_reload_tracker >= 3;
        }

        // Handle document redirect / masking and track xml documents.
        let (current_url_cow, had_replacer) =
            self.handle_document_replacement_and_tracking(event, document_resource);

        let current_url: &str = current_url_cow.as_ref();

        // Main initial check (visuals, stylesheets, simple JS blocking).
        if !skip_networking {
            // Allow XSL for sitemap XML.
            if self.xml_document && current_url.ends_with(".xsl") {
                skip_networking = false;
            } else {
                skip_networking = self.should_skip_for_visuals_and_basic_js(
                    resource_type,
                    javascript_resource,
                    current_url,
                );
            }
        }

        // Ad blocking (only active when feature = "adblock").
        skip_networking = self.detect_ad_if_enabled(event, skip_networking);

        // Ignore embedded scripts when only_html or ignore_visuals is set.
        if !skip_networking
            && (self.only_html || self.ignore_visuals)
            && (javascript_resource || document_resource)
        {
            skip_networking = ignore_script_embedded(current_url);
        }

        // Analytics check for JS.
        if !skip_networking && javascript_resource {
            skip_networking =
                self.ignore_script(current_url, self.block_analytics, self.intercept_manager);
        }

        // XHR / data resources.
        skip_networking = self.skip_xhr(skip_networking, event, network_resource);

        // Custom interception layer.
        if !skip_networking && (javascript_resource || network_resource || document_resource) {
            skip_networking = self.intercept_manager.intercept_detection(
                current_url,
                self.ignore_visuals,
                network_resource,
            );
        }

        // Custom website block list.
        if !skip_networking && (javascript_resource || network_resource) {
            skip_networking = crate::handler::blockers::block_websites::block_website(current_url);
        }

        if skip_networking {
            tracing::debug!("Blocked: {:?} - {}", resource_type, current_url);
            self.fulfill_request_empty_200(&event.request_id);
        } else {
            #[cfg(feature = "cache")]
            {
                if let Some(cache_site_key) = self.cache_site_key.as_deref() {
                    if let Some((res, _cache_policy)) =
                        crate::cache::remote::get_session_cache_item(cache_site_key, current_url)
                    {
                        tracing::debug!("Remote Cached: {:?} - {}", resource_type, current_url);
                        return self.fulfill_request_from_cache(
                            &event.request_id,
                            &res.body,
                            &res.headers,
                            res.status as i64,
                        );
                    }
                }
            }

            // check our frame cache for the run.
            tracing::debug!("Allowed: {:?} - {}", resource_type, current_url);
            self.continue_request_with_url(
                &event.request_id,
                if had_replacer {
                    Some(current_url)
                } else {
                    None
                },
                !had_replacer,
            );
        }
    }

    /// Does the network manager have a target domain?
    pub fn has_target_domain(&self) -> bool {
        !self.document_target_domain.is_empty()
    }

    /// Set the target page url for tracking.
    pub fn set_page_url(&mut self, page_target_url: String) {
        self.document_target_domain = page_target_url;
    }

    /// Clear the initial target domain on every navigation.
    pub fn clear_target_domain(&mut self) {
        self.document_reload_tracker = 0;
        self.document_target_domain = Default::default();
    }

    /// Handles:
    /// - document reload tracking (`document_reload_tracker`)
    /// - redirect masking / replacement
    /// - xml document detection (`xml_document`)
    /// - `document_target_domain` updates
    ///
    /// Returns (current_url, had_replacer).
    #[inline]
    fn handle_document_replacement_and_tracking<'a>(
        &mut self,
        event: &'a EventRequestPaused,
        document_resource: bool,
    ) -> (Cow<'a, str>, bool) {
        let mut replacer: Option<String> = None;
        let current_url = event.request.url.as_str();

        if document_resource {
            if self.document_target_domain == current_url {
                // Prevent redirect loop (3 attempts are considered enough).
                self.document_reload_tracker += 1;
            } else if !self.document_target_domain.is_empty()
                && event.redirected_request_id.is_some()
            {
                // Build http/https mask pair for redirect masking.
                let (http_document_replacement, mut https_document_replacement) =
                    if self.document_target_domain.starts_with("http://") {
                        (
                            self.document_target_domain.replace("http://", "http//"),
                            self.document_target_domain.replace("http://", "https://"),
                        )
                    } else {
                        (
                            self.document_target_domain.replace("https://", "https//"),
                            self.document_target_domain.replace("https://", "http://"),
                        )
                    };

                // Track trailing slash to restore later.
                let trailing = https_document_replacement.ends_with('/');
                if trailing {
                    https_document_replacement.pop();
                }
                if https_document_replacement.ends_with('/') {
                    https_document_replacement.pop();
                }

                let redirect_mask = format!(
                    "{}{}",
                    https_document_replacement, http_document_replacement
                );

                if current_url == redirect_mask {
                    replacer = Some(if trailing {
                        format!("{}/", https_document_replacement)
                    } else {
                        https_document_replacement
                    });
                }
            }

            if self.document_target_domain.is_empty() && current_url.ends_with(".xml") {
                self.xml_document = true;
            }

            // Track last seen document URL.
            self.document_target_domain = event.request.url.clone();
        }

        let current_url_cow = match replacer {
            Some(r) => Cow::Owned(r),
            None => Cow::Borrowed(event.request.url.as_str()),
        };

        let had_replacer = matches!(current_url_cow, Cow::Owned(_));
        (current_url_cow, had_replacer)
    }

    /// Shared "visuals + basic JS blocking" logic.
    #[inline]
    fn should_skip_for_visuals_and_basic_js(
        &self,
        resource_type: &ResourceType,
        javascript_resource: bool,
        current_url: &str,
    ) -> bool {
        (self.ignore_visuals && IGNORE_VISUAL_RESOURCE_MAP.contains(resource_type.as_ref()))
            || (self.block_stylesheets && *resource_type == ResourceType::Stylesheet)
            || (self.block_javascript
                && javascript_resource
                && self.intercept_manager == NetworkInterceptManager::Unknown
                && !ALLOWED_MATCHER.is_match(current_url))
    }

    /// Perform a page intercept for chrome
    #[cfg(feature = "adblock")]
    pub fn detect_ad(&self, event: &EventRequestPaused) -> bool {
        use adblock::{
            lists::{FilterSet, ParseOptions, RuleTypes},
            Engine,
        };

        lazy_static::lazy_static! {
            static ref AD_ENGINE: Engine = {
                let mut filter_set = FilterSet::new(false);
                let mut rules = ParseOptions::default();
                rules.rule_types = RuleTypes::All;

                filter_set.add_filters(
                    &*spider_network_blocker::adblock::ADBLOCK_PATTERNS,
                    rules,
                );

                Engine::from_filter_set(filter_set, true)
            };
        };

        let blockable = ResourceType::Image == event.resource_type
            || event.resource_type == ResourceType::Media
            || event.resource_type == ResourceType::Stylesheet
            || event.resource_type == ResourceType::Document
            || event.resource_type == ResourceType::Fetch
            || event.resource_type == ResourceType::Xhr;

        let u = &event.request.url;

        let block_request = blockable
            // set it to example.com for 3rd party handling is_same_site
        && {
            let request = adblock::request::Request::preparsed(
                 &u,
                 "example.com",
                 "example.com",
                 &event.resource_type.as_ref().to_lowercase(),
                 !event.request.is_same_site.unwrap_or_default());

            AD_ENGINE.check_network_request(&request).matched
        };

        block_request
    }

    pub fn on_fetch_auth_required(&mut self, event: &EventAuthRequired) {
        let response = if self
            .attempted_authentications
            .contains(event.request_id.as_ref())
        {
            AuthChallengeResponseResponse::CancelAuth
        } else if self.credentials.is_some() {
            self.attempted_authentications
                .insert(event.request_id.clone().into());
            AuthChallengeResponseResponse::ProvideCredentials
        } else {
            AuthChallengeResponseResponse::Default
        };

        let mut auth = AuthChallengeResponse::new(response);
        if let Some(creds) = self.credentials.clone() {
            auth.username = Some(creds.username);
            auth.password = Some(creds.password);
        }
        self.push_cdp_request(ContinueWithAuthParams::new(event.request_id.clone(), auth));
    }

    pub fn set_offline_mode(&mut self, value: bool) {
        if self.offline == value {
            return;
        }
        self.offline = value;
        if let Ok(network) = EmulateNetworkConditionsParams::builder()
            .offline(self.offline)
            .latency(0)
            .download_throughput(-1.)
            .upload_throughput(-1.)
            .build()
        {
            self.push_cdp_request(network);
        }
    }

    /// Request interception doesn't happen for data URLs with Network Service.
    pub fn on_request_will_be_sent(&mut self, event: &EventRequestWillBeSent) {
        if self.protocol_request_interception_enabled && !event.request.url.starts_with("data:") {
            if let Some(interception_id) = self
                .request_id_to_interception_id
                .remove(event.request_id.as_ref())
            {
                self.on_request(event, Some(interception_id));
            } else {
                // TODO remove the clone for event
                self.requests_will_be_sent
                    .insert(event.request_id.clone(), event.clone());
            }
        } else {
            self.on_request(event, None);
        }
    }

    /// The request was served from the cache.
    pub fn on_request_served_from_cache(&mut self, event: &EventRequestServedFromCache) {
        if let Some(request) = self.requests.get_mut(event.request_id.as_ref()) {
            request.from_memory_cache = true;
        }
    }

    /// On network response received.
    pub fn on_response_received(&mut self, event: &EventResponseReceived) {
        let mut request_failed = false;

        // Track how many bytes we actually deducted from this target.
        let mut deducted: u64 = 0;

        if let Some(max_bytes) = self.max_bytes_allowed.as_mut() {
            let before = *max_bytes;

            // encoded_data_length -> saturating cast to u64
            let received_bytes: u64 = event.response.encoded_data_length as u64;

            // Safe parse of Content-Length
            let content_length: Option<u64> = event
                .response
                .headers
                .inner()
                .get("content-length")
                .and_then(|v| v.as_str())
                .and_then(|s| s.trim().parse::<u64>().ok());

            // Deduct what we actually received
            *max_bytes = max_bytes.saturating_sub(received_bytes);

            // If the declared size can't fit, zero out now
            if let Some(cl) = content_length {
                if cl > *max_bytes {
                    *max_bytes = 0;
                }
            }

            request_failed = *max_bytes == 0;

            // Compute exact delta deducted on this event
            deducted = before.saturating_sub(*max_bytes);
        }

        // Bubble up the deduction (even if request continues)
        if deducted > 0 {
            self.queued_events
                .push_back(NetworkEvent::BytesConsumed(deducted));
        }

        // block all network request moving forward.
        if request_failed && self.max_bytes_allowed.is_some() {
            self.set_block_all(true);
        }

        if let Some(mut request) = self.requests.remove(event.request_id.as_ref()) {
            request.set_response(event.response.clone());
            self.queued_events.push_back(if request_failed {
                NetworkEvent::RequestFailed(request)
            } else {
                NetworkEvent::RequestFinished(request)
            });
        }
    }

    pub fn on_network_loading_finished(&mut self, event: &EventLoadingFinished) {
        if let Some(request) = self.requests.remove(event.request_id.as_ref()) {
            if let Some(interception_id) = request.interception_id.as_ref() {
                self.attempted_authentications
                    .remove(interception_id.as_ref());
            }
            self.queued_events
                .push_back(NetworkEvent::RequestFinished(request));
        }
    }

    pub fn on_network_loading_failed(&mut self, event: &EventLoadingFailed) {
        if let Some(mut request) = self.requests.remove(event.request_id.as_ref()) {
            request.failure_text = Some(event.error_text.clone());
            if let Some(interception_id) = request.interception_id.as_ref() {
                self.attempted_authentications
                    .remove(interception_id.as_ref());
            }
            self.queued_events
                .push_back(NetworkEvent::RequestFailed(request));
        }
    }

    fn on_request(
        &mut self,
        event: &EventRequestWillBeSent,
        interception_id: Option<InterceptionId>,
    ) {
        let mut redirect_chain = Vec::new();
        let mut redirect_location = None;

        if let Some(redirect_resp) = &event.redirect_response {
            if let Some(mut request) = self.requests.remove(event.request_id.as_ref()) {
                if is_redirect_status(redirect_resp.status) {
                    if let Some(location) = redirect_resp.headers.inner()["Location"].as_str() {
                        if redirect_resp.url != location {
                            let fixed_location = location.replace(&redirect_resp.url, "");

                            request.response.as_mut().map(|resp| {
                                resp.headers.0["Location"] =
                                    serde_json::Value::String(fixed_location.clone());
                            });

                            redirect_location = Some(fixed_location);
                        }
                    }
                }

                self.handle_request_redirect(
                    &mut request,
                    if let Some(redirect_location) = redirect_location {
                        let mut redirect_resp = redirect_resp.clone();

                        redirect_resp.headers.0["Location"] =
                            serde_json::Value::String(redirect_location);

                        redirect_resp
                    } else {
                        redirect_resp.clone()
                    },
                );

                redirect_chain = std::mem::take(&mut request.redirect_chain);
                redirect_chain.push(request);
            }
        }

        let request = HttpRequest::new(
            event.request_id.clone(),
            event.frame_id.clone(),
            interception_id,
            self.user_request_interception_enabled,
            redirect_chain,
        );

        self.requests.insert(event.request_id.clone(), request);
        self.queued_events
            .push_back(NetworkEvent::Request(event.request_id.clone()));
    }

    fn handle_request_redirect(&mut self, request: &mut HttpRequest, response: Response) {
        request.set_response(response);
        if let Some(interception_id) = request.interception_id.as_ref() {
            self.attempted_authentications
                .remove(interception_id.as_ref());
        }
    }
}

#[derive(Debug)]
pub enum NetworkEvent {
    SendCdpRequest((MethodId, serde_json::Value)),
    Request(RequestId),
    Response(RequestId),
    RequestFailed(HttpRequest),
    RequestFinished(HttpRequest),
    BytesConsumed(u64),
}
