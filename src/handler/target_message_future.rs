use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};

use crate::handler::sender::PageSender;
use crate::handler::target::TargetMessage;
use crate::{error::Result, ArcHttpRequest};

type SendFut = Pin<
    Box<dyn Future<Output = std::result::Result<(), mpsc::error::SendError<TargetMessage>>> + Send>,
>;

pin_project! {
    pub struct TargetMessageFuture<T> {
        #[pin]
        rx_request: oneshot::Receiver<T>,
        target_sender: PageSender,
        #[pin]
        delay: tokio::time::Sleep,
        request_timeout: std::time::Duration,
        message: Option<TargetMessage>,
        send_fut: Option<SendFut>,
    }
}

impl<T> TargetMessageFuture<T> {
    pub fn new(
        target_sender: PageSender,
        message: TargetMessage,
        rx_request: oneshot::Receiver<T>,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            target_sender,
            rx_request,
            delay: tokio::time::sleep(request_timeout),
            request_timeout,
            message: Some(message),
            send_fut: None,
        }
    }

    /// Helper to build a `TargetMessageFuture<ArcHttpRequest>` for any
    /// "wait" style target message (navigation, network idle, etc.).
    ///
    /// The `make_msg` closure receives the `oneshot::Sender<ArcHttpRequest>` and
    /// must wrap it into the appropriate `TargetMessage` variant
    /// (e.g. `TargetMessage::WaitForNavigation(tx)`).
    pub(crate) fn wait(
        target_sender: PageSender,
        request_timeout: std::time::Duration,
        make_msg: impl FnOnce(oneshot::Sender<ArcHttpRequest>) -> TargetMessage,
    ) -> TargetMessageFuture<ArcHttpRequest> {
        let (tx, rx_request) = oneshot::channel();
        let message = make_msg(tx);
        TargetMessageFuture::new(target_sender, message, rx_request, request_timeout)
    }

    /// Wait for the main-frame navigation to finish.
    ///
    /// This triggers a `TargetMessage::WaitForNavigation` and resolves with
    /// the final `ArcHttpRequest` associated with that navigation (if any).
    pub fn wait_for_navigation(
        target_sender: PageSender,
        request_timeout: std::time::Duration,
    ) -> TargetMessageFuture<ArcHttpRequest> {
        Self::wait(
            target_sender,
            request_timeout,
            TargetMessage::WaitForNavigation,
        )
    }

    /// Wait for `DOMContentLoaded` — fires once the HTML is fully parsed and
    /// synchronous scripts have executed, but before subresources (images,
    /// fonts, late XHRs) finish loading.  Much faster than `wait_for_navigation`
    /// through slow proxies.
    pub fn wait_for_dom_content_loaded(
        target_sender: PageSender,
        request_timeout: std::time::Duration,
    ) -> TargetMessageFuture<ArcHttpRequest> {
        Self::wait(
            target_sender,
            request_timeout,
            TargetMessage::WaitForDomContentLoaded,
        )
    }

    /// Wait for the `load` event — all subresources (images, fonts, XHRs)
    /// have finished loading. Slower than `wait_for_navigation` /
    /// `wait_for_dom_content_loaded` through proxies.
    pub fn wait_for_load(
        target_sender: PageSender,
        request_timeout: std::time::Duration,
    ) -> TargetMessageFuture<ArcHttpRequest> {
        Self::wait(target_sender, request_timeout, TargetMessage::WaitForLoad)
    }

    /// Wait until the main frame reaches `networkIdle`.
    ///
    /// This triggers a `TargetMessage::WaitForNetworkIdle` and resolves with
    /// the `ArcHttpRequest` associated with the navigation that led to the
    /// idle state (if any).
    pub fn wait_for_network_idle(
        target_sender: PageSender,
        request_timeout: std::time::Duration,
    ) -> TargetMessageFuture<ArcHttpRequest> {
        Self::wait(
            target_sender,
            request_timeout,
            TargetMessage::WaitForNetworkIdle,
        )
    }

    /// Replace the timeout used by the initial delay and by `reset_deadline`.
    pub(crate) fn set_timeout(&mut self, timeout: std::time::Duration) {
        self.request_timeout = timeout;
        self.delay = tokio::time::sleep(timeout);
    }

    /// Reset the internal timer deadline to `now + request_timeout`.
    /// Used by `HttpFuture` to start the navigation timeout only after
    /// the command phase completes, not from future creation time.
    pub fn reset_deadline(self: Pin<&mut Self>) {
        let this = self.project();
        let deadline = tokio::time::Instant::now() + *this.request_timeout;
        this.delay.reset(deadline);
    }

    /// Wait until the main frame reaches `networkAlmostIdle`.
    ///
    /// This triggers a `TargetMessage::WaitForNetworkAlmostIdle` and resolves
    /// with the `ArcHttpRequest` associated with that navigation (if any).
    pub fn wait_for_network_almost_idle(
        target_sender: PageSender,
        request_timeout: std::time::Duration,
    ) -> TargetMessageFuture<ArcHttpRequest> {
        Self::wait(
            target_sender,
            request_timeout,
            TargetMessage::WaitForNetworkAlmostIdle,
        )
    }
}

impl<T> Future for TargetMessageFuture<T> {
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // Phase 1: deliver the message to the target channel.
        if let Some(message) = this.message.take() {
            match this.target_sender.try_send(message) {
                Ok(()) => {
                    // Sent — fall through to phase 2.
                }
                Err(mpsc::error::TrySendError::Full(msg)) => {
                    // Channel full — park via async send with timeout enforcement.
                    // The send_fut path (below) polls both the send and the delay,
                    // so we fall through instead of returning Pending with a spurious wake.
                    let sender = this.target_sender.clone();
                    *this.send_fut = Some(Box::pin(async move { sender.send(msg).await }));
                }
                Err(e) => return Poll::Ready(Err(e.into())),
            }
        }

        if let Some(fut) = this.send_fut.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    *this.send_fut = None;
                    // Sent — fall through to phase 2.
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                Poll::Pending => {
                    // Enforce timeout while waiting for channel capacity.
                    if this.delay.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Err(crate::error::CdpError::Timeout));
                    }
                    return Poll::Pending;
                }
            }
        }

        // Phase 2: wait for the result on the oneshot.
        if this.delay.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(crate::error::CdpError::Timeout))
        } else {
            this.rx_request.as_mut().poll(cx).map_err(Into::into)
        }
    }
}
