//! Top-level router task for the parallel handler.
//!
//! Responsibilities:
//! * Drives the WebSocket reader and demuxes inbound CDP messages by call-id
//!   routing entries (responses) or session_id (events).
//! * Owns the slot allocator and routing tables (`target_id → slot`,
//!   `session_id → slot`, `slot → session_tx`).
//! * Handles browser-level commands (`HandlerMessage::CreatePage`,
//!   `HandlerMessage::Command` without session, etc.) on slot 0.
//! * Spawns a `SessionTask` per attached page and hands off `Target` state to
//!   it.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide_cdp::cdp::browser_protocol::browser::{CloseParams, CloseReturns};
use chromiumoxide_cdp::cdp::browser_protocol::target::{
    CreateTargetParams, EventAttachedToTarget, TargetId, TargetInfo,
};
use chromiumoxide_cdp::cdp::CdpEvent;
use chromiumoxide_cdp::cdp::CdpEventMessage;
use chromiumoxide_types::{CallId, Message, Method, MethodCall, MethodId, Response};
use futures_util::future::{AbortHandle, Abortable, Aborted, BoxFuture};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc;
use tokio::sync::oneshot::Sender as OneshotSender;
use tokio::sync::Notify;
use tokio::task::{Id, JoinError, JoinSet};

use crate::cmd::{to_command_response, CommandMessage};
use crate::conn::WsReader;
use crate::error::{CdpError, Result};
use crate::handler::target::{Target, TargetConfig};
use crate::handler::{BrowserContext, HandlerConfig, HandlerMessage};
use crate::listeners::EventListeners;
use crate::page::Page;

use super::ids::CallIdAllocator;
use super::session::SessionTask;
use super::types::{RouterToSession, SessionToRouter};

/// Capacity of the per-session inbound channel (Router → SessionTask).
const SESSION_INBOX_CAPACITY: usize = 1024;

/// Extra ordered envelopes retained while a session is briefly starved.
/// Both space and delivery time are bounded; one stalled page never stalls
/// the router, nor can it allocate an unbounded backlog.
const SESSION_OVERFLOW_CAPACITY: usize = 4096;

type Delivery = Abortable<BoxFuture<'static, (u16, Result<mpsc::OwnedPermit<RouterToSession>>)>>;

/// Capacity of the SessionTask → Router lifecycle channel.
const SESSION_LIFECYCLE_CAPACITY: usize = 256;

/// Pending router-side request awaiting a slot-0 response.
enum RouterPending {
    /// `Target.createTarget` initiated by `Browser::new_page`.
    CreateTarget(OneshotSender<Result<Page>>),
    /// Browser-level `Browser::execute` command (no session).
    BrowserCommand(OneshotSender<Result<Response>>),
    /// Explicit close is acknowledged only after its CDP response.
    CloseBrowser(OneshotSender<Result<CloseReturns>>),
    /// `Target.setDiscoverTargets` boot command — fire-and-forget.
    Boot,
}

struct SessionEntry {
    inbox: mpsc::Sender<RouterToSession>,
    target_id: TargetId,
    /// `None` until SessionTask reports the session_id back.
    session_id: Option<String>,
    /// Wake handle the page sender raises on every `TargetMessage` send.
    /// Held for future eviction / forced-wake paths.
    #[allow(dead_code)]
    page_wake: Arc<Notify>,
    /// Out-of-band cancellation: never competes with a full inbox.
    shutdown: Option<OneshotSender<()>>,
    overflow: VecDeque<(RouterToSession, tokio::time::Instant)>,
    delivery_abort: Option<AbortHandle>,
}

impl SessionEntry {
    fn stop(&mut self) {
        self.shutdown.take();
        if let Some(abort) = self.delivery_abort.take() {
            abort.abort();
        }
        for (msg, _) in self.overflow.drain(..) {
            if let RouterToSession::SetInitiator(tx) = msg {
                let _ = tx.send(Err(CdpError::msg("session delivery stopped")));
            }
        }
    }
}

impl Drop for SessionEntry {
    fn drop(&mut self) {
        // Also covers router cancellation and task exit before delivery.
        self.stop();
    }
}

pub(crate) struct Router {
    config: HandlerConfig,
    default_browser_context: BrowserContext,
    /// Browser → handler channel.
    from_browser: mpsc::Receiver<HandlerMessage>,
    /// WebSocket reader (decoded).
    ws_reader: WsReader<CdpEventMessage>,
    /// Shared writer half — cloned to every SessionTask.
    ws_tx: mpsc::Sender<MethodCall>,
    /// Shared call-id allocator. Mints monotonic small ids (so Chrome's
    /// JSON serializer keeps them as integers) and records the routing
    /// slot for every dispatched command in a sharded DashMap.
    ids: CallIdAllocator,
    /// Pending requests waiting for a slot-0 response. The `Instant` is
    /// the dispatch time so the eviction tick can time them out.
    pending: HashMap<CallId, (RouterPending, MethodId, Instant)>,
    /// Insertion times for `pending_initiators` so the eviction tick can
    /// time out parked CreateTarget initiators if the matching
    /// `Target.targetCreated` event never arrives.
    pending_initiator_ts: HashMap<TargetId, Instant>,
    /// All live sessions, indexed by slot.
    sessions: HashMap<u16, SessionEntry>,
    /// `target_id → slot` so we can route `Target.attachedToTarget` events.
    target_id_to_slot: HashMap<TargetId, u16>,
    /// `session_id → slot` so we can route session-keyed events.
    session_id_to_slot: HashMap<String, u16>,
    /// Slot allocator — slot 0 reserved for the Router.
    next_slot: u16,
    /// Slots remain occupied until the old task has completely exited.
    session_tasks: JoinSet<u16>,
    /// Join errors retain the task id even when a panic loses its slot output.
    session_task_slots: HashMap<Id, u16>,
    deliveries: FuturesUnordered<Delivery>,
    closing: bool,
    /// Short-lived, bounded rejection markers cover event-before-response.
    rejected_targets: HashMap<TargetId, Instant>,
    /// Initiators parked while waiting for `Target.targetCreated` to arrive.
    /// Real Chrome can deliver the `Target.createTarget` response before the
    /// matching event; the initiator gets handed off as soon as the event
    /// lands and the SessionTask is spawned.
    pending_initiators: HashMap<TargetId, OneshotSender<Result<Page>>>,
    /// SessionTasks signal lifecycle events here (attached / detached).
    session_lifecycle_rx: mpsc::Receiver<SessionToRouter>,
    session_lifecycle_tx: mpsc::Sender<SessionToRouter>,
    /// Browser-level event listeners (subscribed via `Browser::event_listener`).
    /// Per-target listeners ride on Target's own `EventListeners` and are
    /// driven by the SessionTask via `target.on_event`.
    event_listeners: EventListeners,
}

