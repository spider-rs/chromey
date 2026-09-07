use crate::listeners::{EventListenerRequest, EventListeners};
use chromiumoxide_cdp::cdp::browser_protocol::browser::*;
use chromiumoxide_cdp::cdp::browser_protocol::target::*;
use chromiumoxide_cdp::cdp::events::CdpEvent;
use chromiumoxide_cdp::cdp::events::CdpEventMessage;
use chromiumoxide_types::{CallId, Message, Method, Response};
use chromiumoxide_types::{MethodId, Request as CdpRequest};
use fnv::FnvHashMap;
use futures_util::Stream;
use hashbrown::{HashMap, HashSet};
use spider_network_blocker::intercept_manager::NetworkInterceptManager;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot::Sender as OneshotSender;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::Error;

use std::sync::Arc;
use tokio::sync::Notify;

use crate::cmd::{to_command_response, CommandMessage};
use crate::conn::Connection;
use crate::error::{CdpError, Result};
use crate::handler::browser::BrowserContext;
use crate::handler::frame::FrameRequestedNavigation;
use crate::handler::frame::{NavigationError, NavigationId, NavigationOk};
use crate::handler::httpfuture::navigation_continues;
use crate::handler::job::PeriodicJob;
use crate::handler::session::Session;
use crate::handler::target::TargetEvent;
use crate::handler::target::{Target, TargetConfig};
use crate::handler::viewport::Viewport;
use crate::page::Page;
pub(crate) use page::PageInner;

/// Standard timeout in MS
pub const REQUEST_TIMEOUT: u64 = 30_000;

pub mod blockers;
pub mod browser;
pub mod commandfuture;
pub mod domworld;
pub mod emulation;
pub mod frame;
pub mod http;
pub mod httpfuture;
mod job;
pub mod network;
pub mod network_utils;
pub mod page;
#[cfg(feature = "parallel-handler")]
pub mod parallel;
pub mod sender;
mod session;
pub mod target;
pub mod target_message_future;
pub mod viewport;

/// The handler that monitors the state of the chromium browser and drives all
/// the requests and events.
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct Handler {
    pub default_browser_context: BrowserContext,
    pub browser_contexts: HashSet<BrowserContext>,
    /// Commands that are being processed and awaiting a response from the
    /// chromium instance together with the timestamp when the request
    /// started.
    pending_commands: FnvHashMap<CallId, (PendingRequest, MethodId, Instant)>,
    /// Connection to the browser instance
    from_browser: Receiver<HandlerMessage>,
    /// Used to loop over all targets in a consistent manner
    target_ids: Vec<TargetId>,
    /// The created and attached targets
    targets: HashMap<TargetId, Target>,
    /// Currently queued in navigations for targets
    navigations: FnvHashMap<NavigationId, NavigationRequest>,
    /// Keeps track of all the current active sessions
    ///
    /// There can be multiple sessions per target.
    sessions: HashMap<SessionId, Session>,
    /// The websocket connection to the chromium instance.
    /// `Option` so that `run()` can `.take()` it for splitting.
    conn: Option<Connection<CdpEventMessage>>,
    /// Evicts timed out requests periodically
    evict_command_timeout: PeriodicJob,
    /// The internal identifier for a specific navigation
    next_navigation_id: usize,
    /// How this handler will configure targets etc,
    config: HandlerConfig,
    /// All registered event subscriptions
    event_listeners: EventListeners,
    /// Keeps track is the browser is closing
    closing: bool,
    /// Track the bytes remainder until network request will be blocked.
    remaining_bytes: Option<u64>,
    /// The budget is exhausted.
    budget_exhausted: bool,
    /// Tracks which targets we've already attached to, to avoid multiple sessions per target.
    attached_targets: HashSet<TargetId>,
    /// Optional notify for waking `Handler::run()`'s `tokio::select!` loop
    /// when a page sends a message.  `None` when using the `Stream` API.
    page_wake: Option<Arc<Notify>>,
}

lazy_static::lazy_static! {
    /// Set the discovery ID target.
    static ref DISCOVER_ID: (std::borrow::Cow<'static, str>, serde_json::Value) = {
        let discover = SetDiscoverTargetsParams::new(true);
        (discover.identifier(), serde_json::to_value(discover).expect("valid discover target params"))
    };
    /// Targets params id.
    static ref TARGET_PARAMS_ID: (std::borrow::Cow<'static, str>, serde_json::Value) = {
        let msg = GetTargetsParams { filter: None };
        (msg.identifier(), serde_json::to_value(msg).expect("valid paramtarget"))
    };
    /// Set the close targets.
    static ref CLOSE_PARAMS_ID: (std::borrow::Cow<'static, str>, serde_json::Value) = {
        let close_msg = CloseParams::default();
        (close_msg.identifier(), serde_json::to_value(close_msg).expect("valid close params"))
    };
}

fn maybe_store_attach_session_id(target: &mut Target, method: &MethodId, resp: &Response) {
    if method.as_ref() != AttachToTargetParams::IDENTIFIER {
        return;
    }

    if let Ok(resp) = to_command_response::<AttachToTargetParams>(resp.clone(), method.clone()) {
        target.set_session_id(resp.result.session_id);
    }
}

impl Handler {
    /// Create a new `Handler` that drives the connection and listens for
    /// messages on the receiver `rx`.
    pub(crate) fn new(
        mut conn: Connection<CdpEventMessage>,
        rx: Receiver<HandlerMessage>,
        config: HandlerConfig,
    ) -> Self {
        let discover = DISCOVER_ID.clone();
        let _ = conn.submit_command(discover.0, None, discover.1);
        let conn = Some(conn);

        let browser_contexts = config
            .context_ids
            .iter()
            .map(|id| BrowserContext::from(id.clone()))
            .collect();

        Self {
            pending_commands: Default::default(),
            from_browser: rx,
            default_browser_context: Default::default(),
            browser_contexts,
            target_ids: Default::default(),
            targets: Default::default(),
            navigations: Default::default(),
            sessions: Default::default(),
            conn,
            evict_command_timeout: PeriodicJob::new(config.request_timeout),
            next_navigation_id: 0,
            config,
            event_listeners: Default::default(),
            closing: false,
            remaining_bytes: None,
            budget_exhausted: false,
            attached_targets: Default::default(),
            page_wake: None,
        }
    }

    /// Borrow the WebSocket connection, returning an error if it has been
    /// consumed by [`Handler::run()`].
    #[inline]
    fn conn(&mut self) -> Result<&mut Connection<CdpEventMessage>> {
        self.conn
            .as_mut()
            .ok_or_else(|| CdpError::msg("connection consumed by Handler::run()"))
    }

    /// Return the target with the matching `target_id`
    pub fn get_target(&self, target_id: &TargetId) -> Option<&Target> {
        self.targets.get(target_id)
    }

