//! Client bindings for the vendor `Content` CDP domain — fetch the current
//! page rendered as **Markdown** instead of HTML.
//!
//! Some remote engines expose a non-standard, capability-noun `Content`
//! domain (alongside standard protocol domains) that can serialize the page
//! as Markdown server-side. These bindings are hand-written
//! [`Command`](chromey_types::Command) implementations issued as raw
//! method strings — no PDL definitions or code generation involved — mirroring
//! how other vendor methods (e.g. `WebMCP.listTools`) are sent.
//!
//! Two consumption modes are supported:
//!
//! - **Single-shot** (`stream: false`): `Content.getMarkdown` responds with
//!   `{ "markdown": "..." }` — see [`content_markdown`].
//! - **Streaming** (`stream: true`): the response is a plain acknowledgement
//!   and the Markdown is delivered as a series of `Content.markdownChunk`
//!   push events followed by a terminal `Content.markdownDone` event — see
//!   [`content_markdown_streaming`] and [`content_markdown_stream`]. A server
//!   that answers a streaming request with the Markdown inline in the
//!   response (single-shot style) is tolerated: the inline result is used
//!   directly and no events are awaited.
//!
//! Not every engine implements `Content.getMarkdown`. Depending on the
//! engine's unknown-method policy, an unimplemented call surfaces either a
//! protocol error or an empty success result — treat an empty object as "not
//! supported yet" and fall back to [`Page::content`] plus local HTML→Markdown
//! conversion. The streaming helpers apply the same convention: an
//! acknowledged request that never produces a single stream event within the
//! event timeout is reported as unsupported rather than hanging forever.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{select, Either};
use futures_util::stream::{try_unfold, Stream};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::error::{CdpError, Result};
use crate::listeners::EventStream;
use crate::page::Page;
use chromey_cdp::cdp::CustomEvent;

/// Default per-event wait when consuming a Markdown stream, in milliseconds.
/// Matches the crate's standard request timeout
/// ([`crate::handler::REQUEST_TIMEOUT`]).  Override at runtime with the
/// `CHROMEY_MARKDOWN_STREAM_TIMEOUT_MS` env var.
pub const DEFAULT_STREAM_EVENT_TIMEOUT_MS: u64 = crate::handler::REQUEST_TIMEOUT;

/// Resolve the per-event stream timeout for this process.  Honours
/// `CHROMEY_MARKDOWN_STREAM_TIMEOUT_MS` (integer milliseconds) when set;
/// otherwise returns [`DEFAULT_STREAM_EVENT_TIMEOUT_MS`].
#[inline]
fn stream_event_timeout() -> Duration {
    let ms = std::env::var("CHROMEY_MARKDOWN_STREAM_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_STREAM_EVENT_TIMEOUT_MS);
    Duration::from_millis(ms)
}

// ---------------------------------------------------------------------------
// Command: Content.getMarkdown
// ---------------------------------------------------------------------------

/// Fetch the current page rendered as Markdown.
/// [getMarkdown]: method `Content.getMarkdown`.
///
/// With `stream: false` the engine responds with the whole document in
/// [`GetMarkdownReturns::markdown`].  With `stream: true` the response is an
/// acknowledgement and the Markdown arrives as `Content.markdownChunk`
/// events terminated by `Content.markdownDone` (see [`EventMarkdownChunk`] /
/// [`EventMarkdownDone`]).
///
/// Not every engine implements `Content.getMarkdown`. Depending on the
/// engine's unknown-method policy, an unimplemented call surfaces either a
/// protocol error or an empty success result — treat an empty object as "not
/// supported yet" and fall back to [`Page::content`] plus local conversion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetMarkdownParams {
    /// When `true`, deliver the Markdown as `Content.markdownChunk` push
    /// events plus a terminal `Content.markdownDone` event instead of one
    /// big response.
    pub stream: bool,
}

impl GetMarkdownParams {
    pub const IDENTIFIER: &'static str = "Content.getMarkdown";

