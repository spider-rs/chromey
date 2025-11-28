use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::sync::{mpsc, OnceCell};

use crate::http::HttpVersion;

static REMOTE_DUMP_TX: OnceCell<mpsc::Sender<DumpJob>> = OnceCell::const_new();

#[derive(Debug, Clone)]
pub struct DumpJob {
    pub cache_key: String,
    pub cache_site: String,
    pub url: String,
    pub method: String,
    pub status: u16,
    pub request_headers: HashMap<String, String>,
    pub response_headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub http_version: HttpVersion,
    /// None => default endpoint
    /// Some("true") => default endpoint
    /// Some("http://...") => override base URL
    pub dump_remote: Option<String>,
}

async fn init_inner(queue_cap: usize, qps: u32, timeout_ms: u64) -> mpsc::Sender<DumpJob> {
    let (tx, mut rx) = mpsc::channel::<DumpJob>(queue_cap.max(1));

    tokio::spawn(async move {
        let qps = qps.max(1);
        let tick_ms = (1000u64 / qps as u64).max(1);
        let mut tick = tokio::time::interval(Duration::from_millis(tick_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut inflight: HashSet<String> = HashSet::new();

        while let Some(job) = rx.recv().await {
            if !inflight.insert(job.cache_key.clone()) {
                continue;
            }
            tick.tick().await;

            let timeout = Duration::from_millis(timeout_ms);
            let res = tokio::time::timeout(timeout, dump_job(job.clone())).await;

            if res.is_err() {
                tracing::warn!(
                    "remote cache dump: timed out after {}ms for {}",
                    timeout_ms,
                    job.cache_key
                );
            }

            inflight.remove(&job.cache_key);
        }
    });

    tx
}

/// Manual init (optional). Safe to call multiple times.
pub async fn init_remote_dump_worker(
    queue_cap: usize,
    qps: u32,
    timeout_ms: u64,
) -> mpsc::Sender<DumpJob> {
    REMOTE_DUMP_TX
        .get_or_init(|| init_inner(queue_cap, qps, timeout_ms))
        .await
        .clone()
}

/// Auto-init on first use + best-effort enqueue (never blocks on full queue).
pub async fn enqueue_best_effort(job: DumpJob) -> bool {
    let tx =
        init_remote_dump_worker(default_queue_cap(), default_qps(), default_timeout_ms()).await;

    tx.try_send(job).is_ok()
}

/// If you prefer backpressure instead of drop-on-full:
pub async fn enqueue(job: DumpJob) -> Result<(), mpsc::error::SendError<DumpJob>> {
    let tx =
        init_remote_dump_worker(default_queue_cap(), default_qps(), default_timeout_ms()).await;

    tx.send(job).await
}

/// Dump the remote cache job.
async fn dump_job(job: DumpJob) {
    super::remote::dump_to_remote_cache_parts(
        &job.cache_key,
        &job.cache_site,
        &job.url,
        &job.body,
        &job.method,
        job.status,
        &job.request_headers,
        &job.response_headers,
        &job.http_version,
        job.dump_remote.as_deref(),
    )
    .await;
}

/// Worker inited.
pub fn worke_inited() -> bool {
    REMOTE_DUMP_TX.initialized()
}

/// Non-async enqueue (fast path). Drops if queue is full.
pub fn try_enqueue(job: DumpJob) -> bool {
    REMOTE_DUMP_TX
        .get()
        .and_then(|tx| tx.try_send(job).ok())
        .is_some()
}

pub fn default_queue_cap() -> usize {
    std::env::var("HYBRID_CACHE_REMOTE_QUEUE_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

pub fn default_qps() -> u32 {
    std::env::var("HYBRID_CACHE_REMOTE_QPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
}

pub fn default_timeout_ms() -> u64 {
    std::env::var("HYBRID_CACHE_REMOTE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_500)
}

/// Init the cache worker.
pub async fn init_default_cache_worker() {
    init_remote_dump_worker(default_queue_cap(), default_qps(), default_timeout_ms()).await;
}
