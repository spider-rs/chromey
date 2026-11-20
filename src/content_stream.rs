//! Streaming reader for `page.content()`-style HTML extraction.
//!
//! Unlike [`crate::cache::stream`], which drains a CDP IO stream handle from
//! `Fetch.takeResponseBodyAsStream`, there is no CDP handle for
//! `document.documentElement.outerHTML`.  Instead this module serialises the
//! HTML into a **wrapper object that lives only in V8** (no `window` pollution)
//! and pulls fixed-size slices over repeated `Runtime.callFunctionOn` calls
//! using the wrapper's `objectId`.
//!
//! Release of the V8 remote reference is handled by the shared batching
//! worker in [`crate::runtime_release`].  A tiny RAII guard on the Rust
//! side enqueues the `RemoteObjectId` into the worker's channel on drop —
//! wait-free, no per-call `tokio::spawn`, and still covers cancellation
//! and panic paths.  The worker fires releases in concurrent batches.
//!
//! When `_cache_stream_disk` is enabled, the Rust-side accumulator spills to
//! a temp file with transparent memory fallback on any I/O error, matching
//! the behaviour of the network-body sink in `cache::stream`.

use crate::error::{CdpError, Result};
use crate::page::Page;
use chromey_cdp::cdp::js_protocol::runtime::{
    CallArgument, CallFunctionOnParams, EvaluateParams, RemoteObjectId,
};
use futures_util::stream::{try_unfold, Stream};

/// Default UTF-16 code units requested per chunk (`65_536` = 64 Ki).
/// Chrome returns the slice as a native JSON string (UTF-8 over the wire),
/// which for BMP-heavy HTML lands at ~64–128 KiB per round-trip and up to
/// ~192 KiB worst case.
pub const DEFAULT_CHUNK_UNITS: u32 = 65_536;

/// Minimum UTF-16 code units a caller-supplied chunk size can be clamped
/// to (`1024` = 1 Ki).  Smaller than this and CDP round-trip overhead
/// starts dominating real payload transfer.
pub const MIN_CHUNK_UNITS: u32 = 1024;

/// Maximum UTF-16 code units a caller-supplied chunk size can be clamped
/// to (`4_194_304` = 4 Mi, worst-case ~12 MiB over the wire).  Caps
/// per-message memory on both ends.
pub const MAX_CHUNK_UNITS: u32 = 4_194_304;

/// Hard ceiling on total document size (UTF-16 code units).  Any page whose
/// HTML exceeds this is rejected immediately after the length probe rather
/// than streamed — a malicious or runaway page can't coerce the client
/// into a multi-gigabyte transfer.  Default: 256 Mi units ≈ 512 MiB–1 GiB
/// of UTF-8 on the wire.
pub const MAX_DOCUMENT_UNITS: u32 = 256 * 1024 * 1024;

/// Default hard ceiling on total accumulated bytes for the non-stream
/// accumulating API ([`content_bytes_streaming`]).  Can be overridden by
/// setting the `CHROMEY_CONTENT_STREAM_MAX_BYTES` env var.  Default:
/// 512 MiB.  The pump-style [`content_bytes_stream`] does not enforce
/// this cap since the caller decides how much to consume.
pub const DEFAULT_MAX_ACCUMULATED_BYTES: usize = 512 * 1024 * 1024;

/// Clamp a caller-supplied chunk size into the safe range.
#[inline]
fn clamp_chunk_units(units: u32) -> u32 {
    units.clamp(MIN_CHUNK_UNITS, MAX_CHUNK_UNITS)
}

/// Hard cap on chunk round-trips (guards against a mutating page or
/// pathological slice loop).  At [`DEFAULT_CHUNK_UNITS`] per chunk this
/// caps streaming at ~16 Gi code units — far beyond any realistic document.
/// Shared with [`crate::content_markdown`] so both content streams honour
/// the same bound.
pub(crate) const MAX_CHUNKS: usize = 262_144;