impl Router {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: HandlerConfig,
        default_browser_context: BrowserContext,
        from_browser: mpsc::Receiver<HandlerMessage>,
        ws_reader: WsReader<CdpEventMessage>,
        ws_tx: mpsc::Sender<MethodCall>,
        boot_call_id: CallId,
        boot_method: MethodId,
        next_call_id: usize,
    ) -> Self {
        let (session_lifecycle_tx, session_lifecycle_rx) =
            mpsc::channel(SESSION_LIFECYCLE_CAPACITY);

        let mut pending = HashMap::new();
        pending.insert(
            boot_call_id,
            (RouterPending::Boot, boot_method, Instant::now()),
        );

        Self {
            config,
            default_browser_context,
            from_browser,
            ws_reader,
            ws_tx,
            ids: CallIdAllocator::new(next_call_id as u64),
            pending,
            sessions: HashMap::new(),
            target_id_to_slot: HashMap::new(),
            session_id_to_slot: HashMap::new(),
            next_slot: 1,
            session_tasks: JoinSet::new(),
            session_task_slots: HashMap::new(),
            deliveries: FuturesUnordered::new(),
            closing: false,
            rejected_targets: HashMap::new(),
            pending_initiators: HashMap::new(),
            pending_initiator_ts: HashMap::new(),
            session_lifecycle_rx,
            session_lifecycle_tx,
            event_listeners: EventListeners::default(),
        }
    }

    pub async fn run(mut self) -> Result<()> {
        let request_timeout = self.config.request_timeout;
        let mut evict = crate::handler::job::eviction_interval(request_timeout)?;

        loop {
            if self.closing {
                break;
            }
            // Push any queued listener events to subscribers between
            // select wakeups. Runs on every iteration regardless of arm —
            // that's where the existing serial handler does it too.
            self.event_listeners.flush();

            tokio::select! {
                _ = evict.tick() => {
                    self.evict_router_pending(Instant::now(), request_timeout);
                }

                msg = self.ws_reader.next_message() => {
                    match msg {
                        Some(Ok(boxed)) => self.on_ws_message(*boxed),
                        Some(Err(_)) | None => break,
                    }
                }

                completed = self.session_tasks.join_next_with_id(), if !self.session_tasks.is_empty() => {
                    if let Some(completed) = completed {
                        self.on_session_exit(completed);
                    }
                }

                lifecycle = self.session_lifecycle_rx.recv() => {
                    if let Some(lifecycle) = lifecycle { self.on_lifecycle(lifecycle); }
                }

                delivery = self.deliveries.next(), if !self.deliveries.is_empty() => {
                    if let Some(delivery) = delivery { self.on_delivery(delivery); }
                }

                browser = self.from_browser.recv() => {
                    match browser {
                        Some(msg) => self.on_browser_message(msg),
                        None => break,
                    }
                }
            }
        }

        for entry in self.sessions.values_mut() {
            entry.stop();
        }
        while let Some(completed) = self.session_tasks.join_next_with_id().await {
            self.on_session_exit(completed);
        }
        Ok(())
    }

    fn on_session_exit(&mut self, completed: std::result::Result<(Id, u16), JoinError>) {
        let task_id = match completed {
            Ok((task_id, _)) => task_id,
            Err(err) => {
                // A failed page must not abort unrelated sessions. Its Drop
                // resolves callers; reclaim only its routes after joining it.
                tracing::warn!("parallel session task failed: {err}");
                err.id()
            }
        };
        if let Some(slot) = self.session_task_slots.remove(&task_id) {
            self.remove_session(slot);
        }
    }

    fn on_ws_message(&mut self, message: Message<CdpEventMessage>) {
        match message {
            Message::Response(resp) => self.on_response(resp),
            Message::Event(ev) => self.on_event(ev),
        }
    }

    fn on_response(&mut self, resp: Response) {
        let call_id = resp.id;
        // Resolve routing: SessionTask-owned ids land in the allocator's
        // routing map; ids without an entry are router-owned (slot 0).
        match self.ids.take_route(call_id) {
            Some(slot) => {
                // Some CDP servers only return the sessionId in the attach
                // response. Register it before reading the next WS frame;
                // the session's lifecycle hint may be queued behind traffic.
                if self
                    .sessions
                    .get(&slot)
                    .is_some_and(|entry| entry.session_id.is_none())
                {
                    if let Some(sid) = resp
                        .result
                        .as_ref()
                        .and_then(|result| result.get("sessionId"))
                        .and_then(serde_json::Value::as_str)
                    {
                        self.on_lifecycle(SessionToRouter::SessionAttached {
                            slot,
                            session_id: sid.to_owned(),
                        });
                    }
                }
                self.send_session(
                    slot,
                    RouterToSession::Response(call_id, resp, MethodId::from("")),
                );
            }
            None => self.handle_router_response(call_id, resp),
        }
    }

    fn handle_router_response(&mut self, call_id: CallId, resp: Response) {
        let Some((pending, method, _ts)) = self.pending.remove(&call_id) else {
            return;
        };
        match pending {
            RouterPending::Boot => {}
            RouterPending::BrowserCommand(tx) => {
                let _ = tx.send(Ok(resp));
            }
            RouterPending::CloseBrowser(tx) => {
                self.closing = true;
                let _ = tx.send(to_command_response::<CloseParams>(resp, method).map(|r| r.result));
            }
            RouterPending::CreateTarget(tx) => {
                match to_command_response::<CreateTargetParams>(resp, method) {
                    Ok(parsed) => {
                        let target_id: TargetId = parsed.result.target_id;
                        if self.rejected_targets.remove(&target_id).is_some() {
                            let _ =
                                tx.send(Err(CdpError::msg("parallel session slot limit reached")));
                            return;
                        }
                        match self.target_id_to_slot.get(&target_id).copied() {
                            Some(slot) => {
                                self.send_session(slot, RouterToSession::SetInitiator(tx));
                            }
                            None => {
                                if self.sessions.len() == usize::from(u16::MAX) {
                                    let _ = tx.send(Err(CdpError::msg(
                                        "parallel session slot limit reached",
                                    )));
                                    return;
                                }
                                // Race: real Chrome may deliver the
                                // `Target.createTarget` response before the
                                // `Target.targetCreated` event. Park the
                                // initiator until the event lands and
                                // resolve it from `on_target_created`.
                                self.pending_initiator_ts
                                    .insert(target_id.clone(), Instant::now());
                                self.pending_initiators.insert(target_id, tx);
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err));
                    }
                }
            }
        }
    }

    fn on_event(&mut self, event: CdpEventMessage) {
        // Fast path: events with a sessionId belong to a session task.
        if let Some(sid) = event.session_id.as_deref() {
            if let Some(slot) = self.session_id_to_slot.get(sid).copied() {
                self.send_session(slot, RouterToSession::Event(Box::new(event)));
                return;
            }
            // Session not yet registered (race during attach): drop the event.
            return;
        }

        // Browser-level events. Only Target.* matters for Phase 2.
        match &event.params {
            CdpEvent::TargetTargetCreated(ev) => {
                self.on_target_created(ev.target_info.clone());
            }
            CdpEvent::TargetAttachedToTarget(ev) => {
                self.on_attached_to_target((**ev).clone());
            }
            CdpEvent::TargetTargetDestroyed(ev) => {
                self.on_target_gone(&ev.target_id);
            }
            CdpEvent::TargetTargetCrashed(ev) => {
                self.on_target_gone(&ev.target_id);
            }
            CdpEvent::TargetDetachedFromTarget(ev) => {
                let sid: &str = ev.session_id.as_ref();
                if let Some(slot) = self.session_id_to_slot.get(sid).copied() {
                    self.stop_session(slot);
                }
            }
            _ => {}
        }

        // Fan out the browser-level event to subscribers (`Browser::event_listener`).
        let CdpEventMessage { params, method, .. } = event;
        chromiumoxide_cdp::consume_event!(match params {
            |ev| self.event_listeners.start_send(ev),
            |json| { let _ = self.event_listeners.try_send_custom(&method, json); }
        });
    }

    /// Cancel the session independently of inbox capacity. Join it before
    /// reclaiming its slot and routing entries.
    fn on_target_gone(&mut self, target_id: &TargetId) {
        if let Some(slot) = self.target_id_to_slot.get(target_id).copied() {
            self.stop_session(slot);
        }
    }

    fn on_target_created(&mut self, info: TargetInfo) {
        // Skip duplicates.
        if self.target_id_to_slot.contains_key(&info.target_id) {
            return;
        }

        // Browser-context filtering: in Phase 2 we only support the default
        // context. The bench/smoke don't use multiple contexts.
        let target_id = info.target_id.clone();
        let Some(slot) = self.alloc_slot() else {
            // Ignore only this new target. Existing pages keep their slots
            // and continue serving commands. Remember event-before-response
            // rejections for one request timeout, with a hard memory bound.
            if let Some(tx) = self.pending_initiators.remove(&target_id) {
                self.pending_initiator_ts.remove(&target_id);
                let _ = tx.send(Err(CdpError::msg("parallel session slot limit reached")));
            } else if self.rejected_targets.len() < usize::from(u16::MAX) {
                self.rejected_targets.insert(target_id, Instant::now());
            }
            return;
        };
        self.rejected_targets.remove(&target_id);
        let browser_ctx = info
            .browser_context_id
            .clone()
            .map(BrowserContext::from)
            .unwrap_or_else(|| self.default_browser_context.clone());

        let page_wake = Arc::new(Notify::new());
        let target = Target::new(
            info,
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
                page_wake: Some(page_wake.clone()),
                page_channel_capacity: self.config.page_channel_capacity,
            },
            browser_ctx,
        );

        let (router_tx, router_rx) = mpsc::channel(SESSION_INBOX_CAPACITY);
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();

        let session = SessionTask::new(
            slot,
            target,
            page_wake.clone(),
            router_rx,
            self.ws_tx.clone(),
            self.session_lifecycle_tx.clone(),
            self.ids.clone(),
            self.config.request_timeout,
            shutdown_rx,
        );

        self.target_id_to_slot.insert(target_id.clone(), slot);
        let parked_initiator = self.pending_initiators.remove(&target_id);
        self.pending_initiator_ts.remove(&target_id);
        self.sessions.insert(
            slot,
            SessionEntry {
                inbox: router_tx.clone(),
                target_id,
                session_id: None,
                page_wake,
                shutdown: Some(shutdown),
                overflow: VecDeque::new(),
                delivery_abort: None,
            },
        );
        let task = self.session_tasks.spawn(async move {
            session.run().await;
            slot
        });
        self.session_task_slots.insert(task.id(), slot);
        // If the createTarget response already landed before this event,
        // hand its initiator off now that the SessionTask is alive.
        if let Some(tx) = parked_initiator {
            self.send_session(slot, RouterToSession::SetInitiator(tx));
        }
    }

    fn on_attached_to_target(&mut self, ev: EventAttachedToTarget) {
        let target_id = &ev.target_info.target_id;
        if let Some(slot) = self.target_id_to_slot.get(target_id).copied() {
            // Eagerly populate session_id_to_slot so the very first
            // session-keyed event after attach finds its task.
            let sid_str: String = ev.session_id.clone().into();
            self.session_id_to_slot.insert(sid_str.clone(), slot);
            if let Some(entry) = self.sessions.get_mut(&slot) {
                entry.session_id = Some(sid_str);
            }
        }
    }

    fn on_browser_message(&mut self, msg: HandlerMessage) {
        match msg {
            HandlerMessage::CreatePage(params, tx) => {
                self.create_page(params, tx);
            }
            HandlerMessage::Command(cmd) => {
                self.dispatch_browser_command(cmd);
            }
            HandlerMessage::CloseBrowser(tx) => {
                self.close_browser(tx);
            }
            HandlerMessage::GetPages(tx) => {
                let _ = tx.send(Vec::new());
            }
            HandlerMessage::GetPage(_, tx) => {
                let _ = tx.send(None);
            }
            HandlerMessage::FetchTargets(tx) => {
                let _ = tx.send(Ok(Vec::new()));
            }
            HandlerMessage::InsertContext(_) | HandlerMessage::DisposeContext(_) => {}
            HandlerMessage::AddEventListener(req) => {
                self.event_listeners.add_listener(req);
            }
        }
    }

    fn close_browser(&mut self, tx: OneshotSender<Result<CloseReturns>>) {
        let call_id = self.ids.alloc(0);
        let method: MethodId = CloseParams::IDENTIFIER.into();
        let call = MethodCall {
            id: call_id,
            method: method.clone(),
            session_id: None,
            params: serde_json::json!({}),
        };
        if let Err(err) = super::try_submit(&self.ws_tx, call) {
            // Explicit shutdown remains serviceable with a saturated writer,
            // but must not claim that the remote browser was closed.
            self.closing = true;
            let _ = tx.send(Err(err));
            return;
        }
        self.pending.insert(
            call_id,
            (RouterPending::CloseBrowser(tx), method, Instant::now()),
        );
    }

    fn create_page(&mut self, params: CreateTargetParams, tx: OneshotSender<Result<Page>>) {
        if self.sessions.len() == usize::from(u16::MAX) {
            let _ = tx.send(Err(CdpError::msg("parallel session slot limit reached")));
            return;
        }
        let about_blank = params.url == "about:blank";
        let http_check =
            !about_blank && (params.url.starts_with("http") || params.url.starts_with("file://"));
        if !about_blank && !http_check {
            let _ = tx.send(Err(CdpError::NotFound));
            return;
        }

        let method = params.identifier();
        let value = match serde_json::to_value(params) {
            Ok(v) => v,
            Err(err) => {
                let _ = tx.send(Err(err.into()));
                return;
            }
        };

        let call_id = self.ids.alloc(0);
        let call = MethodCall {
            id: call_id,
            method: method.clone(),
            session_id: None,
            params: value,
        };
        if let Err(err) = super::try_submit(&self.ws_tx, call) {
            let _ = tx.send(Err(err));
            return;
        }
        self.pending.insert(
            call_id,
            (RouterPending::CreateTarget(tx), method, Instant::now()),
        );
    }

    fn dispatch_browser_command(&mut self, cmd: CommandMessage) {
        // Browser::execute owns its response even when session-keyed. Keep
        // its id on slot 0 and preserve the sessionId on the wire.
        let call_id = self.ids.alloc(0);
        let method = cmd.method.clone();
        let (req, sender) = cmd.split();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        if let Err(err) = super::try_submit(&self.ws_tx, call) {
            let _ = sender.send(Err(err));
            return;
        }
        self.pending.insert(
            call_id,
            (
                RouterPending::BrowserCommand(sender),
                method,
                Instant::now(),
            ),
        );
    }

    fn stop_session(&mut self, slot: u16) {
        if let Some(entry) = self.sessions.get_mut(&slot) {
            entry.stop();
        }
    }

    /// Preserve FIFO across the inbox and overflow. Waiting for capacity is
    /// polled by the router's select, never awaited inside event dispatch.
    fn send_session(&mut self, slot: u16, mut msg: RouterToSession) {
        if let Some(entry) = self
            .sessions
            .get_mut(&slot)
            .filter(|e| e.shutdown.is_some())
        {
            if entry.overflow.is_empty() {
                match entry.inbox.try_send(msg) {
                    Ok(()) => return,
                    Err(mpsc::error::TrySendError::Full(unsent)) => msg = unsent,
                    Err(mpsc::error::TrySendError::Closed(unsent)) => {
                        Self::reject_delivery(unsent);
                        entry.stop();
                        return;
                    }
                }
            }
            let deadline = tokio::time::Instant::now()
                .checked_add(self.config.request_timeout.min(Duration::from_secs(30)));
            if let Some(deadline) =
                deadline.filter(|_| entry.overflow.len() < SESSION_OVERFLOW_CAPACITY)
            {
                entry.overflow.push_back((msg, deadline));
                self.schedule_delivery(slot);
                return;
            }
        }
        // Persistent overload is terminal for this session: its Drop resolves
        // accepted requests, rather than silently losing a CDP response/event.
        Self::reject_delivery(msg);
        self.stop_session(slot);
    }

    fn reject_delivery(msg: RouterToSession) {
        if let RouterToSession::SetInitiator(tx) = msg {
            let _ = tx.send(Err(CdpError::msg("session delivery full or closed")));
        }
    }

    fn schedule_delivery(&mut self, slot: u16) {
        if let Some(entry) = self.sessions.get_mut(&slot) {
            if entry.delivery_abort.is_some() || entry.shutdown.is_none() {
                return;
            }
            if let Some((_, deadline)) = entry.overflow.front() {
                let deadline = *deadline;
                let inbox = entry.inbox.clone();
                let (abort, registration) = AbortHandle::new_pair();
                entry.delivery_abort = Some(abort);
                self.deliveries.push(Abortable::new(
                    Box::pin(async move {
                        let result =
                            match tokio::time::timeout_at(deadline, inbox.reserve_owned()).await {
                                Ok(Ok(permit)) => Ok(permit),
                                Ok(Err(_)) => Err(CdpError::msg("session inbox closed")),
                                Err(_) => Err(CdpError::Timeout),
                            };
                        (slot, result)
                    }),
                    registration,
                ));
            }
        }
    }

    fn on_delivery(
        &mut self,
        delivery: std::result::Result<(u16, Result<mpsc::OwnedPermit<RouterToSession>>), Aborted>,
    ) {
        let Ok((slot, result)) = delivery else { return };
        if let Some(entry) = self.sessions.get_mut(&slot) {
            entry.delivery_abort = None;
            match result {
                Ok(permit) if entry.shutdown.is_some() => {
                    if let Some((msg, deadline)) = entry.overflow.pop_front() {
                        // timeout_at polls the inner future first: enforce the
                        // deadline even when a permit and timer are both ready.
                        if tokio::time::Instant::now() < deadline {
                            permit.send(msg);
                        } else {
                            Self::reject_delivery(msg);
                            entry.stop();
                        }
                    }
                }
                _ => entry.stop(),
            }
            self.schedule_delivery(slot);
        }
    }

    fn alloc_slot(&mut self) -> Option<u16> {
        if self.sessions.len() == usize::from(u16::MAX) {
            return None;
        }
        for _ in 0..u16::MAX {
            let slot = self.next_slot;
            self.next_slot = if slot == u16::MAX { 1 } else { slot + 1 };
            if !self.sessions.contains_key(&slot) {
                return Some(slot);
            }
        }
        None
    }

    fn on_lifecycle(&mut self, msg: SessionToRouter) {
        let SessionToRouter::SessionAttached { slot, session_id } = msg;
        if let Some(entry) = self.sessions.get_mut(&slot) {
            entry.session_id = Some(session_id.clone());
            self.session_id_to_slot.insert(session_id, slot);
        }
    }

    /// Time out any router-owned (slot 0) pending command and any parked
    /// CreateTarget initiator older than `request_timeout`. Mirrors
    /// `Handler::evict_timed_out_commands` for the parallel split.
    fn evict_router_pending(&mut self, now: Instant, request_timeout: Duration) {
        let deadline = match now.checked_sub(request_timeout) {
            Some(d) => d,
            None => return,
        };
        self.rejected_targets
            .retain(|_, timestamp| *timestamp >= deadline);
        let stale: Vec<CallId> = self
            .pending
            .iter()
            .filter(|(_, (_, _, ts))| *ts < deadline)
            .map(|(k, _)| *k)
            .collect();
        for id in stale {
            if let Some((pending, _, _)) = self.pending.remove(&id) {
                match pending {
                    RouterPending::Boot => {}
                    RouterPending::BrowserCommand(tx) => {
                        let _ = tx.send(Err(CdpError::Timeout));
                    }
                    RouterPending::CloseBrowser(tx) => {
                        self.closing = true;
                        let _ = tx.send(Err(CdpError::Timeout));
                    }
                    RouterPending::CreateTarget(tx) => {
                        let _ = tx.send(Err(CdpError::Timeout));
                    }
                }
            }
        }
        // Time out parked initiators (createTarget responded but the
        // matching targetCreated event never arrived).
        let stale_targets: Vec<TargetId> = self
            .pending_initiator_ts
            .iter()
            .filter(|(_, ts)| **ts < deadline)
            .map(|(k, _)| k.clone())
            .collect();
        for tid in stale_targets {
            self.pending_initiator_ts.remove(&tid);
            if let Some(tx) = self.pending_initiators.remove(&tid) {
                let _ = tx.send(Err(CdpError::Timeout));
            }
        }
    }

    fn remove_session(&mut self, slot: u16) {
        // Called only after join: the old task can no longer allocate ids or
        // enqueue lifecycle messages. Consume its last hints before reuse.
        while let Ok(msg) = self.session_lifecycle_rx.try_recv() {
            self.on_lifecycle(msg);
        }
        if let Some(entry) = self.sessions.remove(&slot) {
            self.target_id_to_slot.remove(&entry.target_id);
            // Attach events and explicit attach responses can register more
            // than one session id for a target. Remove every alias before
            // reusing the slot so late events cannot reach its new owner.
            self.session_id_to_slot.retain(|_, owner| *owner != slot);
            // Drop any in-flight routing entries pointing at this slot so
            // the DashMap doesn't grow unbounded under heavy session churn.
            self.ids.drop_slot(slot);
        }
    }

    #[allow(dead_code)]
    pub fn request_timeout(&self) -> Duration {
        self.config.request_timeout
    }
}

