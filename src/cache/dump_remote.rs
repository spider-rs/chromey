//! Remote cache dump worker — delegates to `spider_remote_cache`.
//!
//! This module re-exports the shared worker from `spider_remote_cache` so
//! existing call sites in chromey continue to work unchanged.
//!
//! [`init_default_cache_worker`] injects chromey's `REQUEST_CLIENT` into
//! the shared crate so the remote cache uploads reuse the same connection
//! pool and TLS stack.

pub use spider_remote_cache::worker::{
    default_max_concurrent, default_qps, default_queue_cap, default_timeout_ms, enqueue,
    enqueue_best_effort, init_remote_dump_worker, try_enqueue, worker_inited as worke_inited,
    DumpJob,
};

/// Init the cache worker, injecting chromey's `REQUEST_CLIENT` so the
/// remote cache uploads share the same connection pool and TLS config.
pub async fn init_default_cache_worker() {
    // Normally already done at browser construction; idempotent.
    crate::cache::remote::install_cache_client();
    spider_remote_cache::init_default_worker().await;
}
