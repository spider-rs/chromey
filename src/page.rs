use std::path::Path;
use std::sync::Arc;

use chromiumoxide_cdp::cdp::browser_protocol::accessibility::{
    GetFullAxTreeReturns, GetPartialAxTreeReturns,
};
use chromiumoxide_cdp::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams,
    SetGeolocationOverrideParams, SetHardwareConcurrencyOverrideParams, SetLocaleOverrideParams,
    SetScriptExecutionDisabledParams, SetTimezoneOverrideParams, UserAgentBrandVersion,
    UserAgentMetadata,
};
use chromiumoxide_cdp::cdp::browser_protocol::input::{
    DispatchDragEventType, DispatchMouseEventParams, DispatchMouseEventType, DragData, MouseButton,
};
use chromiumoxide_cdp::cdp::browser_protocol::network::{
    BlockPattern, Cookie, CookieParam, DeleteCookiesParams, GetCookiesParams, SetBlockedUrLsParams,
    SetCookiesParams, SetExtraHttpHeadersParams, SetUserAgentOverrideParams, TimeSinceEpoch,
};
use chromiumoxide_cdp::cdp::browser_protocol::page::*;
use chromiumoxide_cdp::cdp::browser_protocol::performance::{GetMetricsParams, Metric};
use chromiumoxide_cdp::cdp::browser_protocol::storage::ClearCookiesParams;
use chromiumoxide_cdp::cdp::browser_protocol::target::{SessionId, TargetId};
use chromiumoxide_cdp::cdp::browser_protocol::{dom::*, emulation};
use chromiumoxide_cdp::cdp::js_protocol;
use chromiumoxide_cdp::cdp::js_protocol::debugger::GetScriptSourceParams;
use chromiumoxide_cdp::cdp::js_protocol::runtime::{
    AddBindingParams, CallFunctionOnParams, EvaluateParams, ExecutionContextId, RemoteObjectType,
    ScriptId,
};
use chromiumoxide_cdp::cdp::{browser_protocol, IntoEventKind};
use chromiumoxide_types::*;
use futures_util::{stream, StreamExt};
use spider_fingerprint::configs::{AgentOs, Tier};
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::oneshot::channel as oneshot_channel;

use crate::auth::Credentials;
use crate::element::Element;
use crate::error::{CdpError, Result};
use crate::handler::commandfuture::CommandFuture;
use crate::handler::httpfuture::HttpFuture;
use crate::handler::target::{GetName, GetParent, GetUrl, TargetMessage};
use crate::handler::PageInner;
use crate::javascript::extract::{
    generate_marker_js, FULL_XML_SERIALIZER_JS, OUTER_HTML, OUTER_HTML_BYTE_LEN, OUTER_HTML_LEN,
};
use crate::js::{Evaluation, EvaluationResult};
use crate::layout::{Delta, Point, ScrollBehavior};
use crate::listeners::{EventListenerRequest, EventStream};
use crate::navigate_deadline::NavigateWithDeadlineParams;
use crate::webmcp::{CallToolParams, ListToolsParams, Tool};
use crate::{utils, ArcHttpRequest};
use aho_corasick::AhoCorasick;

lazy_static::lazy_static! {
    /// Determine the platform used.
    static ref PLATFORM_MATCHER: AhoCorasick = {
         AhoCorasick::builder()
        .match_kind(aho_corasick::MatchKind::LeftmostFirst)
        .ascii_case_insensitive(true)
        .build([
            "ipad",        // 0
            "ipod",        // 1
            "iphone",      // 2
            "android",     // 3
            "macintosh",   // 4
            "mac os x",    // 5
            "windows",     // 6
            "linux",       // 7
        ])
        .expect("valid pattern")
    };
}

/// Determine the platform used from a user-agent.
pub fn platform_from_user_agent(user_agent: &str) -> &'static str {
    match PLATFORM_MATCHER.find(user_agent) {
        Some(mat) => match mat.pattern().as_usize() {
            0 => "iPad",
            1 => "iPod",
            2 => "iPhone",
            3 => "Linux armv8l",
            4 | 5 => "MacIntel",
            6 => "Win32",
            7 => "Linux x86_64",
            _ => "",
        },
        None => "",
    }
}

/// Collect scope nodeIds you may want to run DOM.querySelector(All) agains.
fn collect_scopes_iterative(root: &Node) -> Vec<NodeId> {
    use hashbrown::HashSet;

    let mut scopes = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut stack: Vec<&Node> = Vec::new();

    stack.push(root);

    while let Some(n) = stack.pop() {
        if seen.insert(n.node_id) {
            scopes.push(n.node_id);
        }

        if let Some(shadow_roots) = n.shadow_roots.as_ref() {
            // push in reverse to preserve roughly DOM order (optional)
            for sr in shadow_roots.iter().rev() {
                stack.push(sr);
            }
        }

        if let Some(cd) = n.content_document.as_ref() {
            stack.push(cd);
        }

        if let Some(children) = n.children.as_ref() {
            for c in children.iter().rev() {
                stack.push(c);
            }
        }
    }

    scopes
}

#[derive(Debug, Clone)]
pub struct Page {
    inner: Arc<PageInner>,
}

impl Page {
    /// Add a custom script to eval on new document immediately.
    pub async fn add_script_to_evaluate_immediately_on_new_document(
        &self,
        source: Option<String>,
    ) -> Result<&Self> {
        if source.is_some() {
            let source = source.unwrap_or_default();

            if !source.is_empty() {
                self.send_command(AddScriptToEvaluateOnNewDocumentParams {
                    source,
                    world_name: None,
                    include_command_line_api: None,
                    run_immediately: Some(true),
                })
                .await?;
            }
        }
        Ok(self)
    }

    /// Add a custom script to eval on new document.
    pub async fn add_script_to_evaluate_on_new_document(
        &self,
        source: Option<String>,
    ) -> Result<&Self> {
        if source.is_some() {
            let source = source.unwrap_or_default();

            if !source.is_empty() {
                self.send_command(AddScriptToEvaluateOnNewDocumentParams {
                    source,
                    world_name: None,
                    include_command_line_api: None,
                    run_immediately: None,
                })
                .await?;
            }
        }
        Ok(self)
    }

    /// Removes the `navigator.webdriver` property
    /// changes permissions, pluggins rendering contexts and the `window.chrome`
    /// property to make it harder to detect the scraper as a bot.
    pub async fn _enable_real_emulation(
        &self,
        user_agent: &str,
        config: &spider_fingerprint::EmulationConfiguration,
        viewport: &Option<&spider_fingerprint::spoof_viewport::Viewport>,
        custom_script: Option<&str>,
    ) -> Result<&Self> {
        let emulation_script = spider_fingerprint::emulate(
            user_agent,
            config,
            viewport,
            &custom_script.as_ref().map(|s| Box::new(s.to_string())),
        )
        .unwrap_or_default();

        let source = if let Some(cs) = custom_script {
            format!(
                "{};{};",
                emulation_script,
                spider_fingerprint::wrap_eval_script(cs)
            )
        } else {
            emulation_script
        };

        self.add_script_to_evaluate_on_new_document(Some(source))
            .await?;

        Ok(self)
    }

    /// Removes the `navigator.webdriver` property
    /// changes permissions, pluggins rendering contexts and the `window.chrome`
    /// property to make it harder to detect the scraper as a bot
    pub async fn _enable_stealth_mode(
        &self,
        custom_script: Option<&str>,
        os: Option<AgentOs>,
        tier: Option<Tier>,
    ) -> Result<&Self> {
        let os = os.unwrap_or_default();
        let tier = match tier {
            Some(tier) => tier,
            _ => Tier::Basic,
        };

        let source = if let Some(cs) = custom_script {
            format!(
                "{};{};",
                spider_fingerprint::build_stealth_script(tier, os),
                spider_fingerprint::wrap_eval_script(cs)
            )
        } else {
            spider_fingerprint::build_stealth_script(tier, os)
        };

        self.add_script_to_evaluate_on_new_document(Some(source))
            .await?;

        Ok(self)
    }

    /// Changes your user_agent, removes the `navigator.webdriver` property
    /// changes permissions, pluggins rendering contexts and the `window.chrome`
    /// property to make it harder to detect the scraper as a bot
    pub async fn enable_stealth_mode(&self) -> Result<&Self> {
        let _ = self._enable_stealth_mode(None, None, None).await;

        Ok(self)
    }

    /// Changes your user_agent, removes the `navigator.webdriver` property
    /// changes permissions, pluggins rendering contexts and the `window.chrome`
    /// property to make it harder to detect the scraper as a bot
    pub async fn enable_stealth_mode_os(
        &self,
        os: Option<AgentOs>,
        tier: Option<Tier>,
    ) -> Result<&Self> {
        let _ = self._enable_stealth_mode(None, os, tier).await;

        Ok(self)
    }

    /// Changes your user_agent with a custom agent, removes the `navigator.webdriver` property
    /// changes permissions, pluggins rendering contexts and the `window.chrome`
    /// property to make it harder to detect the scraper as a bot
    pub async fn enable_stealth_mode_with_agent(&self, ua: &str) -> Result<&Self> {
        let _ = tokio::join!(
            self._enable_stealth_mode(None, None, None),
            self.set_user_agent(ua)
        );
        Ok(self)
    }

    /// Changes your user_agent with a custom agent, removes the `navigator.webdriver` property
    /// changes permissions, pluggins rendering contexts and the `window.chrome`
    /// property to make it harder to detect the scraper as a bot. Also add dialog polyfill to prevent blocking the page.
    pub async fn enable_stealth_mode_with_dimiss_dialogs(&self, ua: &str) -> Result<&Self> {
        let _ = tokio::join!(
            self._enable_stealth_mode(
                Some(spider_fingerprint::spoofs::DISABLE_DIALOGS),
                None,
                None
            ),
            self.set_user_agent(ua)
        );
        Ok(self)
    }

    /// Changes your user_agent with a custom agent, removes the `navigator.webdriver` property
    /// changes permissions, pluggins rendering contexts and the `window.chrome`
    /// property to make it harder to detect the scraper as a bot. Also add dialog polyfill to prevent blocking the page.
    pub async fn enable_stealth_mode_with_agent_and_dimiss_dialogs(
        &self,
        ua: &str,
    ) -> Result<&Self> {
        let _ = tokio::join!(
            self._enable_stealth_mode(
                Some(spider_fingerprint::spoofs::DISABLE_DIALOGS),
                None,
                None
            ),
            self.set_user_agent(ua)
        );
        Ok(self)
    }

    /// Enable page Content Security Policy by-passing.
    pub async fn set_bypass_csp(&self, enabled: bool) -> Result<&Self> {
        self.inner.set_bypass_csp(enabled).await?;
        Ok(self)
    }

    /// Reset the navigation history.
    pub async fn reset_navigation_history(&self) -> Result<&Self> {
        self.send_command(ResetNavigationHistoryParams::default())
            .await?;
        Ok(self)
    }

    /// Reset the navigation history execute.
    pub async fn reset_navigation_history_execute(&self) -> Result<&Self> {
        self.execute(ResetNavigationHistoryParams::default())
            .await?;
        Ok(self)
    }

    /// Sets `window.chrome` on frame creation and console.log methods.
    pub async fn hide_chrome(&self) -> Result<&Self, CdpError> {
        self.execute(AddScriptToEvaluateOnNewDocumentParams {
            source: spider_fingerprint::spoofs::HIDE_CHROME.to_string(),
            world_name: None,
            include_command_line_api: None,
            run_immediately: None,
        })
        .await?;
        Ok(self)
    }

    /// Obfuscates WebGL vendor on frame creation
    pub async fn hide_webgl_vendor(&self) -> Result<&Self, CdpError> {
        self.execute(AddScriptToEvaluateOnNewDocumentParams {
            source: spider_fingerprint::spoofs::HIDE_WEBGL.to_string(),
            world_name: None,
            include_command_line_api: None,
            run_immediately: None,
        })
        .await?;
        Ok(self)
    }

    /// Obfuscates browser plugins and hides the navigator object on frame creation
    pub async fn hide_plugins(&self) -> Result<&Self, CdpError> {
        self.execute(AddScriptToEvaluateOnNewDocumentParams {
            source: spider_fingerprint::generate_hide_plugins(),
            world_name: None,
            include_command_line_api: None,
            run_immediately: None,
        })
        .await?;

        Ok(self)
    }

    /// Obfuscates browser permissions on frame creation
    pub async fn hide_permissions(&self) -> Result<&Self, CdpError> {
        self.execute(AddScriptToEvaluateOnNewDocumentParams {
            source: spider_fingerprint::spoofs::HIDE_PERMISSIONS.to_string(),
            world_name: None,
            include_command_line_api: None,
            run_immediately: None,
        })
        .await?;
        Ok(self)
    }

    /// Removes the `navigator.webdriver` property on frame creation
    pub async fn hide_webdriver(&self) -> Result<&Self, CdpError> {
        self.execute(AddScriptToEvaluateOnNewDocumentParams {
            source: spider_fingerprint::spoofs::HIDE_WEBDRIVER.to_string(),
            world_name: None,
            include_command_line_api: None,
            run_immediately: None,
        })
        .await?;
        Ok(self)
    }

    /// Execute a command and return the `Command::Response`
    pub async fn execute<T: Command>(&self, cmd: T) -> Result<CommandResponse<T::Response>> {
        self.command_future(cmd)?.await
    }

    /// Execute a command without waiting for a response.
    pub async fn send_command<T: Command>(&self, cmd: T) -> Result<&Self> {
        let _ = self.inner.send_command(cmd).await;
        Ok(self)
    }

    /// Execute a command and return the `Command::Response`
    pub fn command_future<T: Command>(&self, cmd: T) -> Result<CommandFuture<T>> {
        self.inner.command_future(cmd)
    }

    /// Execute a command and return the `Command::Response`
    pub fn http_future<T: Command>(&self, cmd: T) -> Result<HttpFuture<T>> {
        self.inner.http_future(cmd)
    }