    /// Create params requesting single-shot (`stream = false`) or streamed
    /// (`stream = true`) delivery.
    pub fn new(stream: bool) -> Self {
        Self { stream }
    }
}

impl chromey_types::Method for GetMarkdownParams {
    fn identifier(&self) -> chromey_types::MethodId {
        Self::IDENTIFIER.into()
    }
}

impl chromey_types::MethodType for GetMarkdownParams {
    fn method_id() -> chromey_types::MethodId
    where
        Self: Sized,
    {
        Self::IDENTIFIER.into()
    }
}

/// The response to `Content.getMarkdown`.
///
/// For a single-shot request a supporting engine sets [`markdown`]; for a
/// streaming request the response is typically an acknowledgement with no
/// `markdown` field.  An engine that does not implement the method may also
/// answer with an empty object — [`GetMarkdownReturns::is_empty`] detects
/// that "not supported yet" shape.  Any additional server-specific fields are
/// preserved losslessly in [`extra`], so the client stays neutral and
/// forward-compatible.
///
/// [`markdown`]: GetMarkdownReturns::markdown
/// [`extra`]: GetMarkdownReturns::extra
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GetMarkdownReturns {
    /// The page rendered as Markdown (single-shot mode).  Absent in a
    /// streaming acknowledgement and in the empty "not supported" response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    /// Any additional fields the server reported, preserved losslessly and
    /// opaquely (round-trips on re-serialization).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl GetMarkdownReturns {
    /// `true` when the response was a completely empty object — the
    /// graceful-degradation signal an engine without `Content.getMarkdown`
    /// support may answer with.
    pub fn is_empty(&self) -> bool {
        self.markdown.is_none() && self.extra.is_empty()
    }
}

impl chromey_types::Command for GetMarkdownParams {
    type Response = GetMarkdownReturns;
}

// ---------------------------------------------------------------------------
// Push events: Content.markdownChunk / Content.markdownDone
// ---------------------------------------------------------------------------

/// One chunk of streamed Markdown, pushed by the engine while serving a
/// `Content.getMarkdown` request with `stream: true`.
/// [markdownChunk]: event `Content.markdownChunk`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventMarkdownChunk {
    /// The Markdown fragment carried by this event.
    #[serde(default)]
    pub chunk: String,
    /// Any additional fields the server reported, preserved losslessly.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl EventMarkdownChunk {
    pub const IDENTIFIER: &'static str = "Content.markdownChunk";
}

impl chromey_types::MethodType for EventMarkdownChunk {
    fn method_id() -> chromey_types::MethodId
    where
        Self: Sized,
    {
        Self::IDENTIFIER.into()
    }
}

impl CustomEvent for EventMarkdownChunk {}

/// Terminal event closing a streamed `Content.getMarkdown` response: every
/// `Content.markdownChunk` for the request has been delivered.
/// [markdownDone]: event `Content.markdownDone`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventMarkdownDone {
    /// Any additional fields the server reported, preserved losslessly.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl EventMarkdownDone {
    pub const IDENTIFIER: &'static str = "Content.markdownDone";
}

impl chromey_types::MethodType for EventMarkdownDone {
    fn method_id() -> chromey_types::MethodId
    where
        Self: Sized,
    {
        Self::IDENTIFIER.into()
    }
}

