use pin_project_lite::pin_project;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};

use crate::cmd::{to_command_response, CommandMessage};
use crate::error::Result;
use crate::handler::sender::PageSender;
use crate::handler::target::TargetMessage;
use chromiumoxide_cdp::cdp::browser_protocol::target::SessionId;
use chromiumoxide_types::{Command, CommandResponse, MethodId, Response};

pin_project! {

    pub struct CommandFuture<T, M = Result<Response>> {
        #[pin]
        rx_command: oneshot::Receiver<M>,
        target_sender: PageSender,
        #[pin]
        delay: tokio::time::Sleep,

        message: Option<TargetMessage>,
        send_fut: Option<Pin<Box<dyn Future<Output = std::result::Result<(), mpsc::error::SendError<TargetMessage>>> + Send>>>,

        method: MethodId,

        _marker: PhantomData<T>
    }
}

impl<T: Command> CommandFuture<T> {
    /// Attach a navigation deadline to the not-yet-sent `Page.navigate` message.
    /// No-op once the message has been sent.
    pub(crate) fn set_navigation_timeout(&mut self, timeout: std::time::Duration) {
        if let Some(TargetMessage::Command(cmd)) = self.message.as_mut() {
            cmd.navigation_timeout = Some(timeout);
        }
    }

    /// A new command future.
    pub fn new(
        cmd: T,
        target_sender: PageSender,
        session: Option<SessionId>,
        request_timeout: std::time::Duration,
    ) -> Result<Self> {
        let (tx, rx_command) = oneshot::channel::<Result<Response>>();
        let method = cmd.identifier();

        let message = Some(TargetMessage::Command(CommandMessage::with_session(
            cmd, tx, session,
        )?));

        let delay = tokio::time::sleep(request_timeout);

        Ok(Self {
            target_sender,
            rx_command,
            message,
            send_fut: None,
            delay,
            method,
            _marker: PhantomData,
        })
    }
}

impl<T> Future for CommandFuture<T>
where
    T: Command,
{
    type Output = Result<CommandResponse<T::Response>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // Phase 1: deliver the command to the target channel.
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

        // Phase 2: wait for the response on the oneshot.
        if this.delay.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(crate::error::CdpError::Timeout))
        } else {
            match this.rx_command.as_mut().poll(cx) {
                Poll::Ready(Ok(Ok(response))) => {
                    Poll::Ready(to_command_response::<T>(response, this.method.clone()))
                }
                Poll::Ready(Ok(Err(e))) => Poll::Ready(Err(e)),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
                Poll::Pending => Poll::Pending,
            }
        }
    }
}
