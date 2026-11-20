#[cfg(any(feature = "adblock", feature = "firewall"))]
use super::blockers::block_websites::block_ads;
use super::blockers::{
    block_websites::block_xhr, ignore_script_embedded, ignore_script_xhr, ignore_script_xhr_media,
    xhr::IGNORE_XHR_ASSETS,
};
use crate::auth::Credentials;
#[cfg(feature = "_cache")]
use crate::cache::BasicCachePolicy;
use crate::cmd::CommandChain;
use crate::handler::http::HttpRequest;
use crate::handler::network_utils::{base_domain_from_host, host_and_rest};
use aho_corasick::AhoCorasick;
use case_insensitive_string::CaseInsensitiveString;
use chromey_cdp::cdp::browser_protocol::fetch::{RequestPattern, RequestStage};
use chromey_cdp::cdp::browser_protocol::network::{
    EmulateNetworkConditionsByRuleParams, EventLoadingFailed, EventLoadingFinished,
    EventRequestServedFromCache, EventRequestWillBeSent, EventResponseReceived, Headers,
    InitiatorType, InterceptionId, NetworkConditions, RequestId, ResourceType, Response,
    SetCacheDisabledParams, SetExtraHttpHeadersParams,
};
use chromey_cdp::cdp::browser_protocol::{
    fetch::{
        self, AuthChallengeResponse, AuthChallengeResponseResponse, ContinueRequestParams,
        ContinueWithAuthParams, DisableParams, EventAuthRequired, EventRequestPaused,
    },
    network::SetBypassServiceWorkerParams,
};
use chromey_cdp::cdp::browser_protocol::{
    network::EnableParams, security::SetIgnoreCertificateErrorsParams,
};
use chromey_types::{Command, Method, MethodId};
use hashbrown::{HashMap, HashSet};
use lazy_static::lazy_static;
use reqwest::header::PROXY_AUTHORIZATION;
use spider_network_blocker::intercept_manager::NetworkInterceptManager;
pub use spider_network_blocker::scripts::{
    URL_IGNORE_SCRIPT_BASE_PATHS, URL_IGNORE_SCRIPT_STYLES_PATHS, URL_IGNORE_TRIE_PATHS,
};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
        "captcha",
        "client",
        "/cdn-cgi/challenge-platform/",
        "/wp-content/js/",  // Covers Wordpress content
        // Verified 3rd parties for request
        "https://m.stripe.network/",
        "https://challenges.cloudflare.com/",
        "https://www.google.com/recaptcha/",
        "https://google.com/recaptcha/api.js",
        "https://www.gstatic.com/recaptcha/",
        "https://captcha.px-cloud.net/",
        "https://geo.captcha-delivery.com/",
        "https://api.leminnow.com/captcha/",
        "https://cdn.auth0.com/js/lock/",
        "https://captcha.gtimg.com",
        "https://client-api.arkoselabs.com/",
        "https://www.capy.me/puzzle/",
        "https://newassets.hcaptcha.com/",
        "https://cdn.auth0.com/client",
        "https://js.stripe.com/",
        "https://cdn.prod.website-files.com/", // webflow cdn scripts
        "https://cdnjs.cloudflare.com/",        // cloudflare cdn scripts
        "https://code.jquery.com/jquery-"
    ];

    /// Determine if a script should be rendered in the browser by name.
    ///
    /// NOTE: with "allow all scripts unless blocklisted", this is not used as a gate anymore,
    /// but we keep it for compatibility and other call sites.
    pub static ref ALLOWED_MATCHER: AhoCorasick = AhoCorasick::new(JS_FRAMEWORK_ALLOW.iter()).expect("matcher to build");

    /// General patterns for popular libraries and resources
    static ref JS_FRAMEWORK_ALLOW_3RD_PARTY: Vec<&'static str> = vec![
        // Verified 3rd parties for request
        "https://m.stripe.network/",
        "https://challenges.cloudflare.com/",
        "https://js.stripe.com/",
        "https://cdn.prod.website-files.com/", // webflow cdn scripts
        "https://cdnjs.cloudflare.com/",        // cloudflare cdn scripts
        "https://code.jquery.com/jquery-",
        "https://ct.captcha-delivery.com/",
        "https://geo.captcha-delivery.com/",
        "https://img1.wsimg.com/parking-lander/static/js/main.d9ebbb8c.js", // parking landing page iframe
        "https://cdn.auth0.com/client",
        "https://captcha.px-cloud.net/",
        "https://www.capy.me/puzzle/",
        "https://www.gstatic.com/recaptcha/",
        "https://google.com/recaptcha/",
        "https://www.google.com/recaptcha/",
        "https://www.recaptcha.net/recaptcha/",
        "https://js.hcaptcha.com/1/api.js",
        "https://hcaptcha.com/1/api.js",
        "https://js.datadome.co/tags.js",
        "https://api-js.datadome.co/",
        "https://client.perimeterx.net/",
        "https://captcha.px-cdn.net/",
        "https://newassets.hcaptcha.com/",
        "https://captcha.px-cloud.net/",
        "https://s.perimeterx.net/",
        "https://api.leminnow.com/captcha/",
        "https://client-api.arkoselabs.com/",
        "https://static.geetest.com/v4/gt4.js",
        "https://static.geetest.com/",
        "https://cdn.jsdelivr.net/npm/@friendlycaptcha/",
        "https://cdn.perfdrive.com/aperture/",
        "https://assets.queue-it.net/",
        "discourse-cdn.com/",
        "hcaptcha.com",
        "/cdn-cgi/challenge-platform/",
        "/_Incapsula_Resource"
    ];

    /// Determine if a script should be rendered in the browser by name.
    pub static ref ALLOWED_MATCHER_3RD_PARTY: AhoCorasick = AhoCorasick::new(JS_FRAMEWORK_ALLOW_3RD_PARTY.iter()).expect("matcher to build");

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
        "image/webp",
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
    pub static ref ENABLE_FETCH: chromey_cdp::cdp::browser_protocol::fetch::EnableParams = {
        fetch::EnableParams::builder()
        .handle_auth_requests(true)
        .pattern(RequestPattern::builder().url_pattern("*").request_stage(RequestStage::Request).build())
        .build()
    };
}

/// Determine if a redirect is true.
pub(crate) fn is_redirect_status(status: i64) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// How long a buffered `requests_will_be_sent` / `request_id_to_interception_id`
/// entry may linger before being evicted. 30 seconds is generous — the CDP
/// round-trip that reconciles the two racing events normally completes in
/// milliseconds.
const STALE_BUFFER_SECS: u64 = 30;

/// How long an in-flight request entry (`requests` map) can live without
/// being resolved by a `loadingFinished` / `loadingFailed` / `loadingCanceled`
/// event before it is considered orphaned and evicted.  Longer than the
/// race-condition buffer timeout because real requests can legitimately take
/// tens of seconds (streaming, slow origins, etc.).
const STALE_REQUEST_SECS: u64 = 120;

/// Wrapper around `adblock::Engine` that implements `Debug`.
#[cfg(feature = "adblock")]
pub struct AdblockEngine(std::sync::Arc<adblock::Engine>);

#[cfg(feature = "adblock")]
impl std::fmt::Debug for AdblockEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdblockEngine").finish()
    }
}