/// JS that builds the document HTML (matching
/// [`crate::javascript::extract::OUTER_HTML`]) and wraps it in an object so
/// the returned `RemoteObject` has a stable `objectId` we can hold onto.
/// Strings are *primitive* in V8 and primitive evaluate results do not carry
/// an `objectId`, hence the single-property wrapper.
const INIT_JS: &str = r###"(()=>{
  let rv='';
  if(document.doctype){rv+=new XMLSerializer().serializeToString(document.doctype);}
  if(document.documentElement){rv+=document.documentElement.outerHTML;}
  return {h:rv};
})()"###;

/// Function body for reading the length of the wrapped HTML (UTF-16 code
/// units).  Called once via `Runtime.callFunctionOn` against the wrapper.
const LEN_FN: &str = "function(){return this.h.length}";

/// Function body for slicing the wrapped HTML.  Adjusts `end` backwards if it
/// would split a UTF-16 surrogate pair, so every returned chunk is valid
/// UTF-16 (and therefore valid UTF-8 after JSON serialisation by Chrome).
const SLICE_FN: &str = r###"function(start,size){
  const s=this.h;
  const L=s.length;
  if(start>=L)return '';
  let end=start+size;
  if(end>L)end=L;
  if(end<L){
    const c=s.charCodeAt(end-1);
    if(c>=0xD800&&c<=0xDBFF)end-=1;
  }
  return s.slice(start,end);
}"###;

// ---------------------------------------------------------------------------
// Tiny RAII guard: enqueue release into the batching worker on drop
// ---------------------------------------------------------------------------

/// Holds the wrapper's `RemoteObjectId` and enqueues it with
/// [`crate::runtime_release::try_release`] when dropped.  Drop is
/// synchronous, wait-free, and panic-free — no `tokio::spawn`, no CDP call
/// issued here.
///
/// The id is stored directly (not in an `Option`) so `id()` is a simple
/// reference access with no runtime unwrap.  On `Drop` the id is swapped
/// out via `mem::take` (leaving a default empty id in its place); the
/// extracted id is enqueued only if non-empty, so double-drop or zero-init
/// paths are inert.
struct RemoteRefGuard {
    page: Page,
    object_id: RemoteObjectId,
}

impl RemoteRefGuard {
    #[inline]
    fn new(page: Page, object_id: RemoteObjectId) -> Self {
        Self { page, object_id }
    }

    #[inline]
    fn id(&self) -> &RemoteObjectId {
        &self.object_id
    }
}

impl Drop for RemoteRefGuard {
    fn drop(&mut self) {
        let id = std::mem::take(&mut self.object_id);
        if !id.0.is_empty() {
            crate::runtime_release::try_release(self.page.clone(), id);
        }
    }
}

// ---------------------------------------------------------------------------
// Chunk sink: disk with transparent memory fallback
// ---------------------------------------------------------------------------

#[cfg(feature = "_cache_stream_disk")]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "_cache_stream_disk")]
static CONTENT_FILE_SEQ: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "_cache_stream_disk")]
enum Sink {
    Disk {
        file: tokio::fs::File,
        path: std::path::PathBuf,
    },
    Memory {
        buf: Vec<u8>,
    },
}

#[cfg(feature = "_cache_stream_disk")]
impl Sink {
    async fn open(cap_hint: usize) -> Self {
        match Self::try_open_disk().await {
            Ok(s) => s,
            Err(err) => {
                tracing::debug!("content stream disk init failed, using memory: {err}");
                Sink::Memory {
                    buf: Vec::with_capacity(cap_hint),
                }
            }
        }
    }

    async fn try_open_disk() -> std::io::Result<Self> {
        let tmp_dir = std::env::temp_dir();
        tokio::fs::create_dir_all(&tmp_dir).await?;
        let seq = CONTENT_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("chromey_content_{}_{}.tmp", std::process::id(), seq);
        let path = tmp_dir.join(name);
        let file = tokio::fs::File::create(&path).await?;
        Ok(Sink::Disk { file, path })
    }

