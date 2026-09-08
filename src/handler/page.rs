use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chromiumoxide_cdp::cdp::browser_protocol::accessibility::{
    GetFullAxTreeParamsBuilder, GetFullAxTreeReturns, GetPartialAxTreeParamsBuilder,
    GetPartialAxTreeReturns,
};
use tokio::sync::mpsc::{channel, Receiver};
use tokio::sync::oneshot::channel as oneshot_channel;
use tokio::sync::Notify;

use chromiumoxide_cdp::cdp::browser_protocol::browser::{GetVersionParams, GetVersionReturns};
use chromiumoxide_cdp::cdp::browser_protocol::dom::{
    BackendNodeId, DiscardSearchResultsParams, GetOuterHtmlParams, GetSearchResultsParams, NodeId,
    PerformSearchParams, QuerySelectorAllParams, QuerySelectorParams, Rgba,
};
use chromiumoxide_cdp::cdp::browser_protocol::emulation::{
    ClearDeviceMetricsOverrideParams, SetDefaultBackgroundColorOverrideParams,
    SetDeviceMetricsOverrideParams,
};
use chromiumoxide_cdp::cdp::browser_protocol::input::{
    DispatchDragEventParams, DispatchDragEventType, DispatchKeyEventParams, DispatchKeyEventType,
    DispatchMouseEventParams, DispatchMouseEventType, DragData, MouseButton,
};
use chromiumoxide_cdp::cdp::browser_protocol::page::{
    FrameId, GetLayoutMetricsParams, GetLayoutMetricsReturns, NavigateParams, PrintToPdfParams,
    SetBypassCspParams, Viewport,
};
use chromiumoxide_cdp::cdp::browser_protocol::target::{ActivateTargetParams, SessionId, TargetId};
use chromiumoxide_cdp::cdp::js_protocol::runtime::{
    CallFunctionOnParams, CallFunctionOnReturns, EvaluateParams, ExecutionContextId, RemoteObjectId,
};
use chromiumoxide_types::{Command, CommandResponse};

use crate::cmd::{to_command_response, CommandMessage};
use crate::error::{CdpError, Result};
use crate::handler::commandfuture::CommandFuture;
use crate::handler::domworld::DOMWorldKind;
use crate::handler::httpfuture::{navigate_committed, navigate_error_text, HttpFuture};
use crate::handler::sender::PageSender;
use crate::handler::target::{GetExecutionContext, TargetMessage};
use crate::handler::target_message_future::TargetMessageFuture;
use crate::js::EvaluationResult;
use crate::layout::{Delta, Point, ScrollBehavior};
use crate::mouse::SmartMouse;
use crate::page::ScreenshotParams;
use crate::{keys, utils, ArcHttpRequest};

/// Global count of live `PageInner` instances. Incremented on creation,
/// decremented on `Drop`. Used to dynamically tune memory-sensitive
/// thresholds (e.g. CDP body-streaming chunk size) under high concurrency.
static ACTIVE_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Returns the number of currently live page instances across the process.
#[inline]
pub fn active_page_count() -> usize {
    ACTIVE_PAGES.load(Ordering::Relaxed)
}

#[derive(Debug)]
pub struct PageHandle {
    pub(crate) rx: Receiver<TargetMessage>,
    page: Arc<PageInner>,
}

/// Default capacity of the per-page `TargetMessage` channel.
///
/// Historical hard-coded value preserved for backwards compatibility —
/// `PageHandle::new` delegates to `PageHandle::with_capacity` with this
/// value. Override via `HandlerConfig::page_channel_capacity` (plumbed
/// through `BrowserConfigBuilder::page_channel_capacity`) to tune under
/// bursty per-page command load, which otherwise forces every extra
/// command into the `CommandFuture` async-send fallback path.
pub(crate) const DEFAULT_PAGE_CHANNEL_CAPACITY: usize = 2048;

impl PageHandle {
    /// Create a `PageHandle` with the default per-page channel capacity
    /// (`DEFAULT_PAGE_CHANNEL_CAPACITY`). Preserved unchanged for
    /// backwards compatibility — call `with_capacity` to override.
    pub fn new(
        target_id: TargetId,
        session_id: SessionId,
        opener_id: Option<TargetId>,
        request_timeout: std::time::Duration,
        page_wake: Option<Arc<Notify>>,
    ) -> Self {
        Self::with_capacity(
            target_id,
            session_id,
            opener_id,
            request_timeout,
            page_wake,
            DEFAULT_PAGE_CHANNEL_CAPACITY,
        )
    }

    /// Create a `PageHandle` with a caller-chosen channel capacity.
    ///
    /// `capacity` is the tokio mpsc buffer size for `TargetMessage`s flowing
    /// from this page to the handler. Capacity is clamped to at least `1`
    /// because `tokio::sync::mpsc::channel(0)` panics; callers passing `0`
    /// get a 1-slot channel instead of an abort.
    ///
    /// Under bursty per-page load, larger capacities reduce the rate at
    /// which `CommandFuture` / `TargetMessageFuture` fall back to the
    /// boxed async-send slow path on `TrySendError::Full`; smaller
    /// capacities apply back-pressure sooner at the cost of that fallback.
    pub fn with_capacity(
        target_id: TargetId,
        session_id: SessionId,
        opener_id: Option<TargetId>,
        request_timeout: std::time::Duration,
        page_wake: Option<Arc<Notify>>,
        capacity: usize,
    ) -> Self {
        let (commands, rx) = channel(capacity.max(1));
        let page = PageInner {
            target_id,
            session_id,
            opener_id,
            sender: PageSender::new(commands, page_wake),
            smart_mouse: SmartMouse::new(),
            request_timeout,
        };
        ACTIVE_PAGES.fetch_add(1, Ordering::Relaxed);
        Self {
            rx,
            page: Arc::new(page),
        }
    }