#[cfg(feature = "adblock")]
impl std::ops::Deref for AdblockEngine {
    type Target = adblock::Engine;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
/// The base network manager.
pub struct NetworkManager {
    /// FIFO queue of internal `NetworkEvent`s emitted by the manager.
    ///
    /// The manager pushes events here as CDP commands are scheduled (e.g. `SendCdpRequest`)
    /// and as request lifecycle transitions occur (`RequestFinished`, `RequestFailed`, etc.).
    /// Consumers pull from this queue via `poll()`.
    queued_events: VecDeque<NetworkEvent>,
    /// If `true`, the init command chain includes `Security.setIgnoreCertificateErrors(true)`.
    ///
    /// This is used to allow navigation / resource loading to proceed on sites with invalid TLS
    /// certificates (self-signed, expired, MITM proxies, etc.).
    ignore_httpserrors: bool,
    /// Active in-flight requests keyed by CDP `RequestId`.
    ///
    /// Each entry tracks request/response metadata, redirect chain, optional interception id,
    /// and final state used to emit `RequestFinished` / `RequestFailed`.
    requests: HashMap<RequestId, HttpRequest>,
    /// Temporary storage for `Network.requestWillBeSent` events when the corresponding
    /// `Fetch.requestPaused` arrives later (or vice versa).
    ///
    /// When Fetch interception is enabled, `requestPaused` and `requestWillBeSent` can race.
    /// We buffer `requestWillBeSent` here until we can attach the `InterceptionId`.
    /// Entries older than `STALE_BUFFER_SECS` are evicted to prevent unbounded growth.
    requests_will_be_sent: HashMap<RequestId, (EventRequestWillBeSent, Instant)>,
    /// Extra HTTP headers to apply to subsequent network requests via CDP.
    ///
    /// This map is mirrored from user-supplied headers but stripped of proxy auth headers
    /// (`Proxy-Authorization`) to avoid accidental leakage / incorrect forwarding.
    extra_headers: std::collections::HashMap<String, String>,
    /// Mapping from Network `RequestId` to Fetch `InterceptionId`.
    ///
    /// When `Fetch.requestPaused` fires before `Network.requestWillBeSent`, we temporarily
    /// store the interception id here so it can be attached to the `HttpRequest` once the
    /// network request is observed.
    /// Entries older than `STALE_BUFFER_SECS` are evicted to prevent unbounded growth.
    request_id_to_interception_id: HashMap<RequestId, (InterceptionId, Instant)>,
    /// Whether the user has disabled the browser cache.
    ///
    /// This is surfaced via `Network.setCacheDisabled(true/false)` and toggled through
    /// `set_cache_enabled()`. Internally the field is stored as “disabled” to match the CDP API.
    user_cache_disabled: bool,
    /// Tracks which requests have already attempted authentication.
    ///
    /// Used to prevent infinite auth retry loops when the origin repeatedly issues
    /// authentication challenges (407/401). Once a request id is present here, subsequent
    /// challenges for the same request are canceled.
    attempted_authentications: HashSet<RequestId>,
    /// Optional credentials used to respond to `Fetch.authRequired` challenges.
    ///
    /// When set, the manager will answer challenges with `ProvideCredentials` once per request
    /// (guarded by `attempted_authentications`), otherwise it falls back to default handling.
    credentials: Option<Credentials>,
    /// User-facing toggle indicating whether request interception is desired.
    ///
    /// This is the “intent” flag controlled by `set_request_interception()`. On its own it does
    /// not guarantee interception is active; interception is actually enabled/disabled by
    /// `update_protocol_request_interception()` which reconciles this flag with `credentials`.
    ///
    /// In other words: if this is `false` but `credentials.is_some()`, interception may still be
    /// enabled to satisfy auth challenges.
    pub(crate) user_request_interception_enabled: bool,
    /// Hard kill-switch to block all network traffic.
    ///
    /// When `true`, the manager immediately blocks requests (typically via
    /// `FailRequest(BlockedByClient)` or fulfillment with an empty response depending on path),
    /// and short-circuits most decision logic. This is used for safety conditions such as
    /// exceeding `max_bytes_allowed` or other runtime protections.
    block_all: bool,
    /// Tracks whether the Fetch interception protocol is currently enabled in CDP.
    ///
    /// This is the “actual state” flag that reflects whether we have sent `Fetch.enable` or
    /// `Fetch.disable` to the browser. It is updated by `update_protocol_request_interception()`
    /// when `user_request_interception_enabled` or `credentials` change.
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
    ///
    /// NOTE: With "allow all scripts unless blocklisted", this no longer blocks scripts
    /// by itself (it remains for config compatibility).
    pub block_javascript: bool,
    /// When `block_stylesheets` would skip a stylesheet, allow it through if
    /// the request URL is first-party (registrable domain matches the page's
    /// primary frame). Default `true` so SPAs that load their own CSS via
    /// dynamic imports still hydrate when callers pass `block_stylesheets`
    /// for bandwidth. Set `false` to strictly block ALL stylesheets.
    pub allow_first_party_stylesheets: bool,
    /// When a downstream blocker (intercept_manager / adblock / blocklists)
    /// would skip a script, allow it through if the request URL is
    /// first-party. Default `true` so SPA bootloaders are not collateral
    /// damage from third-party tracker rules.
    pub allow_first_party_javascript: bool,
    /// When `ignore_visuals` would skip an image/media/font, allow it through
    /// if the request URL is first-party. Default `true` so first-party
    /// image-driven SPA renderers (gallery code-splits, font-blocking
    /// hydration) still complete when callers pass `ignore_visuals`.
    pub allow_first_party_visuals: bool,
    /// Block analytics from rendering
    pub block_analytics: bool,
    /// Block pre-fetch request
    pub block_prefetch: bool,
    /// Only html from loading.
    pub only_html: bool,
    /// Is xml document?
    pub xml_document: bool,
    /// The custom intercept handle logic to run on the website.
    pub intercept_manager: NetworkInterceptManager,
    /// Track the amount of times the document reloaded.
    pub document_reload_tracker: u8,
    /// The initial target url. We want to use a new page on every navigation to prevent re-using the old domain.
    pub document_target_url: String,
    /// The initial target domain. We want to use a new page on every navigation to prevent re-using the old domain.
    pub document_target_domain: String,
    /// The max bytes to receive.
    pub max_bytes_allowed: Option<u64>,
    /// Cap on main-frame Document redirect hops before the navigation is aborted.
    ///
    /// `None` disables enforcement (default, preserves prior behavior). When `Some(n)`,
    /// the (n+1)th Document redirect short-circuits: a synthetic `RequestFailed` event
    /// is emitted with `failure_text = "net::ERR_TOO_MANY_REDIRECTS"` and
    /// `Page.stopLoading` is dispatched to abort in-flight navigation. The accumulated
    /// `redirect_chain` is preserved on the failed request so consumers can inspect it.
    pub max_redirects: Option<usize>,
    #[cfg(feature = "_cache")]
    /// The cache site_key to use.
    pub cache_site_key: Option<String>,
    /// The cache policy to use.
    #[cfg(feature = "_cache")]
    pub cache_policy: Option<BasicCachePolicy>,
    /// Optional per-run/per-site whitelist of URL substrings (scripts/resources).
    whitelist_patterns: Vec<String>,
    /// Compiled matcher for whitelist_patterns (rebuilt when patterns change).
    whitelist_matcher: Option<AhoCorasick>,
    /// Optional per-run/per-site blacklist of URL substrings (scripts/resources).
    blacklist_patterns: Vec<String>,
    /// Compiled matcher for blacklist_patterns (rebuilt when patterns change).
    blacklist_matcher: Option<AhoCorasick>,
    /// If true, blacklist always wins (cannot be unblocked by whitelist/3p allow).
    blacklist_strict: bool,
    /// When true, push the interception policy (flags + per-job
    /// blacklist/whitelist + page url) to a capable remote engine once per
    /// navigation via `Interception.setPolicy`, so it can resolve block/allow
    /// decisions locally instead of round-tripping each `Fetch.requestPaused`.
    /// Default `false` — a real Chrome target 200-OK-ignores the unknown
    /// method, and an engine that does not implement it simply keeps the
    /// round-trip path, so this is safe to leave on. Carries no engine-
    /// specific data: only this manager's existing config fields are
    /// serialized; the per-job lists travel as the opaque strings the caller
    /// already supplied.
    remote_local_policy: bool,
    /// Custom adblock engine built from user-supplied filter rules.
    /// When `Some`, takes precedence over the global default engine.
    #[cfg(feature = "adblock")]
    adblock_engine: Option<AdblockEngine>,
}

impl NetworkManager {
    /// A new network manager.
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
            allow_first_party_stylesheets: true,
            allow_first_party_javascript: true,
            allow_first_party_visuals: true,
            block_prefetch: true,
            block_analytics: true,
            only_html: false,
            xml_document: false,
            intercept_manager: NetworkInterceptManager::Unknown,
            document_reload_tracker: 0,
            document_target_url: String::new(),
            document_target_domain: String::new(),
            whitelist_patterns: Vec::new(),
            whitelist_matcher: None,
            blacklist_patterns: Vec::new(),
            blacklist_matcher: None,
            blacklist_strict: true,
            remote_local_policy: false,
            max_bytes_allowed: None,
            max_redirects: None,
            #[cfg(feature = "_cache")]
            cache_site_key: None,
            #[cfg(feature = "_cache")]
            cache_policy: None,
            #[cfg(feature = "adblock")]
            adblock_engine: None,
        }
    }

    /// Set a custom adblock engine built from user-supplied filter rules.
    #[cfg(feature = "adblock")]
    pub fn set_adblock_engine(&mut self, engine: std::sync::Arc<adblock::Engine>) {
        self.adblock_engine = Some(AdblockEngine(engine));
    }

    /// Replace the whitelist patterns (compiled once).
    pub fn set_whitelist_patterns<I, S>(&mut self, patterns: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.whitelist_patterns = patterns.into_iter().map(Into::into).collect();
        self.rebuild_whitelist_matcher();
    }

    /// Replace the blacklist patterns (compiled once).
    pub fn set_blacklist_patterns<I, S>(&mut self, patterns: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.blacklist_patterns = patterns.into_iter().map(Into::into).collect();
        self.rebuild_blacklist_matcher();
    }

    /// Add one pattern (cheap) and rebuild (call this sparingly).
    pub fn add_blacklist_pattern<S: Into<String>>(&mut self, pattern: S) {
        self.blacklist_patterns.push(pattern.into());
        self.rebuild_blacklist_matcher();
    }

    /// Add many patterns and rebuild once.
    pub fn add_blacklist_patterns<I, S>(&mut self, patterns: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.blacklist_patterns
            .extend(patterns.into_iter().map(Into::into));
        self.rebuild_blacklist_matcher();
    }

    /// Clear blacklist entirely.
    pub fn clear_blacklist(&mut self) {
        self.blacklist_patterns.clear();
        self.blacklist_matcher = None;
    }

    /// Control precedence: when true, blacklist always wins.
    pub fn set_blacklist_strict(&mut self, strict: bool) {
        self.blacklist_strict = strict;
    }

    #[inline]
    fn rebuild_blacklist_matcher(&mut self) {
        if self.blacklist_patterns.is_empty() {
            self.blacklist_matcher = None;
            return;
        }

        self.blacklist_matcher =
            AhoCorasick::new(self.blacklist_patterns.iter().map(|s| s.as_str())).ok();
    }

    #[inline]
    fn is_blacklisted(&self, url: &str) -> bool {
        self.blacklist_matcher
            .as_ref()
            .map(|m| m.is_match(url))
            .unwrap_or(false)
    }

    /// Add one pattern (cheap) and rebuild (call this sparingly).
    pub fn add_whitelist_pattern<S: Into<String>>(&mut self, pattern: S) {
        self.whitelist_patterns.push(pattern.into());
        self.rebuild_whitelist_matcher();
    }

    /// Add many patterns and rebuild once.
    pub fn add_whitelist_patterns<I, S>(&mut self, patterns: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.whitelist_patterns
            .extend(patterns.into_iter().map(Into::into));
        self.rebuild_whitelist_matcher();
    }

    #[inline]
    fn rebuild_whitelist_matcher(&mut self) {
        if self.whitelist_patterns.is_empty() {
            self.whitelist_matcher = None;
            return;
        }

        // If building fails (shouldn’t for simple patterns), just disable matcher.
        self.whitelist_matcher =
            AhoCorasick::new(self.whitelist_patterns.iter().map(|s| s.as_str())).ok();
    }

    #[inline]
    fn is_whitelisted(&self, url: &str) -> bool {
        self.whitelist_matcher
            .as_ref()
            .map(|m| m.is_match(url))
            .unwrap_or(false)
    }

    /// Commands to init the chain with.
    pub fn init_commands(&self) -> CommandChain {
        let cmds = if self.ignore_httpserrors {
            INIT_CHAIN_IGNORE_HTTP_ERRORS.clone()
        } else {
            INIT_CHAIN.clone()
        };
        CommandChain::new(cmds, self.request_timeout)
    }

    /// Push the CDP request.
    pub(crate) fn push_cdp_request<T: Command>(&mut self, cmd: T) {
        let method = cmd.identifier();
        if let Ok(params) = serde_json::to_value(cmd) {
            self.queued_events
                .push_back(NetworkEvent::SendCdpRequest((method, params)));
        }
    }

    /// The next event to handle.
    pub fn poll(&mut self) -> Option<NetworkEvent> {
        self.queued_events.pop_front()
    }

    /// Evict stale entries from the race-condition buffers and from
    /// `attempted_authentications`. Call this periodically (e.g. from the
    /// handler's eviction tick) so that lost CDP events cannot cause unbounded
    /// map growth.
    pub fn evict_stale_entries(&mut self, now: Instant) {
        let cutoff = now - Duration::from_secs(STALE_BUFFER_SECS);

        self.requests_will_be_sent.retain(|_, (_, ts)| *ts > cutoff);
        self.request_id_to_interception_id
            .retain(|_, (_, ts)| *ts > cutoff);

        // Evict orphaned in-flight requests whose completion events
        // (`loadingFinished` / `loadingFailed` / `loadingCanceled`) were
        // never received.  Uses a longer timeout than the race-condition
        // buffers since real requests can legitimately be long-lived.
        let request_cutoff = now - Duration::from_secs(STALE_REQUEST_SECS);
        self.requests
            .retain(|_, req| req.created_at > request_cutoff);

        // `attempted_authentications` entries reference interception IDs that
        // are cleaned up on loading-finished / loading-failed. If those events
        // are lost, the set grows forever. Cross-reference with `requests`:
        // any interception ID that no longer appears in a live request is stale.
        if !self.attempted_authentications.is_empty() {
            let live: HashSet<&str> = self
                .requests
                .values()
                .filter_map(|r| r.interception_id.as_ref().map(|id| id.as_ref()))
                .collect();
            self.attempted_authentications
                .retain(|id| live.contains(id.as_ref()));
        }
    }

    /// Get the extra headers.
    pub fn extra_headers(&self) -> &std::collections::HashMap<String, String> {
        &self.extra_headers
    }

    /// Set extra HTTP headers.
    pub fn set_extra_headers(&mut self, headers: std::collections::HashMap<String, String>) {
        self.extra_headers = headers;
        self.extra_headers.remove(PROXY_AUTHORIZATION.as_str());
        self.extra_headers.remove("Proxy-Authorization");
        if !self.extra_headers.is_empty() {
            if let Ok(headers) = serde_json::to_value(&self.extra_headers) {
                self.push_cdp_request(SetExtraHttpHeadersParams::new(Headers::new(headers)));
            }
        }
    }

    pub fn set_service_worker_enabled(&mut self, bypass: bool) {
        self.push_cdp_request(SetBypassServiceWorkerParams::new(bypass));
    }

    pub fn set_block_all(&mut self, block_all: bool) {
        self.block_all = block_all;
    }

    /// Enable/disable pushing the interception policy to a capable remote
    /// engine (see [`NetworkManager::remote_local_policy`]).
    pub fn set_remote_local_policy(&mut self, enabled: bool) {
        self.remote_local_policy = enabled;
    }

    /// Serialize the current interception configuration into the
    /// `Interception.setPolicy` params. Pure serialization of fields this
    /// manager already holds — no engine-specific logic, no list contents
    /// beyond the caller-supplied per-job substrings.
    fn request_policy_params(&self) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "enabled": true,
            "flags": {
                "blockAll": self.block_all,
                "blockVisuals": self.ignore_visuals,
                "blockStylesheets": self.block_stylesheets,
                "blockJavascript": self.block_javascript,
                "blockAnalytics": self.block_analytics,
                "blockAds": self.block_ads_enabled(),
                "blockPrefetch": self.block_prefetch,
                "onlyHtml": self.only_html,
                "blacklistStrict": self.blacklist_strict,
                "allowFirstPartyStylesheets": self.allow_first_party_stylesheets,
                "allowFirstPartyJavascript": self.allow_first_party_javascript,
                "allowFirstPartyVisuals": self.allow_first_party_visuals,
            },
            "blacklist": self.blacklist_patterns,
            "whitelist": self.whitelist_patterns,
            // Discriminant only — a non-default manager signals the engine to
            // keep the round-trip (the manager's detection stays here).
            "interceptManager": format!("{:?}", self.intercept_manager),
            "pageUrl": self.document_target_url,
        })
    }

    /// Ad blocking is governed by the `firewall` build (the `block_websites`
    /// /`detect_ad` path). When that feature is off this manager performs no
    /// ad blocking, so the pushed policy must report `false` to stay in parity.
    #[inline]
    fn block_ads_enabled(&self) -> bool {
        cfg!(feature = "firewall")
    }

    /// Push the one-shot `Interception.setPolicy` to the remote engine when
    /// enabled and Fetch interception is active. No-op otherwise (default), so
    /// callers that never opt in pay nothing and behavior is unchanged.
    pub fn emit_request_policy(&mut self) {
        if !self.remote_local_policy || !self.protocol_request_interception_enabled {
            return;
        }
        let params = self.request_policy_params();
        self.queued_events.push_back(NetworkEvent::SendCdpRequest((
            Cow::Borrowed("Interception.setPolicy"),
            params,
        )));
    }

    pub fn set_request_interception(&mut self, enabled: bool) {
        self.user_request_interception_enabled = enabled;
        self.update_protocol_request_interception();
    }

    pub fn set_cache_enabled(&mut self, enabled: bool) {
        let run = self.user_cache_disabled == enabled;
        self.user_cache_disabled = !enabled;
        if run {
            self.update_protocol_cache_disabled();
        }
    }

    /// Enable fetch interception.
    pub fn enable_request_intercept(&mut self) {
        self.protocol_request_interception_enabled = true;
    }

    /// Disable fetch interception.
    pub fn disable_request_intercept(&mut self) {
        self.protocol_request_interception_enabled = false;
    }

    /// Set the cache site key.
    #[cfg(feature = "_cache")]
    pub fn set_cache_site_key(&mut self, cache_site_key: Option<String>) {
        self.cache_site_key = cache_site_key;
    }

    /// Set the cache policy.
    #[cfg(feature = "_cache")]
    pub fn set_cache_policy(&mut self, cache_policy: Option<BasicCachePolicy>) {
        self.cache_policy = cache_policy;
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

    /// Blocklist-only script blocking.
    /// Returns true only when the URL matches an explicit blocklist condition.
    #[inline]
    fn should_block_script_blocklist_only(&self, url: &str) -> bool {
        // If analytics blocking is off, skip all analytics tries.
        let block_analytics = self.block_analytics;

        // 1) Explicit full-URL prefix trie (some rules are full URL prefixes).
        if block_analytics && spider_network_blocker::scripts::URL_IGNORE_TRIE.contains_prefix(url)
        {
            return true;
        }

        // 2) Custom website block list (explicit).
        if crate::handler::blockers::block_websites::block_website(url) {
            return true;
        }

        // 3) Path-based explicit tries / fallbacks.
        //
        // We run these on:
        // - path with leading slash ("/js/app.js")
        // - path without leading slash ("js/app.js")
        // - basename ("app.js") for filename-only rules (this is the fast "analytics.js" fallback)
        if let Some(path_with_slash) = Self::url_path_with_leading_slash(url) {
            // Remove query/fragment so matching stays stable.
            let p_slash = Self::strip_query_fragment(path_with_slash);
            let p_noslash = p_slash.strip_prefix('/').unwrap_or(p_slash);

            // Basename for filename-only lists.
            let base = match p_slash.rsplit('/').next() {
                Some(b) => b,
                None => p_slash,
            };

            // ---- Trie checks ----
            // Some tries store prefixes like "/cdn-cgi/..." (leading slash) OR "cdn-cgi/..." (no slash).
            if block_analytics && URL_IGNORE_TRIE_PATHS.contains_prefix(p_slash) {
                return true;
            }
            if block_analytics && URL_IGNORE_TRIE_PATHS.contains_prefix(p_noslash) {
                return true;
            }
            if block_analytics && URL_IGNORE_TRIE_PATHS.contains_prefix(base) {
                return true;
            }

            // Base-path ignore tries (framework noise / known ignorable script paths).
            // Note: these are explicit tries, so they are valid “blocklist-only” checks.
            if URL_IGNORE_SCRIPT_BASE_PATHS.contains_prefix(p_noslash) {
                return true;
            }

            // Style path ignores only when visuals are ignored.
            if self.ignore_visuals && URL_IGNORE_SCRIPT_STYLES_PATHS.contains_prefix(p_noslash) {
                return true;
            }
        }

        false
    }

    /// Extract the absolute URL path portion WITH the leading slash.
    ///
    /// Example:
    /// - "https://cdn.example.net/js/app.js?x=y" -> Some("/js/app.js?x=y")
    #[inline]
    fn url_path_with_leading_slash(url: &str) -> Option<&str> {
        // find scheme separator
        let bytes = url.as_bytes();
        let idx = memchr::memmem::find(bytes, b"//")?;
        let after_slashes = idx + 2;

        // find first slash after host
        let slash_rel = memchr::memchr(b'/', &bytes[after_slashes..])?;
        let slash_idx = after_slashes + slash_rel;

        if slash_idx < url.len() {
            Some(&url[slash_idx..])
        } else {
            None
        }
    }

    /// Strip query string and fragment from a path-ish string.
    ///
    /// Example:
    /// - "/a/b.js?x=1#y" -> "/a/b.js"
    #[inline]
    fn strip_query_fragment(s: &str) -> &str {
        match memchr::memchr2(b'?', b'#', s.as_bytes()) {
            Some(i) => &s[..i],
            None => s,
        }
    }

    /// Determine if the request should be skipped.
    #[inline]
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

                if let Some(position) = memchr::memrchr(b'.', request_url.as_bytes()) {
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
                            block_request = CaseInsensitiveString::from(
                                &request_url.as_bytes()[next_position..],
                            )
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
            block_ads(&event.request.url) || self.detect_ad(event)
        }
    }

    /// When adblock feature is disabled, this is a no-op.
    #[cfg(not(feature = "adblock"))]
    #[inline]
    fn detect_ad_if_enabled(&mut self, event: &EventRequestPaused, skip_networking: bool) -> bool {
        use crate::handler::blockers::block_websites::block_ads;
        if skip_networking {
            true
        } else {
            block_ads(&event.request.url)
        }
    }

    #[inline]
    /// Fail request
    fn fail_request_blocked(
        &mut self,
        request_id: &chromey_cdp::cdp::browser_protocol::fetch::RequestId,
    ) {
        let params = chromey_cdp::cdp::browser_protocol::fetch::FailRequestParams::new(
            request_id.clone(),
            chromey_cdp::cdp::browser_protocol::network::ErrorReason::BlockedByClient,
        );
        self.push_cdp_request(params);
    }

    #[inline]
    /// Fulfill request
    fn fulfill_request_empty_200(
        &mut self,
        request_id: &chromey_cdp::cdp::browser_protocol::fetch::RequestId,
    ) {
        let params = chromey_cdp::cdp::browser_protocol::fetch::FulfillRequestParams::new(
            request_id.clone(),
            200,
        );
        self.push_cdp_request(params);
    }

    #[cfg(feature = "_cache")]
    #[inline]
    /// Fulfill a paused Fetch request from cached bytes + header map.
    ///
    /// `headers` should be response headers (e.g. Content-Type, Cache-Control, etc).
    fn fulfill_request_from_cache(
        &mut self,
        request_id: &chromey_cdp::cdp::browser_protocol::fetch::RequestId,
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
                name: k.clone(),
                value: v.clone(),
            });
        }

        let mut params = FulfillRequestParams::new(request_id.clone(), status);

        // TODO: have this already encoded prior.
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
        request_id: &chromey_cdp::cdp::browser_protocol::fetch::RequestId,
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

    /// On fetch request paused interception.
    #[inline]
    pub fn on_fetch_request_paused(&mut self, event: &EventRequestPaused) {
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

        // Capture the CDP initiator type (set by Chrome on
        // `Network.requestWillBeSent`) before consuming the cached event.
        // Used by the legacy stylesheet heuristic below as an additive
        // fallback alongside the new `allow_first_party_*` flags — keeping
        // both keeps 2.48.2's third-party-with-unknown-initiator stylesheet
        // pass-through bug-compatible (strict superset of allowed traffic).
        // Cheap clone of an `Option<InitiatorType>` enum (no allocation).
        let initiator_type: Option<InitiatorType> = event
            .network_id
            .as_ref()
            .and_then(|nid| self.requests_will_be_sent.get(nid.as_ref()))
            .map(|(rwbs, _)| rwbs.initiator.r#type.clone());

        if let Some(network_id) = event.network_id.as_ref() {
            if let Some((request_will_be_sent, _)) =
                self.requests_will_be_sent.remove(network_id.as_ref())
            {
                self.on_request(&request_will_be_sent, Some(event.request_id.clone().into()));
            } else {
                self.request_id_to_interception_id.insert(
                    network_id.clone(),
                    (event.request_id.clone().into(), Instant::now()),
                );
            }
        }

        // From here on, we handle the full decision tree.
        let javascript_resource = event.resource_type == ResourceType::Script;
        let document_resource = event.resource_type == ResourceType::Document;
        let network_resource =
            !document_resource && crate::utils::is_data_resource(&event.resource_type);

        // Start with static / cheap skip checks.
        let mut skip_networking =
            self.block_all || IGNORE_NETWORKING_RESOURCE_MAP.contains(event.resource_type.as_ref());

        if event.resource_type == ResourceType::Prefetch && !self.block_prefetch {
            skip_networking = true;
        }

        // Also short-circuit if we've reloaded this document too many times.
        if !skip_networking {
            skip_networking = self.document_reload_tracker >= 3;
        }

        // Handle document redirect / masking and track xml documents.
        let (current_url_cow, had_replacer) =
            self.handle_document_replacement_and_tracking(event, document_resource);

        let current_url: &str = current_url_cow.as_ref();

        let blacklisted = self.is_blacklisted(current_url);

        if !self.blacklist_strict && blacklisted {
            skip_networking = true;
        }

        if !skip_networking {
            // Allow XSL for sitemap XML.
            if self.xml_document && current_url.ends_with(".xsl") {
                skip_networking = false;
            } else {
                skip_networking = self.should_skip_for_visuals_and_basic(&event.resource_type);
            }
        }

        // Skip ad detection for the user-requested top-level Document and
        // every step of its redirect chain. The crawler explicitly targets
        // this URL — fulfilling-empty-200 a page just because its host
        // matches an ad classifier breaks the user's intent (you can
        // legitimately want to scrape an ad page). Reproduced on
        // https://logrocket.com/careers, where the firewall ad list
        // flagged the host and chromey emitted a 17-byte stub for the
        // document; downstream sub-resources (script/img/iframe/etc.)
        // remain subject to ad blocking through the rest of the tree.
        //
        // Signals in short-circuit order (cheap → expensive):
        //   1. `redirected_request_id.is_some()` — explicit redirect hop
        //   2. `had_replacer` — chromey's masked-URL repair path
        //   3. `document_target_url.is_empty()` — very first nav, tracker
        //      not yet populated
        //   4. URL equality against the target — last because string
        //      compare is the only non-O(1) op (`handle_document_
        //      replacement_and_tracking` above just set the target to
        //      the current url, so this is the always-true fallback)
        //
        // Sub-resources (Script/Image/Font/Stylesheet/XHR/iframe content)
        // remain subject to ad blocking through the rest of the tree.
        let is_main_document_request = document_resource
            && (event.redirected_request_id.is_some()
                || had_replacer
                || self.document_target_url.is_empty()
                || event.request.url == self.document_target_url);
        if !is_main_document_request {
            skip_networking = self.detect_ad_if_enabled(event, skip_networking);
        }

        // Ignore embedded scripts, tracker stylesheets, and tracker images when only_html or ignore_visuals is set.
        if !skip_networking
            && self.block_javascript
            && (self.only_html || self.ignore_visuals)
            && (javascript_resource
                || document_resource
                || event.resource_type == ResourceType::Stylesheet
                || event.resource_type == ResourceType::Image)
        {
            skip_networking = ignore_script_embedded(current_url);
        }

        // Script policy: allow-by-default.
        // Block only if explicit block list patterns match.
        if !skip_networking && javascript_resource {
            skip_networking = self.should_block_script_blocklist_only(current_url);
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

        // whitelist 3rd party
        // not required unless explicit blocking.
        if skip_networking && javascript_resource && ALLOWED_MATCHER_3RD_PARTY.is_match(current_url)
        {
            skip_networking = false;
        }

        // check if the url is in the whitelist.
        if skip_networking && self.is_whitelisted(current_url) {
            skip_networking = false;
        }

        // First-party allow (default ON for stylesheets/javascript/visuals).
        //
        // `block_stylesheets` and `ignore_visuals` were originally coarse
        // "drop all" bandwidth optimizations, but modern SPAs (React/Next.js
        // with dynamic `import()`, AppFabric, requirejs-style loaders, etc.)
        // gate hydration on the `load` event of resources they themselves
        // load — blocking those leaves outer_html_bytes capturing only the
        // pre-hydration shell. To stay flexible without regressing the
        // bandwidth case, we use registrable-domain (eTLD+1) matching:
        // when a request is first-party to the page's primary frame, the
        // corresponding `allow_first_party_*` flag (default `true`) lets it
        // through; third-party requests still hit the original block path.
        //
        // Set the matching `allow_first_party_*` flag to `false` to restore
        // the strict "block ALL of this resource type" semantics.
        if skip_networking && !self.document_target_domain.is_empty() {
            let allow = match event.resource_type {
                ResourceType::Stylesheet => self.allow_first_party_stylesheets,
                ResourceType::Script => self.allow_first_party_javascript,
                _ if IGNORE_VISUAL_RESOURCE_MAP.contains(event.resource_type.as_ref()) => {
                    self.allow_first_party_visuals
                }
                _ => false,
            };
            if allow && self.is_first_party_url(current_url) {
                skip_networking = false;
            }
        }

        // Legacy stylesheet allow (kept as additive fallback for strict
        // bug-compat with chromey 2.48.2). For parser-dispatched
        // <link rel="stylesheet"> Chrome routinely fires
        // `Fetch.requestPaused` *before* the companion
        // `Network.requestWillBeSent`, so `initiator_type` is `None`; this
        // rescues those even on cross-origin CDNs where the eTLD+1 differs
        // from the page (e.g. intuit.com → intuitcdn.net). Tracker CSS
        // injected by JS that runs after parser yield carries
        // `Some(InitiatorType::Script)` and stays blocked here.
        //
        // Gated on `allow_first_party_stylesheets=true` so callers who opt
        // out of first-party allow get a strict "block ALL stylesheets"
        // semantics with no surprises from the heuristic side-channel.
        if skip_networking
            && self.allow_first_party_stylesheets
            && self.block_stylesheets
            && event.resource_type == ResourceType::Stylesheet
            && !matches!(initiator_type, Some(InitiatorType::Script))
        {
            skip_networking = false;
        }

        if self.blacklist_strict && blacklisted {
            skip_networking = true;
        }

        if skip_networking {
            tracing::debug!("Blocked: {:?} - {}", event.resource_type, current_url);
            self.fulfill_request_empty_200(&event.request_id);
        } else {
            #[cfg(feature = "_cache")]
            {
                if let (Some(policy), Some(cache_site_key)) =
                    (self.cache_policy.as_ref(), self.cache_site_key.as_deref())
                {
                    let current_url = format!("{}:{}", event.request.method, &current_url);

                    if let Some((res, cache_policy)) =
                        crate::cache::remote::get_session_cache_item(cache_site_key, &current_url)
                    {
                        if policy.allows_cached(&cache_policy) {
                            tracing::debug!(
                                "Remote Cached: {:?} - {}",
                                &event.resource_type,
                                &current_url
                            );
                            let flat_headers = crate::http::headers_from_multi(&res.headers);
                            return self.fulfill_request_from_cache(
                                &event.request_id,
                                &res.body,
                                &flat_headers,
                                res.status as i64,
                            );
                        }
                    }
                }
            }

            // check our frame cache for the run.
            tracing::debug!("Allowed: {:?} - {}", event.resource_type, current_url);
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

    /// Shared "visuals + basic blocking" logic.
    ///
    /// IMPORTANT: Scripts are NOT blocked here anymore.
    /// Scripts are allowed by default and only blocked via explicit blocklists
    /// (should_block_script_blocklist_only / adblock / block_websites / intercept_manager).
    #[inline]
    fn should_skip_for_visuals_and_basic(&self, resource_type: &ResourceType) -> bool {
        (self.ignore_visuals && IGNORE_VISUAL_RESOURCE_MAP.contains(resource_type.as_ref()))
            || (self.block_stylesheets && *resource_type == ResourceType::Stylesheet)
    }

    /// Does the network manager have a target domain?
    pub fn has_target_domain(&self) -> bool {
        !self.document_target_url.is_empty()
    }

    /// True when `url`'s registrable domain matches the page's primary
    /// frame. Empty `document_target_domain` (no nav yet, or a redirect
    /// reset) returns `false` so we don't accidentally treat every URL
    /// as first-party.
    #[inline]
    fn is_first_party_url(&self, url: &str) -> bool {
        if self.document_target_domain.is_empty() {
            return false;
        }
        match host_and_rest(url) {
            Some((host, _)) => base_domain_from_host(host) == self.document_target_domain,
            None => false,
        }
    }

    /// Set the target page url for tracking.
    pub fn set_page_url(&mut self, page_target_url: String) {
        let host_base = host_and_rest(&page_target_url)
            .map(|(h, _)| base_domain_from_host(h))
            .unwrap_or("");

        self.document_target_domain = host_base.to_string();
        self.document_target_url = page_target_url;

        // Re-push the policy on navigation so the remote engine's first-party
        // origin tracks the new page. No-op unless `remote_local_policy` is on
        // and interception is active.
        self.emit_request_policy();
    }

    /// Clear the initial target domain on every navigation.
    pub fn clear_target_domain(&mut self) {
        self.document_reload_tracker = 0;
        self.document_target_url = Default::default();
        self.document_target_domain = Default::default();
    }

    /// Handles:
    /// - document reload tracking (`document_reload_tracker`)
    /// - redirect masking / replacement
    /// - xml document detection (`xml_document`)
    /// - `document_target_url` updates
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
            if self.document_target_url == current_url {
                self.document_reload_tracker += 1;
            } else if !self.document_target_url.is_empty() && event.redirected_request_id.is_some()
            {
                let (http_document_replacement, mut https_document_replacement) =
                    if self.document_target_url.starts_with("http://") {
                        (
                            self.document_target_url.replacen("http://", "http//", 1),
                            self.document_target_url.replacen("http://", "https://", 1),
                        )
                    } else {
                        (
                            self.document_target_url.replacen("https://", "https//", 1),
                            self.document_target_url.replacen("https://", "http://", 1),
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

            if self.document_target_url.is_empty() && current_url.ends_with(".xml") {
                self.xml_document = true;
            }

            // Track last seen document URL.
            self.document_target_url = event.request.url.clone();
            self.document_target_domain = host_and_rest(&self.document_target_url)
                .map(|(h, _)| base_domain_from_host(h).to_string())
                .unwrap_or_default();
        }

        let current_url_cow = match replacer {
            Some(r) => Cow::Owned(r),
            None => Cow::Borrowed(event.request.url.as_str()),
        };

        let had_replacer = matches!(current_url_cow, Cow::Owned(_));
        (current_url_cow, had_replacer)
    }

    /// Perform a page intercept for chrome using the adblock engine.
    /// Uses the custom engine when user-supplied filter rules are configured,
    /// otherwise falls back to the global default engine with built-in patterns.
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

                // When adblock_easylist is enabled, EasyList + EasyPrivacy are
                // embedded at build time for zero-cost runtime loading.
                #[cfg(feature = "adblock_easylist")]
                {
                    static EASYLIST: &str = include_str!(concat!(env!("OUT_DIR"), "/easylist.txt"));
                    static EASYPRIVACY: &str = include_str!(concat!(env!("OUT_DIR"), "/easyprivacy.txt"));

                    if !EASYLIST.is_empty() {
                        filter_set.add_filter_list(EASYLIST, rules);
                    }
                    if !EASYPRIVACY.is_empty() {
                        filter_set.add_filter_list(EASYPRIVACY, rules);
                    }
                }

                Engine::from_filter_set(filter_set, true)
            };
        }

        let blockable = event.resource_type == ResourceType::Script
            || event.resource_type == ResourceType::Image
            || event.resource_type == ResourceType::Media
            || event.resource_type == ResourceType::Stylesheet
            || event.resource_type == ResourceType::Document
            || event.resource_type == ResourceType::Fetch
            || event.resource_type == ResourceType::Xhr;

        if !blockable {
            return false;
        }

        let u = &event.request.url;

        let source_domain = if self.document_target_domain.is_empty() {
            "example.com"
        } else {
            &self.document_target_domain
        };

        // Fast hostname extraction without full URL parsing.
        // preparsed(url, request_hostname, source_hostname, type, third_party)
        let hostname = u
            .strip_prefix("https://")
            .or_else(|| u.strip_prefix("http://"))
            .and_then(|rest| rest.split('/').next())
            // Strip userinfo (user:pass@) if present.
            .map(
                |authority| match memchr::memrchr(b'@', authority.as_bytes()) {
                    Some(i) => &authority[i + 1..],
                    None => authority,
                },
            )
            // Strip port (:8080) if present.
            .and_then(|host_port| host_port.split(':').next())
            .unwrap_or(source_domain);

        let resource_type_str = match event.resource_type {
            ResourceType::Script => "script",
            ResourceType::Image => "image",
            ResourceType::Media => "media",
            ResourceType::Stylesheet => "stylesheet",
            ResourceType::Document => "document",
            ResourceType::Fetch => "fetch",
            ResourceType::Xhr => "xhr",
            _ => "other",
        };

        let request = adblock::request::Request::preparsed(
            u,
            hostname,
            source_domain,
            resource_type_str,
            !event.request.is_same_site.unwrap_or_default(),
        );

        let engine: &Engine = match self.adblock_engine.as_ref() {
            Some(custom) => custom,
            None => &AD_ENGINE,
        };

        engine.check_network_request(&request).matched
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

    /// Set the page offline network emulation condition.
    pub fn set_offline_mode(&mut self, value: bool) {
        if self.offline == value {
            return;
        }
        self.offline = value;
        if let Ok(condition) = NetworkConditions::builder()
            .url_pattern("")
            .latency(0)
            .download_throughput(-1.)
            .upload_throughput(-1.)
            .build()
        {
            if let Ok(network) = EmulateNetworkConditionsByRuleParams::builder()
                .offline(self.offline)
                .matched_network_condition(condition)
                .build()
            {
                self.push_cdp_request(network);
            }
        }
    }

    /// Request interception doesn't happen for data URLs with Network Service.
    pub fn on_request_will_be_sent(&mut self, event: &EventRequestWillBeSent) {
        if self.protocol_request_interception_enabled && !event.request.url.starts_with("data:") {
            if let Some((interception_id, _)) = self
                .request_id_to_interception_id
                .remove(event.request_id.as_ref())
            {
                self.on_request(event, Some(interception_id));
            } else {
                self.requests_will_be_sent
                    .insert(event.request_id.clone(), (event.clone(), Instant::now()));
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

    /// On network loading finished.
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

    /// On network loading failed.
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

    /// On request will be sent.
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

                            if !fixed_location.is_empty() {
                                if let Some(resp) = request.response.as_mut() {
                                    resp.headers.0["Location"] =
                                        serde_json::Value::String(fixed_location.clone());
                                }
                            }

                            redirect_location = Some(fixed_location);
                        }
                    }
                }

                {
                    let mut redirect_resp = redirect_resp.clone();

                    if let Some(redirect_location) = redirect_location {
                        if !redirect_location.is_empty() {
                            redirect_resp.headers.0["Location"] =
                                serde_json::Value::String(redirect_location);
                        }
                    }

                    self.handle_request_redirect(&mut request, redirect_resp);
                }

                redirect_chain = std::mem::take(&mut request.redirect_chain);
                redirect_chain.push(request);
            }
        }

        // Redirect cap: applies only to Document-type hops and only when
        // `max_redirects` is set. Sub-resource chains are untouched.
        if let Some(cap) = self.max_redirects {
            let is_document = matches!(event.r#type, Some(ResourceType::Document));
            if is_document && redirect_chain.len() > cap {
                let mut failed = HttpRequest::new(
                    event.request_id.clone(),
                    event.frame_id.clone(),
                    interception_id,
                    self.user_request_interception_enabled,
                    redirect_chain,
                );
                failed.url = Some(event.request.url.clone());
                failed.method = Some(event.request.method.clone());
                failed.failure_text = Some("net::ERR_TOO_MANY_REDIRECTS".into());
                self.push_cdp_request(
                    chromey_cdp::cdp::browser_protocol::page::StopLoadingParams::default(),
                );
                self.queued_events
                    .push_back(NetworkEvent::RequestFailed(failed));
                return;
            }
        }

        let request = HttpRequest::new(
            event.request_id.clone(),
            event.frame_id.clone(),
            interception_id,
            self.user_request_interception_enabled,
            redirect_chain,
        );

        let rid = event.request_id.clone();
        self.queued_events
            .push_back(NetworkEvent::Request(rid.clone()));
        self.requests.insert(rid, request);
    }

    /// Handle request redirect.
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
    /// Send a CDP request.
    SendCdpRequest((MethodId, serde_json::Value)),
    /// Request.
    Request(RequestId),
    /// Response
    Response(RequestId),
    /// Request failed.
    RequestFailed(HttpRequest),
    /// Request finished.
    RequestFinished(HttpRequest),
    /// Bytes consumed.
    BytesConsumed(u64),
}

