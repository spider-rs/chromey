use crate::handler::commandfuture::CommandFuture;
use crate::handler::http::HttpRequest;
use crate::handler::sender::PageSender;
use crate::handler::target_message_future::TargetMessageFuture;
use crate::{ArcHttpRequest, Result};
use chromiumoxide_cdp::cdp::browser_protocol::page::NavigateReturns;
use chromiumoxide_types::Command;
use futures_util::future::{Fuse, FusedFuture};
use futures_util::FutureExt;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

type ArcRequest = ArcHttpRequest;

const ERR_ABORTED: &str = "net::ERR_ABORTED";

/// ERR_ABORTED can mean a redirect, download, or second navigate superseded
/// this navigation; keep waiting for its replacement. HTTP response code
/// failures can still commit a 4xx/5xx body and fire lifecycle events, and the
/// status may be appended (e.g. " (403)"), so that exception uses a prefix match.
pub fn navigation_continues(err: &str) -> bool {
    err == ERR_ABORTED || err.starts_with("net::ERR_HTTP_RESPONSE_CODE_FAILURE")
}

/// Failure probe for `Page.navigate`: the ack's `errorText`.
pub fn navigate_error_text(response: &NavigateReturns) -> Option<&str> {
    response.error_text.as_deref()
}

pin_project! {
    /// Executes a command and waits for navigation, unless an optional failure
    /// probe resolves early with a synthetic failed HTTP request.
    pub struct HttpFuture<T: Command> {
        #[pin]
        command: Fuse<CommandFuture<T>>,
        #[pin]
        navigation: TargetMessageFuture<ArcHttpRequest>,
        // Reads a failure text out of the command response. `None` for the
        // generic constructor: the future then always waits for navigation.
        failure_check: Option<fn(&T::Response) -> Option<&str>>,
        // URL stamped onto the synthetic failed request.
        url: Option<String>,
    }
}

impl<T: Command> HttpFuture<T> {
    pub fn new(
        sender: PageSender,
        command: CommandFuture<T>,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            command: command.fuse(),
            navigation: TargetMessageFuture::<T>::wait_for_navigation(sender, request_timeout),
            failure_check: None,
            url: None,
        }
    }

    pub fn with_failure_check(
        sender: PageSender,
        command: CommandFuture<T>,
        request_timeout: std::time::Duration,
        failure_check: fn(&T::Response) -> Option<&str>,
        url: Option<String>,
    ) -> Self {
        Self {
            command: command.fuse(),
            navigation: TargetMessageFuture::<T>::wait_for_navigation(sender, request_timeout),
            failure_check: Some(failure_check),
            url,
        }
    }
}

impl<T> Future for HttpFuture<T>
where
    T: Command,
{
    type Output = Result<ArcRequest>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // 1. First complete command request future
        // 2. Switch polls navigation
        if this.command.is_terminated() {
            this.navigation.poll(cx)
        } else {
            match this.command.poll(cx) {
                Poll::Ready(Ok(command_response)) => {
                    if let Some(check) = *this.failure_check {
                        if let Some(err) = check(&command_response.result) {
                            if !err.is_empty() && !navigation_continues(err) {
                                let req = HttpRequest {
                                    failure_text: Some(err.to_owned()),
                                    is_navigation_request: true,
                                    url: this.url.take(),
                                    ..Default::default()
                                };
                                return Poll::Ready(Ok(Some(Arc::new(req))));
                            }
                        }
                    }
                    // Command succeeded — reset the navigation timer so it
                    // gets a full request_timeout from NOW, not from when
                    // HttpFuture was constructed, then immediately start
                    // polling navigation (avoids a full wake round-trip).
                    this.navigation.as_mut().reset_deadline();
                    this.navigation.poll(cx)
                }
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        }
    }
}