    pub(crate) fn inner(&self) -> &Arc<PageInner> {
        &self.page
    }
}

#[derive(Debug)]
pub(crate) struct PageInner {
    /// The page target ID.
    target_id: TargetId,
    /// The session ID.
    session_id: SessionId,
    /// The opener ID.
    opener_id: Option<TargetId>,
    /// The sender for the target (with optional handler notification).
    sender: PageSender,
    /// Smart mouse with position tracking and human-like movement.
    pub(crate) smart_mouse: SmartMouse,
    /// The request timeout for CDP commands issued from this page.
    request_timeout: std::time::Duration,
}

impl Drop for PageInner {
    fn drop(&mut self) {
        ACTIVE_PAGES.fetch_sub(1, Ordering::Relaxed);
    }
}

impl PageInner {
    /// Execute a PDL command and return its response
    pub(crate) async fn execute<T: Command>(&self, cmd: T) -> Result<CommandResponse<T::Response>> {
        execute(
            cmd,
            self.sender.clone(),
            Some(self.session_id.clone()),
            self.request_timeout,
        )
        .await
    }

    /// Execute a PDL command without waiting for the response.
    pub(crate) async fn send_command<T: Command>(&self, cmd: T) -> Result<&Self> {
        let _ = send_command(
            cmd,
            self.sender.clone(),
            Some(self.session_id.clone()),
            self.request_timeout,
        )
        .await;
        Ok(self)
    }

    /// Create a PDL command future
    pub(crate) fn command_future<T: Command>(&self, cmd: T) -> Result<CommandFuture<T>> {
        CommandFuture::new(
            cmd,
            self.sender.clone(),
            Some(self.session_id.clone()),
            self.request_timeout,
        )
    }

    /// This creates navigation future with the final http response when the page is loaded
    pub(crate) fn wait_for_navigation(&self) -> TargetMessageFuture<ArcHttpRequest> {
        TargetMessageFuture::<ArcHttpRequest>::wait_for_navigation(
            self.sender.clone(),
            self.request_timeout,
        )
    }

    /// Resolves once `DOMContentLoaded` fires (before `load`).
    pub(crate) fn wait_for_dom_content_loaded(&self) -> TargetMessageFuture<ArcHttpRequest> {
        TargetMessageFuture::<ArcHttpRequest>::wait_for_dom_content_loaded(
            self.sender.clone(),
            self.request_timeout,
        )
    }

    /// Resolves once the `load` event fires (all subresources done).
    pub(crate) fn wait_for_load(&self) -> TargetMessageFuture<ArcHttpRequest> {
        TargetMessageFuture::<ArcHttpRequest>::wait_for_load(
            self.sender.clone(),
            self.request_timeout,
        )
    }

    /// This creates navigation future with the final http response when the page network is idle
    pub(crate) fn wait_for_network_idle(&self) -> TargetMessageFuture<ArcHttpRequest> {
        TargetMessageFuture::<ArcHttpRequest>::wait_for_network_idle(
            self.sender.clone(),
            self.request_timeout,
        )
    }

    /// This creates navigation future with the final http response when the page network is almost idle
    pub(crate) fn wait_for_network_almost_idle(&self) -> TargetMessageFuture<ArcHttpRequest> {
        TargetMessageFuture::<ArcHttpRequest>::wait_for_network_almost_idle(
            self.sender.clone(),
            self.request_timeout,
        )
    }

    /// This creates HTTP future with navigation and responds with the final
    /// http response when the page is loaded
    /// This generic path does not infer a navigation timeout from command params.
    pub(crate) fn http_future<T: Command>(&self, cmd: T) -> Result<HttpFuture<T>> {
        Ok(HttpFuture::new(
            self.sender.clone(),
            self.command_future(cmd)?,
            self.request_timeout,
        ))
    }

    pub(crate) fn navigate_http_future(
        &self,
        params: NavigateParams,
    ) -> Result<HttpFuture<NavigateParams>> {
        let url = params.url.clone();
        let navigation_timeout = params.timeout.filter(|ms| *ms > 0);
        let cmd = self.command_future(params)?;
        let mut future = HttpFuture::with_failure_check(
            self.sender.clone(),
            cmd,
            self.request_timeout,
            navigate_error_text,
            Some(url),
        )
        .with_commit_check(navigate_committed);
        if let Some(ms) = navigation_timeout {
            future = future.with_navigation_timeout(Duration::from_millis(ms as u64));
        }
        Ok(future)
    }

    /// The identifier of this page's target
    pub fn target_id(&self) -> &TargetId {
        &self.target_id
    }

    /// The identifier of this page's target's session
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The identifier of this page's target's opener target
    pub fn opener_id(&self) -> &Option<TargetId> {
        &self.opener_id
    }