#[cfg(test)]
mod tests {
    use super::ALLOWED_MATCHER_3RD_PARTY;
    use crate::handler::network::NetworkManager;
    use std::time::Duration;

    #[test]
    fn test_allowed_matcher_3rd_party() {
        // Should be allowed (matches "/cdn-cgi/challenge-platform/")
        let cf_challenge = "https://www.something.com.ba/cdn-cgi/challenge-platform/h/g/orchestrate/chl_page/v1?ray=9abf7b523d90987e";
        assert!(
            ALLOWED_MATCHER_3RD_PARTY.is_match(cf_challenge),
            "expected Cloudflare challenge script to be allowed"
        );

        // Should NOT be allowed (not in allow-list)
        let cf_insights = "https://static.cloudflareinsights.com/beacon.min.js/vcd15cbe7772f49c399c6a5babf22c1241717689176015";
        assert!(
            !ALLOWED_MATCHER_3RD_PARTY.is_match(cf_insights),
            "expected Cloudflare Insights beacon to remain blocked (not in allow-list)"
        );

        // A couple sanity checks for existing allow patterns
        assert!(ALLOWED_MATCHER_3RD_PARTY.is_match("https://js.stripe.com/v3/"));
        assert!(ALLOWED_MATCHER_3RD_PARTY
            .is_match("https://www.google.com/recaptcha/api.js?render=explicit"));
        assert!(ALLOWED_MATCHER_3RD_PARTY.is_match("https://code.jquery.com/jquery-3.7.1.min.js"));
    }

