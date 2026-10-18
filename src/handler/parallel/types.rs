//! Cross-task message types for the parallel handler.

use chromiumoxide_cdp::cdp::CdpEventMessage;
use chromiumoxide_types::{CallId, MethodId, Response};
use tokio::sync::oneshot::Sender as OneshotSender;

use crate::cmd::CommandMessage;
use crate::error::Result;
use crate::page::Page;

/// Messages sent from the Router to a SessionTask.
#[derive(Debug)]
pub(crate) enum RouterToSession {
    /// A response to a command this session issued. The session demuxes
    /// internally between its own pending map (internal CDP commands) and
    /// the external `CommandMessage::sender` it stored on dispatch.
    Response(CallId, Response, MethodId),

    /// A CDP event keyed by this session's session_id.
    Event(Box<CdpEventMessage>),

    /// `Browser::new_page` round-trip: the Router's `Target.createTarget`
    /// completed and resolved to this session's target. Hand the initiator
    /// oneshot over so the SessionTask can fulfill it once init reaches
    /// `Initialized` and the main frame is loaded.
    SetInitiator(OneshotSender<Result<Page>>),
}

/// Lifecycle signal sent from a SessionTask back to the Router.
#[derive(Debug)]
pub(crate) enum SessionToRouter {
    /// The session has resolved its session_id (after `Target.attachToTarget`
    /// succeeded). Router uses this to populate `session_id → slot`.
    SessionAttached { slot: u16, session_id: String },
}

/// External command originating from `Browser::execute` (no session_id) that
/// the Router can dispatch directly at slot 0.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct BrowserCommand {
    pub msg: CommandMessage,
}
