//! Call-id allocator + routing table for the parallel handler.
//!
//! Original design embedded a slot index in the high bits of the CDP `id`
//! so the Router could demux without a lookup. Real Chrome serializes any
//! integer larger than `i32::MAX` as a JSON float (scientific notation),
//! which serde rejects when deserializing into `usize` — so the slot-in-id
//! scheme is incompatible with the wire protocol.
//!
//! Replacement: a process-shared `AtomicU64` mints monotonic small ids
//! (well within `i32`'s range for a single browser session), and a
//! sharded `DashMap<CallId, slot>` records routing on dispatch. The map
//! is read once per response and removed on the same path; under typical
//! parallel-page workloads the live entry count equals the in-flight
//! command count (including commands already drained by the WS writer).
//! No map guard escapes this module. The retain predicate is a pure slot
//! comparison; it must never call back into routing or await while locked.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chromiumoxide_types::CallId;
use dashmap::DashMap;

/// Slot index. 0 = Router (browser-level commands), 1..=N = SessionTasks.
pub type Slot = u16;

#[derive(Clone, Default)]
pub(crate) struct CallIdAllocator {
    next: Arc<AtomicU64>,
    routing: Arc<DashMap<CallId, Slot>>,
}

impl CallIdAllocator {
    pub fn new(start: u64) -> Self {
        Self {
            next: Arc::new(AtomicU64::new(start.max(1))),
            routing: Arc::new(DashMap::with_capacity(1024)),
        }
    }

    /// Allocate a fresh `CallId` and record that responses to it should be
    /// routed to `slot`. Slot 0 means the Router will handle the response
    /// itself (browser-level commands).
    pub fn alloc(&self, slot: Slot) -> CallId {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let id = CallId::new(n as usize);
        if slot != 0 {
            self.routing.insert(id, slot);
        }
        id
    }

    /// Remove and return the routing entry for `id`. Returns `None` for
    /// router-owned (slot 0) ids — those weren't tracked here.
    pub fn take_route(&self, id: CallId) -> Option<Slot> {
        self.routing.remove(&id).map(|(_, slot)| slot)
    }

    /// Best-effort cleanup when a session is removed: drop every entry
    /// pointing at this slot so the map doesn't grow unbounded under heavy
    /// session churn.
    pub fn drop_slot(&self, slot: Slot) {
        self.routing.retain(|_, s| *s != slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_records_route_for_session_slots() {
        let a = CallIdAllocator::new(1);
        let id = a.alloc(7);
        assert_eq!(a.take_route(id), Some(7));
        // Once consumed, the route is gone.
        assert_eq!(a.take_route(id), None);
    }

    #[test]
    fn alloc_skips_routing_for_slot_zero() {
        let a = CallIdAllocator::new(1);
        let id = a.alloc(0);
        assert_eq!(a.take_route(id), None);
    }

    #[test]
    fn drop_slot_clears_only_its_entries() {
        let a = CallIdAllocator::new(1);
        let id1 = a.alloc(3);
        let id2 = a.alloc(4);
        a.drop_slot(3);
        assert_eq!(a.take_route(id1), None);
        assert_eq!(a.take_route(id2), Some(4));
    }
}