    /// Iterator over all currently attached targets
    pub fn targets(&self) -> impl Iterator<Item = &Target> + '_ {
        self.targets.values()
    }

    /// The default Browser context
    pub fn default_browser_context(&self) -> &BrowserContext {
        &self.default_browser_context
    }

    /// Iterator over all currently available browser contexts
    pub fn browser_contexts(&self) -> impl Iterator<Item = &BrowserContext> + '_ {
        self.browser_contexts.iter()
    }

    /// received a response to a navigation request like `Page.navigate`
    fn on_navigation_response(&mut self, id: NavigationId, resp: Response) {
        if let Some(nav) = self.navigations.remove(&id) {
            match nav {
                NavigationRequest::Navigate(target_id, mut nav) => {
                    // Read the field directly rather than decoding NavigateReturns:
                    // cheaper, and a malformed sibling field cannot make the whole
                    // decode fail. A silent decode failure here would park the ack
                    // again, which is the hang this branch exists to remove.
                    // The empty check is load-bearing and duplicated in HttpFuture's
                    // probe: both paths must treat an empty errorText as absent.
                    let failed = resp
                        .result
                        .as_ref()
                        .and_then(|result| result.get("errorText"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|err| !err.is_empty() && !navigation_continues(err));
                    if failed {
                        // Terminal failures may never emit lifecycle events. Release
                        // the ack and remove the watcher before another goto is queued.
                        if let Some(target) = self.targets.get_mut(&target_id) {
                            target.frame_manager_mut().abandon_navigation(id);
                        }
                        let _ = nav.tx.send(Ok(resp));
                    } else if nav.navigated {
                        let _ = nav.tx.send(Ok(resp));
                    } else {
                        nav.set_response(resp);
                        self.navigations
                            .insert(id, NavigationRequest::Navigate(target_id, nav));
                    }
                }
            }
        }
    }

    /// A navigation has finished.
    fn on_navigation_lifecycle_completed(&mut self, res: Result<NavigationOk, NavigationError>) {
        match res {
            Ok(ok) => {
                let id = *ok.navigation_id();
                if let Some(nav) = self.navigations.remove(&id) {
                    match nav {
                        NavigationRequest::Navigate(target_id, mut nav) => {
                            if let Some(resp) = nav.response.take() {
                                let _ = nav.tx.send(Ok(resp));
                            } else {
                                nav.set_navigated();
                                self.navigations
                                    .insert(id, NavigationRequest::Navigate(target_id, nav));
                            }
                        }
                    }
                }
            }
            Err(err) => {
                if let Some(nav) = self.navigations.remove(err.navigation_id()) {
                    match nav {
                        NavigationRequest::Navigate(_, nav) => {
                            let _ = nav.tx.send(Err(err.into()));
                        }
                    }
                }
            }
        }
    }

    /// Received a response to a request.
    fn on_response(&mut self, resp: Response) {
        if let Some((req, method, _)) = self.pending_commands.remove(&resp.id) {
            match req {
                PendingRequest::CreateTarget(tx) => {
                    match to_command_response::<CreateTargetParams>(resp, method) {
                        Ok(resp) => {
                            if let Some(target) = self.targets.get_mut(&resp.target_id) {
                                target.set_initiator(tx);
                            } else {
                                let _ = tx.send(Err(CdpError::NotFound)).ok();
                            }
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err)).ok();
                        }
                    }
                }
                PendingRequest::GetTargets(tx) => {
                    match to_command_response::<GetTargetsParams>(resp, method) {
                        Ok(resp) => {
                            let targets = resp.result.target_infos;
                            let results = targets.clone();

                            for target_info in targets {
                                let event: EventTargetCreated = EventTargetCreated { target_info };
                                self.on_target_created(event);
                            }

                            let _ = tx.send(Ok(results)).ok();
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err)).ok();
                        }
                    }
                }
                PendingRequest::Navigate(id) => {
                    self.on_navigation_response(id, resp);
                    if self.config.only_html && !self.config.created_first_target {
                        self.config.created_first_target = true;
                    }
                }
                PendingRequest::ExternalCommand { tx, .. } => {
                    let _ = tx.send(Ok(resp)).ok();
                }
                PendingRequest::InternalCommand(target_id) => {
                    if let Some(target) = self.targets.get_mut(&target_id) {
                        maybe_store_attach_session_id(target, &method, &resp);
                        target.on_response(resp, method.as_ref());
                    }
                }
                PendingRequest::CloseBrowser(tx) => {
                    self.closing = true;
                    let _ = tx.send(Ok(CloseReturns {})).ok();
                }
            }
        }
    }

    /// Submit a command initiated via channel
    pub(crate) fn submit_external_command(
        &mut self,
        msg: CommandMessage,
        now: Instant,
    ) -> Result<()> {
        // Resolve session_id → target_id before `submit_command`
        // consumes `msg.session_id`. `None` when the session hasn't
        // landed in `self.sessions` yet; that command then relies on
        // the normal request_timeout path if the target later crashes.
        let target_id = msg
            .session_id
            .as_ref()
            .and_then(|sid| self.sessions.get(sid.as_ref()))
            .map(|s| s.target_id().clone());
        let call_id =
            self.conn()?
                .submit_command(msg.method.clone(), msg.session_id, msg.params)?;
        self.pending_commands.insert(
            call_id,
            (
                PendingRequest::ExternalCommand {
                    tx: msg.sender,
                    target_id,
                },
                msg.method,
                now,
            ),
        );
        Ok(())
    }

    pub(crate) fn submit_internal_command(
        &mut self,
        target_id: TargetId,
        req: CdpRequest,
        now: Instant,
    ) -> Result<()> {
        let call_id = self.conn()?.submit_command(
            req.method.clone(),
            req.session_id.map(Into::into),
            req.params,
        )?;
        self.pending_commands.insert(
            call_id,
            (PendingRequest::InternalCommand(target_id), req.method, now),
        );
        Ok(())
    }

    fn submit_fetch_targets(&mut self, tx: OneshotSender<Result<Vec<TargetInfo>>>, now: Instant) {
        let msg = TARGET_PARAMS_ID.clone();

        if let Some(conn) = self.conn.as_mut() {
            if let Ok(call_id) = conn.submit_command(msg.0.clone(), None, msg.1) {
                self.pending_commands
                    .insert(call_id, (PendingRequest::GetTargets(tx), msg.0, now));
            }
        }
    }

    /// Send the Request over to the server and store its identifier to handle
    /// the response once received.
    fn submit_navigation(&mut self, id: NavigationId, req: CdpRequest, now: Instant) {
        if let Some(conn) = self.conn.as_mut() {
            if let Ok(call_id) = conn.submit_command(
                req.method.clone(),
                req.session_id.map(Into::into),
                req.params,
            ) {
                self.pending_commands
                    .insert(call_id, (PendingRequest::Navigate(id), req.method, now));
            }
        }
    }

    fn submit_close(&mut self, tx: OneshotSender<Result<CloseReturns>>, now: Instant) {
        let close_msg = CLOSE_PARAMS_ID.clone();

        if let Some(conn) = self.conn.as_mut() {
            if let Ok(call_id) = conn.submit_command(close_msg.0.clone(), None, close_msg.1) {
                self.pending_commands.insert(
                    call_id,
                    (PendingRequest::CloseBrowser(tx), close_msg.0, now),
                );
            }
        }
    }

    /// Process a message received by the target's page via channel
    fn on_target_message(&mut self, target: &mut Target, msg: CommandMessage, now: Instant) {
        if msg.is_navigation() {
            let (req, tx) = msg.split();
            let id = self.next_navigation_id();

            target.goto(FrameRequestedNavigation::new(
                id,
                req,
                self.config.request_timeout,
            ));

            self.navigations.insert(
                id,
                NavigationRequest::Navigate(
                    target.target_id().clone(),
                    NavigationInProgress::new(tx),
                ),
            );
        } else {
            let _ = self.submit_external_command(msg, now);
        }
    }

    /// An identifier for queued `NavigationRequest`s.
    fn next_navigation_id(&mut self) -> NavigationId {
        let id = NavigationId(self.next_navigation_id);
        self.next_navigation_id = self.next_navigation_id.wrapping_add(1);
        id
    }

    /// Create a new page and send it to the receiver when ready
    ///
    /// First a `CreateTargetParams` is send to the server, this will trigger
    /// `EventTargetCreated` which results in a new `Target` being created.
    /// Once the response to the request is received the initialization process
    /// of the target kicks in. This triggers a queue of initialization requests
    /// of the `Target`, once those are all processed and the `url` fo the
    /// `CreateTargetParams` has finished loading (The `Target`'s `Page` is
    /// ready and idle), the `Target` sends its newly created `Page` as response
    /// to the initiator (`tx`) of the `CreateTargetParams` request.
    fn create_page(&mut self, params: CreateTargetParams, tx: OneshotSender<Result<Page>>) {
        let about_blank = params.url == "about:blank";
        let http_check =
            !about_blank && params.url.starts_with("http") || params.url.starts_with("file://");

        if about_blank || http_check {
            let method = params.identifier();

            let Some(conn) = self.conn.as_mut() else {
                let _ = tx.send(Err(CdpError::msg("connection consumed"))).ok();
                return;
            };
            match serde_json::to_value(params) {
                Ok(params) => match conn.submit_command(method.clone(), None, params) {
                    Ok(call_id) => {
                        self.pending_commands.insert(
                            call_id,
                            (PendingRequest::CreateTarget(tx), method, Instant::now()),
                        );
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err.into())).ok();
                    }
                },
                Err(err) => {
                    let _ = tx.send(Err(err.into())).ok();
                }
            }
        } else {
            let _ = tx.send(Err(CdpError::NotFound)).ok();
        }
    }

    /// Process an incoming event read from the websocket
    fn on_event(&mut self, event: CdpEventMessage) {
        if let Some(session_id) = &event.session_id {
            if let Some(session) = self.sessions.get(session_id.as_str()) {
                if let Some(target) = self.targets.get_mut(session.target_id()) {
                    return target.on_event(event);
                }
            }
        }
        let CdpEventMessage { params, method, .. } = event;

        match params {
            CdpEvent::TargetTargetCreated(ref ev) => self.on_target_created((**ev).clone()),
            CdpEvent::TargetAttachedToTarget(ref ev) => self.on_attached_to_target(ev.clone()),
            CdpEvent::TargetTargetDestroyed(ref ev) => self.on_target_destroyed(ev.clone()),
            CdpEvent::TargetTargetCrashed(ref ev) => self.on_target_crashed(ev.clone()),
            CdpEvent::TargetDetachedFromTarget(ref ev) => self.on_detached_from_target(ev.clone()),
            _ => {}
        }

        chromiumoxide_cdp::consume_event!(match params {
            |ev| self.event_listeners.start_send(ev),
            |json| { let _ = self.event_listeners.try_send_custom(&method, json);}
        });
    }

    /// Fired when a new target was created on the chromium instance
    ///
    /// Creates a new `Target` instance and keeps track of it
    fn on_target_created(&mut self, event: EventTargetCreated) {
        if !self.browser_contexts.is_empty() {
            if let Some(ref context_id) = event.target_info.browser_context_id {
                let bc = BrowserContext {
                    id: Some(context_id.clone()),
                };
                if !self.browser_contexts.contains(&bc) {
                    return;
                }
            }
        }
        let browser_ctx = event
            .target_info
            .browser_context_id
            .clone()
            .map(BrowserContext::from)
            .unwrap_or_else(|| self.default_browser_context.clone());
        let target = Target::new(
            event.target_info,
            TargetConfig {
                ignore_https_errors: self.config.ignore_https_errors,
                request_timeout: self.config.request_timeout,
                viewport: self.config.viewport.clone(),
                request_intercept: self.config.request_intercept,
                cache_enabled: self.config.cache_enabled,
                service_worker_enabled: self.config.service_worker_enabled,
                ignore_visuals: self.config.ignore_visuals,
                ignore_stylesheets: self.config.ignore_stylesheets,
                ignore_javascript: self.config.ignore_javascript,
                ignore_analytics: self.config.ignore_analytics,
                ignore_prefetch: self.config.ignore_prefetch,
                allow_first_party_stylesheets: self.config.allow_first_party_stylesheets,
                allow_first_party_javascript: self.config.allow_first_party_javascript,
                allow_first_party_visuals: self.config.allow_first_party_visuals,
                extra_headers: self.config.extra_headers.clone(),
                only_html: self.config.only_html && self.config.created_first_target,
                intercept_manager: self.config.intercept_manager,
                remote_local_policy: self.config.remote_local_policy,
                max_bytes_allowed: self.config.max_bytes_allowed,
                max_redirects: self.config.max_redirects,
                max_main_frame_navigations: self.config.max_main_frame_navigations,
                whitelist_patterns: self.config.whitelist_patterns.clone(),
                blacklist_patterns: self.config.blacklist_patterns.clone(),
                #[cfg(feature = "adblock")]
                adblock_filter_rules: self.config.adblock_filter_rules.clone(),
                page_wake: self.page_wake.clone(),
                page_channel_capacity: self.config.page_channel_capacity,
            },
            browser_ctx,
        );

        let tid = target.target_id().clone();
        self.target_ids.push(tid.clone());
        self.targets.insert(tid, target);
    }

    /// A new session is attached to a target
    fn on_attached_to_target(&mut self, event: Box<EventAttachedToTarget>) {
        let session = Session::new(event.session_id.clone(), event.target_info.target_id);
        if let Some(target) = self.targets.get_mut(session.target_id()) {
            target.set_session_id(session.session_id().clone())
        }
        self.sessions.insert(event.session_id, session);
    }

    /// The session was detached from target.
    /// Can be issued multiple times per target if multiple session have been
    /// attached to it.
    fn on_detached_from_target(&mut self, event: EventDetachedFromTarget) {
        // remove the session
        if let Some(session) = self.sessions.remove(&event.session_id) {
            if let Some(target) = self.targets.get_mut(session.target_id()) {
                target.session_id_mut().take();
            }
        }
    }

    /// Fired when the target was destroyed in the browser
    fn on_target_destroyed(&mut self, event: EventTargetDestroyed) {
        self.attached_targets.remove(&event.target_id);

        if let Some(target) = self.targets.remove(&event.target_id) {
            // TODO shutdown?
            if let Some(session) = target.session_id() {
                self.sessions.remove(session);
            }
        }
    }

    /// Fired when a target has crashed (`Target.targetCrashed`).
    ///
    /// Unlike `targetDestroyed` (clean teardown), a crash means any
    /// in-flight commands on that target will never receive a
    /// response. Without explicit cancellation those commands sit in
    /// `pending_commands` until the `request_timeout` evicts them,
    /// which surfaces to callers as long latency tails on what is
    /// really an immediate failure.
    ///
    /// Cancellation policy:
    /// * `ExternalCommand { target_id: Some(crashed), .. }` — the
    ///   caller's oneshot resolves with an error carrying the
    ///   termination `status` + `errorCode` from the crash event.
    /// * `InternalCommand(crashed)` — dropped silently; these are
    ///   target-init commands whose caller is the target itself,
    ///   which we're about to remove.
    /// * `ExternalCommand { target_id: None, .. }` — left alone;
    ///   browser-level or pre-attach-race commands aren't bound to
    ///   this target.
    /// * `Navigate(_)` and entries in `self.navigations` — left to
    ///   the normal timeout path; `on_navigation_response` drops
    ///   late responses once the target is removed below.
    fn on_target_crashed(&mut self, event: EventTargetCrashed) {
        let crashed_id = event.target_id.clone();
        let status = event.status.clone();
        let error_code = event.error_code;

        // Two-pass cancellation: collect matching call-ids, then
        // remove + signal. Can't signal inside `iter()` because
        // `OneshotSender::send` consumes the sender, and the
        // borrow checker disallows taking ownership from inside
        // the iterator.
        let to_cancel: Vec<CallId> = self
            .pending_commands
            .iter()
            .filter_map(|(&call_id, (req, _, _))| match req {
                PendingRequest::ExternalCommand {
                    target_id: Some(tid),
                    ..
                } if *tid == crashed_id => Some(call_id),
                PendingRequest::InternalCommand(tid) if *tid == crashed_id => Some(call_id),
                _ => None,
            })
            .collect();

        for call_id in to_cancel {
            if let Some((req, _, _)) = self.pending_commands.remove(&call_id) {
                match req {
                    PendingRequest::ExternalCommand { tx, .. } => {
                        let _ = tx.send(Err(CdpError::msg(format!(
                            "target {:?} crashed: {} (errorCode={})",
                            crashed_id, status, error_code
                        ))));
                    }
                    PendingRequest::InternalCommand(_) => {
                        // Target-init command — the target is gone,
                        // nobody is waiting on a user-facing reply.
                    }
                    _ => {}
                }
            }
        }

        // Same map cleanup as `on_target_destroyed`.
        self.attached_targets.remove(&crashed_id);
        if let Some(target) = self.targets.remove(&crashed_id) {
            if let Some(session) = target.session_id() {
                self.sessions.remove(session);
            }
        }
    }

    /// House keeping of commands
    ///
    /// Remove all commands where `now` > `timestamp of command starting point +
    /// request timeout` and notify the senders that their request timed out.
    fn evict_timed_out_commands(&mut self, now: Instant) {
        let deadline = match now.checked_sub(self.config.request_timeout) {
            Some(d) => d,
            None => return,
        };

        let timed_out: Vec<_> = self
            .pending_commands
            .iter()
            .filter(|(_, (_, _, timestamp))| *timestamp < deadline)
            .map(|(k, _)| *k)
            .collect();

        for call in timed_out {
            if let Some((req, _, _)) = self.pending_commands.remove(&call) {
                match req {
                    PendingRequest::CreateTarget(tx) => {
                        let _ = tx.send(Err(CdpError::Timeout));
                    }
                    PendingRequest::GetTargets(tx) => {
                        let _ = tx.send(Err(CdpError::Timeout));
                    }
                    PendingRequest::Navigate(nav) => {
                        if let Some(nav) = self.navigations.remove(&nav) {
                            match nav {
                                NavigationRequest::Navigate(_, nav) => {
                                    let _ = nav.tx.send(Err(CdpError::Timeout));
                                }
                            }
                        }
                    }
                    PendingRequest::ExternalCommand { tx, .. } => {
                        let _ = tx.send(Err(CdpError::Timeout));
                    }
                    PendingRequest::InternalCommand(_) => {}
                    PendingRequest::CloseBrowser(tx) => {
                        let _ = tx.send(Err(CdpError::Timeout));
                    }
                }
            }
        }
    }

    pub fn event_listeners_mut(&mut self) -> &mut EventListeners {
        &mut self.event_listeners
    }

    // ------------------------------------------------------------------
    //  Tokio-native async entry point
    // ------------------------------------------------------------------

    /// Run the handler as a fully async tokio task.
    ///
    /// This is the high-performance alternative to polling `Handler` as a
    /// `Stream`.  Internally it:
    ///
    /// * Splits the WebSocket into independent read/write halves — the
    ///   writer runs in its own tokio task with natural batching.
    /// * Uses `tokio::select!` to multiplex the browser channel, page
    ///   notifications, WebSocket reads, the eviction timer, and writer
    ///   health.
    /// * Drains every target's page channel via `try_recv()` (non-blocking)
    ///   after each event, with an `Arc<Notify>` ensuring the select loop
    ///   wakes up whenever a page sends a message.
    ///
    /// # Usage
    ///
    /// ```rust,no_run
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// use chromiumoxide::Browser;
    /// let (browser, handler) = Browser::launch(Default::default()).await?;
    /// let handler_task = tokio::spawn(handler.run());
    /// // … use browser …
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run(mut self) -> Result<()> {
        use chromiumoxide_types::Message;
        use tokio::time::MissedTickBehavior;
        use tokio_tungstenite::tungstenite::{self, error::ProtocolError};

        // --- set up page notification ---
        let page_wake = Arc::new(Notify::new());
        self.page_wake = Some(page_wake.clone());

        // --- split WebSocket ---
        let conn = self
            .conn
            .take()
            .ok_or_else(|| CdpError::msg("Handler::run() called with no connection"))?;
        let async_conn = conn.into_async();
        let mut ws_reader = async_conn.reader;
        let ws_tx = async_conn.cmd_tx;
        let mut writer_handle = async_conn.writer_handle;
        let reader_handle = async_conn.reader_handle;
        let mut next_call_id = async_conn.next_id;

        // Helper to mint call-ids without &mut self.conn.
        let mut alloc_call_id = || {
            let id = chromiumoxide_types::CallId::new(next_call_id);
            next_call_id = next_call_id.wrapping_add(1);
            id
        };

        // --- eviction timer ---
        let mut evict_timer = tokio::time::interval_at(
            tokio::time::Instant::now() + self.config.request_timeout,
            self.config.request_timeout,
        );
        evict_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // Helper closure: submit a MethodCall through the WS writer.
        macro_rules! ws_submit {
            ($method:expr, $session_id:expr, $params:expr) => {{
                let id = alloc_call_id();
                let call = chromiumoxide_types::MethodCall {
                    id,
                    method: $method,
                    session_id: $session_id,
                    params: $params,
                };
                match ws_tx.try_send(call) {
                    Ok(()) => Ok::<_, CdpError>(id),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!("WS command channel full — dropping command");
                        Err(CdpError::msg("WS command channel full"))
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        Err(CdpError::msg("WS writer closed"))
                    }
                }
            }};
        }

        // ---- main event loop ----
        //
        // Modeled as an expression-loop producing `Result<()>` so that every
        // exit path falls through to the graceful-shutdown block below
        // (drop ws_tx → writer drains queue + sends WS Close → reader
        // aborted). This matters for remote browsers (`Browser::connect`)
        // where there is no child process whose death closes the socket.
        let run_result: Result<()> = loop {
            let now = std::time::Instant::now();

            // 1. Drain all target page channels (non-blocking) & advance
            //    state machines.
            //
            // Budget: drain at most 128 messages per target per iteration
            // so a single chatty page cannot starve the rest.
            const PER_TARGET_DRAIN_BUDGET: usize = 128;

            for n in (0..self.target_ids.len()).rev() {
                let target_id = self.target_ids.swap_remove(n);

                if let Some((id, mut target)) = self.targets.remove_entry(&target_id) {
                    // Drain page channel (non-blocking — waker is the Notify).
                    {
                        let mut msgs = Vec::new();
                        if let Some(handle) = target.page_mut() {
                            while msgs.len() < PER_TARGET_DRAIN_BUDGET {
                                match handle.rx.try_recv() {
                                    Ok(msg) => msgs.push(msg),
                                    Err(_) => break,
                                }
                            }
                        }
                        for msg in msgs {
                            target.on_page_message(msg);
                        }
                    }

                    // Advance target state machine & process events.
                    while let Some(event) = target.advance(now) {
                        match event {
                            TargetEvent::Request(req) => {
                                if let Ok(call_id) =
                                    ws_submit!(req.method.clone(), req.session_id, req.params)
                                {
                                    self.pending_commands.insert(
                                        call_id,
                                        (
                                            PendingRequest::InternalCommand(
                                                target.target_id().clone(),
                                            ),
                                            req.method,
                                            now,
                                        ),
                                    );
                                }
                            }
                            TargetEvent::Command(msg) => {
                                if msg.is_navigation() {
                                    let (req, tx) = msg.split();
                                    let nav_id = self.next_navigation_id();
                                    target.goto(FrameRequestedNavigation::new(
                                        nav_id,
                                        req.clone(),
                                        self.config.request_timeout,
                                    ));
                                    if let Ok(call_id) =
                                        ws_submit!(req.method.clone(), req.session_id, req.params)
                                    {
                                        self.pending_commands.insert(
                                            call_id,
                                            (PendingRequest::Navigate(nav_id), req.method, now),
                                        );
                                    }
                                    self.navigations.insert(
                                        nav_id,
                                        NavigationRequest::Navigate(
                                            target.target_id().clone(),
                                            NavigationInProgress::new(tx),
                                        ),
                                    );
                                } else if let Ok(call_id) = ws_submit!(
                                    msg.method.clone(),
                                    msg.session_id.map(Into::into),
                                    msg.params
                                ) {
                                    // `target` is in scope here, so bind
                                    // the pending command to its target_id
                                    // directly.
                                    let target_id = Some(target.target_id().clone());
                                    self.pending_commands.insert(
                                        call_id,
                                        (
                                            PendingRequest::ExternalCommand {
                                                tx: msg.sender,
                                                target_id,
                                            },
                                            msg.method,
                                            now,
                                        ),
                                    );
                                }
                            }
                            TargetEvent::NavigationRequest(nav_id, req) => {
                                if let Ok(call_id) =
                                    ws_submit!(req.method.clone(), req.session_id, req.params)
                                {
                                    self.pending_commands.insert(
                                        call_id,
                                        (PendingRequest::Navigate(nav_id), req.method, now),
                                    );
                                }
                            }
                            TargetEvent::NavigationResult(res) => {
                                self.on_navigation_lifecycle_completed(res);
                            }
                            TargetEvent::BytesConsumed(n) => {
                                if let Some(rem) = self.remaining_bytes.as_mut() {
                                    *rem = rem.saturating_sub(n);
                                    if *rem == 0 {
                                        self.budget_exhausted = true;
                                    }
                                }
                            }
                        }
                    }

                    // Flush event listeners (no Context needed).
                    target.event_listeners_mut().flush();

                    self.targets.insert(id, target);
                    self.target_ids.push(target_id);
                }
            }

            // Flush handler-level event listeners.
            self.event_listeners.flush();

            if self.budget_exhausted {
                for t in self.targets.values_mut() {
                    t.network_manager.set_block_all(true);
                }
            }

            if self.closing {
                break Ok(());
            }

            // 2. Multiplex all event sources via tokio::select!
            tokio::select! {
                msg = self.from_browser.recv() => {
                    match msg {
                        Some(msg) => {
                            match msg {
                                HandlerMessage::Command(cmd) => {
                                    // See `submit_external_command` for
                                    // the session_id → target_id resolve.
                                    let target_id = cmd
                                        .session_id
                                        .as_ref()
                                        .and_then(|sid| self.sessions.get(sid.as_ref()))
                                        .map(|s| s.target_id().clone());
                                    if let Ok(call_id) = ws_submit!(
                                        cmd.method.clone(),
                                        cmd.session_id.map(Into::into),
                                        cmd.params
                                    ) {
                                        self.pending_commands.insert(
                                            call_id,
                                            (
                                                PendingRequest::ExternalCommand {
                                                    tx: cmd.sender,
                                                    target_id,
                                                },
                                                cmd.method,
                                                now,
                                            ),
                                        );
                                    }
                                }
                                HandlerMessage::FetchTargets(tx) => {
                                    let msg = TARGET_PARAMS_ID.clone();
                                    if let Ok(call_id) = ws_submit!(msg.0.clone(), None, msg.1) {
                                        self.pending_commands.insert(
                                            call_id,
                                            (PendingRequest::GetTargets(tx), msg.0, now),
                                        );
                                    }
                                }
                                HandlerMessage::CloseBrowser(tx) => {
                                    let close_msg = CLOSE_PARAMS_ID.clone();
                                    if let Ok(call_id) = ws_submit!(close_msg.0.clone(), None, close_msg.1) {
                                        self.pending_commands.insert(
                                            call_id,
                                            (PendingRequest::CloseBrowser(tx), close_msg.0, now),
                                        );
                                    }
                                }
                                HandlerMessage::CreatePage(params, tx) => {
                                    if let Some(ref id) = params.browser_context_id {
                                        self.browser_contexts.insert(BrowserContext::from(id.clone()));
                                    }
                                    self.create_page_async(params, tx, &mut alloc_call_id, &ws_tx, now);
                                }
                                HandlerMessage::GetPages(tx) => {
                                    let pages: Vec<_> = self.targets.values_mut()
                                        .filter(|p| p.is_page())
                                        .filter_map(|target| target.get_or_create_page())
                                        .map(|page| Page::from(page.clone()))
                                        .collect();
                                    let _ = tx.send(pages);
                                }
                                HandlerMessage::InsertContext(ctx) => {
                                    if self.default_browser_context.id().is_none() {
                                        self.default_browser_context = ctx.clone();
                                    }
                                    self.browser_contexts.insert(ctx);
                                }
                                HandlerMessage::DisposeContext(ctx) => {
                                    self.browser_contexts.remove(&ctx);
                                    self.attached_targets.retain(|tid| {
                                        self.targets.get(tid)
                                            .and_then(|t| t.browser_context_id())
                                            .map(|id| Some(id) != ctx.id())
                                            .unwrap_or(true)
                                    });
                                    self.closing = true;
                                }
                                HandlerMessage::GetPage(target_id, tx) => {
                                    let page = self.targets.get_mut(&target_id)
                                        .and_then(|target| target.get_or_create_page())
                                        .map(|page| Page::from(page.clone()));
                                    let _ = tx.send(page);
                                }
                                HandlerMessage::AddEventListener(req) => {
                                    self.event_listeners.add_listener(req);
                                }
                            }
                        }
                        None => break Ok(()), // browser handle dropped
                    }
                }

                frame = ws_reader.next_message() => {
                    match frame {
                        Some(Ok(boxed_msg)) => match *boxed_msg {
                            Message::Response(resp) => {
                                self.on_response(resp);
                            }
                            Message::Event(ev) => {
                                self.on_event(ev);
                            }
                        },
                        Some(Err(err)) => {
                            tracing::error!("WS Connection error: {:?}", err);
                            if let CdpError::Ws(ref ws_error) = err {
                                match ws_error {
                                    tungstenite::Error::AlreadyClosed => break Ok(()),
                                    tungstenite::Error::Protocol(detail)
                                        if detail == &ProtocolError::ResetWithoutClosingHandshake =>
                                    {
                                        break Ok(());
                                    }
                                    _ => break Err(err),
                                }
                            } else {
                                break Err(err);
                            }
                        }
                        None => break Ok(()), // WS closed
                    }
                }

                _ = page_wake.notified() => {
                    // A page sent a message — loop back to drain targets.
                }

                _ = evict_timer.tick() => {
                    self.evict_timed_out_commands(now);
                    for t in self.targets.values_mut() {
                        t.network_manager.evict_stale_entries(now);
                        t.frame_manager_mut().evict_stale_context_ids();
                    }
                }

                result = &mut writer_handle => {
                    // WS writer exited — propagate error or break.
                    match result {
                        Ok(Ok(())) => break Ok(()),
                        Ok(Err(e)) => break Err(e),
                        Err(e) => break Err(CdpError::msg(format!("WS writer panicked: {e}"))),
                    }
                }
            }
        };

        // ---- graceful shutdown ----
        //
        // Drop the WS command sender so the writer task's `rx.recv()`
        // returns `None`. The writer drains any queued commands, sends a
        // WebSocket Close frame to Chrome, and exits. For remote browsers
        // this is the only mechanism that closes the WS — there's no child
        // process whose death would close the socket.
        drop(ws_tx);

        // Wait briefly for the writer to send the Close frame. If it's
        // already done (e.g. exited via the writer-handle select arm),
        // skip the wait. Polling a finished `JoinHandle` again would
        // panic.
        if !writer_handle.is_finished() {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), &mut writer_handle)
                .await;
            if !writer_handle.is_finished() {
                writer_handle.abort();
            }
        }

        // Reader may be parked on `stream.next().await` waiting for
        // frames from Chrome. Its output channel receiver (`ws_reader`)
        // is dropped at function exit, so there is no consumer either
        // way — abort directly rather than waiting for the remote to
        // ack the Close frame.
        reader_handle.abort();

        run_result
    }

    /// `create_page` variant for the `run()` path that submits via `ws_tx`.
    fn create_page_async(
        &mut self,
        params: CreateTargetParams,
        tx: OneshotSender<Result<Page>>,
        alloc_call_id: &mut impl FnMut() -> chromiumoxide_types::CallId,
        ws_tx: &tokio::sync::mpsc::Sender<chromiumoxide_types::MethodCall>,
        now: std::time::Instant,
    ) {
        let about_blank = params.url == "about:blank";
        let http_check =
            !about_blank && params.url.starts_with("http") || params.url.starts_with("file://");

        if about_blank || http_check {
            let method = params.identifier();
            match serde_json::to_value(params) {
                Ok(params) => {
                    let id = alloc_call_id();
                    let call = chromiumoxide_types::MethodCall {
                        id,
                        method: method.clone(),
                        session_id: None,
                        params,
                    };
                    match ws_tx.try_send(call) {
                        Ok(()) => {
                            self.pending_commands
                                .insert(id, (PendingRequest::CreateTarget(tx), method, now));
                        }
                        Err(_) => {
                            let _ = tx
                                .send(Err(CdpError::msg("WS command channel full or closed")))
                                .ok();
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err.into())).ok();
                }
            }
        } else {
            let _ = tx.send(Err(CdpError::NotFound)).ok();
        }
    }

    /// Run the handler with one task per attached page (parallel handler).
    ///
    /// Opt-in via the `parallel-handler` Cargo feature. The single-task
    /// `Handler::run()` path is unchanged. See `src/handler/parallel/mod.rs`
    /// for the architectural notes and current scope limits.
    #[cfg(feature = "parallel-handler")]
    pub async fn run_parallel(mut self) -> Result<()> {
        // Reuse the existing setup that `run()` did inline: split the WS
        // connection, kick the boot `Target.setDiscoverTargets` command,
        // and hand everything to the Router.
        let conn = self
            .conn
            .take()
            .ok_or_else(|| CdpError::msg("Handler::run_parallel() called with no connection"))?;
        let async_conn = conn.into_async();

        // The boot command has already been pushed by `Handler::new`; it
        // sits at call_id `next_id - 1`.
        let next_id = async_conn.next_id;
        let boot_call_id = chromiumoxide_types::CallId::new(next_id.saturating_sub(1));
        let boot_method = DISCOVER_ID.0.clone();

        let router = parallel::Router::new(
            self.config,
            self.default_browser_context,
            self.from_browser,
            async_conn.reader,
            async_conn.cmd_tx,
            boot_call_id,
            boot_method,
            next_id,
        );
        let result = router.run().await;

        // Make sure the writer drains and the reader task exits cleanly.
        async_conn.writer_handle.abort();
        async_conn.reader_handle.abort();

        result
    }
}