impl Drop for Router {
    fn drop(&mut self) {
        // JoinSet aborts the tasks; SessionTask::drop resolves their callers.
        for (_, (pending, _, _)) in self.pending.drain() {
            match pending {
                RouterPending::BrowserCommand(tx) => {
                    let _ = tx.send(Err(CdpError::msg("parallel handler stopped")));
                }
                RouterPending::CloseBrowser(tx) => {
                    let _ = tx.send(Err(CdpError::msg(
                        "parallel handler stopped before close response",
                    )));
                }
                RouterPending::CreateTarget(tx) => {
                    let _ = tx.send(Err(CdpError::msg("parallel handler stopped")));
                }
                RouterPending::Boot => {}
            }
        }
        for (_, tx) in self.pending_initiators.drain() {
            let _ = tx.send(Err(CdpError::msg("parallel handler stopped")));
        }
    }
}

#[cfg(test)]
mod backpressure_tests {
    use super::*;
    use tokio::sync::oneshot;

    struct Fixture {
        router: Router,
        browser: mpsc::Sender<HandlerMessage>,
        _wire: mpsc::Sender<Result<Box<Message<CdpEventMessage>>>>,
        writer: mpsc::Receiver<MethodCall>,
    }

    fn fixture() -> Fixture {
        let (browser, rx) = mpsc::channel(8);
        let (wire, read_rx) = mpsc::channel(8);
        let (ws_tx, writer) = mpsc::channel(1);
        let config = HandlerConfig {
            request_timeout: Duration::from_millis(10),
            ..Default::default()
        };
        let router = Router::new(
            config,
            Default::default(),
            rx,
            WsReader::from_receiver(read_rx),
            ws_tx,
            CallId::new(0),
            "Target.setDiscoverTargets".into(),
            1,
        );
        Fixture {
            router,
            browser,
            _wire: wire,
            writer,
        }
    }

