use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use chromiumoxide_cdp::cdp::browser_protocol::target::DetachFromTargetParams;
use std::task::{Context, Poll};
use tokio::sync::oneshot::Sender;

use tokio::sync::Notify;

use crate::auth::Credentials;
use crate::cdp::browser_protocol::target::CloseTargetParams;
use crate::cmd::CommandChain;
use crate::cmd::CommandMessage;
use crate::error::{CdpError, Result};
use crate::handler::browser::BrowserContext;
use crate::handler::domworld::DOMWorldKind;
use crate::handler::emulation::EmulationManager;
use crate::handler::frame::FrameRequestedNavigation;
use crate::handler::frame::{
    FrameEvent, FrameManager, NavigationError, NavigationId, NavigationOk,
};
use crate::handler::network::{NetworkEvent, NetworkManager};
use crate::handler::page::PageHandle;
use crate::handler::viewport::Viewport;
use crate::handler::{PageInner, REQUEST_TIMEOUT};
use crate::listeners::{EventListenerRequest, EventListeners};
use crate::{page::Page, ArcHttpRequest};
use chromiumoxide_cdp::cdp::browser_protocol::{
    browser::BrowserContextId,
    log as cdplog,
    page::{FrameId, GetFrameTreeParams},
    target::{AttachToTargetParams, SessionId, SetAutoAttachParams, TargetId, TargetInfo},
};
use chromiumoxide_cdp::cdp::events::CdpEvent;
use chromiumoxide_cdp::cdp::js_protocol::runtime::{
    ExecutionContextId, RunIfWaitingForDebuggerParams,
};
use chromiumoxide_cdp::cdp::CdpEventMessage;
use chromiumoxide_types::{Command, Method, Request, Response};
use spider_network_blocker::intercept_manager::NetworkInterceptManager;
use std::time::Duration;

macro_rules! advance_state {
    ($s:ident, $cx:ident, $now:ident, $cmds: ident, $next_state:expr ) => {{
        if let Poll::Ready(poll) = $cmds.poll($now) {
            return match poll {
                None => {
                    $s.init_state = $next_state;
                    $s.poll($cx, $now)
                }
                Some(Ok((method, params))) => Some(TargetEvent::Request(Request {
                    method,
                    session_id: $s.session_id.clone().map(Into::into),
                    params,
                })),
                Some(Err(_)) => Some($s.on_initialization_failed()),
            };
        } else {
            return None;
        }
    }};
}

lazy_static::lazy_static! {
    /// Initial start command params.
    static ref INIT_COMMANDS_PARAMS: Vec<(chromiumoxide_types::MethodId, serde_json::Value)> = {
        if let Ok(attach) = SetAutoAttachParams::builder()
            .flatten(true)
            .auto_attach(true)
            .wait_for_debugger_on_start(true)
            .build() {
                let disable_log = cdplog::DisableParams::default();

                let mut cmds =  vec![
                    (
                        attach.identifier(),
                        serde_json::to_value(attach).unwrap_or_default(),
                    ),
                    (
                        disable_log.identifier(),
                        serde_json::to_value(disable_log).unwrap_or_default(),
                    )
                ];

                // enable performance on pages.
                if cfg!(feature = "collect_metrics") {
                    let enable_performance = chromiumoxide_cdp::cdp::browser_protocol::performance::EnableParams::default();
                    cmds.push((
                        enable_performance.identifier(),
                        serde_json::to_value(enable_performance).unwrap_or_default(),
                    ));
                }

                cmds
            } else {
                vec![]
            }
    };

    /// Attach to target commands
    static ref ATTACH_TARGET: (chromiumoxide_types::MethodId, serde_json::Value) = {
        let runtime_cmd = RunIfWaitingForDebuggerParams::default();

        (runtime_cmd.identifier(), serde_json::to_value(runtime_cmd).unwrap_or_default())
    };
}

/// Per-queue cap on waiter sends per `Target::poll` call.
///
/// Each `wait_for_*` queue can hold an unbounded number of `oneshot::Sender`s
/// registered by concurrent callers. Firing them all in one tight `pop()`
/// loop previously produced multi-hundred-microsecond synchronous bursts
/// inside the handler's event loop under fan-out (e.g. 1000 tasks awaiting
/// `wait_for_load` on one page). Capping at 64 per queue per poll keeps
/// worst-case burst at ~5 × 64 oneshot sends (~6μs) before yielding. Any
/// remainder is drained on subsequent polls, re-armed via `Waker::wake_by_ref`.
const WAITER_DRAIN_BUDGET: usize = 64;

/// Pop up to `budget` senders from `queue` and deliver `value` to each.
///
/// Returns `true` when the queue still contains senders after draining.
/// Dropped receivers (closed senders) are silently ignored — they consume
/// a budget slot but contribute no cost beyond the cheap `send` no-op.
///
/// The queue is pruned of closed senders elsewhere once per `Target::poll`
/// (before this helper runs), so in steady state `budget` slots approximate
/// `budget` live fan-out sends.
#[inline]
fn drain_waiters_bounded(
    queue: &mut Vec<Sender<ArcHttpRequest>>,
    http_request: Option<&Arc<crate::handler::http::HttpRequest>>,
    budget: usize,
) -> bool {
    let to_fire = queue.len().min(budget);
    for _ in 0..to_fire {
        // `pop` cannot be `None` here: `to_fire <= queue.len()`.
        if let Some(tx) = queue.pop() {
            let _ = tx.send(http_request.cloned());
        }
    }
    !queue.is_empty()
}

#[derive(Debug)]
pub struct Target {
    /// Info about this target as returned from the chromium instance
    info: TargetInfo,
    /// The type of this target
    r#type: TargetType,
    /// Configs for this target
    config: TargetConfig,
    /// The context this target is running in
    browser_context: BrowserContext,
    /// The frame manager that maintains the state of all frames and handles
    /// navigations of frames
    frame_manager: FrameManager,
    /// Handles all the https
    pub(crate) network_manager: NetworkManager,
    emulation_manager: EmulationManager,
    /// The identifier of the session this target is attached to
    session_id: Option<SessionId>,
    /// The handle of the browser page of this target
    page: Option<PageHandle>,
    /// Drives this target towards initialization
    pub(crate) init_state: TargetInit,
    /// Currently queued events to report to the `Handler`
    queued_events: VecDeque<TargetEvent>,
    /// All registered event subscriptions
    event_listeners: EventListeners,
    /// Senders that need to be notified once the main frame has loaded
    wait_for_frame_navigation: Vec<Sender<ArcHttpRequest>>,
    /// Senders notified once `DOMContentLoaded` fires (before `load`).
    wait_for_dom_content_loaded: Vec<Sender<ArcHttpRequest>>,
    /// Senders notified once the `load` event fires (all subresources done).
    wait_for_load: Vec<Sender<ArcHttpRequest>>,
    /// Senders that need to be notified once the main frame reaches `networkIdle`.
    wait_for_network_idle: Vec<Sender<ArcHttpRequest>>,
    /// (Optional) for `networkAlmostIdle` if you want it as well.
    wait_for_network_almost_idle: Vec<Sender<ArcHttpRequest>>,
    /// The sender who requested the page.
    initiator: Option<Sender<Result<Page>>>,
}