impl Stream for Handler {
    type Item = Result<()>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Budgets prevent a single chatty target or WS flood from
        // starving other futures on the runtime. Mirror the caps
        // used in `Handler::run()`; on exhaustion, self-wake and
        // return Pending so the executor gets a chance to schedule
        // other work before we resume.
        const BROWSER_MSG_BUDGET: usize = 128;
        const PER_TARGET_DRAIN_BUDGET: usize = 128;
        const WS_MSG_BUDGET: usize = 512;

        let pin = self.get_mut();

        let mut dispose = false;
        let mut budget_hit = false;

        let now = Instant::now();

        loop {
            // temporary pinning of the browser receiver should be safe as we are pinning
            // through the already pinned self. with the receivers we can also
            // safely ignore exhaustion as those are fused.
            let mut browser_msgs = 0usize;
            while let Poll::Ready(Some(msg)) = pin.from_browser.poll_recv(cx) {
                match msg {
                    HandlerMessage::Command(cmd) => {
                        pin.submit_external_command(cmd, now)?;
                    }
                    HandlerMessage::FetchTargets(tx) => {
                        pin.submit_fetch_targets(tx, now);
                    }
                    HandlerMessage::CloseBrowser(tx) => {
                        pin.submit_close(tx, now);
                    }
                    HandlerMessage::CreatePage(params, tx) => {
                        if let Some(ref id) = params.browser_context_id {
                            pin.browser_contexts
                                .insert(BrowserContext::from(id.clone()));
                        }
                        pin.create_page(params, tx);
                    }
                    HandlerMessage::GetPages(tx) => {
                        let pages: Vec<_> = pin
                            .targets
                            .values_mut()
                            .filter(|p: &&mut Target| p.is_page())
                            .filter_map(|target| target.get_or_create_page())
                            .map(|page| Page::from(page.clone()))
                            .collect();
                        let _ = tx.send(pages);
                    }
                    HandlerMessage::InsertContext(ctx) => {
                        if pin.default_browser_context.id().is_none() {
                            pin.default_browser_context = ctx.clone();
                        }
                        pin.browser_contexts.insert(ctx);
                    }
                    HandlerMessage::DisposeContext(ctx) => {
                        pin.browser_contexts.remove(&ctx);
                        pin.attached_targets.retain(|tid| {
                            pin.targets
                                .get(tid)
                                .and_then(|t| t.browser_context_id()) // however you expose it
                                .map(|id| Some(id) != ctx.id())
                                .unwrap_or(true)
                        });
                        pin.closing = true;
                        dispose = true;
                    }
                    HandlerMessage::GetPage(target_id, tx) => {
                        let page = pin
                            .targets
                            .get_mut(&target_id)
                            .and_then(|target| target.get_or_create_page())
                            .map(|page| Page::from(page.clone()));
                        let _ = tx.send(page);
                    }
                    HandlerMessage::AddEventListener(req) => {
                        pin.event_listeners.add_listener(req);
                    }
                }
                browser_msgs += 1;
                if browser_msgs >= BROWSER_MSG_BUDGET {
                    budget_hit = true;
                    break;
                }
            }

            for n in (0..pin.target_ids.len()).rev() {
                let target_id = pin.target_ids.swap_remove(n);

                if let Some((id, mut target)) = pin.targets.remove_entry(&target_id) {
                    let mut drained = 0usize;
                    while let Some(event) = target.poll(cx, now) {
                        match event {
                            TargetEvent::Request(req) => {
                                let _ = pin.submit_internal_command(
                                    target.target_id().clone(),
                                    req,
                                    now,
                                );
                            }
                            TargetEvent::Command(msg) => {
                                pin.on_target_message(&mut target, msg, now);
                            }
                            TargetEvent::NavigationRequest(id, req) => {
                                pin.submit_navigation(id, req, now);
                            }
                            TargetEvent::NavigationResult(res) => {
                                pin.on_navigation_lifecycle_completed(res)
                            }
                            TargetEvent::BytesConsumed(n) => {
                                if let Some(rem) = pin.remaining_bytes.as_mut() {
                                    *rem = rem.saturating_sub(n);
                                    if *rem == 0 {
                                        pin.budget_exhausted = true;
                                    }
                                }
                            }
                        }
                        drained += 1;
                        if drained >= PER_TARGET_DRAIN_BUDGET {
                            budget_hit = true;
                            break;
                        }
                    }

                    // poll the target's event listeners
                    target.event_listeners_mut().poll(cx);

                    pin.targets.insert(id, target);
                    pin.target_ids.push(target_id);
                }
            }

            // poll the handler-level event listeners once per iteration,
            // not once per target.
            pin.event_listeners_mut().poll(cx);

            let mut done = true;

            // Read WS messages into a temporary buffer so the conn borrow
            // is released before we process them (which needs &mut pin).
            let mut ws_msgs = Vec::new();
            let mut ws_err = None;
            {
                let Some(conn) = pin.conn.as_mut() else {
                    return Poll::Ready(Some(Err(CdpError::msg(
                        "connection consumed by Handler::run()",
                    ))));
                };
                while let Poll::Ready(Some(ev)) = Pin::new(&mut *conn).poll_next(cx) {
                    match ev {
                        Ok(msg) => ws_msgs.push(msg),
                        Err(err) => {
                            ws_err = Some(err);
                            break;
                        }
                    }
                    if ws_msgs.len() >= WS_MSG_BUDGET {
                        budget_hit = true;
                        break;
                    }
                }
            }

            for boxed_msg in ws_msgs {
                match *boxed_msg {
                    Message::Response(resp) => {
                        pin.on_response(resp);
                        if pin.closing {
                            return Poll::Ready(None);
                        }
                    }
                    Message::Event(ev) => {
                        pin.on_event(ev);
                    }
                }
                done = false;
            }

            if let Some(err) = ws_err {
                tracing::error!("WS Connection error: {:?}", err);
                if let CdpError::Ws(ref ws_error) = err {
                    match ws_error {
                        Error::AlreadyClosed => {
                            pin.closing = true;
                            dispose = true;
                        }
                        Error::Protocol(detail)
                            if detail == &ProtocolError::ResetWithoutClosingHandshake =>
                        {
                            pin.closing = true;
                            dispose = true;
                        }
                        _ => return Poll::Ready(Some(Err(err))),
                    }
                } else {
                    return Poll::Ready(Some(Err(err)));
                }
            }

            if pin.evict_command_timeout.poll_ready(cx) {
                // evict all commands that timed out
                pin.evict_timed_out_commands(now);
                // evict stale network race-condition buffers and
                // orphaned context_ids / frame entries
                for t in pin.targets.values_mut() {
                    t.network_manager.evict_stale_entries(now);
                    t.frame_manager_mut().evict_stale_context_ids();
                }
            }

            if pin.budget_exhausted {
                for t in pin.targets.values_mut() {
                    t.network_manager.set_block_all(true);
                }
            }

            if dispose {
                return Poll::Ready(None);
            }

            if budget_hit {
                // yield to the scheduler; self-wake so the remaining
                // work resumes on the next tick without waiting for
                // a WS event.
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            if done {
                // no events/responses were read from the websocket
                return Poll::Pending;
            }
        }
    }
}