    /// Navigate and wait for the final HTTP request.
    ///
    /// Resolves early with a synthetic failed [`HttpRequest`](crate::handler::http::HttpRequest)
    /// when the navigate ack carries a non-empty `errorText` other than
    /// `net::ERR_ABORTED` or a `net::ERR_HTTP_RESPONSE_CODE_FAILURE` prefix.
    /// Those exceptions wait for a replacement navigation or committed error
    /// body. [`Self::http_future`] always keeps waiting for lifecycle events.
    pub fn navigate_http_future(
        &self,
        params: impl Into<NavigateParams>,
    ) -> Result<HttpFuture<NavigateParams>> {
        let params = params.into();
        let url = params.url.clone();
        self.inner.navigate_http_future(params, url)
    }

    /// Navigate with a server-side deadline and wait for the final HTTP request.
    ///
    /// Sends the standard `Page.navigate` params plus a `timeout` key holding
    /// `navigation_timeout` in whole milliseconds, so a server that honours it
    /// gives up on its own and acks with an `errorText` instead of leaving the
    /// caller to abort blind. Everything else matches
    /// [`Self::navigate_http_future`]: same method string, same early-resolve
    /// behaviour on a terminal ack. Servers that do not implement the key
    /// ignore it and navigate as usual.
    ///
    /// Durations beyond [`i64::MAX`] milliseconds saturate rather than wrap.
    pub fn navigate_http_future_with_timeout(
        &self,
        params: impl Into<NavigateParams>,
        navigation_timeout: std::time::Duration,
    ) -> Result<HttpFuture<NavigateWithDeadlineParams>> {
        let params = NavigateWithDeadlineParams::new(params.into(), navigation_timeout);
        let url = params.url().to_string();
        self.inner.navigate_http_future(params, url)
    }

    /// Adds an event listener to the `Target` and returns the receiver part as
    /// `EventStream`
    ///
    /// An `EventStream` receives every `Event` the `Target` receives.
    /// All event listener get notified with the same event, so registering
    /// multiple listeners for the same event is possible.
    ///
    /// Custom events rely on being deserializable from the received json params
    /// in the `EventMessage`. Custom Events are caught by the `CdpEvent::Other`
    /// variant. If there are mulitple custom event listener is registered
    /// for the same event, identified by the `MethodType::method_id` function,
    /// the `Target` tries to deserialize the json using the type of the event
    /// listener. Upon success the `Target` then notifies all listeners with the
    /// deserialized event. This means, while it is possible to register
    /// different types for the same custom event, only the type of first
    /// registered event listener will be used. The subsequent listeners, that
    /// registered for the same event but with another type won't be able to
    /// receive anything and therefor will come up empty until all their
    /// preceding event listeners are dropped and they become the first (or
    /// longest) registered event listener for an event.
    ///
    /// # Example Listen for canceled animations
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::animation::EventAnimationCanceled;
    /// # use futures_util::StreamExt;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     let mut events = page.event_listener::<EventAnimationCanceled>().await?;
    ///     while let Some(event) = events.next().await {
    ///         //..
    ///     }
    ///     # Ok(())
    /// # }
    /// ```
    ///
    /// # Example Liste for a custom event
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use futures_util::StreamExt;
    /// # use serde::Deserialize;
    /// # use chromiumoxide::types::{MethodId, MethodType};
    /// # use chromiumoxide::cdp::CustomEvent;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    ///     struct MyCustomEvent {
    ///         name: String,
    ///     }
    ///    impl MethodType for MyCustomEvent {
    ///        fn method_id() -> MethodId {
    ///            "Custom.Event".into()
    ///        }
    ///    }
    ///    impl CustomEvent for MyCustomEvent {}
    ///    let mut events = page.event_listener::<MyCustomEvent>().await?;
    ///    while let Some(event) = events.next().await {
    ///        //..
    ///    }
    ///
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn event_listener<T: IntoEventKind>(&self) -> Result<EventStream<T>> {
        let (tx, rx) = unbounded_channel();

        self.inner
            .send_msg(TargetMessage::AddEventListener(
                EventListenerRequest::new::<T>(tx),
            ))
            .await?;

        Ok(EventStream::new(rx))
    }

    pub async fn expose_function(
        &self,
        name: impl Into<String>,
        function: impl AsRef<str>,
    ) -> Result<()> {
        let name = name.into();
        let expression = utils::evaluation_string(function, &["exposedFun", name.as_str()]);

        self.send_command(AddBindingParams::new(name)).await?;
        self.send_command(AddScriptToEvaluateOnNewDocumentParams::new(
            expression.clone(),
        ))
        .await?;

        // TODO add execution context tracking for frames
        //let frames = self.frames().await?;

        Ok(())
    }

    /// Resolves once `DOMContentLoaded` fires — HTML is parsed and
    /// synchronous scripts have executed. Does **not** wait for subresources
    /// (images, fonts, XHRs), so the page pipeline is unblocked as soon as
    /// useful content is available.
    ///
    /// Use after an interaction that triggers a navigation (`click`,
    /// `press_key`, `goto`) to wait until the new page content is ready.
    pub async fn wait_for_navigation_response(&self) -> Result<ArcHttpRequest> {
        self.inner.wait_for_navigation().await
    }

    /// Same as `wait_for_navigation_response` but returns `Self` instead
    pub async fn wait_for_navigation(&self) -> Result<&Self> {
        self.inner.wait_for_navigation().await?;
        Ok(self)
    }

    /// Alias for `wait_for_navigation` — both resolve on `DOMContentLoaded`.
    pub async fn wait_for_dom_content_loaded(&self) -> Result<&Self> {
        self.inner.wait_for_dom_content_loaded().await?;
        Ok(self)
    }

    /// Same as `wait_for_dom_content_loaded` but returns the HTTP response.
    pub async fn wait_for_dom_content_loaded_response(&self) -> Result<ArcHttpRequest> {
        self.inner.wait_for_dom_content_loaded().await
    }

    /// Wait for the `load` event — all subresources (images, fonts, XHRs)
    /// have finished loading. Use `wait_for_navigation` instead unless you
    /// specifically need all subresources to complete.
    pub async fn wait_for_load(&self) -> Result<&Self> {
        self.inner.wait_for_load().await?;
        Ok(self)
    }

    /// Same as `wait_for_load` but returns the HTTP response.
    pub async fn wait_for_load_response(&self) -> Result<ArcHttpRequest> {
        self.inner.wait_for_load().await
    }