impl Target {
    /// Create a new target instance with `TargetInfo` after a
    /// `CreateTargetParams` request.
    pub fn new(info: TargetInfo, config: TargetConfig, browser_context: BrowserContext) -> Self {
        let ty = TargetType::new(&info.r#type);
        let request_timeout: Duration = config.request_timeout;
        let mut network_manager = NetworkManager::new(config.ignore_https_errors, request_timeout);

        if !config.cache_enabled {
            network_manager.set_cache_enabled(false);
        }

        if !config.service_worker_enabled {
            network_manager.set_service_worker_enabled(true);
        }

        network_manager.set_request_interception(config.request_intercept);
        network_manager.max_bytes_allowed = config.max_bytes_allowed;
        network_manager.max_redirects = config.max_redirects;

        if let Some(headers) = &config.extra_headers {
            network_manager.set_extra_headers(headers.clone());
        }

        if let Some(whitelist) = &config.whitelist_patterns {
            network_manager.set_whitelist_patterns(whitelist.clone());
        }

        if let Some(blacklist) = &config.blacklist_patterns {
            network_manager.set_blacklist_patterns(blacklist);
        }

        network_manager.ignore_visuals = config.ignore_visuals;
        network_manager.block_javascript = config.ignore_javascript;
        network_manager.block_analytics = config.ignore_analytics;
        network_manager.block_prefetch = config.ignore_prefetch;

        network_manager.block_stylesheets = config.ignore_stylesheets;
        network_manager.allow_first_party_stylesheets = config.allow_first_party_stylesheets;
        network_manager.allow_first_party_javascript = config.allow_first_party_javascript;
        network_manager.allow_first_party_visuals = config.allow_first_party_visuals;
        network_manager.only_html = config.only_html;
        network_manager.intercept_manager = config.intercept_manager;
        network_manager.set_remote_local_policy(config.remote_local_policy);

        // Push the one-shot interception policy now that every field above is
        // configured (must run AFTER the field setup — `set_request_interception`
        // above only queued `Fetch.enable`). No-op unless opted in.
        network_manager.emit_request_policy();

        #[cfg(feature = "adblock")]
        if let Some(rules) = &config.adblock_filter_rules {
            use adblock::lists::{FilterSet, ParseOptions, RuleTypes};

            let mut filter_set = FilterSet::new(false);
            let mut opts = ParseOptions::default();
            opts.rule_types = RuleTypes::All;

            // Include built-in patterns.
            filter_set.add_filters(&*spider_network_blocker::adblock::ADBLOCK_PATTERNS, opts);
            // Merge user-supplied rules (e.g. EasyList / EasyPrivacy content).
            filter_set.add_filters(rules.iter().map(|s| s.as_str()), opts);

            let engine = adblock::Engine::from_filter_set(filter_set, true);
            network_manager.set_adblock_engine(std::sync::Arc::new(engine));
        }

        let mut frame_manager = FrameManager::new(request_timeout);
        frame_manager.set_max_main_frame_navigations(config.max_main_frame_navigations);

        Self {
            info,
            r#type: ty,
            config,
            frame_manager,
            network_manager,
            emulation_manager: EmulationManager::new(request_timeout),
            session_id: None,
            page: None,
            init_state: TargetInit::AttachToTarget,
            wait_for_frame_navigation: Default::default(),
            wait_for_dom_content_loaded: Default::default(),
            wait_for_load: Default::default(),
            wait_for_network_idle: Default::default(),
            wait_for_network_almost_idle: Default::default(),
            queued_events: Default::default(),
            event_listeners: Default::default(),
            initiator: None,
            browser_context,
        }
    }

    /// Set the session id.
    pub fn set_session_id(&mut self, id: SessionId) {
        self.session_id = Some(id)
    }

    /// Get the session id.
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// Get the session id mut.
    pub fn session_id_mut(&mut self) -> &mut Option<SessionId> {
        &mut self.session_id
    }

    /// Get the browser context.
    pub fn browser_context(&self) -> &BrowserContext {
        &self.browser_context
    }

    /// The identifier for this target
    pub fn target_id(&self) -> &TargetId {
        &self.info.target_id
    }

    /// The type of this target
    pub fn r#type(&self) -> &TargetType {
        &self.r#type
    }

    /// Whether this target is already initialized
    pub fn is_initialized(&self) -> bool {
        matches!(self.init_state, TargetInit::Initialized)
    }

    /// Navigate a frame
    pub fn goto(&mut self, req: FrameRequestedNavigation) {
        if self.network_manager.has_target_domain() {
            self.network_manager.clear_target_domain();
            let goto_url = req
                .req
                .params
                .as_object()
                .and_then(|o| o.get("url"))
                .and_then(|v| v.as_str());

            if let Some(url) = goto_url {
                self.network_manager.set_page_url(url.into());
            }
        }
        self.frame_manager.goto(req);
    }

    /// Create a new page from the session.
    fn create_page(&mut self) {
        if self.page.is_none() {
            if let Some(session) = self.session_id.clone() {
                let handle = PageHandle::with_capacity(
                    self.target_id().clone(),
                    session,
                    self.opener_id().cloned(),
                    self.config.request_timeout,
                    self.config.page_wake.clone(),
                    self.config.page_channel_capacity,
                );
                self.page = Some(handle);
            }
        }
    }

    /// Tries to create the `PageInner` if this target is already initialized
    pub(crate) fn get_or_create_page(&mut self) -> Option<&Arc<PageInner>> {
        self.create_page();
        self.page.as_ref().map(|p| p.inner())
    }

    /// Mutable access to the page handle (for `try_recv` in `Handler::run()`).
    pub(crate) fn page_mut(&mut self) -> Option<&mut PageHandle> {
        self.page.as_mut()
    }

    /// Is the target a page?
    pub fn is_page(&self) -> bool {
        self.r#type().is_page()
    }

    /// The browser context ID.
    pub fn browser_context_id(&self) -> Option<&BrowserContextId> {
        self.info.browser_context_id.as_ref()
    }

    /// The target connection info.
    pub fn info(&self) -> &TargetInfo {
        &self.info
    }

    /// Get the target that opened this target. Top-level targets return `None`.
    pub fn opener_id(&self) -> Option<&TargetId> {
        self.info.opener_id.as_ref()
    }

    pub fn frame_manager(&self) -> &FrameManager {
        &self.frame_manager
    }

    /// The frame manager.
    pub fn frame_manager_mut(&mut self) -> &mut FrameManager {
        &mut self.frame_manager
    }

    /// Get event listeners mutably.
    pub fn event_listeners_mut(&mut self) -> &mut EventListeners {
        &mut self.event_listeners
    }

    /// Received a response to a command issued by this target
    pub fn on_response(&mut self, resp: Response, method: &str) {
        if let Some(cmds) = self.init_state.commands_mut() {
            cmds.received_response(method);
        }

        if let GetFrameTreeParams::IDENTIFIER = method {
            if let Some(resp) = resp
                .result
                .and_then(|val| GetFrameTreeParams::response_from_value(val).ok())
            {
                self.frame_manager.on_frame_tree(resp.frame_tree);
            }
        }
        // requests originated from the network manager all return an empty response, hence they
        // can be ignored here
    }