    /// Send a `TargetMessage` with the page's request timeout.
    /// Uses a `try_send` fast path to avoid the async overhead when
    /// the channel has capacity (common case under normal load).
    pub(crate) async fn send_msg(&self, msg: TargetMessage) -> Result<()> {
        match self.sender.try_send(msg) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(msg)) => {
                tokio::time::timeout(self.request_timeout, self.sender.send(msg))
                    .await
                    .map_err(|_| CdpError::Timeout)?
                    .map_err(|_| CdpError::ChannelSendError(crate::error::ChannelError::Send))?;
                Ok(())
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err(CdpError::ChannelSendError(crate::error::ChannelError::Send))
            }
        }
    }

    /// Await a oneshot response with the page's request timeout.
    pub(crate) async fn recv_msg<T>(&self, rx: tokio::sync::oneshot::Receiver<T>) -> Result<T> {
        tokio::time::timeout(self.request_timeout, rx)
            .await
            .map_err(|_| CdpError::Timeout)?
            .map_err(|e| CdpError::ChannelSendError(crate::error::ChannelError::Canceled(e)))
    }

    /// Returns the first element in the node which matches the given CSS
    /// selector.
    pub async fn find_element(&self, selector: impl Into<String>, node: NodeId) -> Result<NodeId> {
        Ok(self
            .execute(QuerySelectorParams::new(node, selector))
            .await?
            .node_id)
    }

    /// Returns the outer html of the page.
    pub async fn outer_html(
        &self,
        object_id: RemoteObjectId,
        node_id: NodeId,
        backend_node_id: BackendNodeId,
    ) -> Result<String> {
        let cmd = GetOuterHtmlParams {
            backend_node_id: Some(backend_node_id),
            node_id: Some(node_id),
            object_id: Some(object_id),
            ..Default::default()
        };

        let chromiumoxide_types::CommandResponse { result, .. } = self.execute(cmd).await?;

        Ok(result.outer_html)
    }

    /// Activates (focuses) the target.
    pub async fn activate(&self) -> Result<&Self> {
        self.execute(ActivateTargetParams::new(self.target_id().clone()))
            .await?;
        Ok(self)
    }

    /// Version information about the browser
    pub async fn version(&self) -> Result<GetVersionReturns> {
        Ok(self.execute(GetVersionParams::default()).await?.result)
    }

    /// Return all `Element`s inside the node that match the given selector
    pub(crate) async fn find_elements(
        &self,
        selector: impl Into<String>,
        node: NodeId,
    ) -> Result<Vec<NodeId>> {
        Ok(self
            .execute(QuerySelectorAllParams::new(node, selector))
            .await?
            .result
            .node_ids)
    }

    /// Returns all elements which matches the given xpath selector
    pub async fn find_xpaths(&self, query: impl Into<String>) -> Result<Vec<NodeId>> {
        let perform_search_returns = self
            .execute(PerformSearchParams {
                query: query.into(),
                include_user_agent_shadow_dom: Some(true),
            })
            .await?
            .result;

        let search_results = self
            .execute(GetSearchResultsParams::new(
                perform_search_returns.search_id.clone(),
                0,
                perform_search_returns.result_count,
            ))
            .await?
            .result;

        self.execute(DiscardSearchResultsParams::new(
            perform_search_returns.search_id,
        ))
        .await?;

        Ok(search_results.node_ids)
    }

    /// Moves the mouse to this point (dispatches a mouseMoved event).
    /// Also updates the tracked mouse position.
    pub async fn move_mouse(&self, point: Point) -> Result<&Self> {
        self.smart_mouse.set_position(point);
        self.execute(DispatchMouseEventParams::new(
            DispatchMouseEventType::MouseMoved,
            point.x,
            point.y,
        ))
        .await?;
        Ok(self)
    }

    /// Moves the mouse to `target` along a human-like bezier curve path,
    /// dispatching intermediate `mouseMoved` events with natural timing.
    ///
    /// Concurrency shape:
    /// * Each step is dispatched fire-and-forget via
    ///   [`send_command`](Self::send_command) — the CDP command lands on
    ///   the handler channel (with `try_send` + async fallback) and the
    ///   response oneshot is dropped. Per-step CDP round-trips (~1-2ms
    ///   each) are eliminated.
    /// * Pacing uses [`tokio::time::sleep_until`] against an absolute
    ///   deadline that accumulates each step's delay. This is
    ///   **drift-free**: if one iteration's send or scheduler wake-up
    ///   runs long, the next deadline is unchanged so the following
    ///   wait shrinks to compensate. Total wall-clock convergence is
    ///   `sum(step.delay)` regardless of per-iteration variance.
    ///
    /// Ordering: CDP processes commands in the order they arrive on the
    /// single-session WebSocket, and the bounded mpsc between `Page`
    /// and handler is FIFO, so the sequence of `mouseMoved` events
    /// reaches Chrome in issue order.
    pub async fn move_mouse_smooth(&self, target: Point) -> Result<&Self> {
        let path = self.smart_mouse.path_to(target);
        let last_idx = path.len().saturating_sub(1);
        let mut deadline = tokio::time::Instant::now();
        for (i, step) in path.iter().enumerate() {
            self.send_command(DispatchMouseEventParams::new(
                DispatchMouseEventType::MouseMoved,
                step.point.x,
                step.point.y,
            ))
            .await?;
            // Absolute-deadline pacing: advancing by `step.delay`
            // means a slow send on iteration N shortens the wait
            // before iteration N+1 instead of pushing the whole
            // schedule back.
            //
            // Skip the sleep after the final step — there's no
            // subsequent event to pace against, so the delay would
            // be pure wall-clock waste (~5-30ms typical).
            if i < last_idx {
                deadline += step.delay;
                tokio::time::sleep_until(deadline).await;
            }
        }
        Ok(self)
    }

    /// Returns the current tracked mouse position.
    pub fn mouse_position(&self) -> Point {
        self.smart_mouse.position()
    }

    /// Scrolls the current page by the specified horizontal and vertical offsets.
    /// This method helps when Chrome version may not support certain CDP dispatch events.
    pub async fn scroll_by(
        &self,
        delta_x: f64,
        delta_y: f64,
        behavior: ScrollBehavior,
    ) -> Result<&Self> {
        let behavior_str = match behavior {
            ScrollBehavior::Auto => "auto",
            ScrollBehavior::Instant => "instant",
            ScrollBehavior::Smooth => "smooth",
        };

        self.evaluate_expression(format!(
            "window.scrollBy({{top: {}, left: {}, behavior: '{}'}});",
            delta_y, delta_x, behavior_str
        ))
        .await?;

        Ok(self)
    }

    /// Dispatches a `DragEvent`, moving the element to the given `point`.
    ///
    /// `point.x` defines the horizontal target, and `point.y` the vertical mouse position.
    /// Accepts `drag_type`, `drag_data`, and optional keyboard `modifiers`.
    pub async fn drag(
        &self,
        drag_type: DispatchDragEventType,
        point: Point,
        drag_data: DragData,
        modifiers: Option<i64>,
    ) -> Result<&Self> {
        let mut params: DispatchDragEventParams =
            DispatchDragEventParams::new(drag_type, point.x, point.y, drag_data);

        if let Some(modifiers) = modifiers {
            params.modifiers = Some(modifiers);
        }

        self.execute(params).await?;
        Ok(self)
    }

    /// Moves the mouse to this point (dispatches a mouseWheel event).
    /// If you get an error use page.scroll_by instead.
    pub async fn scroll(&self, point: Point, delta: Delta) -> Result<&Self> {
        let mut params: DispatchMouseEventParams =
            DispatchMouseEventParams::new(DispatchMouseEventType::MouseWheel, point.x, point.y);

        params.delta_x = Some(delta.delta_x);
        params.delta_y = Some(delta.delta_y);

        self.execute(params).await?;
        Ok(self)
    }

    /// Performs a mouse click event at the point's location with the amount of clicks and modifier.
    pub async fn click_with_count_base(
        &self,
        point: Point,
        click_count: impl Into<i64>,
        modifiers: impl Into<i64>,
        button: impl Into<MouseButton>,
    ) -> Result<&Self> {
        let cmd = DispatchMouseEventParams::builder()
            .x(point.x)
            .y(point.y)
            .button(button)
            .click_count(click_count)
            .modifiers(modifiers);

        if let Ok(cmd) = cmd
            .clone()
            .r#type(DispatchMouseEventType::MousePressed)
            .build()
        {
            self.move_mouse(point).await?;
            // Pre-click dwell: real users hold the cursor at the target
            // for a few tens of ms before pressing. A zero-gap
            // mousemove→mousedown is a common antibot fingerprint.
            // Disabled by setting `pre_click_dwell_ms = None` (or
            // `Some((0, 0))`) on the SmartMouseConfig.
            if let Some(dwell) = self.smart_mouse.pre_click_dwell() {
                tokio::time::sleep(dwell).await;
            }
            self.send_command(cmd).await?;
        }

        if let Ok(cmd) = cmd.r#type(DispatchMouseEventType::MouseReleased).build() {
            self.execute(cmd).await?;
        }

        self.smart_mouse.set_position(point);
        Ok(self)
    }

    /// Move smoothly to `point` with human-like movement, then click.
    pub async fn click_smooth(&self, point: Point) -> Result<&Self> {
        self.move_mouse_smooth(point).await?;
        self.click(point).await
    }

    /// Performs a mouse click event at the point's location with the amount of clicks and modifier.
    pub async fn click_with_count(
        &self,
        point: Point,
        click_count: impl Into<i64>,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.click_with_count_base(point, click_count, modifiers, MouseButton::Left)
            .await
    }

    /// Performs a mouse right click event at the point's location with the amount of clicks and modifier.
    pub async fn right_click_with_count(
        &self,
        point: Point,
        click_count: impl Into<i64>,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.click_with_count_base(point, click_count, modifiers, MouseButton::Right)
            .await
    }

    /// Performs a mouse middle click event at the point's location with the amount of clicks and modifier.
    pub async fn middle_click_with_count(
        &self,
        point: Point,
        click_count: impl Into<i64>,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.click_with_count_base(point, click_count, modifiers, MouseButton::Middle)
            .await
    }

    /// Performs a mouse back click event at the point's location with the amount of clicks and modifier.
    pub async fn back_click_with_count(
        &self,
        point: Point,
        click_count: impl Into<i64>,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.click_with_count_base(point, click_count, modifiers, MouseButton::Back)
            .await
    }

    /// Performs a mouse forward click event at the point's location with the amount of clicks and modifier.
    pub async fn forward_click_with_count(
        &self,
        point: Point,
        click_count: impl Into<i64>,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.click_with_count_base(point, click_count, modifiers, MouseButton::Forward)
            .await
    }

    /// Performs a click-and-drag from one point to another with optional modifiers.
    pub async fn click_and_drag(
        &self,
        from: Point,
        to: Point,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        let modifiers = modifiers.into();
        let click_count = 1;

        let cmd = DispatchMouseEventParams::builder()
            .button(MouseButton::Left)
            .click_count(click_count)
            .modifiers(modifiers);

        if let Ok(cmd) = cmd
            .clone()
            .x(from.x)
            .y(from.y)
            .r#type(DispatchMouseEventType::MousePressed)
            .build()
        {
            self.move_mouse(from).await?.send_command(cmd).await?;
        }

        if let Ok(cmd) = cmd
            .clone()
            .x(to.x)
            .y(to.y)
            .r#type(DispatchMouseEventType::MouseMoved)
            .build()
        {
            self.move_mouse(to).await?.send_command(cmd).await?;
        }

        if let Ok(cmd) = cmd
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(to.x)
            .y(to.y)
            .build()
        {
            self.send_command(cmd).await?;
        }

        self.smart_mouse.set_position(to);
        Ok(self)
    }

    /// Performs a smooth click-and-drag: moves to `from` with a bezier path,
    /// presses, drags along a bezier path to `to`, then releases.
    pub async fn click_and_drag_smooth(
        &self,
        from: Point,
        to: Point,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        let modifiers = modifiers.into();

        // Smooth move to the starting point
        self.move_mouse_smooth(from).await?;

        // Press at starting point
        if let Ok(cmd) = DispatchMouseEventParams::builder()
            .x(from.x)
            .y(from.y)
            .button(MouseButton::Left)
            .click_count(1)
            .modifiers(modifiers)
            .r#type(DispatchMouseEventType::MousePressed)
            .build()
        {
            self.send_command(cmd).await?;
        }

        // Smooth drag to destination (dispatching MouseMoved with button held).
        // Absolute-deadline pacing (same shape as `move_mouse_smooth`):
        // a slow send on step N shortens the wait before step N+1
        // instead of shifting the whole schedule, so total time tracks
        // `sum(step.delay)` regardless of per-iteration variance. The
        // final step's sleep is skipped — the MouseReleased dispatch
        // follows immediately and any trailing delay would just be
        // wall-clock waste.
        let path = self.smart_mouse.path_to(to);
        let last_idx = path.len().saturating_sub(1);
        let mut deadline = tokio::time::Instant::now();
        for (i, step) in path.iter().enumerate() {
            if let Ok(cmd) = DispatchMouseEventParams::builder()
                .x(step.point.x)
                .y(step.point.y)
                .button(MouseButton::Left)
                .modifiers(modifiers)
                .r#type(DispatchMouseEventType::MouseMoved)
                .build()
            {
                self.send_command(cmd).await?;
            }
            if i < last_idx {
                deadline += step.delay;
                tokio::time::sleep_until(deadline).await;
            }
        }

        // Release at destination
        if let Ok(cmd) = DispatchMouseEventParams::builder()
            .x(to.x)
            .y(to.y)
            .button(MouseButton::Left)
            .click_count(1)
            .modifiers(modifiers)
            .r#type(DispatchMouseEventType::MouseReleased)
            .build()
        {
            self.send_command(cmd).await?;
        }

        Ok(self)
    }

    /// Performs a mouse click event at the point's location
    pub async fn click(&self, point: Point) -> Result<&Self> {
        self.click_with_count(point, 1, 0).await
    }

    /// Performs a mouse double click event at the point's location
    pub async fn double_click(&self, point: Point) -> Result<&Self> {
        self.click_with_count(point, 2, 0).await
    }

    /// Performs a mouse right click event at the point's location
    pub async fn right_click(&self, point: Point) -> Result<&Self> {
        self.right_click_with_count(point, 1, 0).await
    }

    /// Performs a mouse middle click event at the point's location
    pub async fn middle_click(&self, point: Point) -> Result<&Self> {
        self.middle_click_with_count(point, 1, 0).await
    }

    /// Performs a mouse back click event at the point's location
    pub async fn back_click(&self, point: Point) -> Result<&Self> {
        self.back_click_with_count(point, 1, 0).await
    }

    /// Performs a mouse forward click event at the point's location
    pub async fn forward_click(&self, point: Point) -> Result<&Self> {
        self.forward_click_with_count(point, 1, 0).await
    }

    /// Performs a mouse click event at the point's location and modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    pub async fn click_with_modifier(
        &self,
        point: Point,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.click_with_count(point, 1, modifiers).await
    }

    /// Performs a mouse right click event at the point's location and modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    pub async fn right_click_with_modifier(
        &self,
        point: Point,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.right_click_with_count(point, 1, modifiers).await
    }

    /// Performs a mouse middle click event at the point's location and modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    pub async fn middle_click_with_modifier(
        &self,
        point: Point,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.middle_click_with_count(point, 1, modifiers).await
    }

    /// Performs a mouse double click event at the point's location and modifier: Alt=1, Ctrl=2, Meta/Command=4, Shift=8\n(default: 0).
    pub async fn double_click_with_modifier(
        &self,
        point: Point,
        modifiers: impl Into<i64>,
    ) -> Result<&Self> {
        self.click_with_count(point, 2, modifiers).await
    }

    /// This simulates pressing keys on the page.
    ///
    /// # Note The `input` is treated as series of `KeyDefinition`s, where each
    /// char is inserted as a separate keystroke. So sending
    /// `page.type_str("Enter")` will be processed as a series of single
    /// keystrokes:  `["E", "n", "t", "e", "r"]`. To simulate pressing the
    /// actual Enter key instead use `page.press_key(
    /// keys::get_key_definition("Enter").unwrap())`.
    pub async fn type_str(&self, input: impl AsRef<str>) -> Result<&Self> {
        for c in input.as_ref().split("").filter(|s| !s.is_empty()) {
            self._press_key(c, None).await?;
        }
        Ok(self)
    }

    /// Fetches the entire accessibility tree for the root Document
    pub async fn get_full_ax_tree(
        &self,
        depth: Option<i64>,
        frame_id: Option<FrameId>,
    ) -> Result<GetFullAxTreeReturns> {
        let mut builder = GetFullAxTreeParamsBuilder::default();

        if let Some(depth) = depth {
            builder = builder.depth(depth);
        }

        if let Some(frame_id) = frame_id {
            builder = builder.frame_id(frame_id);
        }

        let resp = self.execute(builder.build()).await?;

        Ok(resp.result)
    }

    /// Fetches the accessibility node and partial accessibility tree for this DOM node, if it exists.
    pub async fn get_partial_ax_tree(
        &self,
        node_id: Option<chromiumoxide_cdp::cdp::browser_protocol::dom::NodeId>,
        backend_node_id: Option<BackendNodeId>,
        object_id: Option<RemoteObjectId>,
        fetch_relatives: Option<bool>,
    ) -> Result<GetPartialAxTreeReturns> {
        let mut builder = GetPartialAxTreeParamsBuilder::default();

        if let Some(node_id) = node_id {
            builder = builder.node_id(node_id);
        }

        if let Some(backend_node_id) = backend_node_id {
            builder = builder.backend_node_id(backend_node_id);
        }

        if let Some(object_id) = object_id {
            builder = builder.object_id(object_id);
        }

        if let Some(fetch_relatives) = fetch_relatives {
            builder = builder.fetch_relatives(fetch_relatives);
        }

        let resp = self.execute(builder.build()).await?;

        Ok(resp.result)
    }

    /// This simulates pressing keys on the page.
    ///
    /// # Note The `input` is treated as series of `KeyDefinition`s, where each
    /// char is inserted as a separate keystroke. So sending
    /// `page.type_str("Enter")` will be processed as a series of single
    /// keystrokes:  `["E", "n", "t", "e", "r"]`. To simulate pressing the
    /// actual Enter key instead use `page.press_key(
    /// keys::get_key_definition("Enter").unwrap())`.
    pub async fn type_str_with_modifier(
        &self,
        input: impl AsRef<str>,
        modifiers: Option<i64>,
    ) -> Result<&Self> {
        for c in input.as_ref().split("").filter(|s| !s.is_empty()) {
            self._press_key(c, modifiers).await?;
        }
        Ok(self)
    }

    /// Uses the `DispatchKeyEvent` mechanism to simulate pressing keyboard
    /// keys.
    async fn _press_key(&self, key: impl AsRef<str>, modifiers: Option<i64>) -> Result<&Self> {
        let key = key.as_ref();
        let key_definition = keys::get_key_definition(key)
            .ok_or_else(|| CdpError::msg(format!("Key not found: {key}")))?;
        let mut cmd = DispatchKeyEventParams::builder();

        // See https://github.com/GoogleChrome/puppeteer/blob/62da2366c65b335751896afbb0206f23c61436f1/lib/Input.js#L114-L115
        // And https://github.com/GoogleChrome/puppeteer/blob/62da2366c65b335751896afbb0206f23c61436f1/lib/Input.js#L52
        let key_down_event_type = if let Some(txt) = key_definition.text {
            cmd = cmd.text(txt);
            DispatchKeyEventType::KeyDown
        } else if key_definition.key.len() == 1 {
            cmd = cmd.text(key_definition.key);
            DispatchKeyEventType::KeyDown
        } else {
            DispatchKeyEventType::RawKeyDown
        };

        cmd = cmd
            .r#type(DispatchKeyEventType::KeyDown)
            .key(key_definition.key)
            .code(key_definition.code)
            .windows_virtual_key_code(key_definition.key_code)
            .native_virtual_key_code(key_definition.key_code);

        if let Some(modifiers) = modifiers {
            cmd = cmd.modifiers(modifiers);
        }

        let key_down = cmd.clone().r#type(key_down_event_type).build().ok();
        let key_up = cmd.r#type(DispatchKeyEventType::KeyUp).build().ok();

        match (key_down, key_up) {
            (Some(kd), Some(ku)) => {
                tokio::try_join!(self.execute(kd), self.execute(ku))?;
            }
            (Some(kd), None) => {
                self.execute(kd).await?;
            }
            (None, Some(ku)) => {
                self.execute(ku).await?;
            }
            (None, None) => {}
        }

        Ok(self)
    }

    /// Uses the `DispatchKeyEvent` mechanism to simulate pressing keyboard
    /// keys.
    pub async fn press_key(&self, key: impl AsRef<str>) -> Result<&Self> {
        self._press_key(key, None).await
    }

    /// Uses the `DispatchKeyEvent` mechanism to simulate pressing keyboard
    /// keys and modifiers.
    pub async fn press_key_with_modifier(
        &self,
        key: impl AsRef<str>,
        modifiers: Option<i64>,
    ) -> Result<&Self> {
        self._press_key(key, modifiers).await
    }

    /// Calls function with given declaration on the remote object with the
    /// matching id
    pub async fn call_js_fn(
        &self,
        function_declaration: impl Into<String>,
        await_promise: bool,
        remote_object_id: RemoteObjectId,
    ) -> Result<CallFunctionOnReturns> {
        if let Ok(resp) = CallFunctionOnParams::builder()
            .object_id(remote_object_id)
            .function_declaration(function_declaration)
            .generate_preview(true)
            .await_promise(await_promise)
            .build()
        {
            let resp = self.execute(resp).await?;
            Ok(resp.result)
        } else {
            Err(CdpError::NotFound)
        }
    }

    pub async fn evaluate_expression(
        &self,
        evaluate: impl Into<EvaluateParams>,
    ) -> Result<EvaluationResult> {
        let mut evaluate = evaluate.into();
        if evaluate.context_id.is_none() {
            evaluate.context_id = self.execution_context().await?;
        }
        if evaluate.await_promise.is_none() {
            evaluate.await_promise = Some(true);
        }
        if evaluate.return_by_value.is_none() {
            evaluate.return_by_value = Some(true);
        }

        // evaluate.silent = Some(true);

        let resp = self.execute(evaluate).await?.result;

        if let Some(exception) = resp.exception_details {
            return Err(CdpError::JavascriptException(Box::new(exception)));
        }

        Ok(EvaluationResult::new(resp.result))
    }

    pub async fn evaluate_function(
        &self,
        evaluate: impl Into<CallFunctionOnParams>,
    ) -> Result<EvaluationResult> {
        let mut evaluate = evaluate.into();
        if evaluate.execution_context_id.is_none() {
            evaluate.execution_context_id = self.execution_context().await?;
        }
        if evaluate.await_promise.is_none() {
            evaluate.await_promise = Some(true);
        }
        if evaluate.return_by_value.is_none() {
            evaluate.return_by_value = Some(true);
        }

        // evaluate.silent = Some(true);

        let resp = self.execute(evaluate).await?.result;
        if let Some(exception) = resp.exception_details {
            return Err(CdpError::JavascriptException(Box::new(exception)));
        }
        Ok(EvaluationResult::new(resp.result))
    }

    pub async fn execution_context(&self) -> Result<Option<ExecutionContextId>> {
        self.execution_context_for_world(None, DOMWorldKind::Main)
            .await
    }

    pub async fn secondary_execution_context(&self) -> Result<Option<ExecutionContextId>> {
        self.execution_context_for_world(None, DOMWorldKind::Secondary)
            .await
    }

    pub async fn frame_execution_context(
        &self,
        frame_id: FrameId,
    ) -> Result<Option<ExecutionContextId>> {
        self.execution_context_for_world(Some(frame_id), DOMWorldKind::Main)
            .await
    }

    pub async fn frame_secondary_execution_context(
        &self,
        frame_id: FrameId,
    ) -> Result<Option<ExecutionContextId>> {
        self.execution_context_for_world(Some(frame_id), DOMWorldKind::Secondary)
            .await
    }

    pub async fn execution_context_for_world(
        &self,
        frame_id: Option<FrameId>,
        dom_world: DOMWorldKind,
    ) -> Result<Option<ExecutionContextId>> {
        let (tx, rx) = oneshot_channel();
        let msg = TargetMessage::GetExecutionContext(GetExecutionContext {
            dom_world,
            frame_id,
            tx,
        });
        match self.sender.try_send(msg) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(msg)) => {
                tokio::time::timeout(self.request_timeout, self.sender.send(msg))
                    .await
                    .map_err(|_| CdpError::Timeout)?
                    .map_err(|_| CdpError::ChannelSendError(crate::error::ChannelError::Send))?;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                return Err(CdpError::ChannelSendError(crate::error::ChannelError::Send));
            }
        }
        Ok(tokio::time::timeout(self.request_timeout, rx)
            .await
            .map_err(|_| CdpError::Timeout)??)
    }

    /// Returns metrics relating to the layout of the page
    pub async fn layout_metrics(&self) -> Result<GetLayoutMetricsReturns> {
        Ok(self
            .execute(GetLayoutMetricsParams::default())
            .await?
            .result)
    }

    /// Enable page Content Security Policy by-passing.
    pub async fn set_bypass_csp(&self, enabled: bool) -> Result<&Self> {
        self.execute(SetBypassCspParams::new(enabled)).await?;
        Ok(self)
    }

    /// Take a screenshot of the page.
    pub async fn screenshot(&self, params: impl Into<ScreenshotParams>) -> Result<Vec<u8>> {
        self.activate().await?;
        let params = params.into();
        let full_page = params.full_page();
        let omit_background = params.omit_background();

        let mut cdp_params = params.cdp_params;

        if full_page {
            let metrics = self.layout_metrics().await?;
            let width = metrics.css_content_size.width;
            let height = metrics.css_content_size.height;

            cdp_params.clip = Some(Viewport {
                x: 0.,
                y: 0.,
                width,
                height,
                scale: 1.,
            });

            self.execute(SetDeviceMetricsOverrideParams::new(
                width as i64,
                height as i64,
                1.,
                false,
            ))
            .await?;
        }

        if omit_background {
            self.execute(SetDefaultBackgroundColorOverrideParams {
                color: Some(Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: Some(0.),
                }),
            })
            .await?;
        }

        let res = self.execute(cdp_params).await?.result;

        if omit_background {
            self.send_command(SetDefaultBackgroundColorOverrideParams { color: None })
                .await?;
        }

        if full_page {
            self.send_command(ClearDeviceMetricsOverrideParams {})
                .await?;
        }

        Ok(utils::base64::decode(&res.data)?)
    }

    /// Convert the page to PDF.
    pub async fn print_to_pdf(&self, params: impl Into<PrintToPdfParams>) -> Result<Vec<u8>> {
        self.activate().await?;
        let params = params.into();

        let res = self.execute(params).await?.result;

        Ok(utils::base64::decode(&res.data)?)
    }
}