/// How to configure the handler
#[derive(Debug, Clone)]
pub struct HandlerConfig {
    /// Whether the `NetworkManager`s should ignore https errors
    pub ignore_https_errors: bool,
    /// Window and device settings
    pub viewport: Option<Viewport>,
    /// Context ids to set from the get go
    pub context_ids: Vec<BrowserContextId>,
    /// default request timeout to use
    pub request_timeout: Duration,
    /// Whether to enable request interception
    pub request_intercept: bool,
    /// Whether to enable cache
    pub cache_enabled: bool,
    /// Whether to enable Service Workers
    pub service_worker_enabled: bool,
    /// Whether to ignore visuals.
    pub ignore_visuals: bool,
    /// Whether to ignore stylesheets.
    pub ignore_stylesheets: bool,
    /// Whether to ignore Javascript only allowing critical framework or lib based rendering.
    pub ignore_javascript: bool,
    /// When `ignore_stylesheets` would skip a stylesheet, allow it through if
    /// the request URL is first-party (registrable domain matches the page's
    /// primary frame). Default `true` so SPAs that load their own CSS via
    /// dynamic imports still hydrate. Set `false` for strict block-all.
    pub allow_first_party_stylesheets: bool,
    /// When a downstream blocker (intercept manager / adblock / blocklists)
    /// would skip a script, allow it through if first-party. Default `true`
    /// so SPA bootloaders are not collateral damage from third-party rules.
    pub allow_first_party_javascript: bool,
    /// When `ignore_visuals` would skip an image/media/font, allow it through
    /// if the request URL is first-party. Default `true`. Set `false` for
    /// strict bandwidth-minimal crawls that drop ALL visuals.
    pub allow_first_party_visuals: bool,
    /// Whether to ignore analytics.
    pub ignore_analytics: bool,
    /// Ignore prefetch request. Defaults to true.
    pub ignore_prefetch: bool,
    /// Whether to ignore ads.
    pub ignore_ads: bool,
    /// Extra headers.
    pub extra_headers: Option<std::collections::HashMap<String, String>>,
    /// Only Html.
    pub only_html: bool,
    /// Created the first target.
    pub created_first_target: bool,
    /// The network intercept manager.
    pub intercept_manager: NetworkInterceptManager,
    /// The max bytes to receive.
    pub max_bytes_allowed: Option<u64>,
    /// Cap on main-frame Document redirect hops (per navigation).
    ///
    /// `None` disables enforcement (default); `Some(n)` aborts once the chain length
    /// exceeds `n` by emitting `net::ERR_TOO_MANY_REDIRECTS` and calling
    /// `Page.stopLoading`. Preserves the accumulated `redirect_chain` on the failed
    /// request so consumers can inspect it.
    pub max_redirects: Option<usize>,
    /// Cap on main-frame cross-document navigations per `goto`. Defends against
    /// JS / meta-refresh loops that bypass the HTTP redirect guard. `None`
    /// disables the guard.
    pub max_main_frame_navigations: Option<u32>,
    /// Optional per-run/per-site whitelist of URL substrings (scripts/resources).
    pub whitelist_patterns: Option<Vec<String>>,
    /// Optional per-run/per-site blacklist of URL substrings (scripts/resources).
    pub blacklist_patterns: Option<Vec<String>>,
    /// Push the interception policy to a capable remote engine once per
    /// navigation (`Interception.setPolicy`) so it can resolve block/allow
    /// locally instead of round-tripping each `Fetch.requestPaused`. Default
    /// `false`; safe to enable against any target (unknown method is ignored).
    pub remote_local_policy: bool,
    /// Extra ABP/uBO filter rules for the adblock engine.
    #[cfg(feature = "adblock")]
    pub adblock_filter_rules: Option<Vec<String>>,
    /// Capacity of the channel between browser handle and handler.
    /// Defaults to 1000.
    pub channel_capacity: usize,
    /// Capacity of the per-page mpsc channel carrying `TargetMessage`s
    /// from each `Page` to the handler.
    ///
    /// Defaults to `DEFAULT_PAGE_CHANNEL_CAPACITY` (2048) — the previous
    /// hard-coded value. Tune upward for pages that burst many commands
    /// (heavy `evaluate`/selector use, high-concurrency tasks sharing
    /// one page) to avoid pushing each extra command onto the
    /// `CommandFuture` async-send fallback path on `TrySendError::Full`.
    /// Tune downward to apply back-pressure sooner. Values of `0` are
    /// clamped to `1` at channel creation.
    pub page_channel_capacity: usize,
    /// Number of WebSocket connection retry attempts with exponential backoff.
    /// Defaults to 4.
    pub connection_retries: u32,
}