    /// On CDP Event message.
    pub fn on_event(&mut self, event: CdpEventMessage) {
        let CdpEventMessage {
            params,
            method,
            session_id,
            ..
        } = event;

        let is_session_scoped = matches!(
            params,
            CdpEvent::FetchRequestPaused(_)
                | CdpEvent::FetchAuthRequired(_)
                | CdpEvent::NetworkRequestWillBeSent(_)
                | CdpEvent::NetworkResponseReceived(_)
                | CdpEvent::NetworkLoadingFinished(_)
                | CdpEvent::NetworkLoadingFailed(_)
                | CdpEvent::PageFrameAttached(_)
                | CdpEvent::PageFrameDetached(_)
                | CdpEvent::PageFrameNavigated(_)
                | CdpEvent::PageNavigatedWithinDocument(_)
                | CdpEvent::PageLifecycleEvent(_)
                | CdpEvent::PageFrameStartedLoading(_)
                | CdpEvent::PageFrameStoppedLoading(_)
                | CdpEvent::RuntimeExecutionContextCreated(_)
                | CdpEvent::RuntimeExecutionContextDestroyed(_)
                | CdpEvent::RuntimeExecutionContextsCleared(_)
                | CdpEvent::RuntimeBindingCalled(_)
        );

        if is_session_scoped {
            let ev_sid: &str = match session_id.as_deref() {
                Some(s) => s,
                None => return,
            };

            let self_sid: &str = match self.session_id.as_ref() {
                Some(sid) => sid.as_ref(),
                None => return,
            };

            if self_sid != ev_sid {
                return;
            }
        }

        match &params {
            // `FrameManager` events
            CdpEvent::PageFrameAttached(ev) => self
                .frame_manager
                .on_frame_attached(ev.frame_id.clone(), Some(ev.parent_frame_id.clone())),
            CdpEvent::PageFrameDetached(ev) => self.frame_manager.on_frame_detached(ev),
            CdpEvent::PageFrameNavigated(ev) => {
                self.frame_manager.on_frame_navigated(&ev.frame);
            }
            CdpEvent::PageNavigatedWithinDocument(ev) => {
                self.frame_manager.on_frame_navigated_within_document(ev)
            }
            CdpEvent::RuntimeExecutionContextCreated(ev) => {
                self.frame_manager.on_frame_execution_context_created(ev)
            }
            CdpEvent::RuntimeExecutionContextDestroyed(ev) => {
                self.frame_manager.on_frame_execution_context_destroyed(ev)
            }
            CdpEvent::RuntimeExecutionContextsCleared(_) => {
                self.frame_manager.on_execution_contexts_cleared()
            }
            CdpEvent::RuntimeBindingCalled(ev) => {
                // TODO check if binding registered and payload is json
                self.frame_manager.on_runtime_binding_called(ev)
            }
            CdpEvent::PageLifecycleEvent(ev) => self.frame_manager.on_page_lifecycle_event(ev),
            CdpEvent::PageFrameStartedLoading(ev) => {
                self.frame_manager.on_frame_started_loading(ev);
            }
            CdpEvent::PageFrameStoppedLoading(ev) => {
                self.frame_manager.on_frame_stopped_loading(ev);
            }
            // `Target` events
            CdpEvent::TargetAttachedToTarget(ev) => {
                if ev.waiting_for_debugger {
                    let runtime_cmd = ATTACH_TARGET.clone();

                    self.queued_events.push_back(TargetEvent::Request(Request {
                        method: runtime_cmd.0,
                        session_id: Some(ev.session_id.clone().into()),
                        params: runtime_cmd.1,
                    }));
                }

                if "service_worker" == &ev.target_info.r#type {
                    let detach_command = DetachFromTargetParams::builder()
                        .session_id(ev.session_id.clone())
                        .build();

                    let method = detach_command.identifier();

                    if let Ok(params) = serde_json::to_value(detach_command) {
                        self.queued_events.push_back(TargetEvent::Request(Request {
                            method,
                            session_id: self.session_id.clone().map(Into::into),
                            params,
                        }));
                    }
                }
            }
            // `NetworkManager` events
            CdpEvent::FetchRequestPaused(ev) => self.network_manager.on_fetch_request_paused(ev),
            CdpEvent::FetchAuthRequired(ev) => self.network_manager.on_fetch_auth_required(ev),
            CdpEvent::NetworkRequestWillBeSent(ev) => {
                self.network_manager.on_request_will_be_sent(ev)
            }
            CdpEvent::NetworkRequestServedFromCache(ev) => {
                self.network_manager.on_request_served_from_cache(ev)
            }
            CdpEvent::NetworkResponseReceived(ev) => self.network_manager.on_response_received(ev),
            CdpEvent::NetworkLoadingFinished(ev) => {
                self.network_manager.on_network_loading_finished(ev)
            }
            CdpEvent::NetworkLoadingFailed(ev) => {
                self.network_manager.on_network_loading_failed(ev)
            }
            _ => (),
        }
        chromiumoxide_cdp::consume_event!(match params {
           |ev| self.event_listeners.start_send(ev),
           |json| { let _ = self.event_listeners.try_send_custom(&method, json);}
        });
    }

    /// Called when a init command timed out
    fn on_initialization_failed(&mut self) -> TargetEvent {
        if let Some(initiator) = self.initiator.take() {
            let _ = initiator.send(Err(CdpError::Timeout));
        }
        self.init_state = TargetInit::Closing;
        let close_target = CloseTargetParams::new(self.info.target_id.clone());

        TargetEvent::Request(Request {
            method: close_target.identifier(),
            session_id: self.session_id.clone().map(Into::into),
            params: serde_json::to_value(close_target).unwrap_or_default(),
        })
    }

    /// Advance that target's state
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>, now: Instant) -> Option<TargetEvent> {
        if !self.is_page() {
            // can only poll pages
            return None;
        }

        match &mut self.init_state {
            TargetInit::AttachToTarget => {
                self.init_state = TargetInit::InitializingFrame(FrameManager::init_commands(
                    self.config.request_timeout,
                ));

                if let Ok(params) = AttachToTargetParams::builder()
                    .target_id(self.target_id().clone())
                    .flatten(true)
                    .build()
                {
                    return Some(TargetEvent::Request(Request::new(
                        params.identifier(),
                        serde_json::to_value(params).unwrap_or_default(),
                    )));
                } else {
                    return None;
                }
            }
            TargetInit::InitializingFrame(cmds) => {
                self.session_id.as_ref()?;
                if let Poll::Ready(poll) = cmds.poll(now) {
                    return match poll {
                        None => {
                            if let Some(world_name) = self.frame_manager.get_isolated_world_name() {
                                let world_name = world_name.clone();

                                if let Some(isolated_world_cmds) =
                                    self.frame_manager.ensure_isolated_world(&world_name)
                                {
                                    *cmds = isolated_world_cmds;
                                } else {
                                    self.init_state = TargetInit::InitializingNetwork(
                                        self.network_manager.init_commands(),
                                    );
                                }
                            } else {
                                self.init_state = TargetInit::InitializingNetwork(
                                    self.network_manager.init_commands(),
                                );
                            }
                            self.poll(cx, now)
                        }
                        Some(Ok((method, params))) => Some(TargetEvent::Request(Request {
                            method,
                            session_id: self.session_id.clone().map(Into::into),
                            params,
                        })),
                        Some(Err(_)) => Some(self.on_initialization_failed()),
                    };
                } else {
                    return None;
                }
            }
            TargetInit::InitializingNetwork(cmds) => {
                advance_state!(
                    self,
                    cx,
                    now,
                    cmds,
                    TargetInit::InitializingPage(Self::page_init_commands(
                        self.config.request_timeout
                    ))
                );
            }
            TargetInit::InitializingPage(cmds) => {
                advance_state!(
                    self,
                    cx,
                    now,
                    cmds,
                    match self.config.viewport.as_ref() {
                        Some(viewport) => TargetInit::InitializingEmulation(
                            self.emulation_manager.init_commands(viewport)
                        ),
                        None => TargetInit::Initialized,
                    }
                );
            }
            TargetInit::InitializingEmulation(cmds) => {
                advance_state!(self, cx, now, cmds, TargetInit::Initialized);
            }
            TargetInit::Initialized => {
                if let Some(initiator) = self.initiator.take() {
                    // make sure that the main frame of the page has finished loading
                    if self
                        .frame_manager
                        .main_frame()
                        .map(|frame| frame.is_loaded())
                        .unwrap_or_default()
                    {
                        if let Some(page) = self.get_or_create_page() {
                            let _ = initiator.send(Ok(page.clone().into()));
                        } else {
                            self.initiator = Some(initiator);
                        }
                    } else {
                        self.initiator = Some(initiator);
                    }
                }
            }
            TargetInit::Closing => return None,
        };

        // Prune senders whose receivers have been dropped (caller
        // timed out or was cancelled) so the vecs don't grow unbounded.
        // Done once per poll() call, outside the inner loop.
        if !self.wait_for_frame_navigation.is_empty() {
            self.wait_for_frame_navigation.retain(|tx| !tx.is_closed());
        }
        if !self.wait_for_dom_content_loaded.is_empty() {
            self.wait_for_dom_content_loaded
                .retain(|tx| !tx.is_closed());
        }
        if !self.wait_for_load.is_empty() {
            self.wait_for_load.retain(|tx| !tx.is_closed());
        }
        if !self.wait_for_network_idle.is_empty() {
            self.wait_for_network_idle.retain(|tx| !tx.is_closed());
        }
        if !self.wait_for_network_almost_idle.is_empty() {
            self.wait_for_network_almost_idle
                .retain(|tx| !tx.is_closed());
        }

        loop {
            if self.init_state == TargetInit::Closing {
                break None;
            }

            if let Some(frame) = self.frame_manager.main_frame() {
                let req = frame.http_request();
                let mut waiters_remaining = false;

                if frame.is_dom_content_loaded() {
                    waiters_remaining |= drain_waiters_bounded(
                        &mut self.wait_for_dom_content_loaded,
                        req,
                        WAITER_DRAIN_BUDGET,
                    );
                    waiters_remaining |= drain_waiters_bounded(
                        &mut self.wait_for_frame_navigation,
                        req,
                        WAITER_DRAIN_BUDGET,
                    );
                }

                if frame.is_loaded() {
                    waiters_remaining |=
                        drain_waiters_bounded(&mut self.wait_for_load, req, WAITER_DRAIN_BUDGET);
                }

                if frame.is_network_idle() {
                    waiters_remaining |= drain_waiters_bounded(
                        &mut self.wait_for_network_idle,
                        req,
                        WAITER_DRAIN_BUDGET,
                    );
                }

                if frame.is_network_almost_idle() {
                    waiters_remaining |= drain_waiters_bounded(
                        &mut self.wait_for_network_almost_idle,
                        req,
                        WAITER_DRAIN_BUDGET,
                    );
                }

                if waiters_remaining {
                    // More waiters queued than the per-poll budget.
                    // Self-wake so the handler re-enters and drains the
                    // remainder on the next tick instead of stalling.
                    cx.waker().wake_by_ref();
                }
            }

            // Drain queued messages first.
            if let Some(ev) = self.queued_events.pop_front() {
                return Some(ev);
            }

            if let Some(handle) = self.page.as_mut() {
                while let Poll::Ready(Some(msg)) = handle.rx.poll_recv(cx) {
                    if self.init_state == TargetInit::Closing {
                        break;
                    }

                    match msg {
                        TargetMessage::Command(cmd) => {
                            if cmd.method == "Network.setBlockedURLs" {
                                if let Some(arr) = cmd.params.get("urls").and_then(|v| v.as_array())
                                {
                                    let mut unblock_all = false;
                                    let mut block_all = false;

                                    for s in arr.iter().filter_map(|v| v.as_str()) {
                                        if s == "!*" {
                                            unblock_all = true;
                                            break; // "!*" overrides any block rules
                                        }
                                        if s.contains('*') {
                                            block_all = true;
                                        }
                                    }

                                    if unblock_all {
                                        self.network_manager.set_block_all(false);
                                    } else if block_all {
                                        self.network_manager.set_block_all(true);
                                    }
                                }
                            }
                            self.queued_events.push_back(TargetEvent::Command(cmd));
                        }
                        TargetMessage::MainFrame(tx) => {
                            let _ =
                                tx.send(self.frame_manager.main_frame().map(|f| f.id().clone()));
                        }
                        TargetMessage::AllFrames(tx) => {
                            let _ = tx.send(
                                self.frame_manager
                                    .frames()
                                    .map(|f| f.id().clone())
                                    .collect(),
                            );
                        }
                        #[cfg(feature = "_cache")]
                        TargetMessage::CacheKey((cache_key, cache_policy)) => {
                            self.network_manager.set_cache_site_key(cache_key);
                            self.network_manager.set_cache_policy(cache_policy);
                        }
                        TargetMessage::Url(req) => {
                            let GetUrl { frame_id, tx } = req;
                            let frame = if let Some(frame_id) = frame_id {
                                self.frame_manager.frame(&frame_id)
                            } else {
                                self.frame_manager.main_frame()
                            };
                            let _ = tx.send(frame.and_then(|f| f.url().map(str::to_string)));
                        }
                        TargetMessage::Name(req) => {
                            let GetName { frame_id, tx } = req;
                            let frame = if let Some(frame_id) = frame_id {
                                self.frame_manager.frame(&frame_id)
                            } else {
                                self.frame_manager.main_frame()
                            };
                            let _ = tx.send(frame.and_then(|f| f.name().map(str::to_string)));
                        }
                        TargetMessage::Parent(req) => {
                            let GetParent { frame_id, tx } = req;
                            let frame = self.frame_manager.frame(&frame_id);
                            let _ = tx.send(frame.and_then(|f| f.parent_id().cloned()));
                        }
                        TargetMessage::WaitForNavigation(tx) => {
                            if let Some(frame) = self.frame_manager.main_frame() {
                                if frame.is_dom_content_loaded() {
                                    let _ = tx.send(frame.http_request().cloned());
                                } else {
                                    self.wait_for_frame_navigation.push(tx);
                                }
                            } else {
                                self.wait_for_frame_navigation.push(tx);
                            }
                        }
                        TargetMessage::WaitForDomContentLoaded(tx) => {
                            if let Some(frame) = self.frame_manager.main_frame() {
                                if frame.is_dom_content_loaded() {
                                    let _ = tx.send(frame.http_request().cloned());
                                } else {
                                    self.wait_for_dom_content_loaded.push(tx);
                                }
                            } else {
                                self.wait_for_dom_content_loaded.push(tx);
                            }
                        }
                        TargetMessage::WaitForLoad(tx) => {
                            if let Some(frame) = self.frame_manager.main_frame() {
                                if frame.is_loaded() {
                                    let _ = tx.send(frame.http_request().cloned());
                                } else {
                                    self.wait_for_load.push(tx);
                                }
                            } else {
                                self.wait_for_load.push(tx);
                            }
                        }
                        TargetMessage::WaitForNetworkIdle(tx) => {
                            if let Some(frame) = self.frame_manager.main_frame() {
                                if frame.is_network_idle() {
                                    let _ = tx.send(frame.http_request().cloned());
                                } else {
                                    self.wait_for_network_idle.push(tx);
                                }
                            } else {
                                self.wait_for_network_idle.push(tx);
                            }
                        }
                        TargetMessage::WaitForNetworkAlmostIdle(tx) => {
                            if let Some(frame) = self.frame_manager.main_frame() {
                                if frame.is_network_almost_idle() {
                                    let _ = tx.send(frame.http_request().cloned());
                                } else {
                                    self.wait_for_network_almost_idle.push(tx);
                                }
                            } else {
                                self.wait_for_network_almost_idle.push(tx);
                            }
                        }
                        TargetMessage::AddEventListener(req) => {
                            if req.method == "Fetch.requestPaused" {
                                self.network_manager.enable_request_intercept();
                            }
                            // register a new listener
                            self.event_listeners.add_listener(req);
                        }
                        TargetMessage::GetExecutionContext(ctx) => {
                            let GetExecutionContext {
                                dom_world,
                                frame_id,
                                tx,
                            } = ctx;
                            let frame = if let Some(frame_id) = frame_id {
                                self.frame_manager.frame(&frame_id)
                            } else {
                                self.frame_manager.main_frame()
                            };

                            if let Some(frame) = frame {
                                match dom_world {
                                    DOMWorldKind::Main => {
                                        let _ = tx.send(frame.main_world().execution_context());
                                    }
                                    DOMWorldKind::Secondary => {
                                        let _ =
                                            tx.send(frame.secondary_world().execution_context());
                                    }
                                }
                            } else {
                                let _ = tx.send(None);
                            }
                        }
                        TargetMessage::Authenticate(credentials) => {
                            self.network_manager.authenticate(credentials);
                        }
                        TargetMessage::BlockNetwork(blocked) => {
                            self.network_manager.set_block_all(blocked);
                        }
                        TargetMessage::EnableInterception(enabled) => {
                            // if interception is enabled disable the user facing handling.
                            self.network_manager.user_request_interception_enabled = !enabled;
                        }
                    }
                }
            }

            while let Some(event) = self.network_manager.poll() {
                if self.init_state == TargetInit::Closing {
                    break;
                }
                match event {
                    NetworkEvent::SendCdpRequest((method, params)) => {
                        // send a message to the browser
                        self.queued_events.push_back(TargetEvent::Request(Request {
                            method,
                            session_id: self.session_id.clone().map(Into::into),
                            params,
                        }))
                    }
                    NetworkEvent::Request(_) => {}
                    NetworkEvent::Response(_) => {}
                    NetworkEvent::RequestFailed(request) => {
                        self.frame_manager.on_http_request_finished(request);
                    }
                    NetworkEvent::RequestFinished(request) => {
                        self.frame_manager.on_http_request_finished(request);
                    }
                    NetworkEvent::BytesConsumed(n) => {
                        self.queued_events.push_back(TargetEvent::BytesConsumed(n));
                    }
                }
            }

            while let Some(event) = self.frame_manager.poll(now) {
                if self.init_state == TargetInit::Closing {
                    break;
                }
                match event {
                    FrameEvent::NavigationResult(res) => {
                        self.queued_events
                            .push_back(TargetEvent::NavigationResult(res));
                    }
                    FrameEvent::NavigationRequest(id, req) => {
                        self.queued_events
                            .push_back(TargetEvent::NavigationRequest(id, req));
                    }
                }
            }

            if self.queued_events.is_empty() {
                return None;
            }
        }
    }

    /// Process a single message from the page channel.
    ///
    /// Used by `Handler::run()` after `try_recv()` drains the page channel.
    pub(crate) fn on_page_message(&mut self, msg: TargetMessage) {
        if self.init_state == TargetInit::Closing {
            return;
        }
        match msg {
            TargetMessage::Command(cmd) => {
                if cmd.method == "Network.setBlockedURLs" {
                    if let Some(arr) = cmd.params.get("urls").and_then(|v| v.as_array()) {
                        let mut unblock_all = false;
                        let mut block_all = false;
                        for s in arr.iter().filter_map(|v| v.as_str()) {
                            if s == "!*" {
                                unblock_all = true;
                                break;
                            }
                            if s.contains('*') {
                                block_all = true;
                            }
                        }
                        if unblock_all {
                            self.network_manager.set_block_all(false);
                        } else if block_all {
                            self.network_manager.set_block_all(true);
                        }
                    }
                }
                self.queued_events.push_back(TargetEvent::Command(cmd));
            }
            TargetMessage::MainFrame(tx) => {
                let _ = tx.send(self.frame_manager.main_frame().map(|f| f.id().clone()));
            }
            TargetMessage::AllFrames(tx) => {
                let _ = tx.send(
                    self.frame_manager
                        .frames()
                        .map(|f| f.id().clone())
                        .collect(),
                );
            }
            #[cfg(feature = "_cache")]
            TargetMessage::CacheKey((cache_key, cache_policy)) => {
                self.network_manager.set_cache_site_key(cache_key);
                self.network_manager.set_cache_policy(cache_policy);
            }
            TargetMessage::Url(req) => {
                let GetUrl { frame_id, tx } = req;
                let frame = if let Some(frame_id) = frame_id {
                    self.frame_manager.frame(&frame_id)
                } else {
                    self.frame_manager.main_frame()
                };
                let _ = tx.send(frame.and_then(|f| f.url().map(str::to_string)));
            }
            TargetMessage::Name(req) => {
                let GetName { frame_id, tx } = req;
                let frame = if let Some(frame_id) = frame_id {
                    self.frame_manager.frame(&frame_id)
                } else {
                    self.frame_manager.main_frame()
                };
                let _ = tx.send(frame.and_then(|f| f.name().map(str::to_string)));
            }
            TargetMessage::Parent(req) => {
                let GetParent { frame_id, tx } = req;
                let frame = self.frame_manager.frame(&frame_id);
                let _ = tx.send(frame.and_then(|f| f.parent_id().cloned()));
            }
            TargetMessage::WaitForNavigation(tx) => {
                if let Some(frame) = self.frame_manager.main_frame() {
                    if frame.is_dom_content_loaded() {
                        let _ = tx.send(frame.http_request().cloned());
                    } else {
                        self.wait_for_frame_navigation.push(tx);
                    }
                } else {
                    self.wait_for_frame_navigation.push(tx);
                }
            }
            TargetMessage::WaitForDomContentLoaded(tx) => {
                if let Some(frame) = self.frame_manager.main_frame() {
                    if frame.is_dom_content_loaded() {
                        let _ = tx.send(frame.http_request().cloned());
                    } else {
                        self.wait_for_dom_content_loaded.push(tx);
                    }
                } else {
                    self.wait_for_dom_content_loaded.push(tx);
                }
            }
            TargetMessage::WaitForLoad(tx) => {
                if let Some(frame) = self.frame_manager.main_frame() {
                    if frame.is_loaded() {
                        let _ = tx.send(frame.http_request().cloned());
                    } else {
                        self.wait_for_load.push(tx);
                    }
                } else {
                    self.wait_for_load.push(tx);
                }
            }
            TargetMessage::WaitForNetworkIdle(tx) => {
                if let Some(frame) = self.frame_manager.main_frame() {
                    if frame.is_network_idle() {
                        let _ = tx.send(frame.http_request().cloned());
                    } else {
                        self.wait_for_network_idle.push(tx);
                    }
                } else {
                    self.wait_for_network_idle.push(tx);
                }
            }
            TargetMessage::WaitForNetworkAlmostIdle(tx) => {
                if let Some(frame) = self.frame_manager.main_frame() {
                    if frame.is_network_almost_idle() {
                        let _ = tx.send(frame.http_request().cloned());
                    } else {
                        self.wait_for_network_almost_idle.push(tx);
                    }
                } else {
                    self.wait_for_network_almost_idle.push(tx);
                }
            }
            TargetMessage::AddEventListener(req) => {
                if req.method == "Fetch.requestPaused" {
                    self.network_manager.enable_request_intercept();
                }
                self.event_listeners.add_listener(req);
            }
            TargetMessage::GetExecutionContext(ctx) => {
                let GetExecutionContext {
                    dom_world,
                    frame_id,
                    tx,
                } = ctx;
                let frame = if let Some(frame_id) = frame_id {
                    self.frame_manager.frame(&frame_id)
                } else {
                    self.frame_manager.main_frame()
                };
                if let Some(frame) = frame {
                    match dom_world {
                        DOMWorldKind::Main => {
                            let _ = tx.send(frame.main_world().execution_context());
                        }
                        DOMWorldKind::Secondary => {
                            let _ = tx.send(frame.secondary_world().execution_context());
                        }
                    }
                } else {
                    let _ = tx.send(None);
                }
            }
            TargetMessage::Authenticate(credentials) => {
                self.network_manager.authenticate(credentials);
            }
            TargetMessage::BlockNetwork(blocked) => {
                self.network_manager.set_block_all(blocked);
            }
            TargetMessage::EnableInterception(enabled) => {
                self.network_manager.user_request_interception_enabled = !enabled;
            }
        }
    }

    /// Advance the target's state machine and drain queued events.
    ///
    /// Like [`poll`](Self::poll) but does **not** read from the page channel
    /// (that is handled externally by `Handler::run()` via `try_recv`).
    pub(crate) fn advance(&mut self, now: Instant) -> Option<TargetEvent> {
        if !self.is_page() {
            return None;
        }

        // Init state machine
        match &mut self.init_state {
            TargetInit::AttachToTarget => {
                self.init_state = TargetInit::InitializingFrame(FrameManager::init_commands(
                    self.config.request_timeout,
                ));
                if let Ok(params) = AttachToTargetParams::builder()
                    .target_id(self.target_id().clone())
                    .flatten(true)
                    .build()
                {
                    return Some(TargetEvent::Request(Request::new(
                        params.identifier(),
                        serde_json::to_value(params).unwrap_or_default(),
                    )));
                } else {
                    return None;
                }
            }
            TargetInit::InitializingFrame(cmds) => {
                self.session_id.as_ref()?;
                if let Poll::Ready(poll) = cmds.poll(now) {
                    return match poll {
                        None => {
                            if let Some(world_name) = self.frame_manager.get_isolated_world_name() {
                                let world_name = world_name.clone();
                                if let Some(isolated_world_cmds) =
                                    self.frame_manager.ensure_isolated_world(&world_name)
                                {
                                    *cmds = isolated_world_cmds;
                                } else {
                                    self.init_state = TargetInit::InitializingNetwork(
                                        self.network_manager.init_commands(),
                                    );
                                }
                            } else {
                                self.init_state = TargetInit::InitializingNetwork(
                                    self.network_manager.init_commands(),
                                );
                            }
                            self.advance(now)
                        }
                        Some(Ok((method, params))) => Some(TargetEvent::Request(Request {
                            method,
                            session_id: self.session_id.clone().map(Into::into),
                            params,
                        })),
                        Some(Err(_)) => Some(self.on_initialization_failed()),
                    };
                } else {
                    return None;
                }
            }
            TargetInit::InitializingNetwork(cmds) => {
                if let Poll::Ready(poll) = cmds.poll(now) {
                    return match poll {
                        None => {
                            self.init_state = TargetInit::InitializingPage(
                                Self::page_init_commands(self.config.request_timeout),
                            );
                            self.advance(now)
                        }
                        Some(Ok((method, params))) => Some(TargetEvent::Request(Request {
                            method,
                            session_id: self.session_id.clone().map(Into::into),
                            params,
                        })),
                        Some(Err(_)) => Some(self.on_initialization_failed()),
                    };
                } else {
                    return None;
                }
            }
            TargetInit::InitializingPage(cmds) => {
                if let Poll::Ready(poll) = cmds.poll(now) {
                    return match poll {
                        None => {
                            self.init_state = match self.config.viewport.as_ref() {
                                Some(viewport) => TargetInit::InitializingEmulation(
                                    self.emulation_manager.init_commands(viewport),
                                ),
                                None => TargetInit::Initialized,
                            };
                            self.advance(now)
                        }
                        Some(Ok((method, params))) => Some(TargetEvent::Request(Request {
                            method,
                            session_id: self.session_id.clone().map(Into::into),
                            params,
                        })),
                        Some(Err(_)) => Some(self.on_initialization_failed()),
                    };
                } else {
                    return None;
                }
            }
            TargetInit::InitializingEmulation(cmds) => {
                if let Poll::Ready(poll) = cmds.poll(now) {
                    return match poll {
                        None => {
                            self.init_state = TargetInit::Initialized;
                            self.advance(now)
                        }
                        Some(Ok((method, params))) => Some(TargetEvent::Request(Request {
                            method,
                            session_id: self.session_id.clone().map(Into::into),
                            params,
                        })),
                        Some(Err(_)) => Some(self.on_initialization_failed()),
                    };
                } else {
                    return None;
                }
            }
            TargetInit::Initialized => {
                if let Some(initiator) = self.initiator.take() {
                    if self
                        .frame_manager
                        .main_frame()
                        .map(|frame| frame.is_loaded())
                        .unwrap_or_default()
                    {
                        if let Some(page) = self.get_or_create_page() {
                            let _ = initiator.send(Ok(page.clone().into()));
                        } else {
                            self.initiator = Some(initiator);
                        }
                    } else {
                        self.initiator = Some(initiator);
                    }
                }
            }
            TargetInit::Closing => return None,
        };

        // Prune dead waiters
        if !self.wait_for_frame_navigation.is_empty() {
            self.wait_for_frame_navigation.retain(|tx| !tx.is_closed());
        }
        if !self.wait_for_dom_content_loaded.is_empty() {
            self.wait_for_dom_content_loaded
                .retain(|tx| !tx.is_closed());
        }
        if !self.wait_for_load.is_empty() {
            self.wait_for_load.retain(|tx| !tx.is_closed());
        }
        if !self.wait_for_network_idle.is_empty() {
            self.wait_for_network_idle.retain(|tx| !tx.is_closed());
        }
        if !self.wait_for_network_almost_idle.is_empty() {
            self.wait_for_network_almost_idle
                .retain(|tx| !tx.is_closed());
        }

        // Drain events loop (same as poll's inner loop, minus page channel reading)
        loop {
            if self.init_state == TargetInit::Closing {
                break None;
            }

            if let Some(frame) = self.frame_manager.main_frame() {
                if frame.is_dom_content_loaded() {
                    while let Some(tx) = self.wait_for_dom_content_loaded.pop() {
                        let _ = tx.send(frame.http_request().cloned());
                    }
                    while let Some(tx) = self.wait_for_frame_navigation.pop() {
                        let _ = tx.send(frame.http_request().cloned());
                    }
                }
                if frame.is_loaded() {
                    while let Some(tx) = self.wait_for_load.pop() {
                        let _ = tx.send(frame.http_request().cloned());
                    }
                }
                if frame.is_network_idle() {
                    while let Some(tx) = self.wait_for_network_idle.pop() {
                        let _ = tx.send(frame.http_request().cloned());
                    }
                }
                if frame.is_network_almost_idle() {
                    while let Some(tx) = self.wait_for_network_almost_idle.pop() {
                        let _ = tx.send(frame.http_request().cloned());
                    }
                }
            }

            if let Some(ev) = self.queued_events.pop_front() {
                return Some(ev);
            }

            while let Some(event) = self.network_manager.poll() {
                if self.init_state == TargetInit::Closing {
                    break;
                }
                match event {
                    NetworkEvent::SendCdpRequest((method, params)) => {
                        self.queued_events.push_back(TargetEvent::Request(Request {
                            method,
                            session_id: self.session_id.clone().map(Into::into),
                            params,
                        }));
                    }
                    NetworkEvent::Request(_) => {}
                    NetworkEvent::Response(_) => {}
                    NetworkEvent::RequestFailed(request) => {
                        self.frame_manager.on_http_request_finished(request);
                    }
                    NetworkEvent::RequestFinished(request) => {
                        self.frame_manager.on_http_request_finished(request);
                    }
                    NetworkEvent::BytesConsumed(n) => {
                        self.queued_events.push_back(TargetEvent::BytesConsumed(n));
                    }
                }
            }

            while let Some(event) = self.frame_manager.poll(now) {
                if self.init_state == TargetInit::Closing {
                    break;
                }
                match event {
                    FrameEvent::NavigationResult(res) => {
                        self.queued_events
                            .push_back(TargetEvent::NavigationResult(res));
                    }
                    FrameEvent::NavigationRequest(id, req) => {
                        self.queued_events
                            .push_back(TargetEvent::NavigationRequest(id, req));
                    }
                }
            }

            if self.queued_events.is_empty() {
                return None;
            }
        }
    }

    /// Set the sender half of the channel who requested the creation of this
    /// target
    pub fn set_initiator(&mut self, tx: Sender<Result<Page>>) {
        self.initiator = Some(tx);
    }

    #[cfg(feature = "parallel-handler")]
    pub(crate) fn cancel_initiator(&mut self) {
        if let Some(tx) = self.initiator.take() {
            let _ = tx.send(Err(CdpError::msg(
                "session stopped before page initialization",
            )));
        }
    }

    pub(crate) fn page_init_commands(timeout: Duration) -> CommandChain {
        CommandChain::new(INIT_COMMANDS_PARAMS.clone(), timeout)
    }
}