impl CustomEvent for EventMarkdownDone {}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Fetch the page as Markdown in one round-trip (`Content.getMarkdown` with
/// `stream: false`).
///
/// Returns `Ok(Some(markdown))` from a supporting engine.  `Ok(None)` means
/// the engine acknowledged the call with an empty object — the documented
/// "not supported yet" shape — and the caller should fall back to
/// [`Page::content`] plus local HTML→Markdown conversion.  Engines whose
/// unknown-method policy is to error (e.g. stock Chrome) surface a regular
/// protocol error through [`Result`] instead; treat that the same way.
pub async fn content_markdown(page: &Page) -> Result<Option<String>> {
    Ok(page
        .execute(GetMarkdownParams::new(false))
        .await?
        .result
        .markdown)
}

/// Fetch the page as Markdown via the streamed protocol
/// (`Content.getMarkdown` with `stream: true`), accumulating the
/// `Content.markdownChunk` events into one `String` until
/// `Content.markdownDone`.
///
/// Returns `Ok(Some(markdown))` on success.  `Ok(None)` means the engine
/// acknowledged the request but never emitted a single stream event within
/// the event timeout — the streaming analogue of the documented empty-object
/// "not supported yet" response — so fall back to [`Page::content`] plus
/// local conversion.  A protocol error from an engine that rejects unknown
/// methods surfaces through [`Result`]; treat it the same way.
///
/// A server that answers the streaming request with the Markdown inline in
/// the response is tolerated: the inline result is returned directly.
///
/// # Guardrails
///
/// - Each event wait is bounded by [`DEFAULT_STREAM_EVENT_TIMEOUT_MS`]
///   (override with the `CHROMEY_MARKDOWN_STREAM_TIMEOUT_MS` env var).  A
///   stall *after* streaming began returns [`CdpError::Timeout`] rather than
///   `Ok(None)`.
/// - Accumulated bytes are capped at
///   [`crate::content_stream::DEFAULT_MAX_ACCUMULATED_BYTES`] (override with
///   the `CHROMEY_CONTENT_STREAM_MAX_BYTES` env var); hitting the cap
///   returns an error rather than continuing to allocate.
pub async fn content_markdown_streaming(page: &Page) -> Result<Option<String>> {
    // Subscribe before issuing the command so no early chunk can be missed:
    // listener registration and the command travel the same ordered channel
    // to the handler.
    let mut chunks = page.event_listener::<EventMarkdownChunk>().await?;
    let mut done = page.event_listener::<EventMarkdownDone>().await?;

    let resp = page.execute(GetMarkdownParams::new(true)).await?.result;
    if let Some(markdown) = resp.markdown {
        // Engine answered single-shot despite `stream: true` — tolerated.
        return Ok(Some(markdown));
    }

    let timeout = stream_event_timeout();
    let byte_cap = crate::content_stream::max_accumulated_bytes();
    let mut out = String::new();
    let mut got_any = false;
    let mut rounds: usize = 0;

    loop {
        if rounds >= crate::content_stream::MAX_CHUNKS {
            return Err(CdpError::msg("markdown stream exceeded MAX_CHUNKS"));
        }
        rounds += 1;

        match next_event(&mut chunks, &mut done, timeout).await {
            StreamStep::Chunk(chunk) => {
                got_any = true;
                if out.len().saturating_add(chunk.len()) > byte_cap {
                    return Err(CdpError::msg(format!(
                        "markdown stream: accumulated bytes exceeded cap ({} > {})",
                        out.len().saturating_add(chunk.len()),
                        byte_cap
                    )));
                }
                out.push_str(&chunk);
            }
            StreamStep::Done => return Ok(Some(out)),
            StreamStep::Closed => {
                return Err(CdpError::msg(
                    "markdown stream: event channel closed before Content.markdownDone",
                ));
            }
            StreamStep::TimedOut => {
                if got_any {
                    return Err(CdpError::Timeout);
                }
                // Acknowledged but silent: the engine does not implement
                // streamed Markdown — graceful-degradation convention.
                return Ok(None);
            }
        }
    }
}

/// Pump-style async [`Stream`] of the page's Markdown — yields each
/// `Content.markdownChunk` fragment as the engine pushes it, without
/// accumulating, terminating cleanly on `Content.markdownDone`.
///
/// The first poll subscribes to the events and issues `Content.getMarkdown`
/// with `stream: true`; each subsequent poll yields one chunk.  A server
/// that answers with the Markdown inline in the response is tolerated: the
/// stream yields it as a single item and ends.
///
/// # Errors / graceful degradation
///
/// - An engine that rejects unknown methods errors on first poll.
/// - An engine that acknowledges the request but never emits a single stream
///   event within the event timeout ([`DEFAULT_STREAM_EVENT_TIMEOUT_MS`],
///   override with `CHROMEY_MARKDOWN_STREAM_TIMEOUT_MS`) yields one error
///   identifying the method as unsupported.  On any error, fall back to
///   [`Page::content`] plus local HTML→Markdown conversion.
/// - A stall *after* streaming began yields [`CdpError::Timeout`].
///
/// Drop the stream early (cancellation, `StreamExt::take`, `break`) to stop
/// consuming; the event listeners are pruned by the handler once their
/// receivers are dropped.  The pump itself does **not** enforce a
/// total-bytes cap (that's the caller's responsibility when consuming); for
/// a capped accumulating read use [`content_markdown_streaming`].
pub fn content_markdown_stream(page: &Page) -> impl Stream<Item = Result<String>> + Send + 'static {
    let page = page.clone();
    try_unfold(PumpState::Init { page }, |state| async move {
        match state {
            PumpState::Init { page } => {
                // Subscribe before issuing the command so no early chunk can
                // be missed.
                let chunks = page.event_listener::<EventMarkdownChunk>().await?;
                let done = page.event_listener::<EventMarkdownDone>().await?;

                let resp = page.execute(GetMarkdownParams::new(true)).await?.result;
                if let Some(markdown) = resp.markdown {
                    // Single-shot answer despite `stream: true` — yield it
                    // as the only item, then end.
                    return Ok(Some((markdown, PumpState::Finished)));
                }

                let timeout = stream_event_timeout();
                pump_next(chunks, done, timeout, false, 0).await
            }
            PumpState::Pumping {
                chunks,
                done,
                timeout,
                got_any,
                rounds,
            } => pump_next(chunks, done, timeout, got_any, rounds).await,
            PumpState::Finished => Ok(None),
        }
    })
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Internal state for the pump-stream unfold.  `Send + 'static` so the
/// stream is usable across `.await` points without borrowing the caller.
enum PumpState {
    /// Haven't subscribed / issued the command yet.
    Init { page: Page },
    /// Command acknowledged; consuming chunk events.
    Pumping {
        chunks: EventStream<EventMarkdownChunk>,
        done: EventStream<EventMarkdownDone>,
        timeout: Duration,
        /// Whether at least one stream event arrived (drives the
        /// unsupported-vs-stalled distinction on timeout).
        got_any: bool,
        rounds: usize,
    },
    /// Inline single-shot answer already yielded.
    Finished,
}

/// Await the next stream event (or timeout) and return the next unfold step.
async fn pump_next(
    mut chunks: EventStream<EventMarkdownChunk>,
    mut done: EventStream<EventMarkdownDone>,
    timeout: Duration,
    got_any: bool,
    rounds: usize,
) -> Result<Option<(String, PumpState)>> {
    if rounds >= crate::content_stream::MAX_CHUNKS {
        return Ok(None);
    }
    match next_event(&mut chunks, &mut done, timeout).await {
        StreamStep::Chunk(chunk) => Ok(Some((
            chunk,
            PumpState::Pumping {
                chunks,
                done,
                timeout,
                got_any: true,
                rounds: rounds + 1,
            },
        ))),
        StreamStep::Done => Ok(None),
        StreamStep::Closed => Err(CdpError::msg(
            "markdown stream: event channel closed before Content.markdownDone",
        )),
        StreamStep::TimedOut => {
            if got_any {
                Err(CdpError::Timeout)
            } else {
                Err(CdpError::msg(
                    "Content.getMarkdown: no stream events before timeout — the engine \
                     likely does not support streamed Markdown; fall back to Page::content \
                     and convert locally",
                ))
            }
        }
    }
}

/// Outcome of one bounded wait on the chunk/done event pair.
enum StreamStep {
    /// A `Content.markdownChunk` arrived carrying this fragment.
    Chunk(String),
    /// `Content.markdownDone` arrived: the stream completed.
    Done,
    /// A listener channel closed (handler shut down) before completion.
    Closed,
    /// No event arrived within the timeout window.
    TimedOut,
}

/// Wait (bounded by `timeout`) for whichever of the two event streams fires
/// first.  When both are ready the chunk side wins, so no chunk queued ahead
/// of the terminal event is ever dropped — the done event stays queued for
/// the next call.
async fn next_event(
    chunks: &mut EventStream<EventMarkdownChunk>,
    done: &mut EventStream<EventMarkdownDone>,
    timeout: Duration,
) -> StreamStep {
    match tokio::time::timeout(timeout, select(chunks.next(), done.next())).await {
        Err(_elapsed) => StreamStep::TimedOut,
        Ok(Either::Left((Some(ev), _))) => StreamStep::Chunk(
            // Avoid copying the fragment when we hold the only reference.
            Arc::try_unwrap(ev)
                .map(|e| e.chunk)
                .unwrap_or_else(|shared| shared.chunk.clone()),
        ),
        Ok(Either::Right((Some(_done_ev), _))) => StreamStep::Done,
        Ok(Either::Left((None, _))) | Ok(Either::Right((None, _))) => StreamStep::Closed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn method_identifiers_match_the_domain() {
        use chromey_types::{Method, MethodType};

        assert_eq!(GetMarkdownParams::IDENTIFIER, "Content.getMarkdown");
        assert_eq!(EventMarkdownChunk::IDENTIFIER, "Content.markdownChunk");
        assert_eq!(EventMarkdownDone::IDENTIFIER, "Content.markdownDone");

        assert_eq!(
            GetMarkdownParams::default().identifier(),
            "Content.getMarkdown"
        );
        assert_eq!(EventMarkdownChunk::method_id(), "Content.markdownChunk");
        assert_eq!(EventMarkdownDone::method_id(), "Content.markdownDone");

        // Wire shape: `{ "stream": bool }`, nothing else.
        assert_eq!(
            serde_json::to_value(GetMarkdownParams::new(false)).expect("params must serialize"),
            json!({ "stream": false })
        );
        assert_eq!(
            serde_json::to_value(GetMarkdownParams::new(true)).expect("params must serialize"),
            json!({ "stream": true })
        );
    }

    #[test]
    fn deserializes_single_shot_response() {
        let returns: GetMarkdownReturns =
            serde_json::from_value(json!({ "markdown": "# Title\n\nBody" }))
                .expect("response must deserialize");
        assert_eq!(returns.markdown.as_deref(), Some("# Title\n\nBody"));
        assert!(returns.extra.is_empty());
        assert!(!returns.is_empty());
    }

    #[test]
    fn empty_object_response_means_not_supported() {
        // Graceful-degradation convention: an engine without the method may
        // answer with an empty success object instead of a protocol error.
        let returns: GetMarkdownReturns =
            serde_json::from_value(json!({})).expect("empty object must deserialize");
        assert_eq!(returns.markdown, None);
        assert!(returns.is_empty());
    }

    #[test]
    fn response_extras_are_lossless() {
        let wire = json!({ "markdown": "# Hi", "truncated": false, "units": 2 });
        let returns: GetMarkdownReturns =
            serde_json::from_value(wire.clone()).expect("response must deserialize");
        assert_eq!(returns.markdown.as_deref(), Some("# Hi"));
        assert_eq!(returns.extra["truncated"], json!(false));
        assert_eq!(returns.extra["units"], json!(2));
        // Round-trips byte-identically, extras included.
        assert_eq!(
            serde_json::to_value(&returns).expect("response must serialize"),
            wire
        );
    }

    #[test]
    fn chunk_and_done_events_deserialize() {
        let chunk: EventMarkdownChunk =
            serde_json::from_value(json!({ "chunk": "## Section" })).expect("chunk event");
        assert_eq!(chunk.chunk, "## Section");
        assert!(chunk.extra.is_empty());

        // Tolerates a bare object and preserves unknown fields.
        let chunk: EventMarkdownChunk =
            serde_json::from_value(json!({ "chunk": "x", "seq": 3 })).expect("chunk event");
        assert_eq!(chunk.extra["seq"], json!(3));

        let done: EventMarkdownDone = serde_json::from_value(json!({})).expect("done event");
        assert!(done.extra.is_empty());
        let done: EventMarkdownDone =
            serde_json::from_value(json!({ "totalChunks": 7 })).expect("done event");
        assert_eq!(done.extra["totalChunks"], json!(7));
    }

    /// End-to-end through the crate's listener machinery: register listeners
    /// for the raw vendor method strings, dispatch raw JSON the way the
    /// handler does for unknown (non-PDL) events, and receive typed events.
    #[tokio::test]
    async fn dispatches_through_custom_event_listeners() {
        use crate::listeners::{EventListenerRequest, EventListeners};

        let mut listeners = EventListeners::default();

        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        listeners.add_listener(EventListenerRequest::new::<EventMarkdownChunk>(chunk_tx));
        listeners.add_listener(EventListenerRequest::new::<EventMarkdownDone>(done_tx));

        // Exactly what `consume_event!`'s custom branch does with an
        // unknown-method event's payload.
        listeners
            .try_send_custom("Content.markdownChunk", json!({ "chunk": "# A" }))
            .expect("chunk dispatch");
        listeners
            .try_send_custom("Content.markdownDone", json!({}))
            .expect("done dispatch");
        listeners.flush();

        let mut chunks = EventStream::<EventMarkdownChunk>::new(chunk_rx);
        let mut done = EventStream::<EventMarkdownDone>::new(done_rx);

        let ev = chunks.next().await.expect("one chunk event");
        assert_eq!(ev.chunk, "# A");
        assert!(done.next().await.is_some());
    }

    /// The select in `next_event` must prefer a queued chunk over a queued
    /// done event, and still surface the done event on the following call.
    #[tokio::test]
    async fn next_event_prefers_chunks_then_done() {
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut chunks = EventStream::<EventMarkdownChunk>::new(chunk_rx);
        let mut done = EventStream::<EventMarkdownDone>::new(done_rx);

        let chunk_ev: Arc<dyn chromey_cdp::cdp::Event> = Arc::new(EventMarkdownChunk {
            chunk: "part".to_string(),
            extra: Default::default(),
        });
        let done_ev: Arc<dyn chromey_cdp::cdp::Event> =
            Arc::new(EventMarkdownDone::default());
        chunk_tx.send(chunk_ev).expect("send chunk");
        done_tx.send(done_ev).expect("send done");

        let timeout = Duration::from_secs(5);
        match next_event(&mut chunks, &mut done, timeout).await {
            StreamStep::Chunk(s) => assert_eq!(s, "part"),
            _ => panic!("expected the queued chunk first"),
        }
        match next_event(&mut chunks, &mut done, timeout).await {
            StreamStep::Done => {}
            _ => panic!("expected the queued done event second"),
        }
    }

    /// With nothing queued and both senders alive, the bounded wait reports
    /// a timeout (this is the "acknowledged but silent = unsupported" probe).
    #[tokio::test]
    async fn next_event_times_out_when_silent() {
        let (_chunk_tx, chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut chunks = EventStream::<EventMarkdownChunk>::new(chunk_rx);
        let mut done = EventStream::<EventMarkdownDone>::new(done_rx);

        match next_event(&mut chunks, &mut done, Duration::from_millis(20)).await {
            StreamStep::TimedOut => {}
            _ => panic!("expected a timeout"),
        }
    }
}
