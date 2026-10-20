use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::Stream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use chromiumoxide_cdp::cdp::{Event, EventKind, IntoEventKind};
use chromiumoxide_types::MethodId;

/// Unique identifier for a listener.
pub type ListenerId = u64;

/// Monotonic id generator for listeners.
static NEXT_LISTENER_ID: AtomicU64 = AtomicU64::new(1);

/// Handle returned when you register a listener.
/// Use it to remove a listener immediately.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventListenerHandle {
    pub method: MethodId,
    pub id: ListenerId,
}

/// All the currently active listeners
#[derive(Debug, Default)]
pub struct EventListeners {
    /// Tracks the listeners for each event identified by the key
    listeners: HashMap<MethodId, Vec<EventListener>>,
}

impl EventListeners {
    /// Register a subscription for a method, returning a handle to remove it.
    pub fn add_listener(&mut self, req: EventListenerRequest) -> EventListenerHandle {
        let EventListenerRequest {
            listener,
            method,
            kind,
        } = req;

        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::Relaxed);

        let subs = self.listeners.entry(method.clone()).or_default();
        subs.push(EventListener {
            id,
            listener,
            kind,
            queued_events: Default::default(),
        });

        EventListenerHandle { method, id }
    }

    /// Remove a specific listener immediately.
    /// Returns true if something was removed.
    pub fn remove_listener(&mut self, handle: &EventListenerHandle) -> bool {
        let mut removed = false;
        let mut became_empty = false;

        if let Some(subs) = self.listeners.get_mut(&handle.method) {
            let before = subs.len();
            subs.retain(|s| s.id != handle.id);
            removed = subs.len() != before;
            became_empty = subs.is_empty();
            // `subs` borrow ends here (end of this if block)
        }

        if became_empty {
            self.listeners.remove(&handle.method);
        }

        removed
    }
    /// Remove all listeners for a given method.
    /// Returns how many were removed.
    pub fn remove_all_for_method(&mut self, method: &MethodId) -> usize {
        self.listeners.remove(method).map(|v| v.len()).unwrap_or(0)
    }

    /// Queue in an event that should be sent to all listeners.
    pub fn start_send<T: Event>(&mut self, event: T) {
        if let Some(subscriptions) = self.listeners.get_mut(&T::method_id()) {
            let event: Arc<dyn Event> = Arc::new(event);
            subscriptions
                .iter_mut()
                .for_each(|sub| sub.start_send(Arc::clone(&event)));
        }
    }

    /// Try to queue a custom event if a listener is registered and the json conversion succeeds.
    pub fn try_send_custom(
        &mut self,
        method: &str,
        val: serde_json::Value,
    ) -> serde_json::Result<()> {
        if let Some(subscriptions) = self.listeners.get_mut(method) {
            let mut event = None;

            if let Some(json_to_arc_event) = subscriptions
                .iter()
                .filter_map(|sub| match &sub.kind {
                    EventKind::Custom(conv) => Some(conv),
                    _ => None,
                })
                .next()
            {
                event = Some(json_to_arc_event(val)?);
            }

            if let Some(event) = event {
                subscriptions
                    .iter_mut()
                    .filter(|sub| sub.kind.is_custom())
                    .for_each(|sub| sub.start_send(Arc::clone(&event)));
            }
        }
        Ok(())
    }

    /// Drains all queued events and does housekeeping when the receiver is dropped.
    ///
    /// Uses `retain_mut` for a single-pass flush + prune instead of the
    /// swap-remove/push pattern, avoiding per-listener Vec reshuffling.
    pub fn poll(&mut self, cx: &mut Context<'_>) {
        let _ = cx;
        let mut any_disconnected = false;

        for subscriptions in self.listeners.values_mut() {
            subscriptions.retain_mut(|sub| match sub.flush() {
                // A dropped `EventStream` only surfaces as a send error, which
                // needs a queued event. Check the channel too, so a listener
                // for an event that never fires again is still pruned.
                Ok(()) if !sub.listener.is_closed() => true,
                _ => {
                    any_disconnected = true;
                    false
                }
            });
        }

        if any_disconnected {
            self.listeners.retain(|_, v| !v.is_empty());
        }
    }

    /// Flush all queued events without requiring a waker `Context`.
    ///
    /// Identical to [`poll`](Self::poll) but usable from the
    /// `Handler::run()` async path where no `Context` is available.
    pub fn flush(&mut self) {
        let mut any_disconnected = false;

        for subscriptions in self.listeners.values_mut() {
            subscriptions.retain_mut(|sub| match sub.flush() {
                // A dropped `EventStream` only surfaces as a send error, which
                // needs a queued event. Check the channel too, so a listener
                // for an event that never fires again is still pruned.
                Ok(()) if !sub.listener.is_closed() => true,
                _ => {
                    any_disconnected = true;
                    false
                }
            });
        }

        if any_disconnected {
            self.listeners.retain(|_, v| !v.is_empty());
        }
    }
}

