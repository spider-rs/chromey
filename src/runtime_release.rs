//! Background batcher for `Runtime.releaseObject` calls.
//!
//! # Lock-freedom
//!
//! Every hot-path operation reduces to one or two relaxed-ordering atomic
//! ops, with no mutex, no `Once`-style sync after the first init:
//!
//! | operation       | primitive                                                |
//! | --------------- | -------------------------------------------------------- |
//! | `try_release`   | `OnceLock::get()` (atomic load) + `UnboundedSender::send` (tokio's lock-free MPSC list) |
//! | `init_worker`   | `OnceLock::get()` fast-path check; `get_or_init` only on the *first* call ever |
//! | `Drop` guards   | `Arc::clone` (atomic fetch-add) + `try_release`          |
//!
//! The **first** `init_worker` call runs `OnceLock::get_or_init`, which
//! briefly uses the `Once` sync primitive to serialise racing initialisers.
//! This is a one-time per-process cost; subsequent calls skip it entirely
//! via an explicit `get()` fast-path check.
//!
//! # Deadlock-freedom
//!
//! - `Drop` handlers never await, never acquire a lock another async task
//!   holds, and never allocate beyond an atomic pointer push into the MPSC.
//! - The worker holds no locks across its awaits; `rx.recv().await` parks
//!   on a lock-free `Notify`, and `page.execute` goes through the existing
//!   CDP command pipeline (no new locks introduced).
//! - If the worker task panics or the runtime shuts down, `try_release`
//!   continues to succeed (channel send does not require a live receiver);
//!   releases are silently dropped and V8 reclaims on context teardown.
//!
//! # Batching + per-batch spawn
//!
//! The dispatcher drains up to [`MAX_BATCH`] pending releases per wake
//! via `rx.recv_many(..)`, then hands the batch to **one** spawned
//! worker task that drives a `FuturesUnordered` of every release
//! concurrently. This gives:
//!
//! * ~64× fewer `tokio::spawn` calls than spawn-per-release, while
//!   preserving concurrent execution inside each batch.
//! * **Slow-release isolation**: one `page.execute` that hits
//!   `request_timeout` (e.g. a crashed target) can no longer block
//!   the dispatcher from picking up subsequent batches — the
//!   dispatcher's loop body is strictly `drain → spawn batch-worker`
//!   with no `.await` on cleanup work. Multiple batch workers can be
//!   in flight simultaneously across runtime worker threads.
//! * **No Arc<PageInner> retention backlog**: queued `(Page, id)`
//!   pairs hold Pages alive until drained, so spending 30s inside a
//!   single `page.execute` used to keep up to 63 other Pages pinned
//!   unnecessarily. With per-batch spawn, each batch's Pages are
//!   freed as its `FuturesUnordered` completes, independently of
//!   other batches.

use crate::page::Page;
use chromey_cdp::cdp::js_protocol::runtime::{ReleaseObjectParams, RemoteObjectId};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::sync::OnceLock;
use tokio::sync::mpsc;

/// Max items drained per dispatcher wake. Under burst the dispatcher
/// spawns one batch worker per drain; concurrent batches are not
/// limited by this constant.
const MAX_BATCH: usize = 64;

static RELEASE_TX: OnceLock<mpsc::UnboundedSender<(Page, RemoteObjectId)>> = OnceLock::new();

/// Spawn the dispatcher and return its sender. Only ever invoked once,
/// from inside the `OnceLock::get_or_init` closure on the very first
/// init.
///
/// The dispatcher is strictly a `recv_many + spawn` loop: it waits
/// for at least one release, then hands the whole batch to a spawned
/// worker that drives a `FuturesUnordered` of `page.execute` calls.
/// The dispatcher never `.await`s on CDP work, so a slow or hung
/// release cannot block it from picking up the next batch.
fn spawn_worker() -> mpsc::UnboundedSender<(Page, RemoteObjectId)> {
    let (tx, mut rx) = mpsc::unbounded_channel::<(Page, RemoteObjectId)>();

    tokio::spawn(async move {
        // Reused across iterations; `mem::replace` swaps in a fresh
        // capacity-sized buffer so the next `recv_many` never grows
        // incrementally.
        let mut batch: Vec<(Page, RemoteObjectId)> = Vec::with_capacity(MAX_BATCH);
        loop {
            let n = rx.recv_many(&mut batch, MAX_BATCH).await;
            if n == 0 {
                break; // channel closed — no more producers
            }
            let next_cap = n.min(MAX_BATCH);
            let releases: Vec<(Page, RemoteObjectId)> =
                std::mem::replace(&mut batch, Vec::with_capacity(next_cap));
            tokio::spawn(async move {
                let mut in_flight: FuturesUnordered<_> = releases
                    .into_iter()
                    .map(|(page, id)| async move {
                        let _ = page.execute(ReleaseObjectParams::new(id)).await;
                    })
                    .collect();
                // Drain to completion; each resolved future is dropped
                // immediately, freeing its `Arc<PageInner>` before the
                // batch as a whole finishes.
                while in_flight.next().await.is_some() {}
            });
        }
    });

    tx
}

/// Ensure the background release worker is running.
///
/// Hot path is a single atomic `OnceLock::get()` load; only the very first
/// call ever touches `get_or_init` (and thus the `Once` sync primitive).
/// Must be invoked from a tokio runtime context on the first call.
#[inline]
pub fn init_worker() {
    // Explicit fast path: one atomic load, zero sync primitives after the
    // first init.
    if RELEASE_TX.get().is_some() {
        return;
    }
    let _ = RELEASE_TX.get_or_init(spawn_worker);
}

/// Returns `true` if the worker has been initialised in this process.
#[inline]
pub fn worker_inited() -> bool {
    RELEASE_TX.get().is_some()
}

/// Enqueue a remote-object release for background processing.
///
/// Lock-free (one atomic load + one wait-free MPSC push), safe to invoke
/// from a `Drop` implementation on any thread.  If the worker has not yet
/// been initialised the release is silently dropped; V8 reclaims the
/// object on the next execution-context teardown.
#[inline]
pub fn try_release(page: Page, object_id: RemoteObjectId) {
    if let Some(tx) = RELEASE_TX.get() {
        let _ = tx.send((page, object_id));
    }
}