/// Configuration for how a single target/page should be fetched and processed.
#[derive(Debug, Clone)]
pub struct TargetConfig {
    /// Whether to ignore TLS/HTTPS certificate errors (e.g. self-signed or expired certs).
    /// When `true`, connections will proceed even if certificate validation fails.
    pub ignore_https_errors: bool,
    /// Request timeout to use for the main navigation / resource fetch.
    /// This is the total time allowed before a request is considered failed.
    pub request_timeout: Duration,
    /// Optional browser viewport to use for this target.
    /// When `None`, the default viewport (or headless browser default) is used.
    pub viewport: Option<Viewport>,
    /// Enable request interception for this target.
    /// When `true`, all network requests will pass through the intercept manager.
    pub request_intercept: bool,
    /// Enable caching for this target.
    /// When `true`, responses may be read from and written to the cache layer.
    pub cache_enabled: bool,
    /// If `true`, skip visual/asset resources that are not required for HTML content
    /// (e.g. images, fonts, media). Useful for performance-oriented crawls.
    pub ignore_visuals: bool,
    /// If `true`, block JavaScript execution (or avoid loading JS resources)
    /// for this target. This is useful for purely static HTML crawls.
    pub ignore_javascript: bool,
    /// If `true`, block analytics / tracking requests (e.g. Google Analytics,
    /// common tracker domains, etc.).
    pub ignore_analytics: bool,
    /// Ignore prefetching.
    pub ignore_prefetch: bool,
    /// If `true`, block stylesheets and related CSS resources for this target.
    /// This can reduce bandwidth when only raw HTML is needed.
    pub ignore_stylesheets: bool,
    /// When `ignore_stylesheets` would skip a stylesheet, allow it through if
    /// the request URL is first-party (registrable domain matches the page's
    /// primary frame). Default `true`. Set `false` for strict block-all.
    pub allow_first_party_stylesheets: bool,
    /// When a downstream blocker would skip a script, allow it through if
    /// first-party. Default `true`.
    pub allow_first_party_javascript: bool,
    /// When `ignore_visuals` would skip an image/media/font, allow it through
    /// if first-party. Default `true`.
    pub allow_first_party_visuals: bool,
    /// If `true`, only HTML documents will be fetched/kept.
    /// Non-HTML subresources may be skipped entirely.
    pub only_html: bool,
    /// Whether service workers are allowed for this target.
    /// When `true`, service workers may register and intercept requests.
    pub service_worker_enabled: bool,
    /// Extra HTTP headers to send with each request for this target.
    /// Keys should be header names, values their corresponding header values.
    pub extra_headers: Option<std::collections::HashMap<String, String>>,
    /// Network intercept manager used to make allow/deny/modify decisions
    /// for requests when `request_intercept` is enabled.
    pub intercept_manager: NetworkInterceptManager,
    /// Push the interception policy to a capable remote engine once per
    /// navigation (`Interception.setPolicy`) so it resolves block/allow
    /// locally instead of round-tripping each `Fetch.requestPaused`. Default
    /// `false`; safe against any target.
    pub remote_local_policy: bool,
    /// The maximum number of response bytes allowed for this target.
    /// When set, responses larger than this limit may be truncated or aborted.
    pub max_bytes_allowed: Option<u64>,
    /// Cap on Document-type redirect hops before the navigation is aborted.
    /// `None` disables enforcement; `Some(n)` mirrors `reqwest::redirect::Policy::limited(n)`.
    pub max_redirects: Option<usize>,
    /// Cap on main-frame cross-document navigations per `goto`. Defends against
    /// JS / meta-refresh loops that bypass the HTTP redirect guard. `None`
    /// disables the guard.
    pub max_main_frame_navigations: Option<u32>,
    /// Whitelist patterns to allow through the network.
    pub whitelist_patterns: Option<Vec<String>>,
    /// Blacklist patterns to black through the network.
    pub blacklist_patterns: Option<Vec<String>>,
    /// Extra ABP/uBO filter rules for the adblock engine.
    #[cfg(feature = "adblock")]
    pub adblock_filter_rules: Option<Vec<String>>,
    /// Optional notify handle for waking `Handler::run()`'s select loop.
    /// `None` when using the `impl Stream for Handler` path (no overhead).
    pub page_wake: Option<Arc<Notify>>,
    /// Capacity of the per-page mpsc channel carrying `TargetMessage`s
    /// from the page handle to the handler. Defaults to
    /// `crate::handler::page::DEFAULT_PAGE_CHANNEL_CAPACITY` (2048);
    /// override via `HandlerConfig::page_channel_capacity`. Clamped to
    /// a minimum of 1 at channel creation time.
    pub page_channel_capacity: usize,
}