pub(crate) async fn execute<T: Command>(
    cmd: T,
    sender: PageSender,
    session: Option<SessionId>,
    request_timeout: std::time::Duration,
) -> Result<CommandResponse<T::Response>> {
    let method = cmd.identifier();
    let rx = send_command(cmd, sender, session, request_timeout).await?;
    let resp = tokio::time::timeout(request_timeout, rx)
        .await
        .map_err(|_| CdpError::Timeout)???;
    to_command_response::<T>(resp, method)
}

/// Execute a command without waiting.
///
/// Uses a `try_send` fast path to avoid async overhead when the channel has
/// capacity (common case). Falls back to an async send with timeout when full.
pub(crate) async fn send_command<T: Command>(
    cmd: T,
    sender: PageSender,
    session: Option<SessionId>,
    request_timeout: std::time::Duration,
) -> Result<tokio::sync::oneshot::Receiver<Result<chromiumoxide_types::Response, CdpError>>> {
    let (tx, rx) = oneshot_channel();
    let msg = CommandMessage::with_session(cmd, tx, session)?;
    let target_msg = TargetMessage::Command(msg);
    match sender.try_send(target_msg) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(msg)) => {
            tokio::time::timeout(request_timeout, sender.send(msg))
                .await
                .map_err(|_| CdpError::Timeout)?
                .map_err(|_| CdpError::ChannelSendError(crate::error::ChannelError::Send))?;
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            return Err(CdpError::ChannelSendError(crate::error::ChannelError::Send));
        }
    }
    Ok(rx)
}