    /// Poll until a CSS selector matches an element in the DOM, using
    /// exponential backoff (0 → 20 → 50 → 100 → 100 → 500 ms).
    /// Returns the `Element` on success.
    ///
    /// ```rust,no_run
    /// # async fn example(page: &chromiumoxide::Page) -> chromiumoxide::error::Result<()> {
    /// use std::time::Duration;
    /// let el = page.wait_for_selector("div.loaded", Some(Duration::from_secs(10))).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn wait_for_selector(
        &self,
        selector: impl Into<String>,
        timeout: Option<std::time::Duration>,
    ) -> Result<Element> {
        /// Default timeout when the caller passes `None` — prevents an
        /// infinite polling loop when the selector never matches.
        const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        /// Saturating backoff (ms). The step index clamps to the last entry,
        /// so long polls hold at 500 ms instead of cycling back to 0 ms (which
        /// would re-introduce a near-busy retry path after 6 iterations).
        const BACKOFF_MS: [u64; 6] = [0, 20, 50, 100, 100, 500];

        let selector = selector.into();
        let t = timeout.unwrap_or(DEFAULT_TIMEOUT);
        tokio::time::timeout(t, async move {
            let mut step: usize = 0;
            loop {
                if let Ok(el) = self.find_element(&selector).await {
                    return Ok(el);
                }
                let delay = BACKOFF_MS[step];
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                step = (step + 1).min(BACKOFF_MS.len() - 1);
            }
        })
        .await
        .map_err(|_| CdpError::Timeout)?
    }

    /// Sleep for a fixed duration. Useful as a floor in wait chains.
    pub async fn wait_for_delay(&self, duration: std::time::Duration) -> &Self {
        tokio::time::sleep(duration).await;
        self
    }

    /// Controls whether page will emit lifecycle events
    pub async fn set_page_lifecycles_enabled(&self, enabled: bool) -> Result<&Self> {
        self.execute(SetLifecycleEventsEnabledParams::new(enabled))
            .await?;
        Ok(self)
    }

    /// Wait until the network is idle.
    /// Usage:
    ///   page.goto("https://example.com").await?;
    ///   page.wait_for_network_idle().await?;
    pub async fn wait_for_network_idle(&self) -> Result<&Self> {
        self.inner.wait_for_network_idle().await?;
        Ok(self)
    }

    /// Wait until the network is almost idle.
    /// Usage:
    ///   page.goto("https://example.com").await?;
    ///   page.wait_for_network_almost_idle().await?;
    pub async fn wait_for_network_almost_idle(&self) -> Result<&Self> {
        self.inner.wait_for_network_almost_idle().await?;
        Ok(self)
    }

    /// Wait until the network is idle, but only up to `timeout`.
    /// If the timeout elapses, the error is ignored and the method still returns `Ok(self)`.
    pub async fn wait_for_network_idle_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<&Self> {
        let fut = self.inner.wait_for_network_idle();
        let _ = tokio::time::timeout(timeout, fut).await;
        Ok(self)
    }

    /// Wait until the network is almost idle, but only up to `timeout`.
    /// If the timeout elapses, the error is ignored and the method still returns `Ok(self)`.
    pub async fn wait_for_network_almost_idle_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<&Self> {
        let fut = self.inner.wait_for_network_almost_idle();
        let _ = tokio::time::timeout(timeout, fut).await;
        Ok(self)
    }

    /// Navigate directly to the given URL checking the HTTP cache first.
    ///
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    pub async fn goto_with_cache(
        &self,
        params: impl Into<NavigateParams>,
        auth_opt: Option<&str>,
    ) -> Result<&Self> {
        use crate::cache::{get_cached_url, rewrite_base_tag};
        let navigate_params: NavigateParams = params.into();
        let mut force_navigate = true;

        // todo: pull in the headers from auth.
        if let Some(source) = get_cached_url(&navigate_params.url, auth_opt).await {
            let (html, main_frame, _) = tokio::join!(
                rewrite_base_tag(&source, Some(&navigate_params.url)),
                self.mainframe(),
                self.set_page_lifecycles_enabled(true)
            );

            if let Ok(frame_id) = main_frame {
                if let Err(e) = self
                    .execute(browser_protocol::page::SetDocumentContentParams {
                        frame_id: frame_id.unwrap_or_default(),
                        html,
                    })
                    .await
                {
                    tracing::error!("Set Content Error({:?}) - {:?}", e, &navigate_params.url);
                    force_navigate = false;
                    if let crate::page::CdpError::Timeout = e {
                        force_navigate = true;
                    }
                } else {
                    tracing::info!("Found cached url - ({:?})", &navigate_params.url);
                    force_navigate = false;
                }
            }
        }

        if force_navigate {
            let res = self.execute(navigate_params).await?;

            if let Some(err) = res.result.error_text {
                return Err(CdpError::ChromeMessage(err));
            }
        }

        Ok(self)
    }

    /// Navigate directly to the given URL checking the HTTP cache first.
    ///
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    pub async fn goto_with_cache_http_future(
        &self,
        params: impl Into<NavigateParams>,
        auth_opt: Option<&str>,
    ) -> Result<Arc<crate::HttpRequest>> {
        use crate::cache::{get_cached_url, rewrite_base_tag};
        let navigate_params: NavigateParams = params.into();
        let mut force_navigate = true;
        let mut navigation_result = None;

        // todo: pull in the headers from auth.
        if let Some(source) = get_cached_url(&navigate_params.url, auth_opt).await {
            let (html, main_frame, _) = tokio::join!(
                rewrite_base_tag(&source, Some(&navigate_params.url)),
                self.mainframe(),
                self.set_page_lifecycles_enabled(true)
            );
            if let Ok(frame_id) = main_frame {
                let base = self.http_future(browser_protocol::page::SetDocumentContentParams {
                    frame_id: frame_id.unwrap_or_default(),
                    html,
                });

                if let Ok(page_base) = base {
                    match page_base.await {
                        Ok(result) => {
                            navigation_result = result;
                            tracing::info!("Found cached url - ({:?})", &navigate_params.url);
                            force_navigate = false;
                        }
                        Err(e) => {
                            tracing::error!(
                                "Set Content Error({:?}) - {:?}",
                                e,
                                &navigate_params.url
                            );
                            force_navigate = false;
                            if let crate::page::CdpError::Timeout = e {
                                force_navigate = true;
                            }
                        }
                    }
                }
            }
        }

        if force_navigate {
            if let Ok(page_base) = self.navigate_http_future(navigate_params) {
                let http_result = page_base.await?;

                if let Some(res) = &http_result {
                    if let Some(err) = &res.failure_text {
                        return Err(CdpError::ChromeMessage(err.into()));
                    }
                }
                navigation_result = http_result;
            }
        }

        if let Some(res) = navigation_result {
            Ok(res)
        } else {
            Err(CdpError::ChromeMessage(
                "failed to get navigation result".into(),
            ))
        }
    }

    /// Navigate directly to the given URL concurrenctly checking the cache and seeding.
    ///
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    ///
    /// `namespace` partitions the cache key (country, proxy pool, tenant, …) so
    /// logically distinct variants of the same URL never collide on the same
    /// cached bytes. Pass `None` for the default (empty) namespace.
    #[cfg(feature = "_cache")]
    pub async fn goto_with_cache_fast_seed(
        &self,
        params: impl Into<NavigateParams>,
        cache_policy: Option<crate::cache::BasicCachePolicy>,
        auth_opt: Option<&str>,
        remote: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<&Self> {
        use crate::cache::manager::site_key_for_target_url;

        let navigate_params = params.into();
        let target_url = navigate_params.url.clone();
        let cache_site = site_key_for_target_url(&target_url, auth_opt, namespace);

        let _ = self
            .set_cache_key((Some(cache_site.clone()), cache_policy))
            .await;

        let _ = tokio::join!(
            self.seed_cache(&target_url, auth_opt, remote, namespace),
            self.goto_with_cache(navigate_params, auth_opt)
        );

        let _ = self.clear_local_cache(&cache_site);

        Ok(self)
    }

    /// Navigate directly to the given URL concurrenctly checking the cache, seeding, and dumping.
    ///
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    pub async fn _goto_with_cache_remote(
        &self,
        params: impl Into<NavigateParams>,
        auth_opt: Option<&str>,
        cache_policy: Option<crate::cache::BasicCachePolicy>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
        remote: Option<&str>,
        intercept_enabled: Option<bool>,
        namespace: Option<&str>,
    ) -> Result<&Self> {
        let remote = remote.or(Some("true"));
        let navigate_params = params.into();
        let target_url = navigate_params.url.clone();

        let cache_site =
            crate::cache::manager::site_key_for_target_url(&target_url, auth_opt, namespace);

        let _ = self
            .set_cache_key((Some(cache_site.clone()), cache_policy.clone()))
            .await;

        let run_intercept = async {
            if intercept_enabled.unwrap_or(true) {
                let _ = self
                    .spawn_cache_intercepter(
                        auth_opt.map(|f| f.into()),
                        cache_policy,
                        cache_strategy,
                    )
                    .await;
            }
        };

        let _ = tokio::join!(
            self.spawn_cache_listener(
                &cache_site,
                auth_opt.map(|f| f.into()),
                cache_strategy,
                remote.map(|f| f.into()),
                false,
                namespace,
            ),
            run_intercept,
            self.seed_cache(&target_url, auth_opt, remote, namespace)
        );

        let _ = self.goto_with_cache(navigate_params, auth_opt).await;
        let _ = self.clear_local_cache(&cache_site);

        Ok(self)
    }

    /// Navigate directly to the given URL concurrenctly checking the cache, seeding, and dumping.
    ///
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    pub async fn goto_with_cache_remote(
        &self,
        params: impl Into<NavigateParams>,
        auth_opt: Option<&str>,
        cache_policy: Option<crate::cache::BasicCachePolicy>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
        remote: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<&Self> {
        self._goto_with_cache_remote(
            params,
            auth_opt,
            cache_policy,
            cache_strategy,
            remote,
            Some(true),
            namespace,
        )
        .await
    }

    /// Navigate directly to the given URL concurrenctly checking the cache, seeding, and dumping. Enable this if you connect with request interception.
    ///
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    pub async fn goto_with_cache_remote_intercept_enabled(
        &self,
        params: impl Into<NavigateParams>,
        auth_opt: Option<&str>,
        cache_policy: Option<crate::cache::BasicCachePolicy>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
        remote: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<&Self> {
        self._goto_with_cache_remote(
            params,
            auth_opt,
            cache_policy,
            cache_strategy,
            remote,
            Some(false),
            namespace,
        )
        .await
    }

    /// Execute a command and return the `Command::Response` with caching.
    /// Use page.spawn_cache_intercepter if you do not have interception enabled beforehand to use the cache responses.
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    async fn _http_future_with_cache(
        &self,
        navigate_params: crate::cdp::browser_protocol::page::NavigateParams,
        auth_opt: Option<&str>,
        cache_policy: Option<crate::cache::BasicCachePolicy>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
        remote: Option<&str>,
        intercept_enabled: Option<bool>,
        namespace: Option<&str>,
    ) -> Result<Arc<crate::HttpRequest>> {
        let remote = remote.or(Some("true"));
        let target_url = navigate_params.url.clone();
        let cache_site =
            crate::cache::manager::site_key_for_target_url(&target_url, auth_opt, namespace);

        let _ = self
            .set_cache_key((Some(cache_site.clone()), cache_policy.clone()))
            .await;

        let run_intercept = async {
            if intercept_enabled.unwrap_or(true) {
                let _ = self
                    .spawn_cache_intercepter(
                        auth_opt.map(|f| f.into()),
                        cache_policy,
                        cache_strategy,
                    )
                    .await;
            }
        };

        let _ = tokio::join!(
            self.spawn_cache_listener(
                &cache_site,
                auth_opt.map(|f| f.into()),
                cache_strategy,
                remote.map(|f| f.into()),
                false,
                namespace,
            ),
            run_intercept,
            self.seed_cache(&target_url, auth_opt, remote, namespace)
        );

        let cache_future = self
            .goto_with_cache_http_future(navigate_params, auth_opt)
            .await;
        let _ = self.clear_local_cache(&cache_site);

        cache_future
    }

    /// Execute a command and return the `Command::Response` with caching. Enable this if you connect with request interception.
    /// Use page.spawn_cache_intercepter if you do not have interception enabled beforehand to use the cache responses.
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    pub async fn http_future_with_cache(
        &self,
        navigate_params: crate::cdp::browser_protocol::page::NavigateParams,
        auth_opt: Option<&str>,
        cache_policy: Option<crate::cache::BasicCachePolicy>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
        remote: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<Arc<crate::HttpRequest>> {
        self._http_future_with_cache(
            navigate_params,
            auth_opt,
            cache_policy,
            cache_strategy,
            remote,
            Some(true),
            namespace,
        )
        .await
    }

    /// Execute a command and return the `Command::Response` with caching.
    /// Use page.spawn_cache_intercepter if you do not have interception enabled beforehand to use the cache responses.
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    pub async fn http_future_with_cache_intercept_enabled(
        &self,
        navigate_params: crate::cdp::browser_protocol::page::NavigateParams,
        auth_opt: Option<&str>,
        cache_policy: Option<crate::cache::BasicCachePolicy>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
        remote: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<Arc<crate::HttpRequest>> {
        self._http_future_with_cache(
            navigate_params,
            auth_opt,
            cache_policy,
            cache_strategy,
            remote,
            Some(false),
            namespace,
        )
        .await
    }

    /// Navigate directly to the given URL concurrenctly checking the cache and seeding.
    ///
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(feature = "_cache")]
    pub async fn goto_with_cache_seed(
        &self,
        params: impl Into<NavigateParams>,
        auth_opt: Option<&str>,
        cache_policy: Option<crate::cache::BasicCachePolicy>,
        remote: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<&Self> {
        let navigate_params = params.into();
        let navigation_url = navigate_params.url.to_string();

        let cache_site =
            crate::cache::manager::site_key_for_target_url(&navigation_url, auth_opt, namespace);

        let _ = self
            .set_cache_key((Some(cache_site.clone()), cache_policy.clone()))
            .await;

        self.seed_cache(&navigation_url, auth_opt, remote, namespace)
            .await?;

        self.goto_with_cache(navigate_params, auth_opt).await?;
        let _ = self.clear_local_cache_with_key(&navigation_url, auth_opt, namespace);
        Ok(self)
    }

    /// Navigate directly to the given URL.
    ///
    /// This resolves directly after the requested URL is fully loaded. Does nothing without the 'cache' feature on.
    #[cfg(not(feature = "_cache"))]
    pub async fn goto_with_cache(
        &self,
        params: impl Into<NavigateParams>,
        _auth_opt: Option<&str>,
    ) -> Result<&Self> {
        let res = self.execute(params.into()).await?;

        if let Some(err) = res.result.error_text {
            return Err(CdpError::ChromeMessage(err));
        }

        Ok(self)
    }

    /// Navigate directly to the given URL.
    ///
    /// This resolves directly after the requested URL is fully loaded.
    pub async fn goto(&self, params: impl Into<NavigateParams>) -> Result<&Self> {
        let res = self.execute(params.into()).await?;

        if let Some(err) = res.result.error_text {
            return Err(CdpError::ChromeMessage(err));
        }

        Ok(self)
    }

    /// The identifier of the `Target` this page belongs to
    pub fn target_id(&self) -> &TargetId {
        self.inner.target_id()
    }

    /// The identifier of the `Session` target of this page is attached to
    pub fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }

    /// The identifier of the `Session` target of this page is attached to
    pub fn opener_id(&self) -> &Option<TargetId> {
        self.inner.opener_id()
    }

    /// Returns the name of the frame
    pub async fn frame_name(&self, frame_id: FrameId) -> Result<Option<String>> {
        let (tx, rx) = oneshot_channel();
        self.inner
            .send_msg(TargetMessage::Name(GetName {
                frame_id: Some(frame_id),
                tx,
            }))
            .await?;
        self.inner.recv_msg(rx).await
    }

    pub async fn authenticate(&self, credentials: Credentials) -> Result<()> {
        self.inner
            .send_msg(TargetMessage::Authenticate(credentials))
            .await?;

        Ok(())
    }

    /// Control blocking network on continue fetch request paused.
    pub async fn set_blocked_networking(&self, blocked: bool) -> Result<()> {
        self.inner
            .send_msg(TargetMessage::BlockNetwork(blocked))
            .await?;

        Ok(())
    }

    /// Set the internal paused fetch interception control. Use this if you manually set your own listeners.
    pub async fn set_request_interception(&self, enabled: bool) -> Result<()> {
        self.inner
            .send_msg(TargetMessage::EnableInterception(enabled))
            .await?;

        Ok(())
    }

    /// Returns the current url of the page
    pub async fn url(&self) -> Result<Option<String>> {
        let (tx, rx) = oneshot_channel();
        self.inner
            .send_msg(TargetMessage::Url(GetUrl::new(tx)))
            .await?;
        self.inner.recv_msg(rx).await
    }

    /// Returns the current url of the frame
    pub async fn frame_url(&self, frame_id: FrameId) -> Result<Option<String>> {
        let (tx, rx) = oneshot_channel();
        self.inner
            .send_msg(TargetMessage::Url(GetUrl {
                frame_id: Some(frame_id),
                tx,
            }))
            .await?;
        self.inner.recv_msg(rx).await
    }

    /// Returns the parent id of the frame
    pub async fn frame_parent(&self, frame_id: FrameId) -> Result<Option<FrameId>> {
        let (tx, rx) = oneshot_channel();
        self.inner
            .send_msg(TargetMessage::Parent(GetParent { frame_id, tx }))
            .await?;
        self.inner.recv_msg(rx).await
    }

    /// Return the main frame of the page
    pub async fn mainframe(&self) -> Result<Option<FrameId>> {
        let (tx, rx) = oneshot_channel();
        self.inner.send_msg(TargetMessage::MainFrame(tx)).await?;
        self.inner.recv_msg(rx).await
    }

    /// Return the frames of the page
    pub async fn frames(&self) -> Result<Vec<FrameId>> {
        let (tx, rx) = oneshot_channel();
        self.inner.send_msg(TargetMessage::AllFrames(tx)).await?;
        self.inner.recv_msg(rx).await
    }

    /// Set the cache key of the page
    #[cfg(feature = "_cache")]
    pub async fn set_cache_key(
        &self,
        cache_key: (Option<String>, Option<crate::cache::BasicCachePolicy>),
    ) -> Result<()> {
        self.inner
            .send_msg(TargetMessage::CacheKey(cache_key))
            .await?;
        Ok(())
    }

    /// Allows overriding user agent with the given string.
    pub async fn set_extra_headers(
        &self,
        params: impl Into<SetExtraHttpHeadersParams>,
    ) -> Result<&Self> {
        self.execute(params.into()).await?;
        Ok(self)
    }

    /// Generate the user-agent metadata params
    pub fn generate_user_agent_metadata(
        default_params: &SetUserAgentOverrideParams,
    ) -> Option<UserAgentMetadata> {
        let ua_data = spider_fingerprint::spoof_user_agent::build_high_entropy_data(&Some(
            &default_params.user_agent,
        ));
        let windows = ua_data.platform == "Windows";
        let brands = ua_data
            .full_version_list
            .iter()
            .map(|b| {
                let b = b.clone();
                UserAgentBrandVersion::new(b.brand, b.version)
            })
            .collect::<Vec<_>>();

        let full_versions = ua_data
            .full_version_list
            .into_iter()
            .map(|b| UserAgentBrandVersion::new(b.brand, b.version))
            .collect::<Vec<_>>();

        let user_agent_metadata_builder = emulation::UserAgentMetadata::builder()
            .architecture(ua_data.architecture)
            .bitness(ua_data.bitness)
            .model(ua_data.model)
            .platform_version(ua_data.platform_version)
            .brands(brands)
            .full_version_lists(full_versions)
            .platform(ua_data.platform)
            .mobile(ua_data.mobile);

        let user_agent_metadata_builder = if windows {
            user_agent_metadata_builder.wow64(ua_data.wow64_ness)
        } else {
            user_agent_metadata_builder
        };

        user_agent_metadata_builder.build().ok()
    }

    /// Allows overriding the user-agent for the [network](https://chromedevtools.github.io/devtools-protocol/tot/Network/#method-setUserAgentOverride) and [emulation](https://chromedevtools.github.io/devtools-protocol/tot/Emulation/#method-setUserAgentOverride ) with the given string.
    async fn set_user_agent_base(
        &self,
        params: impl Into<SetUserAgentOverrideParams>,
        metadata: bool,
        emulate: bool,
        accept_language: Option<String>,
    ) -> Result<&Self> {
        let mut default_params: SetUserAgentOverrideParams = params.into();

        if default_params.platform.is_none() {
            let platform = platform_from_user_agent(&default_params.user_agent);
            if !platform.is_empty() {
                default_params.platform = Some(platform.into());
            }
        }

        default_params.accept_language = accept_language;

        if default_params.user_agent_metadata.is_none() && metadata {
            let user_agent_metadata = Self::generate_user_agent_metadata(&default_params);
            if let Some(user_agent_metadata) = user_agent_metadata {
                default_params.user_agent_metadata = Some(user_agent_metadata);
            }
        }

        if emulate {
            let default_params1 = default_params.clone();

            let mut set_emulation_agent_override =
                chromiumoxide_cdp::cdp::browser_protocol::emulation::SetUserAgentOverrideParams::new(
                    default_params1.user_agent,
                );

            set_emulation_agent_override.accept_language = default_params1.accept_language;
            set_emulation_agent_override.platform = default_params1.platform;
            set_emulation_agent_override.user_agent_metadata = default_params1.user_agent_metadata;

            tokio::try_join!(
                self.execute(default_params),
                self.execute(set_emulation_agent_override)
            )?;
        } else {
            self.execute(default_params).await?;
        }

        Ok(self)
    }

    /// Allows overriding the user-agent for the [network](https://chromedevtools.github.io/devtools-protocol/tot/Network/#method-setUserAgentOverride) with the given string.
    pub async fn set_user_agent(
        &self,
        params: impl Into<SetUserAgentOverrideParams>,
    ) -> Result<&Self> {
        self.set_user_agent_base(params, true, true, None).await
    }

    /// Allows overriding the user-agent for the [network](https://chromedevtools.github.io/devtools-protocol/tot/Network/#method-setUserAgentOverride), [emulation](https://chromedevtools.github.io/devtools-protocol/tot/Emulation/#method-setUserAgentOverride ), and userAgentMetadata with the given string.
    pub async fn set_user_agent_advanced(
        &self,
        params: impl Into<SetUserAgentOverrideParams>,
        metadata: bool,
        emulate: bool,
        accept_language: Option<String>,
    ) -> Result<&Self> {
        self.set_user_agent_base(params, metadata, emulate, accept_language)
            .await
    }

    /// Returns the user agent of the browser
    pub async fn user_agent(&self) -> Result<String> {
        Ok(self.inner.version().await?.user_agent)
    }

    /// Returns the root DOM node (and optionally the subtree) of the page.
    ///
    /// # Note: This does not return the actual HTML document of the page. To
    /// retrieve the HTML content of the page see `Page::content`.
    pub async fn get_document(&self) -> Result<Node> {
        let cmd = GetDocumentParams {
            depth: Some(-1),
            pierce: Some(true),
        };

        let resp = self.execute(cmd).await?;

        Ok(resp.result.root)
    }

    /// Returns the first element in the document which matches the given CSS
    /// selector.
    ///
    /// Execute a query selector on the document's node.
    pub async fn find_element(&self, selector: impl Into<String>) -> Result<Element> {
        let root = self.get_document().await?.node_id;
        let node_id = self.inner.find_element(selector, root).await?;
        Element::new(Arc::clone(&self.inner), node_id).await
    }

    /// Returns the outer HTML of the page full target piercing all trees.
    pub async fn outer_html_full(&self) -> Result<String> {
        let root = self.get_document().await?;

        let element = Element::new(Arc::clone(&self.inner), root.node_id).await?;

        self.inner
            .outer_html(
                element.remote_object_id,
                element.node_id,
                element.backend_node_id,
            )
            .await
    }

    /// Returns the outer HTML of the page.
    pub async fn outer_html(&self) -> Result<String> {
        let root = self.get_document().await?;
        let p = chromiumoxide_cdp::cdp::browser_protocol::dom::GetOuterHtmlParams {
            node_id: Some(root.node_id),
            ..Default::default()
        };

        let chromiumoxide_types::CommandResponse { result, .. } = self.execute(p).await?;

        Ok(result.outer_html)
    }

    /// Return all `Element`s in the document that match the given selector
    pub async fn find_elements(&self, selector: impl Into<String>) -> Result<Vec<Element>> {
        let root = self.get_document().await?.node_id;
        let node_ids = self.inner.find_elements(selector, root).await?;
        Element::from_nodes(&self.inner, &node_ids).await
    }

    /// Returns the first element in the document which matches the given xpath
    /// selector.
    ///
    /// Execute a xpath selector on the document's node.
    pub async fn find_xpath(&self, selector: impl Into<String>) -> Result<Element> {
        self.get_document().await?;
        let node_ids = self.inner.find_xpaths(selector).await?;
        let node_id = *node_ids.first().ok_or(CdpError::msg(
            "No element found matching the given xpath selector",
        ))?;
        Element::new(Arc::clone(&self.inner), node_id).await
    }

    /// Return all `Element`s in the document that match the given xpath selector
    pub async fn find_xpaths(&self, selector: impl Into<String>) -> Result<Vec<Element>> {
        self.get_document().await?;
        let node_ids = self.inner.find_xpaths(selector).await?;
        Element::from_nodes(&self.inner, &node_ids).await
    }

    /// Describes node given its id
    pub async fn describe_node(&self, node_id: NodeId) -> Result<Node> {
        let resp = self
            .execute(DescribeNodeParams::builder().node_id(node_id).build())
            .await?;
        Ok(resp.result.node)
    }

    /// Find an element inside the shadow root.
    pub async fn find_in_shadow_root(
        &self,
        host_selector: &str,
        inner_selector: &str,
    ) -> Result<Element> {
        let doc = self.get_document().await?;
        let host = self
            .inner
            .find_element(host_selector.to_string(), doc.node_id)
            .await?;

        let described = self
            .execute(
                DescribeNodeParams::builder()
                    .node_id(host)
                    .depth(0)
                    .pierce(true)
                    .build(),
            )
            .await?
            .result
            .node;

        let shadow_root = described
            .shadow_roots
            .as_ref()
            .and_then(|v| v.first())
            .ok_or_else(|| CdpError::msg("host has no shadow root"))?;

        let inner = self
            .inner
            .find_element(inner_selector.to_string(), shadow_root.node_id)
            .await?;

        Element::new(Arc::clone(&self.inner), inner).await
    }

    /// Find elements pierced nodes.
    pub async fn find_elements_pierced(&self, selector: impl Into<String>) -> Result<Vec<Element>> {
        let selector = selector.into();

        let root = self.get_document().await?;
        let scopes = collect_scopes_iterative(&root);

        let mut all = Vec::new();
        let mut node_seen = hashbrown::HashSet::new();

        for scope in scopes {
            if let Ok(ids) = self.inner.find_elements(selector.clone(), scope).await {
                for id in ids {
                    if node_seen.insert(id) {
                        all.push(id);
                    }
                }
            }
        }

        Element::from_nodes(&self.inner, &all).await
    }

    /// Find an element through pierced nodes.
    pub async fn find_element_pierced(&self, selector: impl Into<String>) -> Result<Element> {
        let selector = selector.into();
        let mut els = self.find_elements_pierced(selector).await?;
        els.pop().ok_or_else(|| CdpError::msg("not found"))
    }

    /// Tries to close page, running its beforeunload hooks, if any.
    /// Calls Page.close with [`CloseParams`]
    pub async fn close(self) -> Result<()> {
        self.send_command(CloseParams::default()).await?;
        Ok(())
    }

    /// Performs a single mouse click event at the point's location.
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.click(point).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    ///
    /// # Example
    ///
    /// Perform custom click
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::input::{DispatchMouseEventParams, MouseButton, DispatchMouseEventType};
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///      // double click
    ///      let cmd = DispatchMouseEventParams::builder()
    ///             .x(point.x)
    ///             .y(point.y)
    ///             .button(MouseButton::Left)
    ///             .click_count(2);
    ///
    ///         page.move_mouse(point).await?.execute(
    ///             cmd.clone()
    ///                 .r#type(DispatchMouseEventType::MousePressed)
    ///                 .build()
    ///                 .unwrap(),
    ///         )
    ///         .await?;
    ///
    ///         page.execute(
    ///             cmd.r#type(DispatchMouseEventType::MouseReleased)
    ///                 .build()
    ///                 .unwrap(),
    ///         )
    ///         .await?;
    ///
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn click(&self, point: Point) -> Result<&Self> {
        self.inner.click(point).await?;
        Ok(self)
    }

    /// Mouse down event.
    pub async fn mouse_down(
        &self,
        point: Point,
        button: MouseButton,
        modifiers: i64,
        click_count: i64,
    ) -> Result<&Self> {
        use crate::page::browser_protocol::input::DispatchMouseEventParams;
        self.move_mouse(point).await?;
        if let Ok(cmd) = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(point.x)
            .y(point.y)
            .button(button)
            .modifiers(modifiers)
            .click_count(click_count)
            .build()
        {
            self.execute(cmd).await?;
        }

        Ok(self)
    }

    /// Mouse up event.
    pub async fn mouse_up(
        &self,
        point: Point,
        button: MouseButton,
        modifiers: i64,
        click_count: i64,
    ) -> Result<&Self> {
        self.move_mouse(point).await?;

        if let Ok(cmd) = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(point.x)
            .y(point.y)
            .button(button)
            .modifiers(modifiers)
            .click_count(click_count)
            .build()
        {
            self.execute(cmd).await?;
        }

        Ok(self)
    }

    /// Click and hold.
    pub async fn click_and_hold(
        &self,
        point: Point,
        hold_for: std::time::Duration,
    ) -> Result<&Self> {
        self.mouse_down(point, MouseButton::Left, 0, 1).await?;
        tokio::time::sleep(hold_for).await;
        self.mouse_up(point, MouseButton::Left, 0, 1).await?;
        Ok(self)
    }

    /// Click and hold with modifiers.
    pub async fn click_and_hold_with_modifier(
        &self,
        point: Point,
        hold_for: std::time::Duration,
        modifiers: i64,
    ) -> Result<&Self> {
        self.mouse_down(point, MouseButton::Left, modifiers, 1)
            .await?;
        tokio::time::sleep(hold_for).await;
        self.mouse_up(point, MouseButton::Left, modifiers, 1)
            .await?;
        Ok(self)
    }

    /// Performs a single mouse click event at the point's location and generate a marker.
    pub(crate) async fn click_with_highlight_base(
        &self,
        point: Point,
        color: Rgba,
    ) -> Result<&Self> {
        use chromiumoxide_cdp::cdp::browser_protocol::overlay::HighlightRectParams;
        let x = point.x.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64;
        let y = point.y.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64;

        let highlight_params = HighlightRectParams {
            x,
            y,
            width: 15,
            height: 15,
            color: Some(color),
            outline_color: Some(Rgba::new(255, 255, 255)),
        };

        let _ = tokio::join!(self.click(point), self.execute(highlight_params));
        Ok(self)
    }

    /// Performs a single mouse click event at the point's location and generate a highlight to the nearest element.
    /// Make sure page.enable_overlay is called first.
    pub async fn click_with_highlight(&self, point: Point) -> Result<&Self> {
        let mut color = Rgba::new(255, 0, 0);
        color.a = Some(1.0);
        self.click_with_highlight_base(point, color).await?;
        Ok(self)
    }

    /// Performs a single mouse click event at the point's location and generate a highlight to the nearest element with the color.
    /// Make sure page.enable_overlay is called first.
    pub async fn click_with_highlight_color(&self, point: Point, color: Rgba) -> Result<&Self> {
        self.click_with_highlight_base(point, color).await?;
        Ok(self)
    }

    /// Performs a single mouse click event at the point's location and generate a marker with pure JS. Useful for debugging.
    pub async fn click_with_marker(&self, point: Point) -> Result<&Self> {
        let _ = tokio::join!(
            self.click(point),
            self.evaluate(generate_marker_js(point.x, point.y))
        );

        Ok(self)
    }

    /// Performs a double mouse click event at the point's location.
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.click(point).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn double_click(&self, point: Point) -> Result<&Self> {
        self.inner.double_click(point).await?;
        Ok(self)
    }

    /// Performs a right mouse click event at the point's location.
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.right_click(point).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn right_click(&self, point: Point) -> Result<&Self> {
        self.inner.right_click(point).await?;
        Ok(self)
    }

    /// Performs a middle mouse click event at the point's location.
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.middle_click(point).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn middle_click(&self, point: Point) -> Result<&Self> {
        self.inner.middle_click(point).await?;
        Ok(self)
    }

    /// Performs a back mouse click event at the point's location.
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseBack` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseBack` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.back_click(point).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn back_click(&self, point: Point) -> Result<&Self> {
        self.inner.back_click(point).await?;
        Ok(self)
    }

    /// Performs a forward mouse click event at the point's location.
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseForward` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseForward` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.forward_click(point).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn forward_click(&self, point: Point) -> Result<&Self> {
        self.inner.forward_click(point).await?;
        Ok(self)
    }

    /// Performs a single mouse click event at the point's location with the modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.click_with_modifier(point, 1).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn click_with_modifier(&self, point: Point, modifiers: i64) -> Result<&Self> {
        self.inner.click_with_modifier(point, modifiers).await?;
        Ok(self)
    }

    /// Performs a single mouse right click event at the point's location with the modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.right_click_with_modifier(point, 1).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn right_click_with_modifier(&self, point: Point, modifiers: i64) -> Result<&Self> {
        self.inner
            .right_click_with_modifier(point, modifiers)
            .await?;
        Ok(self)
    }

    /// Performs a single mouse middle click event at the point's location with the modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.middle_click_with_modifier(point, 1).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn middle_click_with_modifier(&self, point: Point, modifiers: i64) -> Result<&Self> {
        self.inner
            .middle_click_with_modifier(point, modifiers)
            .await?;
        Ok(self)
    }

    /// Performs keyboard typing.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.type_str("abc").await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn type_str(&self, input: impl AsRef<str>) -> Result<&Self> {
        self.inner.type_str(input).await?;
        Ok(self)
    }

    /// Performs keyboard typing with the modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.type_str_with_modifier("abc", Some(1)).await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn type_str_with_modifier(
        &self,
        input: impl AsRef<str>,
        modifiers: Option<i64>,
    ) -> Result<&Self> {
        self.inner.type_str_with_modifier(input, modifiers).await?;
        Ok(self)
    }

    /// Performs a click-and-drag mouse event from a starting point to a destination.
    ///
    /// This scrolls both points into view and dispatches a sequence of `DispatchMouseEventParams`
    /// commands in order: a `MousePressed` event at the start location, followed by a `MouseMoved`
    /// event to the end location, and finally a `MouseReleased` event to complete the drag.
    ///
    /// This is useful for dragging UI elements, sliders, or simulating mouse gestures.
    ///
    /// # Example
    ///
    /// Perform a drag from point A to point B using the Shift modifier:
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, from: Point, to: Point) -> Result<()> {
    ///     page.click_and_drag_with_modifier(from, to, 8).await?;
    ///     Ok(())
    /// # }
    /// ```
    pub async fn click_and_drag(&self, from: Point, to: Point) -> Result<&Self> {
        self.inner.click_and_drag(from, to, 0).await?;
        Ok(self)
    }

    /// Performs a smooth click-and-drag: moves to `from` with a bezier path,
    /// presses, drags along a bezier path to `to`, then releases.
    pub async fn click_and_drag_smooth(&self, from: Point, to: Point) -> Result<&Self> {
        self.inner.click_and_drag_smooth(from, to, 0).await?;
        Ok(self)
    }

    /// Performs a smooth click-and-drag with keyboard modifiers:
    /// Alt = 1, Ctrl = 2, Meta/Command = 4, Shift = 8 (default: 0).
    pub async fn click_and_drag_smooth_with_modifier(
        &self,
        from: Point,
        to: Point,
        modifiers: i64,
    ) -> Result<&Self> {
        self.inner
            .click_and_drag_smooth(from, to, modifiers)
            .await?;
        Ok(self)
    }

    /// Performs a click-and-drag mouse event from a starting point to a destination,
    /// with optional keyboard modifiers: Alt = 1, Ctrl = 2, Meta/Command = 4, Shift = 8 (default: 0).
    ///
    /// This scrolls both points into view and dispatches a sequence of `DispatchMouseEventParams`
    /// commands in order: a `MousePressed` event at the start location, followed by a `MouseMoved`
    /// event to the end location, and finally a `MouseReleased` event to complete the drag.
    ///
    /// This is useful for dragging UI elements, sliders, or simulating mouse gestures.
    ///
    /// # Example
    ///
    /// Perform a drag from point A to point B using the Shift modifier:
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, from: Point, to: Point) -> Result<()> {
    ///     page.click_and_drag_with_modifier(from, to, 8).await?;
    ///     Ok(())
    /// # }
    /// ```
    pub async fn click_and_drag_with_modifier(
        &self,
        from: Point,
        to: Point,
        modifiers: i64,
    ) -> Result<&Self> {
        self.inner.click_and_drag(from, to, modifiers).await?;
        Ok(self)
    }

    /// Performs a double mouse click event at the point's location with the modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    ///
    /// This scrolls the point into view first, then executes a
    /// `DispatchMouseEventParams` command of type `MouseLeft` with
    /// `MousePressed` as single click and then releases the mouse with an
    /// additional `DispatchMouseEventParams` of type `MouseLeft` with
    /// `MouseReleased`
    ///
    /// Bear in mind that if `click()` triggers a navigation the new page is not
    /// immediately loaded when `click()` resolves. To wait until navigation is
    /// finished an additional `wait_for_navigation()` is required:
    ///
    /// # Example
    ///
    /// Trigger a navigation and wait until the triggered navigation is finished
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::layout::Point;
    /// # async fn demo(page: Page, point: Point) -> Result<()> {
    ///     let html = page.double_click_with_modifier(point, 1).await?.wait_for_navigation().await?.content();
    ///     # Ok(())
    /// # }
    /// ```
    /// ```
    pub async fn double_click_with_modifier(&self, point: Point, modifiers: i64) -> Result<&Self> {
        self.inner
            .double_click_with_modifier(point, modifiers)
            .await?;
        Ok(self)
    }

    /// Dispatches a `mouseMoved` event and moves the mouse to the position of
    /// the `point` where `Point.x` is the horizontal position of the mouse and
    /// `Point.y` the vertical position of the mouse.
    pub async fn move_mouse(&self, point: Point) -> Result<&Self> {
        self.inner.move_mouse(point).await?;
        Ok(self)
    }

    /// Moves the mouse to `target` along a human-like bezier curve path,
    /// dispatching intermediate `mouseMoved` events with natural timing.
    ///
    /// This produces realistic cursor movement with acceleration, deceleration,
    /// slight curvature, optional overshoot, and per-step jitter.
    pub async fn move_mouse_smooth(&self, target: Point) -> Result<&Self> {
        self.inner.move_mouse_smooth(target).await?;
        Ok(self)
    }

    /// Move smoothly to `point` with human-like movement, then perform a click.
    pub async fn click_smooth(&self, point: Point) -> Result<&Self> {
        self.inner.click_smooth(point).await?;
        Ok(self)
    }

    /// Returns the current tracked mouse position.
    pub fn mouse_position(&self) -> Point {
        self.inner.mouse_position()
    }

    /// Uses the `DispatchKeyEvent` mechanism to simulate pressing keyboard
    /// keys.
    pub async fn press_key(&self, input: impl AsRef<str>) -> Result<&Self> {
        self.inner.press_key(input).await?;
        Ok(self)
    }

    /// Uses the `DispatchKeyEvent` mechanism to simulate pressing keyboard
    /// keys with the modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0)..
    pub async fn press_key_with_modifier(
        &self,
        input: impl AsRef<str>,
        modifiers: i64,
    ) -> Result<&Self> {
        self.inner
            .press_key_with_modifier(input, Some(modifiers))
            .await?;
        Ok(self)
    }

    /// Dispatches a `DragEvent`, moving the element to the given `point`.
    ///
    /// `point.x` defines the horizontal target, and `point.y` the vertical mouse position.
    /// Accepts `drag_type`, `drag_data`, and optional keyboard `modifiers`.
    pub async fn drag(
        &self,
        drag_type: DispatchDragEventType,
        point: Point,
        drag_data: DragData,
        modifiers: Option<i64>,
    ) -> Result<&Self> {
        self.inner
            .drag(drag_type, point, drag_data, modifiers)
            .await?;
        Ok(self)
    }
    /// Fetches the entire accessibility tree for the root Document
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::cdp::browser_protocol::page::FrameId;
    /// # async fn demo_get_full_ax_tree(page: Page, depth: Option<i64>, frame_id: Option<FrameId>) -> Result<()> {
    ///     let tree = page.get_full_ax_tree(None, None).await;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn get_full_ax_tree(
        &self,
        depth: Option<i64>,
        frame_id: Option<FrameId>,
    ) -> Result<GetFullAxTreeReturns> {
        self.inner.get_full_ax_tree(depth, frame_id).await
    }

    /// Fetches the partial accessibility tree for the root Document
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide::cdp::browser_protocol::dom::BackendNodeId;
    /// # async fn demo_get_partial_ax_tree(page: Page, node_id: Option<chromiumoxide_cdp::cdp::browser_protocol::dom::NodeId>, backend_node_id: Option<BackendNodeId>, object_id: Option<chromiumoxide_cdp::cdp::js_protocol::runtime::RemoteObjectId>, fetch_relatives: Option<bool>,) -> Result<()> {
    ///     let tree = page.get_partial_ax_tree(node_id, backend_node_id, object_id, fetch_relatives).await;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn get_partial_ax_tree(
        &self,
        node_id: Option<chromiumoxide_cdp::cdp::browser_protocol::dom::NodeId>,
        backend_node_id: Option<BackendNodeId>,
        object_id: Option<chromiumoxide_cdp::cdp::js_protocol::runtime::RemoteObjectId>,
        fetch_relatives: Option<bool>,
    ) -> Result<GetPartialAxTreeReturns> {
        self.inner
            .get_partial_ax_tree(node_id, backend_node_id, object_id, fetch_relatives)
            .await
    }

    /// Dispatches a `mouseWheel` event and moves the mouse to the position of
    /// the `point` where `Point.x` is the horizontal position of the mouse and
    /// `Point.y` the vertical position of the mouse.
    pub async fn scroll(&self, point: Point, delta: Delta) -> Result<&Self> {
        self.inner.scroll(point, delta).await?;
        Ok(self)
    }

    /// Scrolls the current page by the specified horizontal and vertical offsets.
    /// This method helps when Chrome version may not support certain CDP dispatch events.
    pub async fn scroll_by(
        &self,
        delta_x: f64,
        delta_y: f64,
        behavior: ScrollBehavior,
    ) -> Result<&Self> {
        self.inner.scroll_by(delta_x, delta_y, behavior).await?;
        Ok(self)
    }

    /// Take a screenshot of the current page
    pub async fn screenshot(&self, params: impl Into<ScreenshotParams>) -> Result<Vec<u8>> {
        self.inner.screenshot(params).await
    }

    /// Take a screenshot of the current page
    pub async fn print_to_pdf(&self, params: impl Into<PrintToPdfParams>) -> Result<Vec<u8>> {
        self.inner.print_to_pdf(params).await
    }

    /// Save a screenshot of the page
    ///
    /// # Example save a png file of a website
    ///
    /// ```no_run
    /// # use chromiumoxide::page::{Page, ScreenshotParams};
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::page::CaptureScreenshotFormat;
    /// # async fn demo(page: Page) -> Result<()> {
    ///         page.goto("http://example.com")
    ///             .await?
    ///             .save_screenshot(
    ///             ScreenshotParams::builder()
    ///                 .format(CaptureScreenshotFormat::Png)
    ///                 .full_page(true)
    ///                 .omit_background(true)
    ///                 .build(),
    ///             "example.png",
    ///             )
    ///             .await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn save_screenshot(
        &self,
        params: impl Into<ScreenshotParams>,
        output: impl AsRef<Path>,
    ) -> Result<Vec<u8>> {
        let img = self.screenshot(params).await?;
        utils::write(output.as_ref(), &img).await?;
        Ok(img)
    }

    /// Print the current page as pdf.
    ///
    /// See [`PrintToPdfParams`]
    ///
    /// # Note Generating a pdf is currently only supported in Chrome headless.
    pub async fn pdf(&self, params: PrintToPdfParams) -> Result<Vec<u8>> {
        let res = self.execute(params).await?;
        Ok(utils::base64::decode(&res.data)?)
    }

    /// Save the current page as pdf as file to the `output` path and return the
    /// pdf contents.
    ///
    /// # Note Generating a pdf is currently only supported in Chrome headless.
    pub async fn save_pdf(
        &self,
        opts: PrintToPdfParams,
        output: impl AsRef<Path>,
    ) -> Result<Vec<u8>> {
        let pdf = self.pdf(opts).await?;
        utils::write(output.as_ref(), &pdf).await?;
        Ok(pdf)
    }

    /// Brings page to front (activates tab)
    pub async fn bring_to_front(&self) -> Result<&Self> {
        self.send_command(BringToFrontParams::default()).await?;
        Ok(self)
    }

    /// Turns on virtual time for all frames (replacing real-time with a synthetic time source) and sets the current virtual time policy. Note this supersedes any previous time budget.
    pub async fn enable_virtual_time_with_budget(
        &self,
        budget_ms: f64,
        policy: Option<chromiumoxide_cdp::cdp::browser_protocol::emulation::VirtualTimePolicy>,
        max_virtual_time_task_starvation_count: Option<i64>,
        initial_virtual_time: Option<TimeSinceEpoch>,
    ) -> Result<&Self> {
        let params =
            chromiumoxide_cdp::cdp::browser_protocol::emulation::SetVirtualTimePolicyParams {
                policy: policy.unwrap_or(
                    chromiumoxide_cdp::cdp::browser_protocol::emulation::VirtualTimePolicy::Advance,
                ),
                budget: Some(budget_ms),
                max_virtual_time_task_starvation_count: max_virtual_time_task_starvation_count
                    .or(Some(10_000)),
                initial_virtual_time,
            };
        self.send_command(params).await?;
        Ok(self)
    }

    /// Emulates hardware concurrency.
    pub async fn emulate_hardware_concurrency(&self, hardware_concurrency: i64) -> Result<&Self> {
        self.send_command(SetHardwareConcurrencyOverrideParams::new(
            hardware_concurrency,
        ))
        .await?;
        Ok(self)
    }

    /// Emulates the given media type or media feature for CSS media queries
    pub async fn emulate_media_features(&self, features: Vec<MediaFeature>) -> Result<&Self> {
        self.send_command(SetEmulatedMediaParams::builder().features(features).build())
            .await?;
        Ok(self)
    }

    /// Changes the CSS media type of the page
    // Based on https://pptr.dev/api/puppeteer.page.emulatemediatype
    pub async fn emulate_media_type(
        &self,
        media_type: impl Into<MediaTypeParams>,
    ) -> Result<&Self> {
        self.execute(
            SetEmulatedMediaParams::builder()
                .media(media_type.into())
                .build(),
        )
        .await?;
        Ok(self)
    }

    /// Overrides default host system timezone
    pub async fn emulate_timezone(
        &self,
        timezoune_id: impl Into<SetTimezoneOverrideParams>,
    ) -> Result<&Self> {
        self.send_command(timezoune_id.into()).await?;
        Ok(self)
    }

    /// Overrides default host system locale with the specified one
    pub async fn emulate_locale(
        &self,
        locale: impl Into<SetLocaleOverrideParams>,
    ) -> Result<&Self> {
        self.send_command(locale.into()).await?;
        Ok(self)
    }

    /// Overrides default viewport
    pub async fn emulate_viewport(
        &self,
        viewport: impl Into<SetDeviceMetricsOverrideParams>,
    ) -> Result<&Self> {
        self.send_command(viewport.into()).await?;
        Ok(self)
    }

    /// Overrides the Geolocation Position or Error. Omitting any of the parameters emulates position unavailable.
    pub async fn emulate_geolocation(
        &self,
        geolocation: impl Into<SetGeolocationOverrideParams>,
    ) -> Result<&Self> {
        self.send_command(geolocation.into()).await?;
        Ok(self)
    }

    /// Sets whether script execution is disabled in the page.
    pub async fn set_javascript_enabled(&self, enabled: bool) -> Result<&Self> {
        self.send_command(SetScriptExecutionDisabledParams::new(!enabled))
            .await?;
        Ok(self)
    }

    /// Disables JavaScript execution on the page.
    pub async fn disable_javascript(&self) -> Result<&Self> {
        self.set_javascript_enabled(false).await
    }

    /// Reloads given page
    ///
    /// To reload ignoring cache run:
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::page::ReloadParams;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     page.execute(ReloadParams::builder().ignore_cache(true).build()).await?;
    ///     page.wait_for_navigation().await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn reload(&self) -> Result<&Self> {
        self.send_command(ReloadParams::default()).await?;
        self.wait_for_navigation().await
    }

    /// Reloads given page without waiting for navigation.
    ///
    /// To reload ignoring cache run:
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::page::ReloadParams;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     page.execute(ReloadParams::builder().ignore_cache(true).build()).await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn reload_no_wait(&self) -> Result<&Self> {
        self.send_command(ReloadParams::default()).await?;
        Ok(self)
    }

    /// Enables ServiceWorkers. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/ServiceWorker#method-enable
    pub async fn enable_service_workers(&self) -> Result<&Self> {
        self.send_command(browser_protocol::service_worker::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables Fetch.
    pub async fn enable_fetch(&self) -> Result<&Self> {
        self.send_command(browser_protocol::fetch::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables Fetch.
    pub async fn disable_fetch(&self) -> Result<&Self> {
        self.send_command(browser_protocol::fetch::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables ServiceWorker. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/ServiceWorker#method-enable
    pub async fn disable_service_workers(&self) -> Result<&Self> {
        self.send_command(browser_protocol::service_worker::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables Performances. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Performance#method-enable
    pub async fn enable_performance(&self) -> Result<&Self> {
        self.send_command(browser_protocol::performance::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables Performances. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Performance#method-disable
    pub async fn disable_performance(&self) -> Result<&Self> {
        self.send_command(browser_protocol::performance::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables Overlay domain notifications. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Overlay#method-enable
    pub async fn enable_overlay(&self) -> Result<&Self> {
        self.send_command(browser_protocol::overlay::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables Overlay domain notifications. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Overlay#method-enable
    pub async fn disable_overlay(&self) -> Result<&Self> {
        self.send_command(browser_protocol::overlay::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables Overlay domain paint rectangles. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Overlay/#method-setShowPaintRects
    pub async fn enable_paint_rectangles(&self) -> Result<&Self> {
        self.send_command(browser_protocol::overlay::SetShowPaintRectsParams::new(
            true,
        ))
        .await?;
        Ok(self)
    }

    /// Disabled Overlay domain paint rectangles. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Overlay/#method-setShowPaintRects
    pub async fn disable_paint_rectangles(&self) -> Result<&Self> {
        self.send_command(browser_protocol::overlay::SetShowPaintRectsParams::new(
            false,
        ))
        .await?;
        Ok(self)
    }

    /// Enables log domain. Disabled by default.
    ///
    /// Sends the entries collected so far to the client by means of the
    /// entryAdded notification.
    ///
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Log#method-enable
    pub async fn enable_log(&self) -> Result<&Self> {
        self.send_command(browser_protocol::log::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables log domain
    ///
    /// Prevents further log entries from being reported to the client
    ///
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Log#method-disable
    pub async fn disable_log(&self) -> Result<&Self> {
        self.send_command(browser_protocol::log::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables runtime domain. Activated by default.
    pub async fn enable_runtime(&self) -> Result<&Self> {
        self.send_command(js_protocol::runtime::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables the network.
    pub async fn enable_network(&self) -> Result<&Self> {
        self.send_command(browser_protocol::network::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables the network.
    pub async fn disable_network(&self) -> Result<&Self> {
        self.send_command(browser_protocol::network::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables runtime domain.
    pub async fn disable_runtime(&self) -> Result<&Self> {
        self.send_command(js_protocol::runtime::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables Debugger. Enabled by default.
    pub async fn enable_debugger(&self) -> Result<&Self> {
        self.send_command(js_protocol::debugger::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables Debugger.
    pub async fn disable_debugger(&self) -> Result<&Self> {
        self.send_command(js_protocol::debugger::DisableParams::default())
            .await?;
        Ok(self)
    }

    /// Enables page domain notifications. Enabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Page/#method-enable
    pub async fn enable_page(&self) -> Result<&Self> {
        self.send_command(browser_protocol::page::EnableParams::default())
            .await?;
        Ok(self)
    }

    /// Disables page domain notifications. Disabled by default.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Page/#method-disable
    pub async fn disable_page(&self) -> Result<&Self> {
        self.send_command(browser_protocol::page::EnableParams::default())
            .await?;
        Ok(self)
    }

    // Enables DOM agent
    pub async fn enable_dom(&self) -> Result<&Self> {
        self.send_command(browser_protocol::dom::EnableParams::default())
            .await?;
        Ok(self)
    }

    // Disables DOM agent
    pub async fn disable_dom(&self) -> Result<&Self> {
        self.send_command(browser_protocol::dom::DisableParams::default())
            .await?;
        Ok(self)
    }

    // Enables the CSS agent
    pub async fn enable_css(&self) -> Result<&Self> {
        self.send_command(browser_protocol::css::EnableParams::default())
            .await?;
        Ok(self)
    }

    // Disables the CSS agent
    pub async fn disable_css(&self) -> Result<&Self> {
        self.send_command(browser_protocol::css::DisableParams::default())
            .await?;
        Ok(self)
    }

    // Disables the cache.
    pub async fn disable_network_cache(&self, disabled: bool) -> Result<&Self> {
        self.send_command(browser_protocol::network::SetCacheDisabledParams::new(
            disabled,
        ))
        .await?;
        Ok(self)
    }

    /// Block urls from networking.
    ///
    /// Prevents further networking
    ///
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Network#method-setBlockedURLs
    pub async fn set_blocked_urls(&self, urls: Vec<String>) -> Result<&Self> {
        self.send_command(SetBlockedUrLsParams {
            url_patterns: Some(
                urls.into_iter()
                    .map(|u| BlockPattern::new(u, true))
                    .collect(),
            ),
        })
        .await?;
        Ok(self)
    }

    /// Force the page stop all navigations and pending resource fetches.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Page#method-stopLoading
    pub async fn stop_loading(&self) -> Result<&Self> {
        self.send_command(browser_protocol::page::StopLoadingParams::default())
            .await?;
        Ok(self)
    }

    /// Block all urls from networking.
    ///
    /// Prevents further networking
    ///
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Network#method-setBlockedURLs
    pub async fn block_all_urls(&self) -> Result<&Self> {
        self.send_command(SetBlockedUrLsParams {
            url_patterns: Some(vec![BlockPattern::new("*", true)]),
        })
        .await?;
        Ok(self)
    }

    /// Force the page stop all navigations and pending resource fetches for the rest of the page life.
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Network#method-setBlockedURLs
    /// See https://chromedevtools.github.io/devtools-protocol/tot/Page#method-stopLoading
    pub async fn force_stop_all(&self) -> Result<&Self> {
        let _ = tokio::join!(
            self.stop_loading(),
            self.set_blocked_urls(vec!["*".to_string()])
        );
        Ok(self)
    }

    /// Activates (focuses) the target.
    pub async fn activate(&self) -> Result<&Self> {
        self.inner.activate().await?;
        Ok(self)
    }

    /// Returns all cookies that match the tab's current URL.
    pub async fn get_cookies(&self) -> Result<Vec<Cookie>> {
        Ok(self
            .execute(GetCookiesParams::default())
            .await?
            .result
            .cookies)
    }

    /// Clear the cookies from the network.
    pub async fn clear_cookies(&self) -> Result<&Self> {
        self.execute(ClearCookiesParams::default()).await?;

        Ok(self)
    }

    /// Lists the agent-callable WebMCP tools the current page declared, via
    /// the `WebMCP.listTools` method.
    ///
    /// This requires a remote engine that implements the non-standard
    /// `WebMCP` domain; browsers without it respond with a protocol error.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        Ok(self.execute(ListToolsParams::default()).await?.result.tools)
    }

    /// Invokes a page-declared WebMCP tool by name with the given JSON
    /// `arguments`, via the `WebMCP.callTool` method, returning the result as
    /// opaque JSON.
    ///
    /// An engine that implements `WebMCP.callTool` runs the page-declared tool
    /// and returns its result as opaque JSON — commonly a WebMCP tool-result
    /// object with a `content` array and an `isError` flag, where a tool that
    /// fails is reported in-band as `isError: true` rather than as a protocol
    /// error. The shape is intentionally left opaque here so the client does
    /// not constrain it.
    ///
    /// Not every engine implements `WebMCP.callTool`. Depending on the engine's
    /// unknown-method policy, an unimplemented call surfaces either a protocol
    /// error or an empty success result — so treat an empty object as "not
    /// supported yet". Discover available tools and their `inputSchema` with
    /// [`Page::list_tools`].
    pub async fn call_tool(
        &self,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        Ok(self
            .execute(CallToolParams::new(name, arguments))
            .await?
            .result)
    }

    /// Set a single cookie
    ///
    /// This fails if the cookie's url or if not provided, the page's url is
    /// `about:blank` or a `data:` url.
    ///
    /// # Example
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::browser_protocol::network::CookieParam;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     page.set_cookie(CookieParam::new("Cookie-name", "Cookie-value")).await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn set_cookie(&self, cookie: impl Into<CookieParam>) -> Result<&Self> {
        let mut cookie = cookie.into();
        if let Some(url) = cookie.url.as_ref() {
            validate_cookie_url(url)?;
        } else {
            let url = self
                .url()
                .await?
                .ok_or_else(|| CdpError::msg("Page url not found"))?;
            validate_cookie_url(&url)?;
            if url.starts_with("http") {
                cookie.url = Some(url);
            }
        }
        self.send_command(DeleteCookiesParams::from_cookie(&cookie))
            .await?;
        self.send_command(SetCookiesParams::new(vec![cookie]))
            .await?;
        Ok(self)
    }

    /// Set all the cookies
    pub async fn set_cookies(&self, mut cookies: Vec<CookieParam>) -> Result<&Self> {
        let url = self
            .url()
            .await?
            .ok_or_else(|| CdpError::msg("Page url not found"))?;
        let is_http = url.starts_with("http");
        if !is_http {
            validate_cookie_url(&url)?;
        }

        for cookie in &mut cookies {
            if let Some(url) = cookie.url.as_ref() {
                validate_cookie_url(url)?;
            } else if is_http {
                cookie.url = Some(url.clone());
            }
        }
        self.delete_cookies_unchecked(cookies.iter().map(DeleteCookiesParams::from_cookie))
            .await?;

        self.send_command(SetCookiesParams::new(cookies)).await?;
        Ok(self)
    }

    /// Delete a single cookie
    pub async fn delete_cookie(&self, cookie: impl Into<DeleteCookiesParams>) -> Result<&Self> {
        let mut cookie = cookie.into();
        if cookie.url.is_none() {
            let url = self
                .url()
                .await?
                .ok_or_else(|| CdpError::msg("Page url not found"))?;
            if url.starts_with("http") {
                cookie.url = Some(url);
            }
        }
        self.send_command(cookie).await?;
        Ok(self)
    }

    /// Delete all the cookies
    pub async fn delete_cookies(&self, mut cookies: Vec<DeleteCookiesParams>) -> Result<&Self> {
        let mut url: Option<(String, bool)> = None;
        for cookie in &mut cookies {
            if cookie.url.is_none() {
                if let Some((url, is_http)) = url.as_ref() {
                    if *is_http {
                        cookie.url = Some(url.clone())
                    }
                } else {
                    let page_url = self
                        .url()
                        .await?
                        .ok_or_else(|| CdpError::msg("Page url not found"))?;
                    let is_http = page_url.starts_with("http");
                    if is_http {
                        cookie.url = Some(page_url.clone())
                    }
                    url = Some((page_url, is_http));
                }
            }
        }
        self.delete_cookies_unchecked(cookies.into_iter()).await?;
        Ok(self)
    }

    /// Convenience method that prevents another channel roundtrip to get the
    /// url and validate it
    async fn delete_cookies_unchecked(
        &self,
        cookies: impl Iterator<Item = DeleteCookiesParams>,
    ) -> Result<&Self> {
        // NOTE: the buffer size is arbitrary
        let mut cmds = stream::iter(cookies.into_iter().map(|cookie| self.send_command(cookie)))
            .buffer_unordered(5);
        while let Some(resp) = cmds.next().await {
            resp?;
        }
        Ok(self)
    }

    /// Returns the title of the document.
    pub async fn get_title(&self) -> Result<Option<String>> {
        let result = self.evaluate("document.title").await?;

        let title: String = result.into_value()?;

        if title.is_empty() {
            Ok(None)
        } else {
            Ok(Some(title))
        }
    }

    /// Retrieve current values of run-time metrics. Enable the 'collect_metrics flag to auto init 'Performance.enable'.
    pub async fn metrics(&self) -> Result<Vec<Metric>> {
        Ok(self
            .execute(GetMetricsParams::default())
            .await?
            .result
            .metrics)
    }

    /// Returns metrics relating to the layout of the page
    pub async fn layout_metrics(&self) -> Result<GetLayoutMetricsReturns> {
        self.inner.layout_metrics().await
    }

    /// Start a background guard that counts **wire bytes** (compressed on the network)
    /// and force-stops the page once `max_bytes` is exceeded.
    ///
    /// - Uses CDP Network.dataReceived -> `encodedDataLength`
    /// - Calls `Page.stopLoading()` when the cap is hit
    /// - Optionally closes the tab after stopping
    ///
    /// Returns a JoinHandle you can `.await` or just detach.
    pub async fn start_wire_bytes_budget_background(
        &self,
        max_bytes: u64,
        close_on_exceed: Option<bool>,
        enable_networking: Option<bool>,
        sent_and_received: Option<bool>,
    ) -> Result<tokio::task::JoinHandle<()>> {
        // prevent re-enabling the network - by default this should be enabled.
        if enable_networking.unwrap_or(false) {
            let _ = self.enable_network().await;
        }

        let close_on_exceed = close_on_exceed.unwrap_or_default();
        let track_all = sent_and_received.unwrap_or_default();

        let mut rx = self
            .event_listener::<crate::page::browser_protocol::network::EventDataReceived>()
            .await
            .map_err(|e| CdpError::msg(format!("event_listener failed: {e}")))?;

        let page = self.clone();

        let handle = tokio::spawn(async move {
            let mut total_bytes: u64 = 0;

            while let Some(ev) = rx.next().await {
                let encoded = ev.encoded_data_length.max(0) as u64;
                let data_length = if track_all {
                    ev.data_length.max(0) as u64
                } else {
                    0
                };
                total_bytes = total_bytes.saturating_add(encoded + data_length);
                if total_bytes > max_bytes {
                    let _ = page.force_stop_all().await;
                    if close_on_exceed {
                        let _ = page.close().await;
                    }
                    break;
                }
            }
        });

        Ok(handle)
    }

    /// Start a guard that counts **wire bytes** (compressed on the network)
    /// and force-stops the page once `max_bytes` is exceeded.
    ///
    /// - Uses CDP Network.dataReceived -> `encodedDataLength`
    /// - Calls `Page.stopLoading()` when the cap is hit
    /// - Optionally closes the tab after stopping
    ///
    /// Returns a JoinHandle you can `.await` or just detach.
    pub async fn start_wire_bytes_budget(
        &self,
        max_bytes: u64,
        close_on_exceed: Option<bool>,
        enable_networking: Option<bool>,
    ) -> Result<()> {
        // prevent re-enabling the network - by default this should be enabled.
        if enable_networking.unwrap_or(false) {
            let _ = self.enable_network().await;
        }

        let close_on_exceed = close_on_exceed.unwrap_or_default();
        let mut rx = self
            .event_listener::<crate::page::browser_protocol::network::EventDataReceived>()
            .await
            .map_err(|e| CdpError::msg(format!("event_listener failed: {e}")))?;

        let page = self.clone();

        let mut total_bytes: u64 = 0;

        while let Some(ev) = rx.next().await {
            total_bytes = total_bytes.saturating_add(ev.encoded_data_length.max(0) as u64);
            if total_bytes > max_bytes {
                let _ = page.force_stop_all().await;
                if close_on_exceed {
                    let _ = page.close().await;
                }
                break;
            }
        }

        Ok(())
    }

    /// This evaluates strictly as expression.
    ///
    /// Same as `Page::evaluate` but no fallback or any attempts to detect
    /// whether the expression is actually a function. However you can
    /// submit a function evaluation string:
    ///
    /// # Example Evaluate function call as expression
    ///
    /// This will take the arguments `(1,2)` and will call the function
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     let sum: usize = page
    ///         .evaluate_expression("((a,b) => {return a + b;})(1,2)")
    ///         .await?
    ///         .into_value()?;
    ///     assert_eq!(sum, 3);
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn evaluate_expression(
        &self,
        evaluate: impl Into<EvaluateParams>,
    ) -> Result<EvaluationResult> {
        self.inner.evaluate_expression(evaluate).await
    }

    /// Evaluates an expression or function in the page's context and returns
    /// the result.
    ///
    /// In contrast to `Page::evaluate_expression` this is capable of handling
    /// function calls and expressions alike. This takes anything that is
    /// `Into<Evaluation>`. When passing a `String` or `str`, this will try to
    /// detect whether it is a function or an expression. JS function detection
    /// is not very sophisticated but works for general cases (`(async)
    /// functions` and arrow functions). If you want a string statement
    /// specifically evaluated as expression or function either use the
    /// designated functions `Page::evaluate_function` or
    /// `Page::evaluate_expression` or use the proper parameter type for
    /// `Page::execute`:  `EvaluateParams` for strict expression evaluation or
    /// `CallFunctionOnParams` for strict function evaluation.
    ///
    /// If you don't trust the js function detection and are not sure whether
    /// the statement is an expression or of type function (arrow functions: `()
    /// => {..}`), you should pass it as `EvaluateParams` and set the
    /// `EvaluateParams::eval_as_function_fallback` option. This will first
    /// try to evaluate it as expression and if the result comes back
    /// evaluated as `RemoteObjectType::Function` it will submit the
    /// statement again but as function:
    ///
    ///  # Example Evaluate function statement as expression with fallback
    /// option
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::js_protocol::runtime::{EvaluateParams, RemoteObjectType};
    /// # async fn demo(page: Page) -> Result<()> {
    ///     let eval = EvaluateParams::builder().expression("() => {return 42;}");
    ///     // this will fail because the `EvaluationResult` returned by the browser will be
    ///     // of type `Function`
    ///     let result = page
    ///                 .evaluate(eval.clone().build().unwrap())
    ///                 .await?;
    ///     assert_eq!(result.object().r#type, RemoteObjectType::Function);
    ///     assert!(result.into_value::<usize>().is_err());
    ///
    ///     // This will also fail on the first try but it detects that the browser evaluated the
    ///     // statement as function and then evaluate it again but as function
    ///     let sum: usize = page
    ///         .evaluate(eval.eval_as_function_fallback(true).build().unwrap())
    ///         .await?
    ///         .into_value()?;
    ///     # Ok(())
    /// # }
    /// ```
    ///
    /// # Example Evaluate basic expression
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     let sum:usize = page.evaluate("1 + 2").await?.into_value()?;
    ///     assert_eq!(sum, 3);
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn evaluate(&self, evaluate: impl Into<Evaluation>) -> Result<EvaluationResult> {
        match evaluate.into() {
            Evaluation::Expression(mut expr) => {
                if expr.context_id.is_none() {
                    expr.context_id = self.execution_context().await?;
                }
                let fallback = expr.eval_as_function_fallback.and_then(|p| {
                    if p {
                        Some(expr.clone())
                    } else {
                        None
                    }
                });
                let res = self.evaluate_expression(expr).await?;

                if res.object().r#type == RemoteObjectType::Function {
                    // expression was actually a function
                    if let Some(fallback) = fallback {
                        return self.evaluate_function(fallback).await;
                    }
                }
                Ok(res)
            }
            Evaluation::Function(fun) => Ok(self.evaluate_function(fun).await?),
        }
    }

    /// Eexecutes a function withinthe page's context and returns the result.
    ///
    /// # Example Evaluate a promise
    /// This will wait until the promise resolves and then returns the result.
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     let sum:usize = page.evaluate_function("() => Promise.resolve(1 + 2)").await?.into_value()?;
    ///     assert_eq!(sum, 3);
    ///     # Ok(())
    /// # }
    /// ```
    ///
    /// # Example Evaluate an async function
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     let val:usize = page.evaluate_function("async function() {return 42;}").await?.into_value()?;
    ///     assert_eq!(val, 42);
    ///     # Ok(())
    /// # }
    /// ```
    /// # Example Construct a function call
    ///
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # use chromiumoxide_cdp::cdp::js_protocol::runtime::{CallFunctionOnParams, CallArgument};
    /// # async fn demo(page: Page) -> Result<()> {
    ///     let call = CallFunctionOnParams::builder()
    ///            .function_declaration(
    ///                "(a,b) => { return a + b;}"
    ///            )
    ///            .argument(
    ///                CallArgument::builder()
    ///                    .value(serde_json::json!(1))
    ///                    .build(),
    ///            )
    ///            .argument(
    ///                CallArgument::builder()
    ///                    .value(serde_json::json!(2))
    ///                    .build(),
    ///            )
    ///            .build()
    ///            .unwrap();
    ///     let sum:usize = page.evaluate_function(call).await?.into_value()?;
    ///     assert_eq!(sum, 3);
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn evaluate_function(
        &self,
        evaluate: impl Into<CallFunctionOnParams>,
    ) -> Result<EvaluationResult> {
        self.inner.evaluate_function(evaluate).await
    }

    /// Returns the default execution context identifier of this page that
    /// represents the context for JavaScript execution.
    pub async fn execution_context(&self) -> Result<Option<ExecutionContextId>> {
        self.inner.execution_context().await
    }

    /// Returns the secondary execution context identifier of this page that
    /// represents the context for JavaScript execution for manipulating the
    /// DOM.
    ///
    /// See `Page::set_contents`
    pub async fn secondary_execution_context(&self) -> Result<Option<ExecutionContextId>> {
        self.inner.secondary_execution_context().await
    }

    #[cfg(feature = "_cache")]
    /// Clear the local cache after navigation.
    pub async fn clear_local_cache(&self, cache_site: &str) -> Result<&Self> {
        crate::cache::remote::clear_local_session_cache(cache_site).await;
        Ok(self)
    }

    #[cfg(feature = "_cache")]
    /// Clear the local cache after navigation with the key
    pub async fn clear_local_cache_with_key(
        &self,
        target_url: &str,
        auth: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<&Self> {
        let cache_site =
            crate::cache::manager::site_key_for_target_url(target_url, auth, namespace);

        crate::cache::remote::clear_local_session_cache(&cache_site).await;

        Ok(self)
    }

    #[cfg(feature = "_cache")]
    /// Seed the cache. This does nothing without the 'cache' flag.
    pub async fn seed_cache(
        &self,
        cache_site: &str,
        auth: Option<&str>,
        remote: Option<&str>,
        namespace: Option<&str>,
    ) -> Result<&Self> {
        crate::cache::remote::get_cache_site(cache_site, auth, remote, namespace).await;
        Ok(self)
    }

    #[cfg(feature = "_cache")]
    /// Spawn a cache listener to store resources to memory. This does nothing without the 'cache' flag.
    /// You can pass an endpoint to `dump_remote` to store the cache to a url endpoint.
    /// The cache_site is used to track all the urls from the point of navigation like page.goto.
    /// Set the value to Some("true") to use the default endpoint.
    ///
    /// Set `dump_readonly = true` to keep local caching + per-session
    /// cache active but suppress the outbound POST to the remote cache
    /// server. Use when an upstream proxy is responsible for uploads
    /// and chromey should only read from the cache.
    pub async fn spawn_cache_listener(
        &self,
        target_url: &str,
        auth: Option<String>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
        dump_remote: Option<String>,
        dump_readonly: bool,
        namespace: Option<&str>,
    ) -> Result<tokio::task::JoinHandle<()>, crate::error::CdpError> {
        // Eagerly init the remote cache worker so uploads are ready before
        // the first response arrives.  set_client + init are both idempotent
        // (OnceLock / OnceCell — first call wins).
        //
        // Wrap in a timeout so a slow network / DNS issue cannot block page
        // creation indefinitely — the worker will auto-init as a fallback
        // when the first dump job is enqueued. Skip init in read-only mode
        // since no dumps will be enqueued.
        if dump_remote.is_some() && !dump_readonly {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::cache::dump_remote::init_default_cache_worker(),
            )
            .await;
        }

        let cache_site =
            crate::cache::manager::site_key_for_target_url(target_url, auth.as_deref(), namespace);

        let handle = crate::cache::spawn_response_cache_listener(
            self.clone(),
            cache_site,
            auth,
            cache_strategy,
            dump_remote,
            dump_readonly,
        )
        .await?;

        Ok(handle)
    }

    #[cfg(feature = "_cache")]
    /// Convenience wrapper over [`spawn_cache_listener`] that runs in
    /// **read-only** mode — local cache and per-session cache continue
    /// to serve hits, but no responses are ever dumped to a remote
    /// cache server. Equivalent to calling `spawn_cache_listener` with
    /// `dump_remote = None` and `dump_readonly = true`.
    ///
    /// Intended for deployments where an upstream proxy handles remote
    /// dumps on chromey's behalf, and chromey should only consume the
    /// cache.
    pub async fn spawn_cache_listener_read_only(
        &self,
        target_url: &str,
        auth: Option<String>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
        namespace: Option<&str>,
    ) -> Result<tokio::task::JoinHandle<()>, crate::error::CdpError> {
        self.spawn_cache_listener(target_url, auth, cache_strategy, None, true, namespace)
            .await
    }

    #[cfg(feature = "_cache")]
    /// Spawn a cache intercepter to load resources to memory. This does nothing without the 'cache' flag.
    pub async fn spawn_cache_intercepter(
        &self,
        auth: Option<String>,
        policy: Option<crate::cache::BasicCachePolicy>,
        cache_strategy: Option<crate::cache::CacheStrategy>,
    ) -> Result<&Self> {
        crate::cache::spawn_fetch_cache_interceptor(self.clone(), auth, policy, cache_strategy)
            .await?;
        Ok(self)
    }

    pub async fn frame_execution_context(
        &self,
        frame_id: FrameId,
    ) -> Result<Option<ExecutionContextId>> {
        self.inner.frame_execution_context(frame_id).await
    }

    pub async fn frame_secondary_execution_context(
        &self,
        frame_id: FrameId,
    ) -> Result<Option<ExecutionContextId>> {
        self.inner.frame_secondary_execution_context(frame_id).await
    }

    /// Evaluates given script in every frame upon creation (before loading
    /// frame's scripts)
    pub async fn evaluate_on_new_document(
        &self,
        script: impl Into<AddScriptToEvaluateOnNewDocumentParams>,
    ) -> Result<ScriptIdentifier> {
        Ok(self.execute(script.into()).await?.result.identifier)
    }

    /// Set the content of the frame.
    ///
    /// # Example
    /// ```no_run
    /// # use chromiumoxide::page::Page;
    /// # use chromiumoxide::error::Result;
    /// # async fn demo(page: Page) -> Result<()> {
    ///     page.set_content("<body>
    ///  <h1>This was set via chromiumoxide</h1>
    ///  </body>").await?;
    ///     # Ok(())
    /// # }
    /// ```
    pub async fn set_content(&self, html: impl AsRef<str>) -> Result<&Self> {
        // Use Runtime.evaluate instead of Runtime.callFunctionOn so that no
        // explicit execution-context ID is required.  callFunctionOn needs
        // either objectId, executionContextId, or uniqueContextId — none of
        // which may be available on about:blank or freshly-created pages
        // (see https://github.com/spider-rs/chromey/issues/4).
        let escaped = serde_json::json!(html.as_ref());
        self.evaluate(format!(
            "document.open(); document.write({escaped}); document.close()"
        ))
        .await?;
        // relying that document.open() will reset frame lifecycle with "init"
        // lifecycle event. @see https://crrev.com/608658
        self.wait_for_navigation().await
    }

    /// Set the document content with lifecycles. Make sure to have a <base> element for proper host matching.
    pub async fn set_html(
        &self,
        html: String,
        // url_target: Option<&str>,
    ) -> Result<&Self> {
        let (main_frame, _) = tokio::join!(
            // rewrite_base_tag(&html, &url_target),
            self.mainframe(),
            self.set_page_lifecycles_enabled(true)
        );

        if let Ok(frame_opt) = main_frame {
            if let Err(e) = self
                .execute(
                    crate::page::browser_protocol::page::SetDocumentContentParams {
                        frame_id: frame_opt.unwrap_or_default(),
                        html,
                    },
                )
                .await
            {
                tracing::info!("Set Content Error({:?})", e,);
            }
        }

        Ok(self)
    }

    /// Returns the HTML content of the page.
    pub async fn content(&self) -> Result<String> {
        Ok(self.evaluate(OUTER_HTML).await?.into_value()?)
    }

    /// Returns the HTML content of the page
    pub async fn content_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.evaluate(OUTER_HTML).await?.into_bytes()?)
    }

    /// Returns the UTF-16 code-unit length of the serialized page HTML —
    /// the same measure tracked by
    /// [`content_stream::MAX_DOCUMENT_UNITS`].  Cheap: one
    /// `Runtime.evaluate` that yields a primitive `Number`, so no
    /// `RemoteObjectId` is retained and no content bytes are shipped
    /// back to Rust.
    ///
    /// Use this to preflight whether a page is safe to buffer before
    /// calling [`Page::content`] or [`Page::content_bytes`].
    ///
    /// [`content_stream::MAX_DOCUMENT_UNITS`]: crate::content_stream::MAX_DOCUMENT_UNITS
    pub async fn content_length(&self) -> Result<u64> {
        Ok(self.evaluate(OUTER_HTML_LEN).await?.into_value()?)
    }

    /// Returns the UTF-8 byte length of the serialized page HTML —
    /// HTTP `Content-Length` semantics.  Computed inside V8 via `Blob`
    /// so the encoded bytes never leave the page.
    pub async fn content_byte_length(&self) -> Result<u64> {
        Ok(self.evaluate(OUTER_HTML_BYTE_LEN).await?.into_value()?)
    }

    /// Streams the HTML content of the page in fixed-size chunks.
    ///
    /// When the document is below the adaptive streaming threshold (scales
    /// with active page count), this falls back to the single-shot
    /// [`Page::content`] path.  Above the threshold the HTML is UTF-8 encoded
    /// inside the page and pulled over multiple `Runtime.evaluate` calls so
    /// that peak Rust-side memory stays at ~64 KiB per chunk.  With the
    /// `_cache_stream_disk` feature enabled, chunks spill to a temp file and
    /// fall back to memory on any I/O error.
    pub async fn content_streaming(&self) -> Result<String> {
        crate::content_stream::content_streaming(self).await
    }

    /// Streaming variant of [`Page::content_bytes`].  See
    /// [`Page::content_streaming`] for behavior.
    ///
    /// Guardrails: documents exceeding
    /// [`content_stream::MAX_DOCUMENT_UNITS`] are rejected before
    /// streaming; accumulated bytes are capped at
    /// [`content_stream::DEFAULT_MAX_ACCUMULATED_BYTES`] (override with the
    /// `CHROMEY_CONTENT_STREAM_MAX_BYTES` env var).
    ///
    /// [`content_stream::MAX_DOCUMENT_UNITS`]: crate::content_stream::MAX_DOCUMENT_UNITS
    /// [`content_stream::DEFAULT_MAX_ACCUMULATED_BYTES`]: crate::content_stream::DEFAULT_MAX_ACCUMULATED_BYTES
    pub async fn content_bytes_streaming(&self) -> Result<Vec<u8>> {
        crate::content_stream::content_bytes_streaming(self).await
    }

    /// Pump-style async [`Stream`] of the page HTML — yields each chunk of
    /// UTF-8 bytes as Chrome returns it, without accumulating into a `Vec`.
    ///
    /// `chunk_units` optionally caps how many UTF-16 code units the page
    /// slices per round-trip.  `None` uses
    /// [`content_stream::DEFAULT_CHUNK_UNITS`] (`65_536` = 64 Ki);
    /// `Some(n)` pins every slice call to at most `n` units, clamped into
    /// `[`[`content_stream::MIN_CHUNK_UNITS`]` (1024),
    /// `[`content_stream::MAX_CHUNK_UNITS`]` (4_194_304)]`.
    ///
    /// Drop the stream early (cancellation, `StreamExt::take`, `break`) to
    /// stop pulling chunks; the V8 wrapper's `RemoteObjectId` is enqueued
    /// for release via the batched worker on the stream's `Drop`.
    ///
    /// Guardrail: a document exceeding
    /// [`content_stream::MAX_DOCUMENT_UNITS`] UTF-16 code units yields an
    /// error on first poll rather than beginning the stream.
    ///
    /// [`Stream`]: futures_util::Stream
    /// [`content_stream::DEFAULT_CHUNK_UNITS`]: crate::content_stream::DEFAULT_CHUNK_UNITS
    /// [`content_stream::MIN_CHUNK_UNITS`]: crate::content_stream::MIN_CHUNK_UNITS
    /// [`content_stream::MAX_CHUNK_UNITS`]: crate::content_stream::MAX_CHUNK_UNITS
    /// [`content_stream::MAX_DOCUMENT_UNITS`]: crate::content_stream::MAX_DOCUMENT_UNITS
    pub fn content_bytes_stream(
        &self,
        chunk_units: Option<u32>,
    ) -> impl futures_util::Stream<Item = Result<Vec<u8>>> + Send + 'static {
        crate::content_stream::content_bytes_stream(self, chunk_units)
    }

    /// [`Page::content_bytes_stream`] but each yielded chunk is validated
    /// and returned as a `String`.
    pub fn content_stream(
        &self,
        chunk_units: Option<u32>,
    ) -> impl futures_util::Stream<Item = Result<String>> + Send + 'static {
        crate::content_stream::content_stream(self, chunk_units)
    }

    /// Returns the page content rendered as **Markdown**, via the vendor
    /// `Content.getMarkdown` CDP method (single-shot, `stream: false`).
    ///
    /// This requires a remote engine that implements the non-standard
    /// `Content` domain. Not every engine implements `Content.getMarkdown`.
    /// Depending on the engine's unknown-method policy, an unimplemented call
    /// surfaces either a protocol error or an empty success result — an empty
    /// object is returned as `Ok(None)`, meaning "not supported yet". On
    /// either signal, fall back to [`Page::content`] and convert the HTML to
    /// Markdown locally.
    pub async fn content_markdown(&self) -> Result<Option<String>> {
        crate::content_markdown::content_markdown(self).await
    }

    /// Streaming variant of [`Page::content_markdown`]: issues
    /// `Content.getMarkdown` with `stream: true` and accumulates the pushed
    /// `Content.markdownChunk` events into one `String` until the terminal
    /// `Content.markdownDone` event.
    ///
    /// `Ok(None)` means the engine acknowledged the request but never emitted
    /// a stream event within the event timeout — the streaming analogue of
    /// the empty-object "not supported yet" response — so fall back to
    /// [`Page::content`] plus local conversion (a protocol error means the
    /// same). See [`content_markdown_streaming`] for the guardrails
    /// (per-event timeout, accumulated-bytes cap and their env overrides).
    ///
    /// [`content_markdown_streaming`]: crate::content_markdown::content_markdown_streaming
    pub async fn content_markdown_streaming(&self) -> Result<Option<String>> {
        crate::content_markdown::content_markdown_streaming(self).await
    }

    /// Pump-style async [`Stream`] of the page's Markdown — yields each
    /// `Content.markdownChunk` fragment as the engine pushes it, without
    /// accumulating, ending cleanly on `Content.markdownDone`.
    ///
    /// The first poll subscribes to the events and issues
    /// `Content.getMarkdown` with `stream: true`. An engine that doesn't
    /// support the method yields one error (protocol error, or an
    /// unsupported-method report if the request was acknowledged but no
    /// stream event ever arrived) — on any error, fall back to
    /// [`Page::content`] plus local conversion. Drop the stream early to
    /// stop consuming. See [`content_markdown_stream`] for details.
    ///
    /// [`Stream`]: futures_util::Stream
    /// [`content_markdown_stream`]: crate::content_markdown::content_markdown_stream
    pub fn content_markdown_stream(
        &self,
    ) -> impl futures_util::Stream<Item = Result<String>> + Send + 'static {
        crate::content_markdown::content_markdown_stream(self)
    }

    /// Returns the full serialized content of the page (HTML or XML)
    pub async fn content_bytes_xml(&self) -> Result<Vec<u8>> {
        Ok(self.evaluate(FULL_XML_SERIALIZER_JS).await?.into_bytes()?)
    }

    /// Returns the HTML outer html of the page
    pub async fn outer_html_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.outer_html().await?.into())
    }

    /// Enable Chrome's experimental ad filter on all sites.
    pub async fn set_ad_blocking_enabled(&self, enabled: bool) -> Result<&Self> {
        self.send_command(SetAdBlockingEnabledParams::new(enabled))
            .await?;
        Ok(self)
    }

    /// Start to screencast a frame.
    pub async fn start_screencast(
        &self,
        params: impl Into<StartScreencastParams>,
    ) -> Result<&Self> {
        self.execute(params.into()).await?;
        Ok(self)
    }

    /// Acknowledges that a screencast frame has been received by the frontend.
    pub async fn ack_screencast(
        &self,
        params: impl Into<ScreencastFrameAckParams>,
    ) -> Result<&Self> {
        self.send_command(params.into()).await?;
        Ok(self)
    }

    /// Stop screencast a frame.
    pub async fn stop_screencast(&self, params: impl Into<StopScreencastParams>) -> Result<&Self> {
        self.send_command(params.into()).await?;
        Ok(self)
    }

    /// Returns source for the script with given id.
    ///
    /// Debugger must be enabled.
    pub async fn get_script_source(&self, script_id: impl Into<String>) -> Result<String> {
        Ok(self
            .execute(GetScriptSourceParams::new(ScriptId::from(script_id.into())))
            .await?
            .result
            .script_source)
    }
}

impl From<Arc<PageInner>> for Page {
    fn from(inner: Arc<PageInner>) -> Self {
        Self { inner }
    }
}

pub(crate) fn validate_cookie_url(url: &str) -> Result<()> {
    if url.starts_with("data:") {
        Err(CdpError::msg("Data URL page can not have cookie"))
    } else if url == "about:blank" {
        Err(CdpError::msg("Blank page can not have cookie"))
    } else {
        Ok(())
    }
}

/// Page screenshot parameters with extra options.
#[derive(Debug, Default)]
pub struct ScreenshotParams {
    /// Chrome DevTools Protocol screenshot options.
    pub cdp_params: CaptureScreenshotParams,
    /// Take full page screenshot.
    pub full_page: Option<bool>,
    /// Make the background transparent (png only).
    pub omit_background: Option<bool>,
}

impl ScreenshotParams {
    pub fn builder() -> ScreenshotParamsBuilder {
        Default::default()
    }

    pub(crate) fn full_page(&self) -> bool {
        self.full_page.unwrap_or(false)
    }

    pub(crate) fn omit_background(&self) -> bool {
        self.omit_background.unwrap_or(false)
            && self
                .cdp_params
                .format
                .as_ref()
                .map_or(true, |f| f == &CaptureScreenshotFormat::Png)
    }
}

/// Page screenshot parameters builder with extra options.
#[derive(Debug, Default)]
pub struct ScreenshotParamsBuilder {
    /// The cdp params.
    cdp_params: CaptureScreenshotParams,
    /// Full page screenshot?
    full_page: Option<bool>,
    /// Hide the background.
    omit_background: Option<bool>,
}

impl ScreenshotParamsBuilder {
    /// Image compression format (defaults to png).
    pub fn format(mut self, format: impl Into<CaptureScreenshotFormat>) -> Self {
        self.cdp_params.format = Some(format.into());
        self
    }

    /// Compression quality from range [0..100] (jpeg only).
    pub fn quality(mut self, quality: impl Into<i64>) -> Self {
        self.cdp_params.quality = Some(quality.into());
        self
    }

    /// Capture the screenshot of a given region only.
    pub fn clip(mut self, clip: impl Into<Viewport>) -> Self {
        self.cdp_params.clip = Some(clip.into());
        self
    }

    /// Capture the screenshot from the surface, rather than the view (defaults to true).
    pub fn from_surface(mut self, from_surface: impl Into<bool>) -> Self {
        self.cdp_params.from_surface = Some(from_surface.into());
        self
    }

    /// Capture the screenshot beyond the viewport (defaults to false).
    pub fn capture_beyond_viewport(mut self, capture_beyond_viewport: impl Into<bool>) -> Self {
        self.cdp_params.capture_beyond_viewport = Some(capture_beyond_viewport.into());
        self
    }

    /// Full page screen capture.
    pub fn full_page(mut self, full_page: impl Into<bool>) -> Self {
        self.full_page = Some(full_page.into());
        self
    }

    /// Make the background transparent (png only)
    pub fn omit_background(mut self, omit_background: impl Into<bool>) -> Self {
        self.omit_background = Some(omit_background.into());
        self
    }

    pub fn build(self) -> ScreenshotParams {
        ScreenshotParams {
            cdp_params: self.cdp_params,
            full_page: self.full_page,
            omit_background: self.omit_background,
        }
    }
}

impl From<CaptureScreenshotParams> for ScreenshotParams {
    fn from(cdp_params: CaptureScreenshotParams) -> Self {
        Self {
            cdp_params,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum MediaTypeParams {
    /// Default CSS media type behavior for page and print
    #[default]
    Null,
    /// Force screen CSS media type for page and print
    Screen,
    /// Force print CSS media type for page and print
    Print,
}
impl From<MediaTypeParams> for String {
    fn from(media_type: MediaTypeParams) -> Self {
        match media_type {
            MediaTypeParams::Null => "null".to_string(),
            MediaTypeParams::Screen => "screen".to_string(),
            MediaTypeParams::Print => "print".to_string(),
        }
    }
}