    #[test]
    fn test_script_allowed_by_default_when_not_blocklisted() {
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url(
            "https://forum.cursor.com/t/is-2000-fast-requests-the-maximum/51085".to_string(),
        );

        // A random script that should not match your block tries.
        let ok = "https://cdn.example.net/assets/some-app-bundle-12345.js";
        assert!(
            !nm.should_block_script_blocklist_only(ok),
            "expected non-blocklisted script to be allowed"
        );
    }

    #[test]
    fn test_script_blocked_when_matches_ignore_trie_or_blocklist() {
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url(
            "https://forum.cursor.com/t/is-2000-fast-requests-the-maximum/51085".to_string(),
        );

        // This should match URL_IGNORE_TRIE_PATHS fallback ("analytics.js") logic.
        let bad = "https://cdn.example.net/js/analytics.js";
        assert!(
            nm.should_block_script_blocklist_only(bad),
            "expected analytics.js to be blocklisted"
        );
    }

    #[test]
    fn test_allowed_matcher_3rd_party_sanity() {
        // Should be allowed (matches "/cdn-cgi/challenge-platform/")
        let cf_challenge = "https://www.something.com.ba/cdn-cgi/challenge-platform/h/g/orchestrate/chl_page/v1?ray=9abf7b523d90987e";
        assert!(
            ALLOWED_MATCHER_3RD_PARTY.is_match(cf_challenge),
            "expected Cloudflare challenge script to be allowed"
        );

        // Should NOT be allowed (not in allow-list)
        let cf_insights = "https://static.cloudflareinsights.com/beacon.min.js/vcd15cbe7772f49c399c6a5babf22c1241717689176015";
        assert!(
            !ALLOWED_MATCHER_3RD_PARTY.is_match(cf_insights),
            "expected Cloudflare Insights beacon to remain blocked (not in allow-list)"
        );

        assert!(ALLOWED_MATCHER_3RD_PARTY.is_match("https://js.stripe.com/v3/"));
        assert!(ALLOWED_MATCHER_3RD_PARTY
            .is_match("https://www.google.com/recaptcha/api.js?render=explicit"));
        assert!(ALLOWED_MATCHER_3RD_PARTY.is_match("https://code.jquery.com/jquery-3.7.1.min.js"));
    }
    #[test]
    fn test_dynamic_blacklist_blocks_url() {
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://example.com/".to_string());

        nm.set_blacklist_patterns(["static.cloudflareinsights.com", "googletagmanager.com"]);
        assert!(nm.is_blacklisted("https://static.cloudflareinsights.com/beacon.min.js"));
        assert!(nm.is_blacklisted("https://www.googletagmanager.com/gtm.js?id=GTM-XXXX"));

        assert!(!nm.is_blacklisted("https://cdn.example.net/assets/app.js"));
    }