#[cfg(test)]
mod page_channel_capacity_tests {
    //! Unit tests for `PageHandle::with_capacity` and the capacity plumbing.
    //!
    //! These verify the three guarantees that keep the change
    //! backwards-compatible and panic-free:
    //!
    //! 1. `PageHandle::new` (legacy signature) still allocates the historic
    //!    2048-slot channel — proven by filling the channel to exactly
    //!    `DEFAULT_PAGE_CHANNEL_CAPACITY` via `try_send` and then expecting
    //!    the next `try_send` to return `Full`. This pins the default.
    //!
    //! 2. `PageHandle::with_capacity(N)` allocates an N-slot channel for
    //!    any N, verified the same way at a small N so the test runs fast.
    //!
    //! 3. `PageHandle::with_capacity(0)` does NOT panic — we clamp to 1
    //!    internally because `tokio::sync::mpsc::channel(0)` aborts. The
    //!    resulting channel has exactly one slot.
    //!
    //! No browser / runtime is spun up — these tests are purely in-process
    //! on the mpsc primitives.
    use super::{PageHandle, DEFAULT_PAGE_CHANNEL_CAPACITY};
    use crate::handler::target::TargetMessage;
    use chromiumoxide_cdp::cdp::browser_protocol::target::{SessionId, TargetId};
    use std::time::Duration;
    use tokio::sync::mpsc::error::TrySendError;