    fn command() -> (CommandMessage, oneshot::Receiver<Result<Response>>) {
        let (sender, rx) = oneshot::channel();
        (
            CommandMessage {
                method: "Test.command".into(),
                session_id: None,
                params: serde_json::json!({}),
                sender,
            },
            rx,
        )
    }

    fn entry(
        id: &str,
    ) -> (
        SessionEntry,
        mpsc::Receiver<RouterToSession>,
        oneshot::Receiver<()>,
    ) {
        let (inbox, rx) = mpsc::channel(1);
        let (shutdown, canceled) = oneshot::channel();
        (
            SessionEntry {
                inbox,
                target_id: id.to_owned().into(),
                session_id: None,
                page_wake: Arc::new(Notify::new()),
                shutdown: Some(shutdown),
                overflow: VecDeque::new(),
                delivery_abort: None,
            },
            rx,
            canceled,
        )
    }

    async fn acknowledge_close(
        writer: &mut mpsc::Receiver<MethodCall>,
        wire: &mpsc::Sender<Result<Box<Message<CdpEventMessage>>>>,
    ) {
        let call = writer.recv().await.unwrap();
        assert_eq!(call.method.as_ref(), CloseParams::IDENTIFIER);
        assert!(call.session_id.is_none());
        wire.send(Ok(Box::new(Message::Response(
            serde_json::from_value(serde_json::json!({"id": call.id, "result": {}})).unwrap(),
        ))))
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn panicking_session_does_not_stop_other_sessions_or_browser_commands() {
        let mut f = fixture();
        let (failed, inbox, canceled) = entry("failed");
        let info = serde_json::from_value(serde_json::json!({
            "targetId": "failed", "type": "browser", "title": "", "url": "",
            "attached": false, "canAccessOpener": false
        }))
        .unwrap();
        let mut target = Target::new(info, TargetConfig::default(), Default::default());
        let (initiator, failed_request) = oneshot::channel();
        target.set_initiator(initiator);
        let (_shutdown, shutdown_rx) = oneshot::channel();
        let session = SessionTask::new(
            1,
            target,
            failed.page_wake.clone(),
            inbox,
            f.router.ws_tx.clone(),
            f.router.session_lifecycle_tx.clone(),
            f.router.ids.clone(),
            f.router.config.request_timeout,
            shutdown_rx,
        );
        f.router.sessions.insert(1, failed);
        f.router
            .target_id_to_slot
            .insert(TargetId::new("failed"), 1);
        f.router.on_lifecycle(SessionToRouter::SessionAttached {
            slot: 1,
            session_id: "failed-session".into(),
        });
        let ids = f.router.ids.clone();
        let failed_id = ids.alloc(1);
        let task = f.router.session_tasks.spawn(async move {
            let _session = session;
            panic!("injected session failure");
        });
        f.router.session_task_slots.insert(task.id(), 1);

        let (live, mut inbox, mut shutdown) = entry("live");
        let (delivered, mut messages) = mpsc::unbounded_channel();
        f.router.sessions.insert(2, live);
        let task = f.router.session_tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown => break,
                    message = inbox.recv() => {
                        let Some(message) = message else { break };
                        delivered.send(message).unwrap();
                    }
                }
            }
            2
        });
        f.router.session_task_slots.insert(task.id(), 2);
        let live_id = ids.alloc(2);
        let driver = tokio::spawn(f.router.run());
        tokio::time::timeout(Duration::from_secs(1), async {
            assert!(failed_request.await.unwrap().is_err(), "Drop must resolve the failed caller");
            assert!(canceled.await.is_err(), "failed entry must be removed after join");
            assert_eq!(ids.take_route(failed_id), None);
            assert!(!driver.is_finished());

            f._wire.send(Ok(Box::new(Message::Response(
                serde_json::from_value(serde_json::json!({"id": live_id, "result": {}})).unwrap(),
            )))).await.unwrap();
            assert!(matches!(messages.recv().await, Some(RouterToSession::Response(id, _, _)) if id == live_id));

            let (cmd, result) = command();
            f.browser.send(HandlerMessage::Command(cmd)).await.unwrap();
            let call = f.writer.recv().await.unwrap();
            f._wire.send(Ok(Box::new(Message::Response(
                serde_json::from_value(serde_json::json!({"id": call.id, "result": {}})).unwrap(),
            )))).await.unwrap();
            assert!(result.await.unwrap().is_ok());

            let (close, closed) = oneshot::channel();
            f.browser.send(HandlerMessage::CloseBrowser(close)).await.unwrap();
            acknowledge_close(&mut f.writer, &f._wire).await;
            closed.await.unwrap().unwrap();
            driver.await.unwrap().unwrap();
        }).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn close_waits_for_cdp_response_and_propagates_protocol_errors() {
        for success in [true, false] {
            let mut f = fixture();
            let (live, mut inbox, mut canceled) = entry("live-during-close");
            f.router.sessions.insert(1, live);
            let accepted_id = f.router.ids.alloc(1);
            let (close, mut closed) = oneshot::channel();
            f.browser
                .try_send(HandlerMessage::CloseBrowser(close))
                .unwrap();
            let driver = tokio::spawn(f.router.run());
            let call = f.writer.recv().await.unwrap();
            assert_eq!(call.method.as_ref(), CloseParams::IDENTIFIER);
            assert!(call.session_id.is_none());
            assert!(matches!(
                closed.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(
                !driver.is_finished(),
                "driver must keep reading the close response"
            );
            f._wire
                .send(Ok(Box::new(Message::Response(
                    serde_json::from_value(serde_json::json!({"id": accepted_id, "result": {}}))
                        .unwrap(),
                ))))
                .await
                .unwrap();
            assert!(
                matches!(inbox.recv().await, Some(RouterToSession::Response(id, _, _)) if id == accepted_id)
            );
            assert!(matches!(
                canceled.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            let response = if success {
                serde_json::json!({"id": call.id, "result": {}})
            } else {
                serde_json::json!({"id": call.id, "error": {"code": -32000, "message": "close refused"}})
            };
            f._wire
                .send(Ok(Box::new(Message::Response(
                    serde_json::from_value(response).unwrap(),
                ))))
                .await
                .unwrap();
            assert_eq!(closed.await.unwrap().is_ok(), success);
            tokio::time::timeout(Duration::from_secs(1), driver)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn close_timeout_stops_driver_without_claiming_remote_success() {
        let mut f = fixture();
        let (close, closed) = oneshot::channel();
        f.router
            .on_browser_message(HandlerMessage::CloseBrowser(close));
        let call = f.writer.try_recv().unwrap();
        assert_eq!(call.method.as_ref(), CloseParams::IDENTIFIER);
        assert!(!f.router.closing);
        for (_, _, ts) in f.router.pending.values_mut() {
            *ts = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), f.router.run())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(closed.await.unwrap(), Err(CdpError::Timeout)));
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_writer_does_not_block_eviction_or_shutdown() {
        let mut f = fixture();
        let (accepted, accepted_rx) = command();
        f.router.dispatch_browser_command(accepted);
        assert_eq!(f.router.ws_tx.capacity(), 0);
        let (rejected, rejected_rx) = command();
        f.router.dispatch_browser_command(rejected);
        assert!(rejected_rx
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("full"));
        // std::Instant is deliberately aged: Tokio's paused clock only drives
        // the wakeup, so this assertion is independent of wall-clock sleeps.
        for (_, _, timestamp) in f.router.pending.values_mut() {
            *timestamp = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        }
        let driver = tokio::spawn(f.router.run());
        let response = tokio::time::timeout(Duration::from_secs(1), accepted_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(response, Err(CdpError::Timeout)));
        let (close, closed) = oneshot::channel();
        f.browser
            .send(HandlerMessage::CloseBrowser(close))
            .await
            .unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), closed)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("full"));
        tokio::time::timeout(Duration::from_secs(1), driver)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(f.writer.try_recv().is_ok(), "accepted command was retained");
    }

    #[tokio::test(start_paused = true)]
    async fn expired_session_overflow_cancels_out_of_band_and_keeps_slot_reserved() {
        let mut f = fixture();
        let (entry, mut inbox, mut canceled) = entry("old");
        f.router.sessions.insert(1, entry);
        let (first, first_rx) = oneshot::channel();
        f.router
            .send_session(1, RouterToSession::SetInitiator(first));
        let (second, second_rx) = oneshot::channel();
        f.router
            .send_session(1, RouterToSession::SetInitiator(second));
        assert!(matches!(
            canceled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(f.router.sessions[&1].overflow.len(), 1);
        tokio::time::advance(Duration::from_millis(11)).await;
        let delivery = f.router.deliveries.next().await.unwrap();
        f.router.on_delivery(delivery);
        assert!(second_rx.await.unwrap().is_err());
        assert!(matches!(
            canceled.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        f.router.next_slot = 1;
        assert_eq!(
            f.router.alloc_slot(),
            Some(2),
            "task must exit before slot reuse"
        );
        // The accepted envelope wasn't overwritten by Shutdown.
        assert!(matches!(
            inbox.try_recv(),
            Ok(RouterToSession::SetInitiator(_))
        ));
        assert!(first_rx.await.is_err());
    }

    #[tokio::test]
    async fn routes_response_to_the_correct_session_without_dropping_it() {
        let mut f = fixture();
        let (entry, mut inbox, _canceled) = entry("one");
        f.router.sessions.insert(1, entry);
        let id = f.router.ids.alloc(1);
        let response: Response = serde_json::from_value(
            serde_json::json!({"id": id, "result": {"ok": true, "sessionId": "attached"}}),
        )
        .unwrap();
        f.router.on_response(response);
        assert_eq!(
            f.router.session_id_to_slot.get("attached"),
            Some(&1),
            "response-only attach must register before the next wire event"
        );
        let RouterToSession::Response(got, response, _) = inbox.try_recv().unwrap() else {
            panic!("response expected")
        };
        assert_eq!(got, id);
        assert_eq!(response.result.unwrap()["ok"], true);
        assert_eq!(f.router.ids.take_route(id), None);
    }

    fn data_event(index: usize) -> CdpEventMessage {
        serde_json::from_value(serde_json::json!({
            "method": "Network.dataReceived", "sessionId": "burst-session",
            "params": {"requestId": index.to_string(), "timestamp": index,
                "dataLength": 1024, "encodedDataLength": 512}
        }))
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_session_retains_4096_events_and_accepted_response_in_order() {
        let mut f = fixture();
        let (mut session, _unused, mut canceled) = entry("burst");
        let (inbox_tx, mut inbox) = mpsc::channel(SESSION_INBOX_CAPACITY);
        session.inbox = inbox_tx;
        f.router.sessions.insert(1, session);
        f.router.on_lifecycle(SessionToRouter::SessionAttached {
            slot: 1,
            session_id: "burst-session".into(),
        });
        // Deliberately do not poll the consumer while dispatching a realistic
        // network burst larger than the old 1024-entry inbox.
        for index in 0..4096 {
            f.router.on_event(data_event(index));
        }
        let id = f.router.ids.alloc(1);
        f.router.on_response(
            serde_json::from_value(serde_json::json!({
                "id": id, "result": {"accepted": true}
            }))
            .unwrap(),
        );
        assert_eq!(inbox.len(), SESSION_INBOX_CAPACITY);
        assert_eq!(
            f.router.sessions[&1].overflow.len(),
            4097 - SESSION_INBOX_CAPACITY
        );
        assert_eq!(f.router.deliveries.len(), 1);
        assert!(matches!(
            canceled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        let driver = tokio::spawn(f.router.run());
        tokio::time::timeout(Duration::from_secs(1), async {
            for index in 0..4096 {
                let RouterToSession::Event(event) = inbox.recv().await.unwrap() else {
                    panic!("event reordered or lost");
                };
                let CdpEvent::NetworkDataReceived(event) = event.params else {
                    panic!("network event expected");
                };
                assert_eq!(event.request_id.as_ref(), index.to_string());
            }
            let RouterToSession::Response(got, response, _) = inbox.recv().await.unwrap() else {
                panic!("accepted response lost");
            };
            assert_eq!(got, id);
            assert_eq!(response.result.unwrap()["accepted"], true);
        })
        .await
        .unwrap();
        assert!(matches!(
            canceled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let (close, closed) = oneshot::channel();
        f.browser
            .send(HandlerMessage::CloseBrowser(close))
            .await
            .unwrap();
        acknowledge_close(&mut f.writer, &f._wire).await;
        closed.await.unwrap().unwrap();
        driver.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_overflow_does_not_suspend_router_eviction_or_shutdown() {
        let mut f = fixture();
        let (session, _inbox, canceled) = entry("blocked");
        f.router.sessions.insert(1, session);
        f.router
            .send_session(1, RouterToSession::Event(Box::new(data_event(0))));
        let (initiator, failed) = oneshot::channel();
        f.router
            .send_session(1, RouterToSession::SetInitiator(initiator));
        let (cmd, expired) = command();
        f.router.dispatch_browser_command(cmd);
        for (_, _, ts) in f.router.pending.values_mut() {
            *ts = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        }
        let driver = tokio::spawn(f.router.run());
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), expired)
                .await
                .unwrap()
                .unwrap(),
            Err(CdpError::Timeout)
        ));
        assert!(tokio::time::timeout(Duration::from_secs(1), failed)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(canceled.await.is_err());
        let (close, closed) = oneshot::channel();
        f.browser
            .send(HandlerMessage::CloseBrowser(close))
            .await
            .unwrap();
        assert!(closed
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("full"));
        tokio::time::timeout(Duration::from_secs(1), driver)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_bypasses_pending_overflow_delivery() {
        let mut f = fixture();
        let (session, _inbox, canceled) = entry("blocked");
        f.router.sessions.insert(1, session);
        f.router
            .send_session(1, RouterToSession::Event(Box::new(data_event(0))));
        let (initiator, failed) = oneshot::channel();
        f.router
            .send_session(1, RouterToSession::SetInitiator(initiator));
        let (close, closed) = oneshot::channel();
        f.browser
            .try_send(HandlerMessage::CloseBrowser(close))
            .unwrap();
        // The delivery timeout is 10ms; shutdown must not wait for it.
        let driver = tokio::spawn(f.router.run());
        tokio::time::timeout(Duration::from_millis(1), async {
            acknowledge_close(&mut f.writer, &f._wire).await;
            driver.await.unwrap().unwrap();
        })
        .await
        .unwrap();
        closed.await.unwrap().unwrap();
        assert!(failed.await.unwrap().is_err());
        assert!(canceled.await.is_err());
    }

    #[tokio::test]
    async fn overflow_capacity_is_bounded_and_cancellation_survives_slot_reuse() {
        let mut f = fixture();
        let (session, _inbox, mut canceled) = entry("blocked");
        f.router.sessions.insert(1, session);
        for index in 0..=SESSION_OVERFLOW_CAPACITY {
            f.router
                .send_session(1, RouterToSession::Event(Box::new(data_event(index))));
        }
        assert_eq!(
            f.router.sessions[&1].overflow.len(),
            SESSION_OVERFLOW_CAPACITY
        );
        assert!(matches!(
            canceled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let (initiator, rejected) = oneshot::channel();
        f.router
            .send_session(1, RouterToSession::SetInitiator(initiator));
        assert!(rejected.await.unwrap().is_err());
        assert!(canceled.await.is_err());
        assert!(f.router.sessions[&1].overflow.is_empty());
        f.router.remove_session(1);
        let (replacement, mut inbox, mut canceled) = entry("replacement");
        f.router.sessions.insert(1, replacement);
        let delivery = f.router.deliveries.next().await.unwrap();
        assert!(
            delivery.is_err(),
            "old delivery must be canceled before slot reuse"
        );
        f.router.on_delivery(delivery);
        assert!(inbox.try_recv().is_err());
        assert!(matches!(
            canceled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn slots_survive_more_than_u16_churn_and_skip_live_sessions() {
        let mut f = fixture();
        let (live, _inbox, _canceled) = entry("live");
        f.router.sessions.insert(1, live);
        for index in 0..70_000 {
            let slot = f.router.alloc_slot().unwrap();
            assert_ne!(slot, 0);
            assert_ne!(slot, 1);
            let (temporary, _rx, _canceled) = entry("temporary");
            f.router.sessions.insert(slot, temporary);
            let id = f.router.ids.alloc(slot);
            f.router.remove_session(slot);
            assert_eq!(f.router.ids.take_route(id), None, "stale route at {index}");
        }
        assert_eq!(f.router.sessions.len(), 1);
    }

    #[tokio::test]
    async fn stale_attachment_is_drained_before_slot_reuse() {
        let mut f = fixture();
        let (old, _rx, _canceled) = entry("old");
        f.router.sessions.insert(1, old);
        f.router
            .session_lifecycle_tx
            .try_send(SessionToRouter::SessionAttached {
                slot: 1,
                session_id: "old-session".into(),
            })
            .unwrap();
        f.router.remove_session(1);
        f.router.next_slot = 1;
        assert_eq!(f.router.alloc_slot(), Some(1));
        assert!(!f.router.session_id_to_slot.contains_key("old-session"));
        assert!(f.router.session_lifecycle_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn slot_reuse_removes_all_previous_session_id_aliases() {
        let mut f = fixture();
        let (old, _rx, _canceled) = entry("old");
        f.router.sessions.insert(1, old);
        for session_id in ["auto-attached", "explicitly-attached"] {
            f.router.on_lifecycle(SessionToRouter::SessionAttached {
                slot: 1,
                session_id: session_id.into(),
            });
        }
        f.router
            .session_id_to_slot
            .insert("other-session".into(), 2);
        f.router.remove_session(1);
        f.router.next_slot = 1;
        assert_eq!(f.router.alloc_slot(), Some(1));
        let (replacement, mut inbox, _canceled) = entry("replacement");
        f.router.sessions.insert(1, replacement);
        for session_id in ["auto-attached", "explicitly-attached"] {
            let event = serde_json::from_value(serde_json::json!({
                "method": "Runtime.executionContextsCleared",
                "params": {},
                "sessionId": session_id
            }))
            .unwrap();
            f.router.on_event(event);
        }
        assert!(matches!(
            inbox.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(f.router.session_id_to_slot.len(), 1);
        assert_eq!(f.router.session_id_to_slot.get("other-session"), Some(&2));
    }

    #[tokio::test]
    async fn all_live_slots_refuse_only_new_targets_and_existing_routes_still_work() {
        let mut f = fixture();
        let (template, _rx, _canceled) = entry("occupied");
        for slot in 1..=u16::MAX {
            f.router.sessions.insert(
                slot,
                SessionEntry {
                    inbox: template.inbox.clone(),
                    target_id: template.target_id.clone(),
                    session_id: None,
                    page_wake: template.page_wake.clone(),
                    shutdown: None,
                    overflow: VecDeque::new(),
                    delivery_abort: None,
                },
            );
        }
        let (live, mut inbox, mut canceled) = entry("live");
        f.router.sessions.insert(1, live);
        assert_eq!(f.router.alloc_slot(), None);
        let (new_page, rejected) = oneshot::channel();
        f.router
            .create_page(CreateTargetParams::new("about:blank"), new_page);
        assert!(rejected
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("slot limit"));
        assert!(
            f.writer.try_recv().is_err(),
            "refused creation must not reach the wire"
        );

        // A creation accepted before exhaustion can produce response/event in
        // either order. Only its initiator fails; the browser keeps running.
        for event_first in [false, true] {
            let target_id = format!("overflow-{event_first}");
            let (tx, rejected) = oneshot::channel();
            let call_id = f.router.ids.alloc(0);
            if event_first {
                f.router.pending.insert(
                    call_id,
                    (
                        RouterPending::CreateTarget(tx),
                        CreateTargetParams::IDENTIFIER.into(),
                        Instant::now(),
                    ),
                );
            } else {
                f.router
                    .pending_initiators
                    .insert(target_id.clone().into(), tx);
                f.router
                    .pending_initiator_ts
                    .insert(target_id.clone().into(), Instant::now());
            }
            let info = serde_json::from_value(serde_json::json!({
                "targetId": target_id, "type": "page", "title": "", "url": "about:blank", "attached": false, "canAccessOpener": false
            })).unwrap();
            f.router.on_target_created(info);
            if event_first {
                // Capacity becoming available between event and response must
                // not turn the explicit rejection into a parked timeout.
                f.router.remove_session(u16::MAX);
                let response = serde_json::from_value(serde_json::json!({
                    "id": call_id, "result": {"targetId": target_id}
                }))
                .unwrap();
                f.router.on_response(response);
            }
            assert!(rejected
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("slot limit"));
        }
        assert!(!f.router.closing);
        assert!(f.router.pending_initiators.is_empty());
        assert!(f.router.pending_initiator_ts.is_empty());
        let call_id = f.router.ids.alloc(1);
        f.router.on_response(
            serde_json::from_value(serde_json::json!({
                "id": call_id, "result": {"existing": true}
            }))
            .unwrap(),
        );
        assert!(
            matches!(inbox.try_recv(), Ok(RouterToSession::Response(id, _, _)) if id == call_id)
        );
        assert!(matches!(
            canceled.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let (command, result) = command();
        f.router.dispatch_browser_command(command);
        let call = f.writer.try_recv().unwrap();
        f.router.on_response(
            serde_json::from_value(serde_json::json!({"id": call.id, "result": {}})).unwrap(),
        );
        assert!(result.await.unwrap().is_ok());
    }
}
