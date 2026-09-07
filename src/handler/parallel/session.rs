//! Per-session task: drives one `Target` independently of all others.
//!
//! Owns the Target's mutable state (frame manager, network manager, init
//! state machine, pending command map). All state mutations happen on this
//! single task — no locks, no contention.
//!
//! Inputs:
//! * `page_wake` — wake signal raised when the Page handle pushed a
//!   `TargetMessage` (the Page's `mpsc::Sender` raises this Notify on send).
//! * `router_rx` — responses + events demuxed by the Router and handed to
//!   this slot.
//!
//! Outputs:
//! * `ws_tx` — shared write half of the WebSocket. All session tasks share
//!   this sender, but the writer task drains it serially so wire ordering
//!   is preserved.
//! * `session_to_router_tx` — lifecycle hints (session_id discovered, task
//!   exited).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide_cdp::cdp::browser_protocol::target::{AttachToTargetParams, SessionId};
use chromiumoxide_cdp::cdp::CdpEventMessage;
use chromiumoxide_types::{CallId, MethodCall, MethodId, Request, Response};
use tokio::sync::mpsc;
use tokio::sync::oneshot::Sender as OneshotSender;
use tokio::sync::Notify;

use crate::cmd::to_command_response;
use crate::error::{CdpError, Result};
use crate::handler::frame::{
    FrameRequestedNavigation, NavigationError, NavigationId, NavigationOk,
};
use crate::handler::target::{Target, TargetEvent};
use crate::handler::NavigationInProgress;

use super::ids::CallIdAllocator;
use super::types::{RouterToSession, SessionToRouter};

/// One in-flight CDP request.
enum SessionPending {
    /// Driven by the Target's init chain — response goes back to
    /// `target.on_response()`. Carries the method id so we can route the
    /// payload through `to_command_response::<X>`.
    Internal,
    /// Driven by `Page::execute()` — response unblocks the caller via this
    /// oneshot.
    External(OneshotSender<Result<Response>>),
    /// Driven by `Page::goto()` (or any navigation command). Resolution is
    /// gated by the navigation lifecycle: the immediate `Page.navigate`
    /// response sets `response`, and the subsequent `Page.lifecycleEvent`
    /// for `load`/`networkIdle` sets `navigated`. Whichever lands second
    /// fires the oneshot.
    Navigate(NavigationId),
}

pub(crate) struct SessionTask {
    /// Slot index. Used to encode call_ids that the Router can demux.
    slot: u16,
    target: Target,
    page_wake: Arc<Notify>,
    router_rx: mpsc::Receiver<RouterToSession>,
    ws_tx: mpsc::Sender<MethodCall>,
    session_to_router_tx: mpsc::Sender<SessionToRouter>,
    /// Shared call-id allocator (Router-owned, cloned in). All commands
    /// route back here via the Router's DashMap.
    ids: CallIdAllocator,
    pending: HashMap<CallId, (SessionPending, MethodId, Instant)>,
    /// In-flight navigations keyed by per-session NavigationId.
    navigations: HashMap<NavigationId, NavigationInProgress<Result<Response>>>,
    next_nav_id: usize,
    /// Set once we've reported `SessionAttached` to the router.
    session_id_reported: bool,
    /// Reserved for future per-session eviction logic.
    #[allow(dead_code)]
    request_timeout: Duration,
}