    async fn write(&mut self, data: &[u8]) {
        match self {
            Sink::Disk { file, path } => {
                use tokio::io::AsyncWriteExt;
                if let Err(err) = file.write_all(data).await {
                    tracing::debug!(
                        "content stream disk write failed, falling back to memory: {err}"
                    );
                    let _ = file.flush().await;
                    let mut recovered = tokio::fs::read(path.as_path()).await.unwrap_or_default();
                    let _ = tokio::fs::remove_file(path.as_path()).await;
                    recovered.extend_from_slice(data);
                    *self = Sink::Memory { buf: recovered };
                }
            }
            Sink::Memory { buf } => buf.extend_from_slice(data),
        }
    }

    async fn finish(&mut self) -> Vec<u8> {
        match self {
            Sink::Disk { file, path } => {
                use tokio::io::AsyncWriteExt;
                let _ = file.flush().await;
                let p = path.clone();
                let body = tokio::fs::read(&p).await.unwrap_or_default();
                let _ = tokio::fs::remove_file(&p).await;
                *self = Sink::Memory { buf: Vec::new() };
                body
            }
            Sink::Memory { buf } => std::mem::take(buf),
        }
    }
}

#[cfg(feature = "_cache_stream_disk")]
impl Drop for Sink {
    fn drop(&mut self) {
        if let Sink::Disk { path, .. } = self {
            // Enqueue the file removal onto the shared background
            // cleanup worker (see `crate::bg_cleanup`). This is a
            // wait-free atomic-load + mpsc-push — no tokio runtime
            // context required on the Drop thread, and no panic surface
            // even during a panic unwind. If the worker hasn't been
            // initialised yet (e.g. Sink dropped before any CDP stream
            // was opened), the task is silently discarded — strictly
            // better than a Drop-time panic-abort.
            crate::bg_cleanup::submit(crate::bg_cleanup::CleanupTask::RemoveFile(path.clone()));
        }
    }
}

// ---------------------------------------------------------------------------
// Threshold
// ---------------------------------------------------------------------------

/// UTF-16 code-unit count at or above which chunked streaming is worthwhile.
/// Below this size a single `callFunctionOn` slice round-trip already fits in
/// one CDP message comfortably.  Decays from 1 Mi → 256 Ki units as active
/// page count rises (mirroring the adaptive curve in `cache::stream`).
#[inline]
fn stream_threshold_units() -> u32 {
    const BASE: u32 = 1_048_576; // ~1 MiB of UTF-16 code units
    const MIN: u32 = 262_144; // ~256 Ki units
    const HIGH_PRESSURE_PAGES: u32 = 128;

    let pages = crate::handler::page::active_page_count() as u32;
    if pages >= HIGH_PRESSURE_PAGES {
        return MIN;
    }
    let range = BASE - MIN;
    let reduction = range * pages / HIGH_PRESSURE_PAGES;
    BASE - reduction
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Streams `document.documentElement.outerHTML` and returns it as UTF-8
/// bytes.  No global state is created on the page — the HTML is held only
/// via a transient V8 `RemoteObjectId` that the batching release worker
/// (see [`crate::runtime_release`]) reclaims after `content_streaming`
/// returns, via a tiny RAII guard.
///
/// # DoS guardrails
///
/// - A page whose HTML exceeds [`MAX_DOCUMENT_UNITS`] UTF-16 code units is
///   rejected immediately after the length probe (no streaming attempted).
/// - Accumulated bytes are capped at [`DEFAULT_MAX_ACCUMULATED_BYTES`]
///   (override with the `CHROMEY_CONTENT_STREAM_MAX_BYTES` env var); hitting
///   the cap returns an error rather than continuing to allocate.
/// - For unbounded consumption, use the pump-style [`content_bytes_stream`]
///   and apply your own per-consumer backpressure.
pub async fn content_bytes_streaming(page: &Page) -> Result<Vec<u8>> {
    let Some((guard, total_units, chunk_units)) = init_state(page).await? else {
        return Ok(Vec::new());
    };
    read_chunks(page, guard.id(), total_units, chunk_units).await
}

/// Pump-style async `Stream` over the page HTML — yields each chunk of
/// UTF-8 bytes as Chrome returns it, without accumulating into a `Vec`.
///
/// `chunk_units` is an optional cap on how many UTF-16 code units the page
/// slices per call:
///
/// - `None` uses [`DEFAULT_CHUNK_UNITS`] (`65_536` = 64 Ki, ~64–128 KiB
///   over the wire).
/// - `Some(n)` pins every slice call to at most `n` units, clamped into
///   `[`[`MIN_CHUNK_UNITS`]` (1024), `[`MAX_CHUNK_UNITS`]` (4_194_304)]`.
///
/// Smaller chunks yield more frequently (lower per-chunk latency); larger
/// chunks amortise CDP round-trip cost.
///
/// # DoS guardrails
///
/// Before streaming begins, a page whose HTML exceeds
/// [`MAX_DOCUMENT_UNITS`] UTF-16 code units is rejected — the length probe
/// returns an error and no slices are read.  The pump itself does **not**
/// enforce a total-bytes cap (that's the caller's responsibility when
/// consuming the stream); if you want one, use [`content_bytes_streaming`]
/// or stop polling once you've read enough.
///
/// Callers drive the stream with `StreamExt::next().await` (or feed it into
/// a writer, compressor, hash, etc.).  The returned stream owns a guard
/// over the V8 `RemoteObjectId`; dropping the stream early — cancellation,
/// break, `take()` — enqueues the release via
/// [`crate::runtime_release::try_release`] on the guard's `Drop`.
///
/// The first poll performs the init + length round-trips and then yields
/// the first chunk.  Subsequent polls each issue one `Runtime.callFunctionOn`
/// slice and yield its bytes.  An empty-byte yield, a length overrun, or
/// a CDP error terminates the stream.
pub fn content_bytes_stream(
    page: &Page,
    chunk_units: Option<u32>,
) -> impl Stream<Item = Result<Vec<u8>>> + Send + 'static {
    let page = page.clone();
    let override_units = chunk_units.map(clamp_chunk_units);
    try_unfold(
        PumpState::Init {
            page,
            override_units,
        },
        |state| async move {
            match state {
                PumpState::Init {
                    page,
                    override_units,
                } => match init_state(&page).await? {
                    None => Ok(None),
                    Some((guard, total, default_units)) => {
                        let chunk_units = override_units.unwrap_or(default_units);
                        pump_next(page, guard, total, 0, chunk_units, 0).await
                    }
                },
                PumpState::Pumping {
                    page,
                    guard,
                    total,
                    offset,
                    chunk_units,
                    rounds,
                } => pump_next(page, guard, total, offset, chunk_units, rounds).await,
            }
        },
    )
}

