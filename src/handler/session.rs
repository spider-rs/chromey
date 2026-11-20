use chromey_cdp::cdp::browser_protocol::target::{SessionId, TargetId};

/// Represents a Session within the cpd.
#[derive(Default, Debug, Clone)]
pub struct Session {
    /// Identifier for this session.
    id: SessionId,
    /// The identifier of the target this session is attached to.
    target_id: TargetId,
}

impl Session {
    /// Creates a new `Session` with the given session and target IDs.
    pub fn new(id: SessionId, target_id: TargetId) -> Self {
        Self { id, target_id }
    }
    /// Get a reference to the session ID.
    pub fn session_id(&self) -> &SessionId {
        &self.id
    }
    /// Get a reference to the target ID associated with this session.
    pub fn target_id(&self) -> &TargetId {
        &self.target_id
    }
}