impl Default for HandlerConfig {
    fn default() -> Self {
        Self {
            ignore_https_errors: true,
            viewport: Default::default(),
            context_ids: Vec::new(),
            request_timeout: Duration::from_millis(REQUEST_TIMEOUT),
            request_intercept: false,
            cache_enabled: true,
            service_worker_enabled: true,
            ignore_visuals: false,
            ignore_stylesheets: false,
            ignore_ads: false,
            ignore_javascript: false,
            allow_first_party_stylesheets: true,
            allow_first_party_javascript: true,
            allow_first_party_visuals: true,
            ignore_analytics: true,
            ignore_prefetch: true,
            only_html: false,
            extra_headers: Default::default(),
            created_first_target: false,
            intercept_manager: NetworkInterceptManager::Unknown,
            max_bytes_allowed: None,
            max_redirects: None,
            max_main_frame_navigations: None,
            whitelist_patterns: None,
            blacklist_patterns: None,
            remote_local_policy: false,
            #[cfg(feature = "adblock")]
            adblock_filter_rules: None,
            channel_capacity: 4096,
            page_channel_capacity: crate::handler::page::DEFAULT_PAGE_CHANNEL_CAPACITY,
            connection_retries: crate::conn::DEFAULT_CONNECTION_RETRIES,
        }
    }
}