/// Convenience wrapper over [`content_bytes_stream`]: yield each chunk as
/// `String` rather than raw bytes.  Each chunk is individually validated as
/// UTF-8 (which it always is, since the page-side slice function avoids
/// splitting UTF-16 surrogate pairs).
pub fn content_stream(
    page: &Page,
    chunk_units: Option<u32>,
) -> impl Stream<Item = Result<String>> + Send + 'static {
    use futures_util::StreamExt;
    content_bytes_stream(page, chunk_units).map(|r| {
        r.and_then(|bytes| {
            String::from_utf8(bytes)
                .map_err(|e| CdpError::msg(format!("invalid UTF-8 in page content chunk: {e}")))
        })
    })
}

/// Internal state for the pump-stream unfold.  `Send + 'static` so the
/// stream is usable across `.await` points without borrowing the caller.
enum PumpState {
    /// Haven't run init yet.
    Init {
        page: Page,
        /// Caller-supplied chunk size override (already clamped).  `None`
        /// defers to the adaptive default from [`init_state`].
        override_units: Option<u32>,
    },
    /// Init done; reading chunks.
    Pumping {
        page: Page,
        guard: RemoteRefGuard,
        total: u32,
        offset: u32,
        chunk_units: u32,
        rounds: usize,
    },
}

/// Fetch one chunk and return the next stream state (or terminate).
async fn pump_next(
    page: Page,
    guard: RemoteRefGuard,
    total: u32,
    offset: u32,
    chunk_units: u32,
    rounds: usize,
) -> Result<Option<(Vec<u8>, PumpState)>> {
    if offset >= total || rounds >= MAX_CHUNKS {
        return Ok(None);
    }
    let bytes = read_slice(&page, guard.id(), offset, chunk_units).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let advanced = utf16_len_of_utf8(&bytes);
    if advanced == 0 {
        return Ok(None);
    }
    let new_offset = offset.saturating_add(advanced);
    Ok(Some((
        bytes,
        PumpState::Pumping {
            page,
            guard,
            total,
            offset: new_offset,
            chunk_units,
            rounds: rounds + 1,
        },
    )))
}

