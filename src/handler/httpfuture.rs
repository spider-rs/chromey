use crate::handler::commandfuture::CommandFuture;
use crate::handler::http::HttpRequest;
use crate::handler::sender::PageSender;
use crate::handler::target_message_future::TargetMessageFuture;
use crate::{ArcHttpRequest, Result};
use chromiumoxide_cdp::cdp::browser_protocol::page::NavigateReturns;
use chromiumoxide_types::Command;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

type ArcRequest = ArcHttpRequest;

const ERR_ABORTED: &str = "net::ERR_ABORTED";

/// ERR_ABORTED can mean a redirect, download, or second navigate superseded
/// this navigation; keep waiting for its replacement. HTTP response code
/// failures can still commit a 4xx/5xx body and fire lifecycle events; navi
/// appends the status (e.g. " (403)"), so that exception uses a prefix match.
pub fn navigation_continues(err: &str) -> bool {
    err == ERR_ABORTED || err.starts_with("net::ERR_HTTP_RESPONSE_CODE_FAILURE")
}

/// Failure probe for `Page.navigate`: the ack's `errorText`.
pub fn navigate_error_text(response: &NavigateReturns) -> Option<&str> {
    response.error_text.as_deref()
}

/// Commit probe for `Page.navigate`: a loaderId and no terminal errorText.
/// The `navigation_continues` carve-outs count as committed-pending.
pub fn navigate_committed(response: &NavigateReturns) -> bool {
    response.loader_id.is_some()
        && response
            .error_text
            .as_deref()
            .map_or(true, |err| err.is_empty() || navigation_continues(err))
}

pin_project! {
    /// Executes a command and waits for navigation, unless an optional failure
    /// probe resolves early with a synthetic failed HTTP request.
    pub struct HttpFuture<T: Command> {
        #[pin]
        command: CommandFuture<T>,
        command_done: bool,
        #[pin]
        navigation: TargetMessageFuture<ArcHttpRequest>,
        // Reads a failure text out of the command response. `None` for the
        // generic constructor: the future then always waits for navigation.
        failure_check: Option<fn(&T::Response) -> Option<&str>>,
        commit_check: Option<fn(&T::Response) -> bool>,
        request_timeout: Duration,
        navigation_timeout: Option<Duration>,
        committed: bool,
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
            command,
            command_done: false,
            navigation: TargetMessageFuture::<T>::wait_for_navigation(sender, request_timeout),
            failure_check: None,
            commit_check: None,
            request_timeout,
            navigation_timeout: None,
            committed: false,
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
            command,
            command_done: false,
            navigation: TargetMessageFuture::<T>::wait_for_navigation(sender, request_timeout),
            failure_check: Some(failure_check),
            commit_check: None,
            request_timeout,
            navigation_timeout: None,
            committed: false,
            url,
        }
    }

    /// Arm a navigation deadline of `cap = min(timeout, request_timeout)`.
    ///
    /// The handler releases a held navigate ack at `cap` instead of dropping it
    /// at the request timeout, and the post-ack lifecycle wait is then reset to
    /// a fresh `cap`. The two phases run in sequence, so the worst case wall
    /// clock is `2 * cap`. Phase two is normally instant because a committed
    /// frame has already reached `DOMContentLoaded`. See
    /// [`crate::Page::navigate_http_future_with_timeout`] for the full budget.
    pub fn with_navigation_timeout(mut self, timeout: Duration) -> Self {
        let capped = timeout.min(self.request_timeout);
        self.navigation_timeout = Some(capped);
        self.command.set_navigation_timeout(capped);
        self.navigation.set_timeout(capped);
        self
    }

    /// Record whether the command response means the navigation committed.
    pub fn with_commit_check(mut self, check: fn(&T::Response) -> bool) -> Self {
        self.commit_check = Some(check);
        self
    }

    /// Navigate ack seen with a loaderId and no terminal errorText.
    pub fn committed(&self) -> bool {
        self.committed
    }

    /// The capped navigation timeout, if one was set.
    pub fn navigation_timeout(&self) -> Option<Duration> {
        self.navigation_timeout
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
        if *this.command_done {
            this.navigation.poll(cx)
        } else {
            match this.command.poll(cx) {
                Poll::Ready(Ok(command_response)) => {
                    *this.command_done = true;
                    *this.committed = this
                        .commit_check
                        .map_or(false, |check| check(&command_response.result));
                    if let Some(check) = *this.failure_check {
                        if let Some(err) = check(&command_response.result) {
                            if !err.is_empty() && !navigation_continues(err) {
                                *this.committed = false;
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
                    // Command succeeded. Reset the navigation timer so it
                    // gets the capped timeout from now, not from when
                    // HttpFuture was constructed, then immediately start
                    // polling navigation (avoids a full wake round-trip).
                    this.navigation.as_mut().reset_deadline();
                    this.navigation.poll(cx)
                }
                Poll::Ready(Err(e)) => {
                    *this.command_done = true;
                    Poll::Ready(Err(e))
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }
}