/// Wraps the sender half of the channel who requested a navigation
#[derive(Debug)]
pub struct NavigationInProgress<T> {
    /// Marker to indicate whether a navigation lifecycle has completed
    navigated: bool,
    /// The response of the issued navigation request
    response: Option<Response>,
    /// Sender who initiated the navigation request
    tx: OneshotSender<T>,
}

impl<T> NavigationInProgress<T> {
    pub(crate) fn new(tx: OneshotSender<T>) -> Self {
        Self {
            navigated: false,
            response: None,
            tx,
        }
    }

    /// The response to the cdp request has arrived
    pub(crate) fn set_response(&mut self, resp: Response) {
        self.response = Some(resp);
    }

    /// The navigation process has finished, the page finished loading.
    pub(crate) fn set_navigated(&mut self) {
        self.navigated = true;
    }

    /// Used by the parallel handler when reconciling Page.navigate response
    /// vs. lifecycle completion order — the existing serial handler reads
    /// the field directly so these accessors are otherwise inert.
    #[cfg_attr(not(feature = "parallel-handler"), allow(dead_code))]
    pub(crate) fn is_navigated(&self) -> bool {
        self.navigated
    }

    #[cfg_attr(not(feature = "parallel-handler"), allow(dead_code))]
    pub(crate) fn take_response(&mut self) -> Option<Response> {
        self.response.take()
    }