/// Init the page-side wrapper and measure length.  Returns `None` if the
/// document has zero UTF-16 code units (empty page).
async fn init_state(page: &Page) -> Result<Option<(RemoteRefGuard, u32, u32)>> {
    // Ensure both background workers are running in this runtime:
    // - `runtime_release` for async JS-object releases
    // - `bg_cleanup` for async temp-file cleanup and CDP IO.close
    // Both are a single `OnceLock` load on the hot path after first
    // init — no await, no sync primitive after the first call.
    crate::runtime_release::init_worker();
    crate::bg_cleanup::init_worker();

    let ctx = page.execution_context().await?;

    // Evaluate, returning a *reference* (returnByValue=false) so we get a
    // stable objectId for the wrapper.
    let mut init = EvaluateParams::new(INIT_JS);
    init.context_id = ctx;
    init.await_promise = Some(true);
    init.return_by_value = Some(false);

    let init_resp = page.execute(init).await?.result;
    if let Some(ex) = init_resp.exception_details {
        return Err(CdpError::JavascriptException(Box::new(ex)));
    }
    let object_id = init_resp
        .result
        .object_id
        .ok_or_else(|| CdpError::msg("content stream: init returned no objectId"))?;

    // Guard covers success, error, cancellation and panic paths — all
    // enqueue the release into the batching worker via a wait-free send.
    let guard = RemoteRefGuard::new(page.clone(), object_id);

    let len_params = CallFunctionOnParams::builder()
        .function_declaration(LEN_FN)
        .object_id(guard.id().clone())
        .return_by_value(true)
        .await_promise(false)
        .build()
        .map_err(CdpError::msg)?;
    let len_resp = page.execute(len_params).await?.result;
    if let Some(ex) = len_resp.exception_details {
        return Err(CdpError::JavascriptException(Box::new(ex)));
    }
    let total_units_u64 = len_resp
        .result
        .value
        .and_then(|v| v.as_u64())
        .ok_or_else(|| CdpError::msg("content stream: length was not a number"))?;

    // DoS guardrail: reject documents larger than MAX_DOCUMENT_UNITS
    // before we start streaming.  Also fails the u32 conversion for
    // anything > u32::MAX.
    if total_units_u64 > MAX_DOCUMENT_UNITS as u64 {
        return Err(CdpError::msg(format!(
            "content stream: document exceeds MAX_DOCUMENT_UNITS ({} > {})",
            total_units_u64, MAX_DOCUMENT_UNITS
        )));
    }
    let total_units: u32 = total_units_u64 as u32;

    if total_units == 0 {
        return Ok(None);
    }

    // Small docs: single-shot slice to avoid N-round-trip overhead.
    let chunk = if total_units < stream_threshold_units() {
        DEFAULT_CHUNK_UNITS.max(total_units)
    } else {
        DEFAULT_CHUNK_UNITS
    };

    Ok(Some((guard, total_units, chunk)))
}

/// Issue one `Runtime.callFunctionOn` slice call and return the bytes.
async fn read_slice(
    page: &Page,
    object_id: &RemoteObjectId,
    offset: u32,
    chunk_units: u32,
) -> Result<Vec<u8>> {
    let params = CallFunctionOnParams::builder()
        .function_declaration(SLICE_FN)
        .object_id(object_id.clone())
        .argument(
            CallArgument::builder()
                .value(serde_json::json!(offset))
                .build(),
        )
        .argument(
            CallArgument::builder()
                .value(serde_json::json!(chunk_units))
                .build(),
        )
        .return_by_value(true)
        .await_promise(false)
        .build()
        .map_err(CdpError::msg)?;

    let resp = page.execute(params).await?.result;
    if let Some(ex) = resp.exception_details {
        return Err(CdpError::JavascriptException(Box::new(ex)));
    }

    match resp.result.value {
        Some(serde_json::Value::String(s)) => Ok(s.into_bytes()),
        Some(serde_json::Value::Null) | None => Ok(Vec::new()),
        other => Err(CdpError::msg(format!(
            "content stream: unexpected slice value: {other:?}"
        ))),
    }
}