    #[test]
    fn test_blacklist_strict_wins_over_whitelist() {
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://example.com/".to_string());

        // Same URL in both lists.
        nm.set_blacklist_patterns(["beacon.min.js"]);
        nm.set_whitelist_patterns(["beacon.min.js"]);

        nm.set_blacklist_strict(true);

        let u = "https://static.cloudflareinsights.com/beacon.min.js";
        assert!(nm.is_whitelisted(u));
        assert!(nm.is_blacklisted(u));

        // In strict mode, it should still be considered blocked at decision time.
        // (We can only directly assert the matchers here; the decision logic is exercised in integration.)
        assert!(nm.blacklist_strict);
    }

    #[cfg(feature = "adblock")]
    fn make_request_paused(
        url: &str,
        resource_type: chromey_cdp::cdp::browser_protocol::network::ResourceType,
        is_same_site: bool,
    ) -> chromey_cdp::cdp::browser_protocol::fetch::EventRequestPaused {
        use chromey_cdp::cdp::browser_protocol::fetch::EventRequestPaused;
        use chromey_cdp::cdp::browser_protocol::network::{
            Headers, Request, RequestReferrerPolicy, ResourcePriority,
        };

        EventRequestPaused {
            request_id: chromey_cdp::cdp::browser_protocol::network::RequestId::from(
                "test-req".to_string(),
            )
            .into(),
            request: Request {
                url: url.to_string(),
                method: "GET".to_string(),
                headers: Headers::new(serde_json::Value::Object(Default::default())),
                initial_priority: ResourcePriority::Medium,
                referrer_policy: RequestReferrerPolicy::NoReferrer,
                url_fragment: None,
                has_post_data: None,
                post_data_entries: None,
                mixed_content_type: None,
                is_link_preload: None,
                trust_token_params: None,
                is_same_site: Some(is_same_site),
                is_ad_related: None,
            },
            frame_id: chromey_cdp::cdp::browser_protocol::page::FrameId::from(
                "frame1".to_string(),
            ),
            resource_type,
            response_error_reason: None,
            response_status_code: None,
            response_status_text: None,
            response_headers: None,
            network_id: None,
            redirected_request_id: None,
        }
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_detect_ad_blocks_known_tracker_scripts() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://www.wine-searcher.com/".to_string());

        let event = make_request_paused(
            "https://www.googletagmanager.com/gtm.js?id=GTM-XXXX",
            ResourceType::Script,
            false,
        );

        assert!(
            nm.detect_ad(&event),
            "googletagmanager.com script should be detected as ad"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_detect_ad_allows_legitimate_scripts() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://www.mylegitsite-test.com/".to_string());

        let event = make_request_paused(
            "https://www.mylegitsite-test.com/static/js/app-bundle.js",
            ResourceType::Script,
            true,
        );

        assert!(
            !nm.detect_ad(&event),
            "legitimate first-party app bundle should not be blocked"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_detect_ad_uses_source_domain() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://www.wine-searcher.com/some-page".to_string());

        assert!(
            !nm.document_target_domain.is_empty(),
            "document_target_domain should be set after set_page_url"
        );

        let event = make_request_paused(
            "https://www.google-analytics.com/analytics.js",
            ResourceType::Script,
            false,
        );

        assert!(
            nm.detect_ad(&event),
            "google-analytics.com should be blocked as tracker"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_custom_adblock_engine_takes_precedence() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://example.com/".to_string());

        // Build a custom engine with a specific rule.
        let mut filter_set = adblock::lists::FilterSet::new(false);
        let mut opts = adblock::lists::ParseOptions::default();
        opts.rule_types = adblock::lists::RuleTypes::All;
        filter_set.add_filters(["||custom-tracker.example.net^"], opts);
        let engine = adblock::Engine::from_filter_set(filter_set, true);
        nm.set_adblock_engine(std::sync::Arc::new(engine));

        let event = make_request_paused(
            "https://custom-tracker.example.net/pixel.js",
            ResourceType::Script,
            false,
        );

        assert!(
            nm.detect_ad(&event),
            "custom engine rule should block custom-tracker.example.net"
        );
    }

    /// Helper: run a URL through the full `on_fetch_request_paused` pipeline
    /// and return whether it was blocked (true) or allowed (false).
    #[cfg(feature = "adblock")]
    fn run_full_interception(
        nm: &mut NetworkManager,
        url: &str,
        resource_type: chromey_cdp::cdp::browser_protocol::network::ResourceType,
        is_same_site: bool,
    ) -> bool {
        use super::NetworkEvent;

        // Drain any prior events.
        while nm.poll().is_some() {}

        let event = make_request_paused(url, resource_type, is_same_site);
        nm.on_fetch_request_paused(&event);

        // Check what was emitted: Fetch.fulfillRequest = blocked, Fetch.continueRequest = allowed.
        let mut blocked = false;
        while let Some(ev) = nm.poll() {
            if let NetworkEvent::SendCdpRequest((method, _)) = &ev {
                let m: &str = method.as_ref();
                if m == "Fetch.fulfillRequest" || m == "Fetch.failRequest" {
                    blocked = true;
                }
            }
        }
        blocked
    }

    // ── End-to-end interception tests ───────────────────────────────────

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_tracker_script_blocked() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://www.wine-searcher.com/".to_string());

        assert!(
            run_full_interception(
                &mut nm,
                "https://www.googletagmanager.com/gtm.js?id=GTM-XXXX",
                ResourceType::Script,
                false,
            ),
            "GTM script should be blocked through full pipeline"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_legitimate_script_allowed() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://www.mylegitsite-test.com/".to_string());

        assert!(
            !run_full_interception(
                &mut nm,
                "https://www.mylegitsite-test.com/static/js/app-bundle.js",
                ResourceType::Script,
                true,
            ),
            "legitimate first-party script should be allowed through full pipeline"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_analytics_xhr_blocked() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://example.org/".to_string());

        assert!(
            run_full_interception(
                &mut nm,
                "https://www.google-analytics.com/g/collect?v=2&tid=UA-123",
                ResourceType::Xhr,
                false,
            ),
            "Google Analytics XHR should be blocked through full pipeline"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_whitelisted_overrides_adblock() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://example.org/".to_string());
        nm.set_whitelist_patterns(["googletagmanager.com"]);

        // GTM would normally be blocked by adblock, but whitelist overrides.
        assert!(
            !run_full_interception(
                &mut nm,
                "https://www.googletagmanager.com/gtm.js?id=GTM-TEST",
                ResourceType::Script,
                false,
            ),
            "whitelisted tracker should be allowed even when adblock would block it"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_blacklist_strict_overrides_whitelist() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://example.org/".to_string());
        nm.set_blacklist_patterns(["cdn.example.net/evil.js"]);
        nm.set_whitelist_patterns(["cdn.example.net/evil.js"]);
        nm.set_blacklist_strict(true);

        assert!(
            run_full_interception(
                &mut nm,
                "https://cdn.example.net/evil.js",
                ResourceType::Script,
                false,
            ),
            "strict blacklist should win over whitelist"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_first_party_stylesheet_passes_when_block_stylesheets_on() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://developer.intuit.com/".to_string());
        nm.block_stylesheets = true;

        assert!(
            !run_full_interception(
                &mut nm,
                "https://developer.intuit.com/static/app.css",
                ResourceType::Stylesheet,
                true,
            ),
            "first-party CSS must pass when allow_first_party_stylesheets default-true"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_first_party_stylesheet_blocked_when_allow_disabled() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://developer.intuit.com/".to_string());
        nm.block_stylesheets = true;
        nm.allow_first_party_stylesheets = false;

        assert!(
            run_full_interception(
                &mut nm,
                "https://developer.intuit.com/static/app.css",
                ResourceType::Stylesheet,
                true,
            ),
            "first-party CSS must be blocked when allow_first_party_stylesheets=false"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_third_party_stylesheet_still_blocked_with_default_allow() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        // requestWillBeSent fired with `initiator.type = "script"` —
        // disqualifies the legacy heuristic fallback. Default first-party
        // allow is on but this URL is third-party, so it should still block.
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://developer.intuit.com/".to_string());
        nm.block_stylesheets = true;
        // Required for `on_request_will_be_sent` to actually cache the
        // RWBS event into `requests_will_be_sent` (otherwise it dispatches
        // straight to `on_request` and the initiator lookup misses).
        nm.protocol_request_interception_enabled = true;

        let rwbs_url = "https://tracker.evil.example/track.css";
        let rwbs_json = serde_json::json!({
            "requestId": "tp-css-1",
            "loaderId": "test-loader",
            "documentURL": "https://developer.intuit.com/",
            "request": {
                "url": rwbs_url,
                "method": "GET",
                "headers": {},
                "initialPriority": "Medium",
                "referrerPolicy": "no-referrer"
            },
            "timestamp": 0.0,
            "wallTime": 0.0,
            "initiator": { "type": "script" },
            "redirectHasExtraInfo": false,
            "type": "Stylesheet",
            "frameId": "frame1"
        });
        let rwbs_event: chromey_cdp::cdp::browser_protocol::network::EventRequestWillBeSent =
            serde_json::from_value(rwbs_json).unwrap();
        nm.on_request_will_be_sent(&rwbs_event);

        // Use the same requestId in the requestPaused event so the
        // initiator capture finds the cached RWBS entry.
        use super::NetworkEvent;
        while nm.poll().is_some() {}
        let mut paused_event = make_request_paused(rwbs_url, ResourceType::Stylesheet, false);
        paused_event.network_id = Some(
            chromey_cdp::cdp::browser_protocol::network::RequestId::from(
                "tp-css-1".to_string(),
            ),
        );
        nm.on_fetch_request_paused(&paused_event);

        let mut blocked = false;
        while let Some(ev) = nm.poll() {
            if let NetworkEvent::SendCdpRequest((method, _)) = &ev {
                let m: &str = method.as_ref();
                if m == "Fetch.fulfillRequest" || m == "Fetch.failRequest" {
                    blocked = true;
                }
            }
        }
        assert!(
            blocked,
            "third-party Script-initiated CSS must remain blocked"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_first_party_image_passes_when_ignore_visuals_on() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://shop.example/".to_string());
        nm.ignore_visuals = true;

        assert!(
            !run_full_interception(
                &mut nm,
                "https://shop.example/img/hero.png",
                ResourceType::Image,
                true,
            ),
            "first-party image must pass when allow_first_party_visuals default-true"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_third_party_image_blocked_when_ignore_visuals_on() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://shop.example/".to_string());
        nm.ignore_visuals = true;

        assert!(
            run_full_interception(
                &mut nm,
                "https://cdn.thirdparty.io/banner.png",
                ResourceType::Image,
                false,
            ),
            "third-party image must remain blocked when ignore_visuals=true"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_first_party_document_not_blocked() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://www.nytimes.com/".to_string());

        assert!(
            !run_full_interception(
                &mut nm,
                "https://www.nytimes.com/2024/article.html",
                ResourceType::Document,
                true,
            ),
            "first-party document navigation should never be blocked"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_custom_engine_blocks_through_pipeline() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://mysite.com/".to_string());

        let mut filter_set = adblock::lists::FilterSet::new(false);
        let mut opts = adblock::lists::ParseOptions::default();
        opts.rule_types = adblock::lists::RuleTypes::All;
        filter_set.add_filters(["||evil-cdn.example.net^$script"], opts);
        let engine = adblock::Engine::from_filter_set(filter_set, true);
        nm.set_adblock_engine(std::sync::Arc::new(engine));

        assert!(
            run_full_interception(
                &mut nm,
                "https://evil-cdn.example.net/tracker.js",
                ResourceType::Script,
                false,
            ),
            "custom engine rule should block through full pipeline"
        );

        // Legitimate script on the same site should still pass.
        assert!(
            !run_full_interception(
                &mut nm,
                "https://mysite.com/app.js",
                ResourceType::Script,
                true,
            ),
            "first-party script should still be allowed with custom engine"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_ad_image_blocked() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://www.mylegitsite-test.com/".to_string());

        // Ad tracking pixel should be blocked via adblock pattern or trie.
        assert!(
            run_full_interception(
                &mut nm,
                "https://googleads.g.doubleclick.net/pagead/viewthroughconversion/123/?random=456",
                ResourceType::Image,
                false,
            ),
            "doubleclick ad image/tracking pixel should be blocked"
        );

        // Legitimate first-party image should pass.
        assert!(
            !run_full_interception(
                &mut nm,
                "https://www.mylegitsite-test.com/images/logo.png",
                ResourceType::Image,
                true,
            ),
            "legitimate first-party image should not be blocked"
        );
    }

    #[cfg(feature = "adblock")]
    #[test]
    fn test_e2e_hostname_with_userinfo() {
        use chromey_cdp::cdp::browser_protocol::network::ResourceType;

        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://example.org/".to_string());

        // URL with userinfo should still correctly identify googletagmanager.com.
        assert!(
            run_full_interception(
                &mut nm,
                "https://user:pass@www.googletagmanager.com/gtm.js?id=GTM-XXXX",
                ResourceType::Script,
                false,
            ),
            "tracker URL with userinfo should still be blocked"
        );
    }

    #[test]
    fn test_blacklist_non_strict_allows_whitelist_override() {
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.set_page_url("https://example.com/".to_string());

        nm.set_blacklist_patterns(["beacon.min.js"]);
        nm.set_whitelist_patterns(["beacon.min.js"]);

        nm.set_blacklist_strict(false);

        let u = "https://static.cloudflareinsights.com/beacon.min.js";
        assert!(nm.is_blacklisted(u));
        assert!(nm.is_whitelisted(u));
        assert!(!nm.blacklist_strict);
    }

    // ── max_redirects enforcement ───────────────────────────────────────
    //
    // The redirect cap short-circuits in NetworkManager::on_request when a
    // Document-type chain exceeds the configured limit. We drive it via the
    // public on_request_will_be_sent entry point by deserializing synthetic
    // events — builder APIs exist but require every non-optional field, and
    // JSON is less fragile to cdp schema additions.

    fn make_request_will_be_sent(
        request_id: &str,
        url: &str,
        resource_type: &str,
        redirect_from_url: Option<&str>,
    ) -> chromey_cdp::cdp::browser_protocol::network::EventRequestWillBeSent {
        let mut v = serde_json::json!({
            "requestId": request_id,
            "loaderId": "test-loader",
            "documentURL": url,
            "request": {
                "url": url,
                "method": "GET",
                "headers": {},
                "initialPriority": "Medium",
                "referrerPolicy": "no-referrer"
            },
            "timestamp": 0.0,
            "wallTime": 0.0,
            "initiator": { "type": "other" },
            "redirectHasExtraInfo": false,
            "type": resource_type,
            "frameId": "frame1"
        });
        if let Some(from) = redirect_from_url {
            v["redirectResponse"] = serde_json::json!({
                "url": from,
                "status": 302,
                "statusText": "Found",
                "headers": { "Location": url },
                "mimeType": "text/html",
                "charset": "",
                "connectionReused": false,
                "connectionId": 0.0,
                "encodedDataLength": 0.0,
                "securityState": "unknown"
            });
        }
        serde_json::from_value(v).expect("EventRequestWillBeSent should deserialize")
    }

    fn drain_too_many_redirects(nm: &mut NetworkManager) -> Option<super::HttpRequest> {
        while let Some(ev) = nm.poll() {
            if let super::NetworkEvent::RequestFailed(req) = ev {
                if req.failure_text.as_deref() == Some("net::ERR_TOO_MANY_REDIRECTS") {
                    return Some(req);
                }
            }
        }
        None
    }

    fn drain_stop_loading(nm: &mut NetworkManager) -> bool {
        while let Some(ev) = nm.poll() {
            if let super::NetworkEvent::SendCdpRequest((method, _)) = ev {
                let m: &str = method.as_ref();
                if m == "Page.stopLoading" {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn test_max_redirects_none_allows_unlimited_chain() {
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        // max_redirects left at its default (None).

        // 10 sequential Document hops sharing the same request_id.
        nm.on_request_will_be_sent(&make_request_will_be_sent(
            "r1",
            "https://example.com/0",
            "Document",
            None,
        ));
        for i in 1..10 {
            nm.on_request_will_be_sent(&make_request_will_be_sent(
                "r1",
                &format!("https://example.com/{i}"),
                "Document",
                Some(&format!("https://example.com/{}", i - 1)),
            ));
        }

        assert!(
            drain_too_many_redirects(&mut nm).is_none(),
            "no cap set: chain of 10 hops must not emit ERR_TOO_MANY_REDIRECTS"
        );
    }

    #[test]
    fn test_max_redirects_caps_document_chain() {
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.max_redirects = Some(3);

        // Initial request + 4 redirect hops. The 4th redirect (chain length 4 > 3)
        // must trip the cap.
        nm.on_request_will_be_sent(&make_request_will_be_sent(
            "r1",
            "https://example.com/0",
            "Document",
            None,
        ));
        for i in 1..=4 {
            nm.on_request_will_be_sent(&make_request_will_be_sent(
                "r1",
                &format!("https://example.com/{i}"),
                "Document",
                Some(&format!("https://example.com/{}", i - 1)),
            ));
        }

        let failed = drain_too_many_redirects(&mut nm)
            .expect("cap of 3 on a 4-hop chain must emit ERR_TOO_MANY_REDIRECTS");
        assert_eq!(
            failed.redirect_chain.len(),
            4,
            "failed request should preserve the full accumulated chain"
        );
        assert_eq!(
            failed.url.as_deref(),
            Some("https://example.com/4"),
            "failed request url should be the hop that tripped the cap"
        );

        // Second navigation after the cap is tripped must also schedule
        // Page.stopLoading to actually abort the tab.
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.max_redirects = Some(3);
        nm.on_request_will_be_sent(&make_request_will_be_sent(
            "r2",
            "https://example.com/0",
            "Document",
            None,
        ));
        for i in 1..=4 {
            nm.on_request_will_be_sent(&make_request_will_be_sent(
                "r2",
                &format!("https://example.com/{i}"),
                "Document",
                Some(&format!("https://example.com/{}", i - 1)),
            ));
        }
        assert!(
            drain_stop_loading(&mut nm),
            "cap hit must dispatch Page.stopLoading to abort navigation"
        );
    }

    #[test]
    fn test_max_redirects_ignores_subresources() {
        let mut nm = NetworkManager::new(false, Duration::from_secs(30));
        nm.max_redirects = Some(2);

        // A 5-hop script redirect chain — sub-resources are exempt by design.
        nm.on_request_will_be_sent(&make_request_will_be_sent(
            "s1",
            "https://cdn.example.com/0.js",
            "Script",
            None,
        ));
        for i in 1..=5 {
            nm.on_request_will_be_sent(&make_request_will_be_sent(
                "s1",
                &format!("https://cdn.example.com/{i}.js"),
                "Script",
                Some(&format!("https://cdn.example.com/{}.js", i - 1)),
            ));
        }

        assert!(
            drain_too_many_redirects(&mut nm).is_none(),
            "sub-resource redirect chains must never be capped"
        );
    }
}
