use std::sync::Arc;

use chromiumoxide_cdp::cdp::browser_protocol::accessibility::{
    GetFullAxTreeParamsBuilder, GetFullAxTreeReturns, GetPartialAxTreeParamsBuilder,
    GetPartialAxTreeReturns,
};
use futures::channel::mpsc::{channel, Receiver, Sender};
use futures::channel::oneshot::channel as oneshot_channel;
use futures::stream::Fuse;
use futures::{SinkExt, StreamExt};

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
    FrameId, GetLayoutMetricsParams, GetLayoutMetricsReturns, PrintToPdfParams, SetBypassCspParams,
    Viewport,
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
use crate::handler::httpfuture::HttpFuture;
use crate::handler::target::{GetExecutionContext, TargetMessage};
use crate::handler::target_message_future::TargetMessageFuture;
use crate::js::EvaluationResult;
use crate::layout::{Delta, Point, ScrollBehavior};
use crate::page::ScreenshotParams;
use crate::{keys, utils, ArcHttpRequest};

#[derive(Debug)]
pub struct PageHandle {
    pub(crate) rx: Fuse<Receiver<TargetMessage>>,
    page: Arc<PageInner>,
}

impl PageHandle {
    pub fn new(target_id: TargetId, session_id: SessionId, opener_id: Option<TargetId>) -> Self {
        let (commands, rx) = channel(100);
        let page = PageInner {
            target_id,
            session_id,
            opener_id,
            sender: commands,
        };
        Self {
            rx: rx.fuse(),
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
    /// The sender for the target.
    sender: Sender<TargetMessage>,
}

impl PageInner {
    /// Execute a PDL command and return its response
    pub(crate) async fn execute<T: Command>(&self, cmd: T) -> Result<CommandResponse<T::Response>> {
        execute(cmd, self.sender.clone(), Some(self.session_id.clone())).await
    }

    /// Execute a PDL command without waiting for the response.
    pub(crate) async fn send_command<T: Command>(&self, cmd: T) -> Result<&Self> {
        let _ = send_command(cmd, self.sender.clone(), Some(self.session_id.clone())).await;
        Ok(self)
    }

    /// Create a PDL command future
    pub(crate) fn command_future<T: Command>(&self, cmd: T) -> Result<CommandFuture<T>> {
        CommandFuture::new(cmd, self.sender.clone(), Some(self.session_id.clone()))
    }

    /// This creates navigation future with the final http response when the page is loaded
    pub(crate) fn wait_for_navigation(&self) -> TargetMessageFuture<ArcHttpRequest> {
        TargetMessageFuture::<ArcHttpRequest>::wait_for_navigation(self.sender.clone())
    }

    /// This creates navigation future with the final http response when the page network is idle
    pub(crate) fn wait_for_network_idle(&self) -> TargetMessageFuture<ArcHttpRequest> {
        TargetMessageFuture::<ArcHttpRequest>::wait_for_network_idle(self.sender.clone())
    }

    /// This creates navigation future with the final http response when the page network is almost idle
    pub(crate) fn wait_for_network_almost_idle(&self) -> TargetMessageFuture<ArcHttpRequest> {
        TargetMessageFuture::<ArcHttpRequest>::wait_for_network_almost_idle(self.sender.clone())
    }

    /// This creates HTTP future with navigation and responds with the final
    /// http response when the page is loaded
    pub(crate) fn http_future<T: Command>(&self, cmd: T) -> Result<HttpFuture<T>> {
        Ok(HttpFuture::new(
            self.sender.clone(),
            self.command_future(cmd)?,
        ))
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

    pub(crate) fn sender(&self) -> &Sender<TargetMessage> {
        &self.sender
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
        let mut cmd = GetOuterHtmlParams::default();

        cmd.backend_node_id = Some(backend_node_id);
        cmd.node_id = Some(node_id);
        cmd.object_id = Some(object_id);

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

    /// Moves the mouse to this point (dispatches a mouseMoved event)
    pub async fn move_mouse(&self, point: Point) -> Result<&Self> {
        self.execute(DispatchMouseEventParams::new(
            DispatchMouseEventType::MouseMoved,
            point.x,
            point.y,
        ))
        .await?;
        Ok(self)
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
            self.move_mouse(point).await?.send_command(cmd).await?;
        }

        if let Ok(cmd) = cmd.r#type(DispatchMouseEventType::MouseReleased).build() {
            self.execute(cmd).await?;
        }

        Ok(self)
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

        // Note: we may want to add some slight movement in between for advanced anti-bot bypassing.
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

        if let Ok(cmd) = cmd.clone().r#type(key_down_event_type).build() {
            self.execute(cmd).await?;
        }

        if let Ok(cmd) = cmd.r#type(DispatchKeyEventType::KeyUp).build() {
            self.execute(cmd).await?;
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
        self.sender
            .clone()
            .send(TargetMessage::GetExecutionContext(GetExecutionContext {
                dom_world,
                frame_id,
                tx,
            }))
            .await?;
        Ok(rx.await?)
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
    sender: Sender<TargetMessage>,
    session: Option<SessionId>,
) -> Result<CommandResponse<T::Response>> {
    let method = cmd.identifier();
    let rx = send_command(cmd, sender, session).await?;
    let resp = rx.await??;
    to_command_response::<T>(resp, method)
}

/// Execute a command without waiting
pub(crate) async fn send_command<T: Command>(
    cmd: T,
    mut sender: Sender<TargetMessage>,
    session: Option<SessionId>,
) -> Result<futures::channel::oneshot::Receiver<Result<chromiumoxide_types::Response, CdpError>>> {
    let (tx, rx) = oneshot_channel();
    let msg = CommandMessage::with_session(cmd, tx, session)?;
    sender.send(TargetMessage::Command(msg)).await?;
    Ok(rx)
}