pub struct EventListenerRequest {
    listener: UnboundedSender<Arc<dyn Event>>,
    pub method: MethodId,
    pub kind: EventKind,
}

impl EventListenerRequest {
    pub fn new<T: IntoEventKind>(listener: UnboundedSender<Arc<dyn Event>>) -> Self {
        Self {
            listener,
            method: T::method_id(),
            kind: T::event_kind(),
        }
    }
}

impl fmt::Debug for EventListenerRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventListenerRequest")
            .field("method", &self.method)
            .field("kind", &self.kind)
            .finish()
    }
}

/// Represents a single event listener.
///
/// Uses an unbounded channel intentionally: `flush()` is called
/// synchronously from the handler's poll loop and cannot await a
/// bounded channel's back-pressure. Bounding the channel would
/// require either dropping events (behaviour change) or making
/// the flush path async (large refactor). Consumers that register
/// a listener must poll the resulting `EventStream` to prevent
/// unbounded growth.
pub struct EventListener {
    /// Unique id for this listener (used for immediate removal).
    pub id: ListenerId,
    /// the sender half of the event channel
    listener: UnboundedSender<Arc<dyn Event>>,
    /// currently queued events
    queued_events: VecDeque<Arc<dyn Event>>,
    /// For what kind of event this event is for
    kind: EventKind,
}

impl EventListener {
    /// queue in a new event
    pub fn start_send(&mut self, event: Arc<dyn Event>) {
        self.queued_events.push_back(event)
    }

    /// Drains all queued events, sending them synchronously via the unbounded channel.
    /// Returns `Err` if the receiver has been dropped.
    pub fn flush(&mut self) -> std::result::Result<(), mpsc::error::SendError<Arc<dyn Event>>> {
        while let Some(event) = self.queued_events.pop_front() {
            self.listener.send(event)?;
        }
        Ok(())
    }
}

impl fmt::Debug for EventListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventListener")
            .field("id", &self.id)
            .finish()
    }
}

/// The receiver part of an event subscription
pub struct EventStream<T: IntoEventKind> {
    events: UnboundedReceiver<Arc<dyn Event>>,
    _marker: PhantomData<T>,
}

impl<T: IntoEventKind> fmt::Debug for EventStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream").finish()
    }
}

impl<T: IntoEventKind> EventStream<T> {
    pub fn new(events: UnboundedReceiver<Arc<dyn Event>>) -> Self {
        Self {
            events,
            _marker: PhantomData,
        }
    }
}

/// Per-poll budget: how many wrong-type events may be drained inside one
/// `poll_next` call before we cooperatively yield.  Wrong-type events only
/// occur when two custom-event listeners share a `method_id` but expect
/// different Rust types (see `EventListeners::try_send_custom`); without a
/// cap, a steady producer of those mismatches could keep one tokio worker
/// inside `poll_next` for an unbounded number of synchronous iterations.
const MAX_WRONG_TYPE_PER_POLL: usize = 32;