    /// Trivial zero-cost `TargetMessage` for filling the channel.
    /// We're only exercising the bounded-mpsc slot count — the payload
    /// shape is irrelevant, so use the smallest variant.
    fn make_msg() -> TargetMessage {
        TargetMessage::BlockNetwork(false)
    }

    fn make_handle(capacity: usize) -> PageHandle {
        PageHandle::with_capacity(
            TargetId::from("t".to_string()),
            SessionId::from("s".to_string()),
            None,
            Duration::from_secs(30),
            None,
            capacity,
        )
    }

    /// Fill a page's channel to capacity via `try_send` and return the
    /// observed slot count — the number of sends that succeeded before
    /// the first `Full` error.
    fn observed_capacity(handle: &PageHandle, upper_bound: usize) -> usize {
        // `PageSender` is the page's send half; `try_send` surfaces the
        // underlying mpsc `TrySendError::Full` once the bounded buffer
        // is saturated without consuming a slot, which is what we count.
        let sender = &handle.page.sender;
        let mut sent = 0;
        for _ in 0..upper_bound {
            match sender.try_send(make_msg()) {
                Ok(()) => sent += 1,
                Err(TrySendError::Full(_)) => return sent,
                Err(TrySendError::Closed(_)) => {
                    panic!("channel unexpectedly closed at {sent} sends")
                }
            }
        }
        sent
    }

