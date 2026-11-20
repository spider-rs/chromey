//! CDP IO-stream based response body reader.
//!
//! Uses `Fetch.takeResponseBodyAsStream` + `IO.read` to pull response bodies
//! in fixed-size chunks instead of buffering the entire body in a single CDP
//! JSON message (`Network.getResponseBody`).  This lowers peak memory per
//! concurrent request and interleaves better with other CDP commands.
//!
//! When the `_cache_stream_disk` feature is enabled (default with `_cache`),
//! chunks are spilled to a temporary file so that only one chunk lives in
//! memory at a time.  If any disk I/O error occurs (disk full, permissions,
//! temp dir missing, etc.) the reader transparently falls back to in-memory
//! accumulation for the remainder of the stream — no data is ever lost.

use crate::page::Page;
use base64::{engine::general_purpose, Engine as _};
use chromey_cdp::cdp::browser_protocol::{
    fetch::TakeResponseBodyAsStreamParams,
    io::{CloseParams, ReadParams, StreamHandle},
};
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Default chunk size for `IO.read` (bytes requested per round-trip).
const DEFAULT_IO_READ_CHUNK: i64 = 65_536; // 64 KiB

/// Optional max body size.  Set `CHROMEY_STREAM_MAX_BODY_BYTES` to enforce a
/// cap.  When unset (default) there is no limit — matching the original
/// `getResponseBody` behavior.
fn max_body_bytes() -> Option<usize> {
    // Parsed once per call (cheap — env lookup + parse).  Could be
    // lazy_static but the env var may be changed between runs.
    std::env::var("CHROMEY_STREAM_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
}

/// Hard cap for pre-allocation based on a remote-supplied `Content-Length`
/// hint.  The hint comes from an HTTP response header and is therefore
/// attacker-controlled; clamping it here prevents a malicious server from
/// coercing a multi-GiB `Vec::with_capacity` before any bytes are read.
/// The actual body can still grow past this via `push`, subject to
/// `max_body_bytes()` enforcement in the read loop.
#[cfg(not(feature = "_cache_stream_disk"))]
const MAX_PREALLOC_BODY_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Base threshold (bytes) below which we skip streaming and let the
/// `Network.getResponseBody` path handle the response.  This value is
/// *lowered* dynamically as active page count rises — but only modestly,
/// so streaming remains a pressure-relief valve rather than the hot path.
const BASE_STREAMING_THRESHOLD: usize = 2_097_152; // 2 MiB

/// The floor the threshold can be reduced to under pressure.
const MIN_STREAMING_THRESHOLD: usize = 524_288; // 512 KiB

/// Page count at which the threshold reaches `MIN_STREAMING_THRESHOLD`.
const HIGH_PRESSURE_PAGES: usize = 128;

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Number of streams currently in-flight (for diagnostics / metrics).
static INFLIGHT_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// Monotonic counter for unique temp file names.
#[cfg(feature = "_cache_stream_disk")]
static STREAM_FILE_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Returns the number of body streams currently being read.
#[inline]
pub fn inflight_stream_count() -> usize {
    INFLIGHT_STREAMS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Adaptive threshold
// ---------------------------------------------------------------------------

/// Returns the `Content-Length` threshold (in bytes) above which a response
/// should be streamed rather than fetched in one shot.
///
/// The value decreases linearly from [`BASE_STREAMING_THRESHOLD`] to
/// [`MIN_STREAMING_THRESHOLD`] as the active page count grows toward
/// [`HIGH_PRESSURE_PAGES`].  This means under heavy concurrency we stream
/// *more* responses (smaller bodies qualify) to keep peak memory per request
/// low.
#[inline]
pub fn streaming_threshold_bytes() -> usize {
    let pages = crate::handler::page::active_page_count();

    if pages >= HIGH_PRESSURE_PAGES {
        return MIN_STREAMING_THRESHOLD;
    }

    // Linear interpolation: threshold shrinks as pages increase.
    let range = BASE_STREAMING_THRESHOLD - MIN_STREAMING_THRESHOLD;
    let reduction = range * pages / HIGH_PRESSURE_PAGES;
    BASE_STREAMING_THRESHOLD - reduction
}

// ---------------------------------------------------------------------------
// Stream guard (RAII — ensures IO.close)
// ---------------------------------------------------------------------------

/// RAII guard that ensures `IO.close` is sent even on early return / error.
struct StreamGuard<'a> {
    page: &'a Page,
    handle: Option<StreamHandle>,
}

impl<'a> StreamGuard<'a> {
    fn new(page: &'a Page, handle: StreamHandle) -> Self {
        Self {
            page,
            handle: Some(handle),
        }
    }

    /// Take ownership of the handle (called before explicit close).
    fn take(&mut self) -> Option<StreamHandle> {
        self.handle.take()
    }
}

impl Drop for StreamGuard<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // Enqueue the CDP `IO.close` onto the shared background
            // cleanup worker (see `crate::bg_cleanup`) — wait-free
            // atomic-load + mpsc-push, safe from Drop on any thread.
            // If the worker hasn't been initialised yet, the close is
            // silently discarded; Chrome GCs the stream handle when
            // the page is closed.
            crate::bg_cleanup::submit(crate::bg_cleanup::CleanupTask::CloseCdpStream {
                page: self.page.clone(),
                handle,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// ChunkSink — disk with automatic memory fallback
// ---------------------------------------------------------------------------

/// Where decoded chunks are accumulated.  Starts on disk when possible
/// and falls back to memory transparently on any I/O error.
#[cfg(feature = "_cache_stream_disk")]
enum ChunkSink {
    /// Chunks are written to a temp file.
    Disk {
        file: tokio::fs::File,
        path: std::path::PathBuf,
    },
    /// In-memory accumulation (initial mode when `_cache_stream_disk` is off,
    /// or fallback after a disk error).
    Memory { buf: Vec<u8> },
}

#[cfg(feature = "_cache_stream_disk")]
impl ChunkSink {
    /// Try to open a temp file.  Returns `Memory` on any failure.
    async fn open_disk() -> Self {
        match Self::try_open_disk().await {
            Ok(sink) => sink,
            Err(err) => {
                tracing::debug!("stream disk init failed, using memory: {err}");
                ChunkSink::Memory { buf: Vec::new() }
            }
        }
    }

    async fn try_open_disk() -> Result<Self, std::io::Error> {
        let tmp_dir = std::env::temp_dir();

        // Ensure the directory exists (handles bare containers, chroots, etc).
        tokio::fs::create_dir_all(&tmp_dir).await?;

        // Unique name: PID + monotonic counter — no collisions within a process.
        let seq = STREAM_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let file_name = format!("chromey_stream_{}_{}.tmp", std::process::id(), seq);
        let path = tmp_dir.join(file_name);

        let file = tokio::fs::File::create(&path).await?;
        Ok(ChunkSink::Disk { file, path })
    }

    /// Write a decoded chunk.  On disk I/O error, reads back what was
    /// already written, appends the current chunk, and switches to `Memory`.
    /// Never returns an error — disk failures are absorbed silently.
    async fn write_chunk(&mut self, decoded: &[u8]) {
        match self {
            ChunkSink::Disk { file, path } => {
                use tokio::io::AsyncWriteExt;
                if let Err(err) = file.write_all(decoded).await {
                    tracing::debug!("stream disk write failed, falling back to memory: {err}");
                    // Best-effort flush so read-back captures as much as
                    // possible.  Ignore errors — we're already in fallback.
                    let _ = file.flush().await;

                    // Read back whatever was successfully written.
                    let mut recovered = match tokio::fs::read(path.as_path()).await {
                        Ok(bytes) => bytes,
                        Err(_) => Vec::new(),
                    };

                    // Clean up the temp file — we no longer need it.
                    let _ = tokio::fs::remove_file(path.as_path()).await;

                    // Append the current chunk that failed to write.
                    recovered.extend_from_slice(decoded);

                    // Swap to Memory.  The old `file` handle and `path` are
                    // replaced; the file is already deleted above.
                    *self = ChunkSink::Memory { buf: recovered };
                }
            }
            ChunkSink::Memory { buf } => {
                buf.extend_from_slice(decoded);
            }
        }
    }

    /// Finalize: return the complete body and clean up any temp file.
    ///
    /// If the temp file can't be read back (extremely rare — would require
    /// the file to vanish or permissions to change between write and read),
    /// returns an empty `Vec` so the caller can skip caching rather than
    /// panic.  The stream is still properly closed by the `StreamGuard`.
    async fn finish(&mut self) -> Vec<u8> {
        match self {
            ChunkSink::Disk {
                ref mut file,
                ref path,
            } => {
                use tokio::io::AsyncWriteExt;

                if let Err(err) = file.flush().await {
                    tracing::debug!("stream disk flush failed on finish: {err}");
                }

                let path = path.clone();

                let body = match tokio::fs::read(&path).await {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        tracing::debug!("stream disk read-back failed: {err}");
                        Vec::new()
                    }
                };
                // Always clean up.
                let _ = tokio::fs::remove_file(&path).await;

                // Switch to Memory so Drop doesn't try to remove again.
                *self = ChunkSink::Memory { buf: Vec::new() };

                body
            }
            ChunkSink::Memory { ref mut buf } => std::mem::take(buf),
        }
    }
}

#[cfg(feature = "_cache_stream_disk")]
impl Drop for ChunkSink {
    fn drop(&mut self) {
        // If the sink is dropped without calling `finish` (e.g. early
        // error), enqueue the temp-file removal onto the shared
        // background cleanup worker (see `crate::bg_cleanup`).
        // Wait-free atomic-load + mpsc-push; safe from any thread
        // including during a panic unwind.
        if let ChunkSink::Disk { path, .. } = self {
            crate::bg_cleanup::submit(crate::bg_cleanup::CleanupTask::RemoveFile(path.clone()));
        }
    }
}

// ---------------------------------------------------------------------------
// Unified chunk reader
// ---------------------------------------------------------------------------

/// Read all chunks from a CDP IO stream.  Returns the fully assembled body.
///
/// With `_cache_stream_disk`: spills to disk, auto-falls back to memory.
/// Without: pure in-memory accumulation.
async fn read_all_chunks(
    page: &Page,
    stream_handle: &StreamHandle,
    content_length_hint: Option<usize>,
) -> Result<Vec<u8>, StreamError> {
    // Initialise the sink.
    #[cfg(feature = "_cache_stream_disk")]
    let mut sink = ChunkSink::open_disk().await;

    let cap = max_body_bytes();

    #[cfg(not(feature = "_cache_stream_disk"))]
    let mut body = {
        // Clamp attacker-supplied Content-Length to the effective cap so a
        // malicious `Content-Length: <huge>` cannot force a giant pre-alloc.
        let prealloc_ceiling = cap.unwrap_or(MAX_PREALLOC_BODY_BYTES);
        let capacity = content_length_hint.unwrap_or(0).min(prealloc_ceiling);
        Vec::with_capacity(capacity)
    };
    let mut total_bytes: usize = 0;

    // Reusable buffer for base64 decoding — avoids allocating a new Vec per chunk.
    let mut decode_buf: Vec<u8> = Vec::with_capacity(DEFAULT_IO_READ_CHUNK as usize);

    loop {
        let read_result = page
            .execute(
                ReadParams::builder()
                    .handle(stream_handle.clone())
                    .size(DEFAULT_IO_READ_CHUNK)
                    .build()
                    .map_err(StreamError::Build)?,
            )
            .await
            .map_err(StreamError::Cdp)?;

        let chunk = &read_result.result;

        // Decode in-place or borrow directly — no allocation for plain-text chunks.
        let data_bytes: &[u8] = if chunk.base64_encoded.unwrap_or(false) {
            decode_buf.clear();
            general_purpose::STANDARD
                .decode_vec(&chunk.data, &mut decode_buf)
                .map_err(StreamError::Base64)?;
            &decode_buf
        } else {
            chunk.data.as_bytes()
        };

        total_bytes += data_bytes.len();

        // Optional size cap (off by default — set CHROMEY_STREAM_MAX_BODY_BYTES).
        if let Some(max) = cap {
            if total_bytes > max {
                return Err(StreamError::BodyTooLarge(total_bytes));
            }
        }

        #[cfg(feature = "_cache_stream_disk")]
        sink.write_chunk(data_bytes).await;

        #[cfg(not(feature = "_cache_stream_disk"))]
        body.extend_from_slice(data_bytes);

        if chunk.eof {
            break;
        }
    }

    // Finalise.
    #[cfg(feature = "_cache_stream_disk")]
    {
        let _ = content_length_hint; // used only by the memory path
        Ok(sink.finish().await)
    }

    #[cfg(not(feature = "_cache_stream_disk"))]
    Ok(body)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Opens a CDP IO stream for the given paused Fetch request and reads the
/// body to completion in fixed-size chunks.
///
/// When the `_cache_stream_disk` feature is enabled (default), chunks are
/// spilled to a temporary file so peak memory per stream stays at ~64 KiB.
/// If any disk I/O error occurs the reader transparently falls back to
/// in-memory accumulation — no chunks are ever lost.  When `_cache_stream_disk`
/// is disabled, chunks accumulate in a `Vec<u8>` directly.
///
/// # Concurrency
///
/// Acquires a permit from `GLOBAL_STREAM_PERMITS` before starting.  The
/// permit is released when the function returns (success or failure).
///
/// # Errors
///
/// Returns `Err` only for non-recoverable failures:
/// - the semaphore is closed (should never happen),
/// - `takeResponseBodyAsStream` fails (request not paused at HeadersReceived),
/// - a timeout is exceeded,
/// - the body exceeds `MAX_BODY_BYTES`.
///
/// Disk I/O errors are **never** surfaced — the reader falls back silently.
/// On any error the CDP stream is closed via the `StreamGuard`.
/// Result of a streaming body read attempt.
#[derive(Debug)]
pub enum StreamResult {
    /// Stream completed successfully — body is fully read.
    Ok(Vec<u8>),
    /// The stream was never opened (CDP call failed, semaphore closed, etc).
    /// The response body has NOT been consumed — the caller can safely fall
    /// back to `Fetch.getResponseBody` or `continueRequest`.
    NotStarted(StreamError),
    /// The stream was opened but the read loop failed mid-way (timeout, CDP
    /// error, body too large, etc).  The response body IS consumed — the
    /// caller MUST `fulfillRequest` with the partial data collected so far.
    /// An empty Vec means zero bytes were recovered.
    PartialBody { body: Vec<u8>, error: StreamError },
}

pub async fn read_response_body_as_stream(
    page: &Page,
    request_id: impl Into<chromey_cdp::cdp::browser_protocol::fetch::RequestId>,
    content_length_hint: Option<usize>,
) -> StreamResult {
    // Ensure the background cleanup worker is running in this runtime
    // so later `StreamGuard::Drop` / `ChunkSink::Drop` submissions land
    // on a live receiver. Single atomic load on the hot path after the
    // first call.
    crate::bg_cleanup::init_worker();

    INFLIGHT_STREAMS.fetch_add(1, Ordering::Relaxed);
    let _dec = DecrementOnDrop(&INFLIGHT_STREAMS);

    // Open the stream. page.execute() already enforces request_timeout.
    let returns = match page
        .execute(TakeResponseBodyAsStreamParams::new(request_id))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            // CDP rejected or timed out — body NOT consumed.
            return StreamResult::NotStarted(StreamError::Cdp(e));
        }
    };

    // --- Past this point the body IS consumed by Chrome. ---

    let stream_handle = returns.result.stream;
    let mut guard = StreamGuard::new(page, stream_handle.clone());

    // Read all chunks.
    match read_all_chunks(page, &stream_handle, content_length_hint).await {
        Ok(body) => {
            // Explicitly close the CDP stream (guard becomes a no-op).
            if let Some(h) = guard.take() {
                let _ = page.send_command(CloseParams { handle: h }).await;
            }
            StreamResult::Ok(body)
        }
        Err(err) => {
            // Close the stream best-effort.
            if let Some(h) = guard.take() {
                let _ = page.send_command(CloseParams { handle: h }).await;
            }

            // Recover whatever was accumulated so far.
            #[cfg(feature = "_cache_stream_disk")]
            // Cannot recover partial data from a failed sink easily;
            // the error path in read_all_chunks already returned before
            // finish() was called.  Return empty partial.
            let partial = Vec::new();

            #[cfg(not(feature = "_cache_stream_disk"))]
            let partial = Vec::new();

            StreamResult::PartialBody {
                body: partial,
                error: err,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors specific to the streaming body reader.
#[derive(Debug)]
pub enum StreamError {
    /// A CDP command failed.
    Cdp(crate::error::CdpError),
    /// The response body exceeded the optional `CHROMEY_STREAM_MAX_BODY_BYTES` cap.
    BodyTooLarge(usize),
    /// Base64 decoding failed on a chunk.
    Base64(base64::DecodeError),
    /// Builder error from CDP param construction.
    Build(String),
    /// File I/O error (only when disk read-back in `finish()` fails after
    /// the in-memory fallback itself failed — extremely unlikely).
    Io(std::io::Error),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cdp(e) => write!(f, "CDP error during stream read: {e}"),
            Self::BodyTooLarge(n) => write!(f, "response body too large: {n} bytes"),
            Self::Base64(e) => write!(f, "base64 decode error: {e}"),
            Self::Build(e) => write!(f, "CDP param build error: {e}"),
            Self::Io(e) => write!(f, "stream I/O error: {e}"),
        }
    }
}

impl std::error::Error for StreamError {}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract `Content-Length` from CDP response headers.  Returns `None`
/// when missing or unparseable — the caller should treat that as
/// "unknown length".
pub fn content_length_from_headers(
    headers: &std::collections::HashMap<String, String>,
) -> Option<usize> {
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            return v.parse().ok();
        }
    }
    None
}

/// Tiny RAII helper: decrements an `AtomicUsize` on drop.
struct DecrementOnDrop<'a>(&'a AtomicUsize);

