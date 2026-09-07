//! Top-level router task for the parallel handler.
//!
//! Responsibilities:
//! * Drives the WebSocket reader and demuxes inbound CDP messages by call-id
//!   slot bits (responses) or session_id (events).
//! * Owns the slot allocator and routing tables (`target_id → slot`,
//!   `session_id → slot`, `slot → session_tx`).
//! * Handles browser-level commands (`HandlerMessage::CreatePage`,
//!   `HandlerMessage::Command` without session, etc.) on slot 0.
//! * Spawns a `SessionTask` per attached page and hands off `Target` state to
//!   it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide_cdp::cdp::browser_protocol::target::{
    CreateTargetParams, EventAttachedToTarget, TargetId, TargetInfo,
};
use chromiumoxide_cdp::cdp::CdpEvent;
use chromiumoxide_cdp::cdp::CdpEventMessage;
use chromiumoxide_types::{CallId, Message, Method, MethodCall, MethodId, Response};
use tokio::sync::mpsc;
use tokio::sync::oneshot::Sender as OneshotSender;
use tokio::sync::Notify;

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

/// Capacity of the SessionTask → Router lifecycle channel.
const SESSION_LIFECYCLE_CAPACITY: usize = 256;

/// Pending router-side request awaiting a slot-0 response.
enum RouterPending {
    /// `Target.createTarget` initiated by `Browser::new_page`.
    CreateTarget(OneshotSender<Result<Page>>),
    /// Browser-level `Browser::execute` command (no session).
    BrowserCommand(OneshotSender<Result<Response>>),
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
    /// slot for every dispatched command in a lock-free DashMap.
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
            pending_initiators: HashMap::new(),
            pending_initiator_ts: HashMap::new(),
            session_lifecycle_rx,
            session_lifecycle_tx,
            event_listeners: EventListeners::default(),
        }
    }

    pub async fn run(mut self) -> Result<()> {
        use tokio::time::MissedTickBehavior;

        let request_timeout = self.config.request_timeout;
        let mut evict = tokio::time::interval_at(
            tokio::time::Instant::now() + request_timeout,
            request_timeout,
        );
        evict.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            // Push any queued listener events to subscribers between
            // select wakeups. Runs on every iteration regardless of arm —
            // that's where the existing serial handler does it too.
            self.event_listeners.flush();

            tokio::select! {
                biased;

                _ = evict.tick() => {
                    self.evict_router_pending(Instant::now(), request_timeout);
                }

                msg = self.ws_reader.next_message() => {
                    match msg {
                        Some(Ok(boxed)) => self.on_ws_message(*boxed).await,
                        Some(Err(_)) | None => break,
                    }
                }

                lifecycle = self.session_lifecycle_rx.recv() => {
                    match lifecycle {
                        Some(SessionToRouter::SessionAttached { slot, session_id }) => {
                            if let Some(entry) = self.sessions.get_mut(&slot) {
                                entry.session_id = Some(session_id.clone());
                            }
                            self.session_id_to_slot.insert(session_id, slot);
                        }
                        Some(SessionToRouter::Detached { slot }) => {
                            self.remove_session(slot);
                        }
                        None => {}
                    }
                }

                browser = self.from_browser.recv() => {
                    match browser {
                        Some(msg) => self.on_browser_message(msg).await,
                        None => break,
                    }
                }
            }
        }

        // Best-effort: tell each session to shut down.
        for entry in self.sessions.values() {
            let _ = entry.inbox.try_send(RouterToSession::Shutdown);
        }
        Ok(())
    }

    async fn on_ws_message(&mut self, message: Message<CdpEventMessage>) {
        match message {
            Message::Response(resp) => self.on_response(resp).await,
            Message::Event(ev) => self.on_event(ev).await,
        }
    }

    async fn on_response(&mut self, resp: Response) {
        let call_id = resp.id;
        // Resolve routing: SessionTask-owned ids land in the allocator's
        // routing map; ids without an entry are router-owned (slot 0).
        match self.ids.take_route(call_id) {
            Some(slot) => {
                if let Some(entry) = self.sessions.get(&slot) {
                    let _ = entry
                        .inbox
                        .send(RouterToSession::Response(call_id, resp, MethodId::from("")))
                        .await;
                }
            }
            None => self.handle_router_response(call_id, resp).await,
        }
    }

    async fn handle_router_response(&mut self, call_id: CallId, resp: Response) {
        let Some((pending, method, _ts)) = self.pending.remove(&call_id) else {
            return;
        };
        match pending {
            RouterPending::Boot => {}
            RouterPending::BrowserCommand(tx) => {
                let _ = tx.send(Ok(resp));
            }
            RouterPending::CreateTarget(tx) => {
                match to_command_response::<CreateTargetParams>(resp, method) {
                    Ok(parsed) => {
                        let target_id: TargetId = parsed.result.target_id;
                        match self.target_id_to_slot.get(&target_id).copied() {
                            Some(slot) => {
                                if let Some(entry) = self.sessions.get(&slot) {
                                    let _ =
                                        entry.inbox.send(RouterToSession::SetInitiator(tx)).await;
                                } else {
                                    let _ = tx.send(Err(CdpError::NotFound));
                                }
                            }
                            None => {
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

    async fn on_event(&mut self, event: CdpEventMessage) {
        // Fast path: events with a sessionId belong to a session task.
        if let Some(sid) = event.session_id.as_deref() {
            if let Some(slot) = self.session_id_to_slot.get(sid).copied() {
                if let Some(entry) = self.sessions.get(&slot) {
                    let _ = entry
                        .inbox
                        .send(RouterToSession::Event(Box::new(event)))
                        .await;
                    return;
                }
            }
            // Session not yet registered (race during attach): drop the event.
            return;
        }

        // Browser-level events. Only Target.* matters for Phase 2.
        match &event.params {
            CdpEvent::TargetTargetCreated(ev) => {
                self.on_target_created(ev.target_info.clone()).await;
            }
            CdpEvent::TargetAttachedToTarget(ev) => {
                self.on_attached_to_target((**ev).clone()).await;
            }
            CdpEvent::TargetTargetDestroyed(ev) => {
                self.on_target_gone(&ev.target_id).await;
            }
            CdpEvent::TargetTargetCrashed(ev) => {
                self.on_target_gone(&ev.target_id).await;
            }
            CdpEvent::TargetDetachedFromTarget(ev) => {
                let sid: &str = ev.session_id.as_ref();
                if let Some(slot) = self.session_id_to_slot.get(sid).copied() {
                    if let Some(entry) = self.sessions.get(&slot) {
                        let _ = entry.inbox.send(RouterToSession::Shutdown).await;
                    }
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

    /// Send Shutdown to the SessionTask owning `target_id`. The SessionTask
    /// is responsible for cancelling its in-flight oneshots and emitting
    /// `Detached` back to the Router.
    async fn on_target_gone(&mut self, target_id: &TargetId) {
        if let Some(slot) = self.target_id_to_slot.get(target_id).copied() {
            if let Some(entry) = self.sessions.get(&slot) {
                let _ = entry.inbox.send(RouterToSession::Shutdown).await;
            }
        }
    }

    async fn on_target_created(&mut self, info: TargetInfo) {
        // Skip duplicates.
        if self.target_id_to_slot.contains_key(&info.target_id) {
            return;
        }

        // Browser-context filtering: in Phase 2 we only support the default
        // context. The bench/smoke don't use multiple contexts.
        let target_id = info.target_id.clone();
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

        let slot = self.alloc_slot();
        let (router_tx, router_rx) = mpsc::channel(SESSION_INBOX_CAPACITY);

        let session = SessionTask::new(
            slot,
            target,
            page_wake.clone(),
            router_rx,
            self.ws_tx.clone(),
            self.session_lifecycle_tx.clone(),
            self.ids.clone(),
            self.config.request_timeout,
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
            },
        );
        tokio::spawn(session.run());
        // If the createTarget response already landed before this event,
        // hand its initiator off now that the SessionTask is alive.
        if let Some(tx) = parked_initiator {
            let _ = router_tx.send(RouterToSession::SetInitiator(tx)).await;
        }
    }

    async fn on_attached_to_target(&mut self, ev: EventAttachedToTarget) {
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

    async fn on_browser_message(&mut self, msg: HandlerMessage) {
        match msg {
            HandlerMessage::CreatePage(params, tx) => {
                self.create_page(params, tx).await;
            }
            HandlerMessage::Command(cmd) => {
                self.dispatch_browser_command(cmd).await;
            }
            HandlerMessage::CloseBrowser(tx) => {
                let _ = tx.send(Ok(Default::default()));
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

    async fn create_page(&mut self, params: CreateTargetParams, tx: OneshotSender<Result<Page>>) {
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
        if self.ws_tx.send(call).await.is_err() {
            let _ = tx.send(Err(CdpError::msg("WS writer closed")));
            return;
        }
        self.pending.insert(
            call_id,
            (RouterPending::CreateTarget(tx), method, Instant::now()),
        );
    }

    async fn dispatch_browser_command(&mut self, cmd: CommandMessage) {
        // Session-keyed command from a `Page` handle: forward to that session.
        if let Some(sid) = cmd.session_id.as_ref() {
            if let Some(slot) = self.session_id_to_slot.get(sid.as_ref()).copied() {
                if let Some(entry) = self.sessions.get(&slot) {
                    // The session's Page tx side raises `page_wake`, but
                    // commands routed through the Router don't go through the
                    // Page channel — they're handed to the session via its
                    // inbox as a synthetic event-style envelope. We instead
                    // forward via the page channel so the SessionTask treats
                    // it the same as a Page::execute call.
                    //
                    // For Phase 2 minimum, route commands via the page mpsc
                    // by re-finding the page sender on the Target. Easier:
                    // just push it as a synthetic External submission via
                    // the inbox is *not* supported by the SessionTask types;
                    // so keep this branch simple by submitting directly
                    // through ws_tx and routing the response back via the
                    // session inbox.
                    let _ = entry; // placate unused warning for now.
                    self.dispatch_session_command_via_ws(slot, cmd).await;
                    return;
                }
            }
        }

        // Browser-level command (no session) — dispatch on slot 0.
        let call_id = self.ids.alloc(0);
        let method = cmd.method.clone();
        let (req, sender) = cmd.split();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        if self.ws_tx.send(call).await.is_err() {
            let _ = sender.send(Err(CdpError::msg("WS writer closed")));
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

    /// Forward a session-keyed `Browser::execute` command through ws_tx with
    /// a router-owned id (responses come back to the Router itself, which
    /// fulfills the caller's oneshot directly).
    async fn dispatch_session_command_via_ws(&mut self, _slot: u16, cmd: CommandMessage) {
        let call_id = self.ids.alloc(0);
        let method = cmd.method.clone();
        let (req, sender) = cmd.split();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        if self.ws_tx.send(call).await.is_err() {
            let _ = sender.send(Err(CdpError::msg("WS writer closed")));
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

    fn alloc_slot(&mut self) -> u16 {
        let slot = self.next_slot;
        self.next_slot = self.next_slot.checked_add(1).expect(
            "router exhausted 65535 session slots — implausibly many tabs in one Browser handle",
        );
        slot
    }

    /// Time out any router-owned (slot 0) pending command and any parked
    /// CreateTarget initiator older than `request_timeout`. Mirrors
    /// `Handler::evict_timed_out_commands` for the parallel split.
    fn evict_router_pending(&mut self, now: Instant, request_timeout: Duration) {
        let deadline = match now.checked_sub(request_timeout) {
            Some(d) => d,
            None => return,
        };
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
        if let Some(entry) = self.sessions.remove(&slot) {
            self.target_id_to_slot.remove(&entry.target_id);
            if let Some(sid) = entry.session_id {
                self.session_id_to_slot.remove(&sid);
            }
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