/// Same as [`content_bytes_streaming`] but validates/returns a `String`.
pub async fn content_streaming(page: &Page) -> Result<String> {
    let bytes = content_bytes_streaming(page).await?;
    String::from_utf8(bytes)
        .map_err(|e| CdpError::msg(format!("invalid UTF-8 in page content: {e}")))
}

async fn read_chunks(
    page: &Page,
    object_id: &RemoteObjectId,
    total_units: u32,
    chunk_units: u32,
) -> Result<Vec<u8>> {
    // Resolve the per-process byte cap once.  `CHROMEY_CONTENT_STREAM_MAX_BYTES`
    // lets operators override the default without rebuilding.
    let byte_cap = max_accumulated_bytes();

    // Best-effort capacity hint: assume 1.5× the UTF-16 unit count as bytes
    // (pessimistic for ASCII-heavy HTML, realistic for mixed content).
    // Clamp to min(8 MiB prealloc ceiling, byte_cap) so a hostile
    // `total_units` can't coerce a giant `Vec::with_capacity`.
    let cap_hint = (total_units as usize).saturating_mul(3) / 2;
    let cap_hint = cap_hint.min(8 * 1024 * 1024).min(byte_cap);

    #[cfg(feature = "_cache_stream_disk")]
    let mut sink = Sink::open(cap_hint).await;

    #[cfg(not(feature = "_cache_stream_disk"))]
    let mut buf: Vec<u8> = Vec::with_capacity(cap_hint);

    let mut offset: u32 = 0;
    let mut rounds: usize = 0;
    let mut total_bytes: usize = 0;

    while offset < total_units {
        if rounds >= MAX_CHUNKS {
            return Err(CdpError::msg("content stream exceeded MAX_CHUNKS"));
        }
        rounds += 1;

        let chunk_bytes = read_slice(page, object_id, offset, chunk_units).await?;
        if chunk_bytes.is_empty() {
            break;
        }

        let units_advanced = utf16_len_of_utf8(&chunk_bytes);
        if units_advanced == 0 {
            break;
        }

        // DoS guardrail: cap total accumulated bytes.
        total_bytes = total_bytes.saturating_add(chunk_bytes.len());
        if total_bytes > byte_cap {
            return Err(CdpError::msg(format!(
                "content stream: accumulated bytes exceeded cap ({} > {})",
                total_bytes, byte_cap
            )));
        }

        #[cfg(feature = "_cache_stream_disk")]
        sink.write(&chunk_bytes).await;

        #[cfg(not(feature = "_cache_stream_disk"))]
        buf.extend_from_slice(&chunk_bytes);

        offset = offset.saturating_add(units_advanced);
    }

    #[cfg(feature = "_cache_stream_disk")]
    {
        Ok(sink.finish().await)
    }

    #[cfg(not(feature = "_cache_stream_disk"))]
    {
        Ok(buf)
    }
}