    #[test]
    fn new_delegates_to_default_capacity() {
        let handle = PageHandle::new(
            TargetId::from("t".to_string()),
            SessionId::from("s".to_string()),
            None,
            Duration::from_secs(30),
            None,
        );
        let n = observed_capacity(&handle, DEFAULT_PAGE_CHANNEL_CAPACITY + 16);
        assert_eq!(
            n, DEFAULT_PAGE_CHANNEL_CAPACITY,
            "legacy PageHandle::new must preserve the 2048-slot default"
        );
    }

    #[test]
    fn with_capacity_respects_arbitrary_value() {
        // Small N keeps the test fast; 4 is enough to distinguish from
        // the 2048 default.
        for n in [1_usize, 4, 16, 64] {
            let handle = make_handle(n);
            assert_eq!(
                observed_capacity(&handle, n + 16),
                n,
                "with_capacity({n}) should produce exactly {n} slots",
            );
        }
    }

    #[test]
    fn zero_capacity_is_clamped_to_one_and_does_not_panic() {
        // `tokio::sync::mpsc::channel(0)` panics — our `.max(1)` clamp
        // must turn that into a single-slot channel rather than an abort.
        let handle = make_handle(0);
        assert_eq!(
            observed_capacity(&handle, 4),
            1,
            "zero capacity must clamp to 1 and not panic"
        );
    }
}