    #[cfg_attr(not(feature = "parallel-handler"), allow(dead_code))]
    pub(crate) fn into_tx(self) -> OneshotSender<T> {
        self.tx
    }
}

/// Request type for navigation
#[derive(Debug)]
enum NavigationRequest {
    /// Represents a simple `NavigateParams` ("Page.navigate")
    Navigate(TargetId, NavigationInProgress<Result<Response>>),
    // TODO are there more?
}

/// Different kind of submitted request submitted from the  `Handler` to the
/// `Connection` and being waited on for the response.
#[derive(Debug)]
enum PendingRequest {
    /// A Request to create a new `Target` that results in the creation of a
    /// `Page` that represents a browser page.
    CreateTarget(OneshotSender<Result<Page>>),
    /// A Request to fetch old `Target`s created before connection
    GetTargets(OneshotSender<Result<Vec<TargetInfo>>>),
    /// A Request to navigate a specific `Target`.
    ///
    /// Navigation requests are not automatically completed once the response to
    /// the raw cdp navigation request (like `NavigateParams`) arrives, but only
    /// after the `Target` notifies the `Handler` that the `Page` has finished
    /// loading, which comes after the response.
    Navigate(NavigationId),
    /// A common request received via a channel (`Page`).
    ///
    /// `target_id` is resolved at submit time from the caller's
    /// `session_id` against `self.sessions`, so `on_target_crashed`
    /// can cancel in-flight user commands immediately. `None` when
    /// the command has no session (browser-level) or was sent
    /// before the attach event arrived — those fall back to the
    /// normal `request_timeout` eviction.
    ExternalCommand {
        tx: OneshotSender<Result<Response>>,
        target_id: Option<TargetId>,
    },
    /// Requests that are initiated directly from a `Target` (all the
    /// initialization commands).
    InternalCommand(TargetId),
    // A Request to close the browser.
    CloseBrowser(OneshotSender<Result<CloseReturns>>),
}