/// Resolve the accumulated-bytes cap for this process.  Honours
/// `CHROMEY_CONTENT_STREAM_MAX_BYTES` (integer bytes) when set; otherwise
/// returns [`DEFAULT_MAX_ACCUMULATED_BYTES`].  Shared with
/// [`crate::content_markdown`] so one operator knob caps both.
#[inline]
pub(crate) fn max_accumulated_bytes() -> usize {
    std::env::var("CHROMEY_CONTENT_STREAM_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_ACCUMULATED_BYTES)
}

/// Count the UTF-16 code units a valid UTF-8 slice decodes to.
///
/// - 1-byte sequence → 1 code unit (ASCII)
/// - 2-byte sequence (U+0080..U+07FF) → 1 code unit
/// - 3-byte sequence (U+0800..U+FFFF) → 1 code unit
/// - 4-byte sequence (U+10000..U+10FFFF) → 2 code units (surrogate pair)
///
/// Identity: `units = total_bytes − continuation_bytes + four_byte_leads`.
/// Each continuation byte (`0b10xx_xxxx`) contributes 0, every other byte
/// contributes 1, and 4-byte leads (`0b1111_0xxx`, i.e. `>= 0xF0`) contribute
/// one extra for the low surrogate.
///
/// The hot loop uses SWAR — 8 bytes per iteration via `u64` words and
/// Mycroft's zero-byte-detection trick — then a scalar tail for the remainder.
/// No `unsafe`, no panics: saturating arithmetic on the outer accumulator and
/// a final `min(u32::MAX)` clamp.
#[inline]
fn utf16_len_of_utf8(bytes: &[u8]) -> u32 {
    let total = bytes.len();

    // Word-parallel counts.  Using `u64` accumulators so we can't overflow a
    // single chunk's count (max 8 per word) regardless of input length.
    let mut cont: u64 = 0;
    let mut four: u64 = 0;

    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        // chunks_exact guarantees the slice is exactly 8 bytes long, so the
        // array conversion is infallible — no panic path.
        let arr: [u8; 8] = [
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ];
        let w = u64::from_ne_bytes(arr);

        // Continuation: `(b & 0xC0) == 0x80` — high two bits are `10`.
        // Zero-lane iff the byte is a continuation.
        let cont_mask = (w & 0xC0C0_C0C0_C0C0_C0C0) ^ 0x8080_8080_8080_8080;
        cont = cont.wrapping_add(count_zero_bytes_u64(cont_mask) as u64);

        // 4-byte lead: `b >= 0xF0` — high four bits are `1111`.
        // Zero-lane iff the byte has all four high bits set.
        let four_mask = (w & 0xF0F0_F0F0_F0F0_F0F0) ^ 0xF0F0_F0F0_F0F0_F0F0;
        four = four.wrapping_add(count_zero_bytes_u64(four_mask) as u64);
    }

    // Scalar tail (<8 bytes).
    for &b in chunks.remainder() {
        if (b & 0xC0) == 0x80 {
            cont += 1;
        }
        if b >= 0xF0 {
            four += 1;
        }
    }

    // units = total − cont + four.  Compute in u64 then clamp to u32.
    let units = (total as u64).saturating_sub(cont).saturating_add(four);
    units.min(u32::MAX as u64) as u32
}

/// Count bytes whose lane value is zero inside a `u64` word.
///
/// Classic Mycroft / Alan Mycroft's "zero byte" bit trick:
/// - `v.wrapping_sub(0x01)` underflows only in zero lanes,
/// - `!v` isolates the zero lanes,
/// - the final `0x80…` mask keeps only the high bit of each flagged lane.
///
/// `count_ones` then gives the number of zero bytes.  Branchless, no
/// panics, portable across architectures — and LLVM lowers it to `popcnt`
/// on x86-64 / AArch64 where available.
#[inline(always)]
fn count_zero_bytes_u64(v: u64) -> u32 {
    let mask = v.wrapping_sub(0x0101_0101_0101_0101) & !v & 0x8080_8080_8080_8080;
    mask.count_ones()
}

#[cfg(test)]
mod utf16_len_tests {
    use super::utf16_len_of_utf8;

    #[test]
    fn empty() {
        assert_eq!(utf16_len_of_utf8(b""), 0);
    }

    #[test]
    fn pure_ascii_short() {
        assert_eq!(utf16_len_of_utf8(b"hello"), 5);
    }

    #[test]
    fn pure_ascii_multi_word() {
        // 24 ASCII bytes → exactly 3 u64 words, zero tail.
        let s = b"abcdefghijklmnopqrstuvwx";
        assert_eq!(utf16_len_of_utf8(s), 24);
    }

    #[test]
    fn ascii_with_tail() {
        // 11 bytes → 1 word + 3-byte tail.
        let s = b"hello world";
        assert_eq!(utf16_len_of_utf8(s), 11);
    }

    #[test]
    fn two_byte_sequences() {
        // U+00E9 "é" = 2 UTF-8 bytes, 1 UTF-16 unit.
        let s = "éééééééé".as_bytes(); // 8 chars * 2 bytes = 16 bytes (2 words)
        assert_eq!(s.len(), 16);
        assert_eq!(utf16_len_of_utf8(s), 8);
    }

