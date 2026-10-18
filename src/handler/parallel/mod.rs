//! Parallel handler — per-session sub-loops.
//!
//! Replaces the single `tokio::select!` driver with one `Router` task plus
//! one `SessionTask` per attached page. The Router demuxes the WS reader
//! using a shared call-id routing table. Each SessionTask owns its `Target`
//! outright. Routing-table operations are synchronous and retain no guards.
//!
//! Opt-in via the `parallel-handler` feature. When the feature is off this
//! module is not compiled.
//!
//! Submission to the bounded WS queue fails fast on overload. A saturated
//! session inbox spills into a bounded FIFO with deadline-aware delivery.
//! Only sustained overload (overflow limit or deadline) terminates that session
//! and resolves its callers with errors; responses/events are never silently
//! discarded while the session continues.
//! Cancellation bypasses the inbox, and a slot is reused only after task exit.
//! A session task panic terminates only that session; its callers fail and the
//! router reclaims its slot after joining it, while other pages keep running.
//!
//! `Browser::close()` sends `Browser.close` and keeps routing until the CDP
//! response or request timeout. It then stops the driver. Submission failure
//! also stops the driver but returns an error, as do timeout and disconnect;
//! only a successful CDP response acknowledges remote browser closure.
//!
//! Multi-context / GetTargets / FetchTargets support remains incomplete.

mod ids;
mod router;
mod session;
mod types;

pub(crate) use router::Router;

/// Reject before acceptance when the bounded writer queue is saturated. No
/// event loop may wait here: it owns request eviction and cancellation.
fn try_submit(
    tx: &tokio::sync::mpsc::Sender<chromiumoxide_types::MethodCall>,
    call: chromiumoxide_types::MethodCall,
) -> crate::error::Result<()> {
    tx.try_send(call).map_err(|err| match err {
        tokio::sync::mpsc::error::TrySendError::Full(_) => {
            crate::error::CdpError::msg("WS command channel full")
        }
        tokio::sync::mpsc::error::TrySendError::Closed(_) => {
            crate::error::CdpError::msg("WS writer closed")
        }
    })
}
