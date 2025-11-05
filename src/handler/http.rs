use chromiumoxide_cdp::cdp::browser_protocol::network::{InterceptionId, RequestId, Response};
use chromiumoxide_cdp::cdp::browser_protocol::page::FrameId;

#[derive(Default, Debug, Clone)]
pub struct HttpRequest {
    /// Unique ID of the request.
    pub request_id: RequestId,
    /// Indicates if the response came from the memory cache.
    pub from_memory_cache: bool,
    /// Reason for failure, if any.
    pub failure_text: Option<String>,
    /// ID used for request interception, if applicable.
    pub interception_id: Option<InterceptionId>,
    /// Response data associated with the request.
    pub response: Option<Response>,
    /// HTTP headers of the request.
    pub headers: std::collections::HashMap<String, String>,
    /// ID of the frame that initiated the request.
    pub frame: Option<FrameId>,
    /// Whether this is a navigation request.
    pub is_navigation_request: bool,
    /// Whether interception is allowed for this request.
    pub allow_interception: bool,
    /// Whether the interception has already been handled.
    pub interception_handled: bool,
    /// HTTP method (e.g., GET, POST).
    pub method: Option<String>,
    /// Request URL.
    pub url: Option<String>,
    /// Resource type (e.g., "Document", "Script").
    pub resource_type: Option<String>,
    /// Raw post body, if present.
    pub post_data: Option<String>,
    /// List of redirect requests leading to this one.
    pub redirect_chain: Vec<HttpRequest>,
}

impl HttpRequest {
    /// Creates a new `HttpRequest` with the given request ID and default values.
    pub fn new(
        request_id: RequestId,
        frame: Option<FrameId>,
        interception_id: Option<InterceptionId>,
        allow_interception: bool,
        redirect_chain: Vec<HttpRequest>,
    ) -> Self {
        Self {
            request_id,
            from_memory_cache: false,
            failure_text: None,
            interception_id,
            response: None,
            headers: Default::default(),
            frame,
            is_navigation_request: false,
            allow_interception,
            interception_handled: false,
            method: None,
            url: None,
            resource_type: None,
            post_data: None,
            redirect_chain,
        }
    }
    /// Returns the request ID.
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }
    /// Sets the response for this request.
    pub fn set_response(&mut self, response: Response) {
        self.response = Some(response)
    }
}
