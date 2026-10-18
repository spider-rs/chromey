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
    /// Retained until the lifecycle channel accepts it. Losing this hint
    /// would lose routing for servers that only send the attach response.
    pending_attachment: Option<String>,
    request_timeout: Duration,
    shutdown: tokio::sync::oneshot::Receiver<()>,
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
        shutdown: tokio::sync::oneshot::Receiver<()>,
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
            pending_attachment: None,
            request_timeout,
            shutdown,
        }
    }

    pub async fn run(mut self) {
        let Ok(mut evict) = crate::handler::job::eviction_interval(self.request_timeout) else {
            return;
        };
        // First tick: kick the init state machine immediately so the Target
        // emits `Target.attachToTarget`.
        if self.drive(Instant::now()).is_err() {
            return;
        }

        // Per-session eviction tick. Bounds in-flight oneshot lifetime to
        // ~request_timeout when the wire goes silent (Chrome stalls, mock
        // drops the response, etc.).
        // Reserve through a separate sender so the select output does not
        // borrow self while another branch updates session state.
        let lifecycle_tx = self.session_to_router_tx.clone();
        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => break,

                _ = evict.tick() => {
                    if self.evict_stale(Instant::now()) { break; }
                }

                permit = lifecycle_tx.reserve(), if self.pending_attachment.is_some() => {
                    let Ok(permit) = permit else { break; };
                    if let Some(session_id) = self.pending_attachment.take() {
                        permit.send(SessionToRouter::SessionAttached { slot: self.slot, session_id });
                    }
                }

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
                        None => {
                            break;
                        }
                    }
                }

                _ = self.page_wake.notified() => {
                    // page_rx will be drained in `drive()` below
                }

            }

            if self.drive(Instant::now()).is_err() {
                break;
            }
        }
        // Drop resolves pending callers. Router joins us before slot reuse.
    }

    /// Time out any pending command that was issued more than
    /// `request_timeout` ago. Mirrors `Handler::evict_timed_out_commands`
    /// for the parallel split.
    fn evict_stale(&mut self, now: Instant) -> bool {
        let deadline = match now.checked_sub(self.request_timeout) {
            Some(d) => d,
            None => return false,
        };
        let stale: Vec<CallId> = self
            .pending
            .iter()
            .filter(|(_, (_, _, ts))| *ts < deadline)
            .map(|(k, _)| *k)
            .collect();
        let mut internal_timed_out = false;
        for id in stale {
            // Drop the routing entry too so a late response is silently
            // ignored by the Router.
            self.ids.take_route(id);
            if let Some((pending, _, _)) = self.pending.remove(&id) {
                match pending {
                    SessionPending::Internal => {
                        internal_timed_out = true;
                    }
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
        internal_timed_out
    }

    /// Drop every in-flight pending command and navigation with a clear
    /// "target gone" error so callers awaiting `Page::execute` /
    /// `Page::goto` get a real result instead of a oneshot RecvError.
    fn cancel_in_flight(&mut self) {
        for (id, (pending, _, _)) in self.pending.drain() {
            self.ids.take_route(id);
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
    fn drive(&mut self, now: Instant) -> Result<()> {
        // Drain page channel non-blocking. The page Sender raises
        // `page_wake` after each send so the select! arm above re-fires.
        // Collect into a Vec first to release the &mut borrow on `target`
        // before calling `on_page_message`.
        let mut pending_msgs = Vec::new();
        if let Some(h) = self.target.page_mut() {
            for _ in 0..128 {
                match h.rx.try_recv() {
                    Ok(msg) => pending_msgs.push(msg),
                    Err(_) => break,
                }
            }
        }
        if pending_msgs.len() == 128 {
            self.page_wake.notify_one();
        }
        for msg in pending_msgs {
            self.target.on_page_message(msg);
        }

        // Push the Target's state machine forward.
        for index in 0..128 {
            let event = self.target.advance(now);
            match event {
                None => break,
                Some(TargetEvent::Request(req)) => {
                    self.submit_internal(req, now)?;
                }
                Some(TargetEvent::Command(msg)) => {
                    if msg.is_navigation() {
                        self.submit_navigation_command(msg, now);
                    } else {
                        self.submit_external(msg, now);
                    }
                }
                Some(TargetEvent::NavigationRequest(nav_id, req)) => {
                    self.submit_nav_request(nav_id, req, now);
                }
                Some(TargetEvent::NavigationResult(res)) => {
                    self.on_navigation_lifecycle_completed(res);
                }
                Some(TargetEvent::BytesConsumed(_)) => {}
            }
            if index == 127 {
                self.page_wake.notify_one();
            }
        }

        self.target.event_listeners_mut().flush();
        Ok(())
    }

    fn alloc_nav_id(&mut self) -> NavigationId {
        let id = NavigationId(self.next_nav_id);
        self.next_nav_id = self.next_nav_id.wrapping_add(1);
        id
    }

    fn submit_navigation_command(&mut self, msg: crate::cmd::CommandMessage, now: Instant) {
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
        if let Err(err) = super::try_submit(&self.ws_tx, call) {
            self.ids.take_route(call_id);
            self.target.frame_manager_mut().abandon_navigation(nav_id);
            let _ = sender.send(Err(err));
            return;
        }
        self.pending
            .insert(call_id, (SessionPending::Navigate(nav_id), method, now));
        self.navigations
            .insert(nav_id, NavigationInProgress::new(sender));
    }

    fn submit_nav_request(&mut self, nav_id: NavigationId, req: Request, now: Instant) {
        let call_id = self.alloc_call_id();
        let method = req.method.clone();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        if let Err(err) = super::try_submit(&self.ws_tx, call) {
            self.ids.take_route(call_id);
            self.target.frame_manager_mut().abandon_navigation(nav_id);
            if let Some(nav) = self.navigations.remove(&nav_id) {
                let _ = nav.into_tx().send(Err(err));
            }
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

    fn submit_internal(&mut self, req: Request, now: Instant) -> Result<()> {
        let call_id = self.alloc_call_id();
        let method = req.method.clone();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        match super::try_submit(&self.ws_tx, call) {
            Ok(()) => {
                self.pending
                    .insert(call_id, (SessionPending::Internal, method, now));
            }
            Err(err) => {
                self.ids.take_route(call_id);
                return Err(err);
            }
        }
        Ok(())
    }

    fn submit_external(&mut self, msg: crate::cmd::CommandMessage, now: Instant) {
        let call_id = self.alloc_call_id();
        let method = msg.method.clone();
        let (req, sender) = msg.split();
        let call = MethodCall {
            id: call_id,
            method: req.method,
            session_id: req.session_id,
            params: req.params,
        };
        match super::try_submit(&self.ws_tx, call) {
            Ok(()) => {
                self.pending
                    .insert(call_id, (SessionPending::External(sender), method, now));
            }
            Err(err) => {
                self.ids.take_route(call_id);
                let _ = sender.send(Err(err));
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
                        self.pending_attachment = Some(sid.into());
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

impl Drop for SessionTask {
    fn drop(&mut self) {
        self.cancel_in_flight();
        self.target.cancel_initiator();
        while let Ok(msg) = self.router_rx.try_recv() {
            if let RouterToSession::SetInitiator(tx) = msg {
                let _ = tx.send(Err(CdpError::msg("session stopped")));
            }
        }
    }
}

#[cfg(test)]
mod backpressure_tests {
    use super::*;
    use crate::cmd::CommandMessage;
    use crate::handler::target::TargetConfig;
    use tokio::sync::oneshot;

    struct Fixture {
        session: SessionTask,
        inbox: mpsc::Sender<RouterToSession>,
        writer: mpsc::Receiver<MethodCall>,
        lifecycle: mpsc::Receiver<SessionToRouter>,
        shutdown: oneshot::Sender<()>,
    }

    fn fixture() -> Fixture {
        // A non-page target keeps the init state machine idle; only the
        // commands explicitly submitted by each test reach the fake writer.
        let info = serde_json::from_value(serde_json::json!({
            "targetId": "test", "type": "browser", "title": "", "url": "", "attached": false,
            "canAccessOpener": false
        }))
        .unwrap();
        let target = Target::new(info, TargetConfig::default(), Default::default());
        let (inbox, rx) = mpsc::channel(1);
        let (ws, writer) = mpsc::channel(1);
        let (lifecycle_tx, lifecycle) = mpsc::channel(1);
        let (shutdown, shutdown_rx) = oneshot::channel();
        let session = SessionTask::new(
            1,
            target,
            Arc::new(Notify::new()),
            rx,
            ws,
            lifecycle_tx,
            CallIdAllocator::new(1),
            Duration::from_millis(10),
            shutdown_rx,
        );
        Fixture {
            session,
            inbox,
            writer,
            lifecycle,
            shutdown,
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

    #[tokio::test]
    async fn failed_submission_removes_route_and_preserves_accepted_response() {
        let mut f = fixture();
        let (first, first_rx) = command();
        f.session.submit_external(first, Instant::now());
        let (second, second_rx) = command();
        f.session.submit_external(second, Instant::now());
        assert!(second_rx
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("full"));
        assert_eq!(f.session.ids.take_route(CallId::new(2)), None);
        assert_eq!(f.session.pending.len(), 1);
        let sent = f.writer.try_recv().unwrap();
        assert_eq!(f.session.ids.take_route(sent.id), Some(1));
        let resp: Response =
            serde_json::from_value(serde_json::json!({"id": sent.id, "result": {"ok": true}}))
                .unwrap();
        f.session.on_response(sent.id, resp, sent.method);
        assert_eq!(first_rx.await.unwrap().unwrap().result.unwrap()["ok"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_writer_and_lifecycle_leave_eviction_and_shutdown_serviceable() {
        let mut f = fixture();
        let (command, result) = command();
        f.session.submit_external(
            command,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        );
        f.session
            .session_to_router_tx
            .try_send(SessionToRouter::SessionAttached {
                slot: 2,
                session_id: "filler".into(),
            })
            .unwrap();
        f.session.pending_attachment = Some("retained".into());
        let ids = f.session.ids.clone();
        let driver = tokio::spawn(f.session.run());
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), result)
                .await
                .unwrap()
                .unwrap(),
            Err(CdpError::Timeout)
        ));
        assert_eq!(ids.take_route(CallId::new(1)), None);
        drop(f.shutdown);
        tokio::time::timeout(Duration::from_secs(1), driver)
            .await
            .unwrap()
            .unwrap();
        assert!(f.inbox.is_closed());
        assert!(f.writer.try_recv().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn attachment_waits_for_capacity_without_losing_routing_hint() {
        let mut f = fixture();
        f.session
            .session_to_router_tx
            .try_send(SessionToRouter::SessionAttached {
                slot: 2,
                session_id: "filler".into(),
            })
            .unwrap();
        let req = Request::new(
            AttachToTargetParams::IDENTIFIER.into(),
            serde_json::json!({}),
        );
        f.session.submit_internal(req, Instant::now()).unwrap();
        let call = f.writer.try_recv().unwrap();
        f.session.ids.take_route(call.id);
        let resp = serde_json::from_value(
            serde_json::json!({"id": call.id, "result": {"sessionId": "attached"}}),
        )
        .unwrap();
        f.session.on_response(call.id, resp, call.method);
        assert_eq!(f.session.pending_attachment.as_deref(), Some("attached"));
        let driver = tokio::spawn(f.session.run());
        tokio::task::yield_now().await;
        let _ = f.lifecycle.recv().await.unwrap();
        let SessionToRouter::SessionAttached { slot, session_id } =
            tokio::time::timeout(Duration::from_secs(1), f.lifecycle.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!((slot, session_id.as_str()), (1, "attached"));
        drop(f.shutdown);
        driver.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_resolves_accepted_commands_and_queued_initiators_with_full_inbox() {
        let mut f = fixture();
        let (command, result) = command();
        f.session.submit_external(command, Instant::now());
        let (tx, initialized) = oneshot::channel();
        f.inbox.try_send(RouterToSession::SetInitiator(tx)).unwrap();
        drop(f.shutdown);
        tokio::time::timeout(Duration::from_secs(1), f.session.run())
            .await
            .unwrap();
        assert!(result.await.unwrap().is_err());
        assert!(initialized.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn failed_internal_submission_is_terminal_and_does_not_leak_a_route() {
        let mut f = fixture();
        let (command, result) = command();
        f.session.submit_external(command, Instant::now());
        let error = f
            .session
            .submit_internal(
                Request::new("Test.init".into(), serde_json::json!({})),
                Instant::now(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("full"));
        assert_eq!(f.session.ids.take_route(CallId::new(2)), None);
        drop(f.session);
        assert!(result.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn rejected_navigation_submissions_report_errors_and_remove_routes() {
        let mut f = fixture();
        let (filler, _result) = command();
        f.session.submit_external(filler, Instant::now());
        let (mut navigation, result) = command();
        navigation.method = "Page.navigate".into();
        navigation.params = serde_json::json!({"url": "about:blank"});
        f.session
            .submit_navigation_command(navigation, Instant::now());
        assert!(result
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("full"));
        assert!(f.session.navigations.is_empty());
        assert_eq!(f.session.ids.take_route(CallId::new(2)), None);

        let nav_id = NavigationId(10);
        let (tx, result) = oneshot::channel();
        f.session
            .navigations
            .insert(nav_id, NavigationInProgress::new(tx));
        f.session.submit_nav_request(
            nav_id,
            Request::new("Page.navigate".into(), serde_json::json!({})),
            Instant::now(),
        );
        assert!(result
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("full"));
        assert!(f.session.navigations.is_empty());
        assert_eq!(f.session.ids.take_route(CallId::new(3)), None);
        assert_eq!(
            f.session.pending.len(),
            1,
            "earlier accepted command is preserved"
        );
    }
}
