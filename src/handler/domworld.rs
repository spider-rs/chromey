use chromey_cdp::cdp::js_protocol::runtime::ExecutionContextId;

#[derive(Debug, Clone, Default)]
pub struct DOMWorld {
    /// The execution context ID associated with this world.
    execution_ctx: Option<ExecutionContextId>,
    /// A unique identifier for the execution context.
    execution_ctx_unique_id: Option<String>,
    /// Whether the world has been detached from the page.
    detached: bool,
}
impl DOMWorld {
    /// Returns a new instance representing the main world.
    pub fn main_world() -> Self {
        Self {
            execution_ctx: None,
            execution_ctx_unique_id: None,
            detached: false,
        }
    }

    /// Returns a new instance representing a secondary world.
    pub fn secondary_world() -> Self {
        Self {
            execution_ctx: None,
            execution_ctx_unique_id: None,
            detached: false,
        }
    }

    /// Returns the execution context ID, if set.
    pub fn execution_context(&self) -> Option<ExecutionContextId> {
        self.execution_ctx
    }

    /// Returns the unique ID of the execution context, if set.
    pub fn execution_context_unique_id(&self) -> Option<&str> {
        self.execution_ctx_unique_id.as_deref()
    }

    /// Sets the execution context and its unique identifier.
    pub fn set_context(&mut self, ctx: ExecutionContextId, unique_id: String) {
        self.execution_ctx = Some(ctx);
        self.execution_ctx_unique_id = Some(unique_id);
    }

    /// Removes and returns the execution context and its unique ID.
    pub fn take_context(&mut self) -> (Option<ExecutionContextId>, Option<String>) {
        (
            self.execution_ctx.take(),
            self.execution_ctx_unique_id.take(),
        )
    }

    /// Returns true if the world is detached.
    pub fn is_detached(&self) -> bool {
        self.detached
    }
}

/// There are two different kinds of worlds tracked for each `Frame`, that
/// represent a context for JavaScript execution. A `Page` might have many
/// execution contexts
/// - each [iframe](https://developer.mozilla.org/en-US/docs/Web/HTML/Element/iframe)
///   has a "default" execution context that is always created after the frame
///   is attached to DOM.
///   [Extension's](https://developer.chrome.com/extensions) content scripts create additional execution contexts.
///
/// Besides pages, execution contexts can be found in
/// [Web Workers](https://developer.mozilla.org/en-US/docs/Web/API/Web_Workers_API).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DOMWorldKind {
    /// The main world of a frame that represents the default execution context
    /// of a frame and is also created.
    #[default]
    Main,
    /// Each frame gets its own isolated world with universal access
    Secondary,
}