    #[test]
    fn three_byte_sequences() {
        // U+20AC "€" = 3 UTF-8 bytes, 1 UTF-16 unit.
        let s = "€€€€€".as_bytes(); // 5 chars * 3 bytes = 15 bytes (1 word + tail)
        assert_eq!(s.len(), 15);
        assert_eq!(utf16_len_of_utf8(s), 5);
    }

    #[test]
    fn four_byte_sequences() {
        // U+1F600 "😀" = 4 UTF-8 bytes, 2 UTF-16 units (surrogate pair).
        let s = "😀😀😀".as_bytes(); // 3 chars * 4 bytes = 12 bytes (1 word + 4-byte tail)
        assert_eq!(s.len(), 12);
        assert_eq!(utf16_len_of_utf8(s), 6);
    }

    #[test]
    fn mixed_matches_std() {
        let s = "<p>héllo 世界 🌍</p>";
        let expected: u32 = s.encode_utf16().count() as u32;
        assert_eq!(utf16_len_of_utf8(s.as_bytes()), expected);
    }

    #[test]
    fn large_ascii_matches_std() {
        let s: String = "abcdefghij".repeat(1024);
        let expected: u32 = s.encode_utf16().count() as u32;
        assert_eq!(utf16_len_of_utf8(s.as_bytes()), expected);
    }

    #[test]
    fn large_mixed_matches_std() {
        let s: String = "a€b😀c".repeat(512);
        let expected: u32 = s.encode_utf16().count() as u32;
        assert_eq!(utf16_len_of_utf8(s.as_bytes()), expected);
    }
}

#[cfg(test)]
mod drop_guard_primitive_tests {
    //! Canary for the `Handle::try_current()` primitive every Drop
    //! guard in this crate relies on (Sink, ChunkSink, StreamGuard).
    //!
    //! `StreamGuard::Drop` holds a `Page` that needs a live browser to
    //! construct and `Sink`/`ChunkSink` variants are feature-gated —
    //! if the primitive ever stops returning `Err` on a plain
    //! `std::thread`, the whole guard pattern falls over silently and
    //! we'd lose the panic-abort protection without noticing. This
    //! test runs in every configuration (no feature flags required).

    #[test]
    fn try_current_is_err_on_plain_thread() {
        let ok_outside = std::thread::spawn(|| tokio::runtime::Handle::try_current().is_ok())
            .join()
            .expect("thread panicked");
        assert!(
            !ok_outside,
            "Handle::try_current() must return Err on a std::thread that \
             is not executing inside a tokio runtime. Every `if let Ok(rt) = \
             Handle::try_current()` guard in this crate depends on it."
        );
    }

    #[test]
    fn try_current_is_ok_inside_runtime() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let ok_inside = rt.block_on(async { tokio::runtime::Handle::try_current().is_ok() });
        assert!(
            ok_inside,
            "Handle::try_current() must return Ok inside a tokio runtime \
             so the production Drop path still schedules its cleanup."
        );
    }
}

#[cfg(all(test, feature = "_cache_stream_disk"))]
mod sink_drop_tests {
    //! `Drop for Sink` enqueues a `CleanupTask::RemoveFile` onto the
    //! shared `bg_cleanup` worker — a wait-free `OnceLock::get()` +
    //! `UnboundedSender::send`. There is no `tokio::spawn` on the
    //! Drop thread, so dropping on a plain `std::thread` (no runtime
    //! context) cannot panic, even during a panic unwind.
    //!
    //! We construct `Sink::Disk` via `tokio::fs::File::from_std` over
    //! a sync-created temp file (which does not require a runtime),
    //! move the sink across a thread boundary, drop it there, and
    //! assert no panic.
    use super::Sink;

    #[test]
    fn drop_outside_runtime_is_best_effort_not_panic() {
        let path = std::env::temp_dir().join(format!(
            "chromey-sink-drop-test-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
        ));
        let std_file = std::fs::File::create(&path).expect("create temp file");
        let tokio_file = tokio::fs::File::from_std(std_file);
        let sink = Sink::Disk {
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