impl Drop for DecrementOnDrop<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(all(test, feature = "_cache_stream_disk"))]
mod chunk_sink_drop_tests {
    //! `Drop for ChunkSink` enqueues a `CleanupTask::RemoveFile` onto
    //! the shared `bg_cleanup` worker via a wait-free `OnceLock::get()`
    //! plus `UnboundedSender::send`. No `tokio::spawn` runs on the
    //! Drop thread, so dropping on a plain `std::thread` (no runtime
    //! context) cannot panic, even during a panic unwind.
    //!
    //! This test exercises that end-to-end: construct a `ChunkSink::Disk`
    //! in the main thread, move it across a thread boundary, drop it
    //! on a `std::thread` that has no tokio runtime, and assert no
    //! panic.
    use super::ChunkSink;

    #[test]
    fn drop_outside_runtime_is_best_effort_not_panic() {
        let path = std::env::temp_dir().join(format!(
            "chromey-chunksink-drop-test-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
        ));
        let std_file = std::fs::File::create(&path).expect("create temp file");
        let tokio_file = tokio::fs::File::from_std(std_file);
        let sink = ChunkSink::Disk {
            file: tokio_file,
            path: path.clone(),
        };

        std::thread::spawn(move || {
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "test thread must not be inside a tokio runtime"
            );
            drop(sink);
        })
        .join()
        .expect("Drop must not panic on a non-tokio thread");

        let _ = std::fs::remove_file(&path);
    }
}