impl SessionTask {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        slot: u16,
        target: Target,
        page_wake: Arc<Notify>,
        router_rx: mpsc::Receiver<RouterToSession>,
        ws_tx: mpsc::Sender<MethodCall>,
        session_to_router_tx: mpsc::Sender<SessionToRouter>,
        ids: CallIdAllocator,
        request_timeout: Duration,
    ) -> Self {
        Self {
            slot,
            target,
            page_wake,
            router_rx,
            ws_tx,
            session_to_router_tx,
            ids,
            pending: HashMap::new(),
            navigations: HashMap::new(),
            next_nav_id: 0,
            session_id_reported: false,
            request_timeout,
        }
    }

    pub async fn run(mut self) {
        use tokio::time::MissedTickBehavior;
        // First tick: kick the init state machine immediately so the Target
        // emits `Target.attachToTarget`.
        self.drive(Instant::now()).await;

        // Per-session eviction tick. Bounds in-flight oneshot lifetime to
        // ~request_timeout when the wire goes silent (Chrome stalls, mock
        // drops the response, etc.).
        let mut evict = tokio::time::interval_at(
            tokio::time::Instant::now() + self.request_timeout,
            self.request_timeout,
        );
        evict.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;

                msg = self.router_rx.recv() => {
                    match msg {
                        Some(RouterToSession::Response(call_id, resp, method)) => {
                            self.on_response(call_id, resp, method);
                        }
                        Some(RouterToSession::Event(ev)) => {
                            self.target.on_event(*ev);
                        }
                        Some(RouterToSession::SetInitiator(tx)) => {
                            self.target.set_initiator(tx);
                        }
                        Some(RouterToSession::Shutdown) | None => {
                            break;
                        }
                    }
                }

                _ = self.page_wake.notified() => {
                    // page_rx will be drained in `drive()` below
                }

                _ = evict.tick() => {
                    self.evict_stale(Instant::now());
                }
            }

            self.drive(Instant::now()).await;
        }

        self.cancel_in_flight();
        // try_send (non-awaiting): a full lifecycle channel only happens
        // if the Router has stalled — and if it has stalled while we are
        // trying to send `Detached`, awaiting here would deadlock against
        // Router waiting on `entry.inbox.send()` for *this* session. The
        // Router will reap our slot on its eviction tick / via the
        // closed-router_rx detection on its next send attempt to us.
        let _ = self
            .session_to_router_tx
            .try_send(SessionToRouter::Detached { slot: self.slot });
    }

    /// Time out any pending command that was issued more than
    /// `request_timeout` ago. Mirrors `Handler::evict_timed_out_commands`
    /// for the parallel split.
    fn evict_stale(&mut self, now: Instant) {
        let deadline = match now.checked_sub(self.request_timeout) {
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
            // Drop the routing entry too so a late response is silently
            // ignored by the Router.
            self.ids.take_route(id);
            if let Some((pending, _, _)) = self.pending.remove(&id) {
                match pending {
                    SessionPending::Internal => {}
                    SessionPending::External(tx) => {
                        let _ = tx.send(Err(CdpError::Timeout));
                    }
                    SessionPending::Navigate(nav_id) => {
                        if let Some(nav) = self.navigations.remove(&nav_id) {
                            let _ = nav.into_tx().send(Err(CdpError::Timeout));
                        }
                    }
                }
            }
        }
    }

    /// Drop every in-flight pending command and navigation with a clear
    /// "target gone" error so callers awaiting `Page::execute` /
    /// `Page::goto` get a real result instead of a oneshot RecvError.
    fn cancel_in_flight(&mut self) {
        for (_, (pending, _, _)) in self.pending.drain() {
            match pending {
                SessionPending::External(tx) => {
                    let _ = tx.send(Err(CdpError::msg("target detached or crashed")));
                }
                SessionPending::Navigate(nav_id) => {
                    if let Some(nav) = self.navigations.remove(&nav_id) {
                        let _ = nav
                            .into_tx()
                            .send(Err(CdpError::msg("target detached or crashed")));
                    }
                }
                SessionPending::Internal => {}
            }
        }
        // Drain any navigations whose pending entry was already retired
        // (e.g. response received but lifecycle not yet completed).
        for (_, nav) in self.navigations.drain() {
            let _ = nav
                .into_tx()
                .send(Err(CdpError::msg("target detached or crashed")));
        }
    }

    /// Drain the page channel, advance the Target, dispatch every event.
    async fn drive(&mut self, now: Instant) {
        // Drain page channel non-blocking. The page Sender raises
        // `page_wake` after each send so the select! arm above re-fires.
        // Collect into a Vec first to release the &mut borrow on `target`
        // before calling `on_page_message`.
        let mut pending_msgs = Vec::new();
        if let Some(h) = self.target.page_mut() {
            while let Ok(msg) = h.rx.try_recv() {
                pending_msgs.push(msg);
            }
        }
        for msg in pending_msgs {
            self.target.on_page_message(msg);
        }

        // Push the Target's state machine forward.
        loop {
            let event = self.target.advance(now);
            match event {
                None => break,
                Some(TargetEvent::Request(req)) => {
                    self.submit_internal(req, now).await;
                }
                Some(TargetEvent::Command(msg)) => {
                    if msg.is_navigation() {
                        self.submit_navigation_command(msg, now).await;
                    } else {
                        self.submit_external(msg, now).await;
                    }
                }
                Some(TargetEvent::NavigationRequest(nav_id, req)) => {
                    self.submit_nav_request(nav_id, req, now).await;
                }
                Some(TargetEvent::NavigationResult(res)) => {
                    self.on_navigation_lifecycle_completed(res);
                }
                Some(TargetEvent::BytesConsumed(_)) => {}
            }
        }

        self.target.event_listeners_mut().flush();
    }

    fn alloc_nav_id(&mut self) -> NavigationId {
        let id = NavigationId(self.next_nav_id);
        self.next_nav_id = self.next_nav_id.wrapping_add(1);
        id
    }

    async fn submit_navigation_command(&mut self, msg: crate::cmd::CommandMessage, now: Instant) {
        let (req, sender) = msg.split();
        let nav_id = self.alloc_nav_id();
        // Hand the request to the FrameManager so it knows what lifecycle
        // events to wait for (it clones the Request internally).
        self.target.goto(FrameRequestedNavigation::new(
            nav_id,
            req.clone(),
            self.request_timeout,
        ));

        let call_id = self.alloc_call_id();
        let method = req.method.clone();
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
        self.pending
            .insert(call_id, (SessionPending::Navigate(nav_id), method, now));
        self.navigations
            .insert(nav_id, NavigationInProgress::new(sender));
    }

    async fn submit_nav_request(&mut self, nav_id: NavigationId, req: Request, now: Instant) {
        let call_id = self.alloc_call_id();
        let method = req.method.clone();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        if self.ws_tx.send(call).await.is_err() {
            // Caller's tx is held inside `self.navigations[nav_id]`; drop the
            // entry to error the caller.
            self.navigations.remove(&nav_id);
            return;
        }
        self.pending
            .insert(call_id, (SessionPending::Navigate(nav_id), method, now));
    }

    fn on_navigation_response(&mut self, nav_id: NavigationId, resp: Response) {
        if let Some(mut nav) = self.navigations.remove(&nav_id) {
            // Same terminal-failure release as `Handler::on_navigation_response`.
            // A CDP server that acks a failed navigation with `errorText` and no
            // lifecycle events would otherwise park this ack until the request
            // timeout, and leaving the sibling implementation unfixed is how the
            // hang survives a feature flag.
            let failed = resp
                .result
                .as_ref()
                .and_then(|result| result.get("errorText"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| {
                    !err.is_empty() && !crate::handler::httpfuture::navigation_continues(err)
                });
            if failed {
                self.target.frame_manager_mut().abandon_navigation(nav_id);
                let _ = nav.into_tx().send(Ok(resp));
            } else if nav.is_navigated() {
                let _ = nav.into_tx().send(Ok(resp));
            } else {
                nav.set_response(resp);
                self.navigations.insert(nav_id, nav);
            }
        }
    }

    fn on_navigation_lifecycle_completed(
        &mut self,
        res: std::result::Result<NavigationOk, NavigationError>,
    ) {
        match res {
            Ok(ok) => {
                let id = *ok.navigation_id();
                if let Some(mut nav) = self.navigations.remove(&id) {
                    if let Some(resp) = nav.take_response() {
                        let _ = nav.into_tx().send(Ok(resp));
                    } else {
                        nav.set_navigated();
                        self.navigations.insert(id, nav);
                    }
                }
            }
            Err(err) => {
                if let Some(nav) = self.navigations.remove(err.navigation_id()) {
                    let _ = nav.into_tx().send(Err(err.into()));
                }
            }
        }
    }

    fn alloc_call_id(&self) -> CallId {
        self.ids.alloc(self.slot)
    }

    async fn submit_internal(&mut self, req: Request, now: Instant) {
        let call_id = self.alloc_call_id();
        let method = req.method.clone();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        match self.ws_tx.send(call).await {
            Ok(()) => {
                self.pending
                    .insert(call_id, (SessionPending::Internal, method, now));
            }
            Err(_) => {
                // Writer is closed — nothing more we can do.
            }
        }
    }

    async fn submit_external(&mut self, msg: crate::cmd::CommandMessage, now: Instant) {
        let call_id = self.alloc_call_id();
        let method = msg.method.clone();
        let (req, sender) = msg.split();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        match self.ws_tx.send(call).await {
            Ok(()) => {
                self.pending
                    .insert(call_id, (SessionPending::External(sender), method, now));
            }
            Err(_) => {
                let _ = sender.send(Err(CdpError::msg("WS writer closed")));
            }
        }
    }

    fn on_response(&mut self, call_id: CallId, resp: Response, _method_hint: MethodId) {
        let Some((pending, method, _ts)) = self.pending.remove(&call_id) else {
            return;
        };
        match pending {
            SessionPending::Internal => {
                // Pick up session_id from `Target.attachToTarget` response.
                if method.as_ref() == AttachToTargetParams::IDENTIFIER {
                    if let Ok(parsed) =
                        to_command_response::<AttachToTargetParams>(resp.clone(), method.clone())
                    {
                        let sid: SessionId = parsed.result.session_id;
                        self.target.set_session_id(sid.clone());
                        if !self.session_id_reported {
                            self.session_id_reported = true;
                            // Lifecycle channel is sized for low-rate signals
                            // (256 slots; one SessionAttached per session). A
                            // failed try_send means the Router has dropped
                            // (browser tearing down) — the SessionTask will
                            // observe the same drop on its next router_rx
                            // recv and exit, so the message is not load-bearing.
                            let _ = self.session_to_router_tx.try_send(
                                SessionToRouter::SessionAttached {
                                    slot: self.slot,
                                    session_id: sid.into(),
                                },
                            );
                        }
                    }
                }
                self.target.on_response(resp, method.as_ref());
            }
            SessionPending::External(tx) => {
                let _ = tx.send(Ok(resp));
            }
            SessionPending::Navigate(nav_id) => {
                self.on_navigation_response(nav_id, resp);
            }
        }
    }

    /// Forward an event the Router routed to this session into the Target.
    #[allow(dead_code)]
    pub(crate) fn dispatch_event(&mut self, event: CdpEventMessage) {
        self.target.on_event(event);
    }

    #[allow(dead_code)]
    pub fn slot(&self) -> u16 {
        self.slot
    }

    #[allow(dead_code)]
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
}