impl Default for TargetConfig {
    fn default() -> Self {
        Self {
            ignore_https_errors: true,
            request_timeout: Duration::from_millis(REQUEST_TIMEOUT),
            viewport: Default::default(),
            request_intercept: false,
            cache_enabled: true,
            service_worker_enabled: true,
            ignore_javascript: false,
            ignore_visuals: false,
            ignore_stylesheets: false,
            allow_first_party_stylesheets: true,
            allow_first_party_javascript: true,
            allow_first_party_visuals: true,
            ignore_analytics: true,
            ignore_prefetch: true,
            only_html: false,
            extra_headers: Default::default(),
            intercept_manager: NetworkInterceptManager::Unknown,
            remote_local_policy: false,
            max_bytes_allowed: None,
            max_redirects: None,
            max_main_frame_navigations: None,
            whitelist_patterns: None,
            blacklist_patterns: None,
            #[cfg(feature = "adblock")]
            adblock_filter_rules: None,
            page_wake: None,
            page_channel_capacity: crate::handler::page::DEFAULT_PAGE_CHANNEL_CAPACITY,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TargetType {
    Page,
    BackgroundPage,
    ServiceWorker,
    SharedWorker,
    Other,
    Browser,
    Webview,
    Unknown(String),
}

impl TargetType {
    pub fn new(ty: &str) -> Self {
        match ty {
            "page" => TargetType::Page,
            "background_page" => TargetType::BackgroundPage,
            "service_worker" => TargetType::ServiceWorker,
            "shared_worker" => TargetType::SharedWorker,
            "other" => TargetType::Other,
            "browser" => TargetType::Browser,
            "webview" => TargetType::Webview,
            s => TargetType::Unknown(s.to_string()),
        }
    }

    pub fn is_page(&self) -> bool {
        matches!(self, TargetType::Page)
    }

    pub fn is_background_page(&self) -> bool {
        matches!(self, TargetType::BackgroundPage)
    }

    pub fn is_service_worker(&self) -> bool {
        matches!(self, TargetType::ServiceWorker)
    }

    pub fn is_shared_worker(&self) -> bool {
        matches!(self, TargetType::SharedWorker)
    }

    pub fn is_other(&self) -> bool {
        matches!(self, TargetType::Other)
    }

    pub fn is_browser(&self) -> bool {
        matches!(self, TargetType::Browser)
    }

    pub fn is_webview(&self) -> bool {
        matches!(self, TargetType::Webview)
    }
}

#[derive(Debug)]
pub(crate) enum TargetEvent {
    /// An internal request
    Request(Request),
    /// An internal navigation request
    NavigationRequest(NavigationId, Request),
    /// Indicates that a previous requested navigation has finished
    NavigationResult(Result<NavigationOk, NavigationError>),
    /// A new command arrived via a channel
    Command(CommandMessage),
    /// The bytes consumed by the network.
    BytesConsumed(u64),
}

// TODO this can be moved into the classes?
#[derive(Debug, PartialEq)]
pub enum TargetInit {
    InitializingFrame(CommandChain),
    InitializingNetwork(CommandChain),
    InitializingPage(CommandChain),
    InitializingEmulation(CommandChain),
    AttachToTarget,
    Initialized,
    Closing,
}

impl TargetInit {
    fn commands_mut(&mut self) -> Option<&mut CommandChain> {
        match self {
            TargetInit::InitializingFrame(cmd) => Some(cmd),
            TargetInit::InitializingNetwork(cmd) => Some(cmd),
            TargetInit::InitializingPage(cmd) => Some(cmd),
            TargetInit::InitializingEmulation(cmd) => Some(cmd),
            TargetInit::AttachToTarget => None,
            TargetInit::Initialized => None,
            TargetInit::Closing => None,
        }
    }
}

#[derive(Debug)]
pub struct GetExecutionContext {
    /// For which world the execution context was requested
    pub dom_world: DOMWorldKind,
    /// The if of the frame to get the `ExecutionContext` for
    pub frame_id: Option<FrameId>,
    /// Sender half of the channel to send the response back
    pub tx: Sender<Option<ExecutionContextId>>,
}

impl GetExecutionContext {
    pub fn new(tx: Sender<Option<ExecutionContextId>>) -> Self {
        Self {
            dom_world: DOMWorldKind::Main,
            frame_id: None,
            tx,
        }
    }
}

#[derive(Debug)]
pub struct GetUrl {
    /// The id of the frame to get the url for (None = main frame)
    pub frame_id: Option<FrameId>,
    /// Sender half of the channel to send the response back
    pub tx: Sender<Option<String>>,
}

impl GetUrl {
    pub fn new(tx: Sender<Option<String>>) -> Self {
        Self { frame_id: None, tx }
    }
}

#[derive(Debug)]
pub struct GetName {
    /// The id of the frame to get the name for (None = main frame)
    pub frame_id: Option<FrameId>,
    /// Sender half of the channel to send the response back
    pub tx: Sender<Option<String>>,
}

#[derive(Debug)]
pub struct GetParent {
    /// The id of the frame to get the parent for (None = main frame)
    pub frame_id: FrameId,
    /// Sender half of the channel to send the response back
    pub tx: Sender<Option<FrameId>>,
}

#[derive(Debug)]
pub enum TargetMessage {
    /// Execute a command within the session of this target
    Command(CommandMessage),
    /// Return the main frame of this target's page
    MainFrame(Sender<Option<FrameId>>),
    /// Return all the frames of this target's page
    AllFrames(Sender<Vec<FrameId>>),
    #[cfg(feature = "_cache")]
    /// Set the cache key and policy for the target page.
    CacheKey((Option<String>, Option<crate::cache::BasicCachePolicy>)),
    /// Return the url if available
    Url(GetUrl),
    /// Return the name if available
    Name(GetName),
    /// Return the parent id of a frame
    Parent(GetParent),
    /// A Message that resolves when the frame finished loading a new url
    WaitForNavigation(Sender<ArcHttpRequest>),
    /// Resolves when `DOMContentLoaded` fires (HTML parsed, sync scripts
    /// executed) — before `load`, so subresources may still be in-flight.
    WaitForDomContentLoaded(Sender<ArcHttpRequest>),
    /// Resolves when the `load` event fires — all subresources (images,
    /// fonts, XHRs) are done. Slower than `WaitForNavigation` through proxies.
    WaitForLoad(Sender<ArcHttpRequest>),
    /// A Message that resolves when the frame network is idle
    WaitForNetworkIdle(Sender<ArcHttpRequest>),
    /// A Message that resolves when the frame network is almost idle
    WaitForNetworkAlmostIdle(Sender<ArcHttpRequest>),
    /// A request to submit a new listener that gets notified with every
    /// received event
    AddEventListener(EventListenerRequest),
    /// Get the `ExecutionContext` if available
    GetExecutionContext(GetExecutionContext),
    Authenticate(Credentials),
    /// Set block/unblocked networking
    BlockNetwork(bool),
    /// Enable/Disable internal request paused interception
    EnableInterception(bool),
}

#[cfg(test)]
mod waiter_drain_tests {
    //! Unit tests for `drain_waiters_bounded`.
    //!
    //! These cover the isolated drain helper — they do not spin up a real
    //! `Target` or browser, so they run in microseconds and exhaustively
    //! exercise the budget / re-arm contract:
    //!
    //! - drain with no waiters is a no-op and reports `remaining = false`
    //! - drain with fewer waiters than budget fires all and reports `false`
    //! - drain with exactly `budget` waiters fires all and reports `false`
    //! - drain with more waiters than `budget` fires `budget` and reports `true`
    //! - senders whose receivers were dropped don't panic or consume extra work
    //! - repeated draining eventually empties any queue (no deadlock)
    //!
    //! The last test is the key "no deadlock" property: if re-arm were broken
    //! (say, we forgot to wake), the handler could stall with waiters pending
    //! forever. Here we prove the helper itself always makes forward progress.
    use super::{drain_waiters_bounded, WAITER_DRAIN_BUDGET};
    use crate::ArcHttpRequest;
    use tokio::sync::oneshot::{self, Sender};

    fn make_waiters(
        n: usize,
    ) -> (
        Vec<Sender<ArcHttpRequest>>,
        Vec<oneshot::Receiver<ArcHttpRequest>>,
    ) {
        let mut txs = Vec::with_capacity(n);
        let mut rxs = Vec::with_capacity(n);
        for _ in 0..n {
            let (tx, rx) = oneshot::channel();
            txs.push(tx);
            rxs.push(rx);
        }
        (txs, rxs)
    }

    #[test]
    fn empty_queue_is_noop() {
        let mut queue: Vec<Sender<ArcHttpRequest>> = Vec::new();
        let remaining = drain_waiters_bounded(&mut queue, None, WAITER_DRAIN_BUDGET);
        assert!(!remaining, "empty queue should not mark 'remaining'");
        assert!(queue.is_empty());
    }

    #[test]
    fn drains_fewer_than_budget() {
        let (mut queue, mut rxs) = make_waiters(10);
        let remaining = drain_waiters_bounded(&mut queue, None, WAITER_DRAIN_BUDGET);
        assert!(!remaining);
        assert!(queue.is_empty());
        // All receivers got a value.
        for rx in rxs.iter_mut() {
            assert!(rx.try_recv().is_ok(), "every waiter must receive a value");
        }
    }

    #[test]
    fn drains_exactly_budget() {
        let (mut queue, mut rxs) = make_waiters(WAITER_DRAIN_BUDGET);
        let remaining = drain_waiters_bounded(&mut queue, None, WAITER_DRAIN_BUDGET);
        assert!(!remaining, "exactly-budget drain should empty the queue");
        assert!(queue.is_empty());
        for rx in rxs.iter_mut() {
            assert!(rx.try_recv().is_ok());
        }
    }

    #[test]
    fn drains_budget_when_over_capacity() {
        let n = WAITER_DRAIN_BUDGET * 3 + 7; // 199 waiters at the default 64
        let (mut queue, _rxs) = make_waiters(n);
        let remaining = drain_waiters_bounded(&mut queue, None, WAITER_DRAIN_BUDGET);
        assert!(remaining, "over-budget drain must mark 'remaining = true'");
        assert_eq!(
            queue.len(),
            n - WAITER_DRAIN_BUDGET,
            "exactly `budget` waiters should be popped per call"
        );
    }

    #[test]
    fn dropped_receiver_does_not_panic() {
        let (mut queue, mut rxs) = make_waiters(4);
        // Drop half the receivers — their senders become closed.
        rxs.truncate(2);
        let remaining = drain_waiters_bounded(&mut queue, None, WAITER_DRAIN_BUDGET);
        assert!(!remaining);
        assert!(queue.is_empty());
        // The remaining receivers either got a value or were the popped ones;
        // at minimum, no panic occurred.
    }

    #[test]
    fn repeated_draining_empties_any_queue() {
        // "No deadlock" property: repeatedly calling the helper always makes
        // forward progress and eventually empties the queue. If this loop
        // ever ran forever, the re-arm contract would be unreachable.
        let n = 10_000;
        let (mut queue, _rxs) = make_waiters(n);
        let mut rounds = 0;
        loop {
            let remaining = drain_waiters_bounded(&mut queue, None, WAITER_DRAIN_BUDGET);
            rounds += 1;
            if !remaining {
                break;
            }
            assert!(rounds < n, "drain must make forward progress on every call");
        }
        assert!(queue.is_empty());
        // 10_000 / 64 = 156.25 → 157 full rounds + final clean-up = 157
        assert_eq!(
            rounds,
            n.div_ceil(WAITER_DRAIN_BUDGET),
            "each round should pop exactly `budget` waiters until the tail"
        );
    }
}