impl<T: IntoEventKind + Unpin> Stream for EventStream<T> {
    type Item = Arc<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();
        for _ in 0..MAX_WRONG_TYPE_PER_POLL {
            match pin.events.poll_recv(cx) {
                Poll::Ready(Some(event)) => {
                    if let Ok(e) = event.into_any_arc().downcast() {
                        return Poll::Ready(Some(e));
                    }
                    // wrong type — drop and try the next message
                    continue;
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
        // Hit the per-poll cap.  Re-arm ourselves so the runtime
        // re-polls us, then yield — other tasks get a chance to run.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use chromiumoxide_cdp::cdp::browser_protocol::animation::EventAnimationCanceled;
    use chromiumoxide_cdp::cdp::CustomEvent;
    use chromiumoxide_types::{MethodId, MethodType};

    use super::*;

    /// A listener whose `EventStream` was dropped must be pruned on the next
    /// flush even if its event never fires again. Before, a closed listener
    /// was only noticed when an event for its method was sent to it.
    #[test]
    fn flush_prunes_closed_listeners_without_pending_events() {
        let mut listeners = EventListeners::default();
        let (live_tx, _live_rx) = tokio::sync::mpsc::unbounded_channel();
        listeners.add_listener(EventListenerRequest::new::<EventAnimationCanceled>(live_tx));
        for _ in 0..1_000 {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            listeners.add_listener(EventListenerRequest::new::<EventAnimationCanceled>(tx));
            drop(rx);
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        listeners.add_listener(EventListenerRequest::new::<
            chromiumoxide_cdp::cdp::browser_protocol::animation::EventAnimationStarted,
        >(tx));
        drop(rx);

        listeners.flush();
        let remaining: usize = listeners.listeners.values().map(Vec::len).sum();
        assert_eq!(remaining, 1, "only the live listener may remain");
        assert_eq!(listeners.listeners.len(), 1, "empty method entries removed");

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        listeners.add_listener(EventListenerRequest::new::<
            chromiumoxide_cdp::cdp::browser_protocol::animation::EventAnimationStarted,
        >(tx));
        drop(rx);
        listeners.poll(&mut Context::from_waker(
            futures_util::task::noop_waker_ref(),
        ));
        let remaining: usize = listeners.listeners.values().map(Vec::len).sum();
        assert_eq!(remaining, 1, "poll prunes the same way");
    }

    #[tokio::test]
    async fn event_stream() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut stream = EventStream::<EventAnimationCanceled>::new(rx);

        let event = EventAnimationCanceled {
            id: "id".to_string(),
        };
        let msg: Arc<dyn Event> = Arc::new(event.clone());
        tx.send(msg).unwrap();
        let next = stream.next().await.unwrap();
        assert_eq!(&*next, &event);
    }

    #[tokio::test]
    async fn custom_event_stream() {
        use serde::Deserialize;

        #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
        struct MyCustomEvent {
            name: String,
        }

        impl MethodType for MyCustomEvent {
            fn method_id() -> MethodId {
                "Custom.Event".into()
            }
        }
        impl CustomEvent for MyCustomEvent {}

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut stream = EventStream::<MyCustomEvent>::new(rx);

        let event = MyCustomEvent {
            name: "my event".to_string(),
        };
        let msg: Arc<dyn Event> = Arc::new(event.clone());
        tx.send(msg).unwrap();
        let next = stream.next().await.unwrap();
        assert_eq!(&*next, &event);
    }

    #[tokio::test]
    async fn remove_listener_immediately_stops_delivery() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut listeners = EventListeners::default();

        let handle =
            listeners.add_listener(EventListenerRequest::new::<EventAnimationCanceled>(tx));
        assert!(listeners.remove_listener(&handle));

        listeners.start_send(EventAnimationCanceled {
            id: "nope".to_string(),
        });

        std::future::poll_fn(|cx| {
            listeners.poll(cx);
            Poll::Ready(())
        })
        .await;

        // The listener was removed, so nothing should have been sent
        assert!(rx.try_recv().is_err());
    }

    // ---------------------------------------------------------------
    // Per-poll budget regression tests for `EventStream::poll_next`.
    //
    // Wrong-type messages reach a stream when two custom-event listeners
    // share a `method_id` but have different Rust types (the dispatcher
    // sends one converted `Arc<dyn Event>` to all of them; the second
    // listener's `downcast` then fails). Without a per-poll cap, a steady
    // stream of those would loop synchronously inside one `poll_next`
    // call, blocking the worker.
    // ---------------------------------------------------------------

    use serde::Deserialize;

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    struct WrongA {
        a: i32,
    }
    impl MethodType for WrongA {
        fn method_id() -> MethodId {
            "Custom.PollBudget".into()
        }
    }
    impl CustomEvent for WrongA {}

    #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
    struct RightB {
        b: i32,
    }
    impl MethodType for RightB {
        fn method_id() -> MethodId {
            "Custom.PollBudget".into()
        }
    }
    impl CustomEvent for RightB {}

    /// A flood of wrong-type events larger than `MAX_WRONG_TYPE_PER_POLL`
    /// is fully drained and the trailing right-type event is still
    /// delivered — proving the per-poll cap doesn't lose events.
    #[tokio::test]
    async fn poll_next_drains_wrong_type_flood() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut stream = EventStream::<RightB>::new(rx);

        // Use ~10x the budget so the stream must yield-and-resume several
        // times across separate poll calls.
        let flood = MAX_WRONG_TYPE_PER_POLL * 10;
        for i in 0..flood {
            let msg: Arc<dyn Event> = Arc::new(WrongA { a: i as i32 });
            tx.send(msg).unwrap();
        }
        let target = RightB { b: 7 };
        let target_msg: Arc<dyn Event> = Arc::new(target.clone());
        tx.send(target_msg).unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("stream must not hang under wrong-type flood")
            .expect("stream should yield the right-type event");
        assert_eq!(&*got, &target);
    }

    /// One `poll_next` call must consume at most `MAX_WRONG_TYPE_PER_POLL`
    /// wrong-type messages and then return `Pending` (re-arming itself via
    /// the waker). With strictly more wrong-type events queued than the
    /// budget, the first poll must NOT keep going to completion.
    #[tokio::test]
    async fn poll_next_returns_pending_after_budget() {
        use std::pin::Pin;
        use std::task::Poll;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut stream = EventStream::<RightB>::new(rx);

        // Strictly more wrong-type events than the budget, with no
        // right-type event queued yet. Without the cap, the loop would
        // synchronously drain everything and then block on `Pending`.
        let queued = MAX_WRONG_TYPE_PER_POLL + 5;
        for i in 0..queued {
            let msg: Arc<dyn Event> = Arc::new(WrongA { a: i as i32 });
            tx.send(msg).unwrap();
        }

        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let res = Pin::new(&mut stream).poll_next(&mut cx);
        assert!(
            matches!(res, Poll::Pending),
            "first poll must yield once the per-poll budget is consumed"
        );

        // The exact remaining count isn't part of the contract, but at
        // least `queued - MAX_WRONG_TYPE_PER_POLL` events must still be
        // sitting in the channel — the cap really did stop early.
        let mut remaining = 0usize;
        while stream.events.try_recv().is_ok() {
            remaining += 1;
        }
        assert!(
            remaining >= queued - MAX_WRONG_TYPE_PER_POLL,
            "expected at least {} events left after budget poll, found {}",
            queued - MAX_WRONG_TYPE_PER_POLL,
            remaining
        );
    }

    /// After yielding under the budget cap, a follow-up poll must resume
    /// draining and ultimately deliver a trailing right-type event — the
    /// re-arm via `wake_by_ref` is what keeps the stream live.
    #[tokio::test]
    async fn poll_next_resumes_after_budget_yield() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut stream = EventStream::<RightB>::new(rx);

        // > 1 budget worth of wrong types, then the right-type tail.
        for i in 0..(MAX_WRONG_TYPE_PER_POLL + 5) {
            let msg: Arc<dyn Event> = Arc::new(WrongA { a: i as i32 });
            tx.send(msg).unwrap();
        }
        let target = RightB { b: 99 };
        let target_msg: Arc<dyn Event> = Arc::new(target.clone());
        tx.send(target_msg).unwrap();

        // Awaiting `next()` exercises the wake-and-resume path — if the
        // re-arm were missing, this would hang.
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("re-arm must wake the stream after budget yield")
            .expect("right-type event should be delivered");
        assert_eq!(&*got, &target);
    }
}