/// Events used internally to communicate with the handler, which are executed
/// in the background
// TODO rename to BrowserMessage
#[derive(Debug)]
pub(crate) enum HandlerMessage {
    CreatePage(CreateTargetParams, OneshotSender<Result<Page>>),
    FetchTargets(OneshotSender<Result<Vec<TargetInfo>>>),
    InsertContext(BrowserContext),
    DisposeContext(BrowserContext),
    GetPages(OneshotSender<Vec<Page>>),
    Command(CommandMessage),
    GetPage(TargetId, OneshotSender<Option<Page>>),
    AddEventListener(EventListenerRequest),
    CloseBrowser(OneshotSender<Result<CloseReturns>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide_cdp::cdp::browser_protocol::target::{AttachToTargetReturns, TargetInfo};

    #[test]
    fn attach_to_target_response_sets_session_id_before_event_arrives() {
        let info = TargetInfo::builder()
            .target_id("target-1".to_string())
            .r#type("page")
            .title("")
            .url("about:blank")
            .attached(false)
            .can_access_opener(false)
            .build()
            .expect("target info");
        let mut target = Target::new(info, TargetConfig::default(), BrowserContext::default());
        let method: MethodId = AttachToTargetParams::IDENTIFIER.into();
        let result = serde_json::to_value(AttachToTargetReturns::new("session-1".to_string()))
            .expect("attach result");
        let resp = Response {
            id: CallId::new(1),
            result: Some(result),
            error: None,
        };

        maybe_store_attach_session_id(&mut target, &method, &resp);

        assert_eq!(
            target.session_id().map(AsRef::as_ref),
            Some("session-1"),
            "attach response should seed the flat session id even before Target.attachedToTarget"
        );
    }

    /// Regression guard: `page_channel_capacity` must default to 2048
    /// everywhere, so existing callers see identical behavior to the
    /// previous hard-coded value. If this test ever fails, every caller
    /// that relied on the implicit 2048-slot channel silently changed.
    #[test]
    fn page_channel_capacity_defaults_to_2048_across_configs() {
        use crate::browser::BrowserConfigBuilder;
        use crate::handler::page::DEFAULT_PAGE_CHANNEL_CAPACITY;
        use crate::handler::target::TargetConfig;

        assert_eq!(DEFAULT_PAGE_CHANNEL_CAPACITY, 2048);
        assert_eq!(
            HandlerConfig::default().page_channel_capacity,
            DEFAULT_PAGE_CHANNEL_CAPACITY,
            "HandlerConfig default must match the historical 2048 slot count"
        );
        assert_eq!(
            TargetConfig::default().page_channel_capacity,
            DEFAULT_PAGE_CHANNEL_CAPACITY,
            "TargetConfig default must match the historical 2048 slot count"
        );
        // BrowserConfigBuilder default → build a builder (no executable
        // check needed: we only inspect the numeric field, not `build()`).
        let builder = BrowserConfigBuilder::default();
        let bc = format!("{:?}", builder);
        assert!(
            bc.contains("page_channel_capacity: 2048"),
            "BrowserConfigBuilder must default page_channel_capacity to 2048, got: {bc}",
        );
    }
}
