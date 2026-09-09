use std::collections::VecDeque;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::ready;

use futures_util::stream::{FuturesOrdered, SplitSink};
use futures_util::{SinkExt, Stream, StreamExt};
use std::future::Future;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::http::{HeaderMap, HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::{tungstenite::protocol::WebSocketConfig, WebSocketStream};

use chromiumoxide_cdp::cdp::browser_protocol::target::SessionId;
use chromiumoxide_types::{CallId, EventMessage, Message, MethodCall, MethodId};

use crate::error::CdpError;
use crate::error::Result;

type ConnectStream = MaybeTlsStream<tokio::net::TcpStream>;

/// Exchanges the messages with the websocket
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct Connection<T: EventMessage> {
    /// Queue of commands to send.
    pending_commands: VecDeque<MethodCall>,
    /// The websocket of the chromium instance
    ws: WebSocketStream<ConnectStream>,
    /// The identifier for a specific command
    next_id: usize,
    /// Whether the write buffer has unsent data that needs flushing.
    needs_flush: bool,
    /// The phantom marker.
    _marker: PhantomData<T>,
}

lazy_static::lazy_static! {
    /// Nagle's algorithm disabled?
    static ref DISABLE_NAGLE: bool = match std::env::var("DISABLE_NAGLE") {
        Ok(disable_nagle) => disable_nagle == "true",
        _ => true
    };
    /// Websocket config defaults
    static ref WEBSOCKET_DEFAULTS: bool = match std::env::var("WEBSOCKET_DEFAULTS") {
        Ok(d) => d == "true",
        _ => false
    };
}

/// Default number of WebSocket connection retry attempts.
pub const DEFAULT_CONNECTION_RETRIES: u32 = 4;

/// Initial backoff delay between connection retries (in milliseconds).
const INITIAL_BACKOFF_MS: u64 = 50;

/// Maximum backoff delay between connection retries (in milliseconds).
pub(crate) const MAX_BACKOFF_MS: u64 = 2_000;

/// Why a header was refused by [`ConnectHeaders`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectHeaderError {
    /// The name is not a valid HTTP header name.
    #[error("invalid connect header name: {0}")]
    InvalidName(String),
    /// The value is not visible ASCII. The upgrade request is written as text,
    /// so a value tungstenite cannot render as a `str` would fail the handshake
    /// at connect time instead of here.
    #[error("connect header `{0}` must be visible ASCII")]
    NonAscii(String),
    /// The name belongs to the WebSocket handshake itself (`host`,
    /// `connection`, `upgrade`, `sec-websocket-*`). tungstenite writes those
    /// and rejects duplicates, so setting one can only break the upgrade.
    #[error("connect header `{0}` is owned by the WebSocket handshake")]
    Reserved(String),
    /// Adding the header would push the set past
    /// [`ConnectHeaders::MAX_TOTAL_BYTES`].
    #[error("connect headers would total {total} bytes, over the {max} byte limit")]
    TooLarge {
        /// Wire size the set would have after the insert.
        total: usize,
        /// The limit, [`ConnectHeaders::MAX_TOTAL_BYTES`].
        max: usize,
    },
}

/// Headers added to the CDP WebSocket upgrade request (the HTTP `GET` that
/// opens the socket), as opposed to `extra_headers`, which are applied to the
/// page's own network requests via `Network.setExtraHTTPHeaders`.
///
/// A gateway in front of the browser sees these on the upgrade head and can
/// use them to tag or route the connection. Each name appears once; inserting
/// a name that is already present replaces its value. The set is validated at
/// insert time, so a `ConnectHeaders` that exists can always be sent. An empty
/// set leaves the upgrade request byte for byte as it was without one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectHeaders {
    headers: Vec<(HeaderName, HeaderValue)>,
    /// Wire bytes the current set would occupy, see [`Self::wire_len`].
    total: usize,
}

impl ConnectHeaders {
    /// Upper bound on the wire size of the whole set, counted as
    /// `name: value\r\n` per header. Keeps a misconfigured caller from
    /// shipping a request head a proxy will reject.
    pub const MAX_TOTAL_BYTES: usize = 8 * 1024;

    const RESERVED: [&'static str; 3] = ["host", "connection", "upgrade"];

    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one header, replacing any existing header of the same name.
    ///
    /// `name` and `value` accept anything the `http` crate can convert
    /// (`&str`, `String`, or the typed `HeaderName` / `HeaderValue`).
    pub fn insert<N, V>(&mut self, name: N, value: V) -> std::result::Result<(), ConnectHeaderError>
    where
        N: TryInto<HeaderName>,
        N::Error: std::fmt::Display,
        V: TryInto<HeaderValue>,
        V::Error: std::fmt::Display,
    {
        let name: HeaderName = name
            .try_into()
            .map_err(|e| ConnectHeaderError::InvalidName(e.to_string()))?;
        let value: HeaderValue = value
            .try_into()
            .map_err(|_| ConnectHeaderError::NonAscii(name.as_str().to_string()))?;

        let lower = name.as_str();
        if Self::RESERVED.contains(&lower) || lower.starts_with("sec-websocket-") {
            return Err(ConnectHeaderError::Reserved(lower.to_string()));
        }
        // `HeaderValue` allows obs-text (0x80..=0xFF); the handshake writer
        // calls `to_str`, which does not.
        if value.to_str().is_err() {
            return Err(ConnectHeaderError::NonAscii(lower.to_string()));
        }

        let existing = self.headers.iter().position(|(n, _)| *n == name);
        let replaced = existing.map_or(0, |i| Self::wire_len(&self.headers[i]));
        let total = self.total - replaced + Self::wire_len(&(name.clone(), value.clone()));
        if total > Self::MAX_TOTAL_BYTES {
            return Err(ConnectHeaderError::TooLarge {
                total,
                max: Self::MAX_TOTAL_BYTES,
            });
        }

        match existing {
            Some(i) => self.headers[i] = (name, value),
            None => self.headers.push((name, value)),
        }
        self.total = total;
        Ok(())
    }

    /// Builder form of [`Self::insert`].
    pub fn with<N, V>(mut self, name: N, value: V) -> std::result::Result<Self, ConnectHeaderError>
    where
        N: TryInto<HeaderName>,
        N::Error: std::fmt::Display,
        V: TryInto<HeaderValue>,
        V::Error: std::fmt::Display,
    {
        self.insert(name, value)?;
        Ok(self)
    }

    /// Whether the set holds no headers.
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// Number of headers in the set.
    pub fn len(&self) -> usize {
        self.headers.len()
    }

    /// Wire bytes the set occupies on the upgrade request.
    pub fn total_bytes(&self) -> usize {
        self.total
    }

    /// The headers in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&HeaderName, &HeaderValue)> {
        self.headers.iter().map(|(n, v)| (n, v))
    }

    /// Bytes `name: value\r\n` takes on the wire.
    fn wire_len((name, value): &(HeaderName, HeaderValue)) -> usize {
        name.as_str().len() + 2 + value.len() + 2
    }

    /// Put the set on an upgrade request. A no-op when empty, so the
    /// request tungstenite builds from the URL is untouched.
    pub(crate) fn apply(
        &self,
        request: &mut tokio_tungstenite::tungstenite::handshake::client::Request,
    ) {
        let headers = request.headers_mut();
        for (name, value) in &self.headers {
            headers.insert(name.clone(), value.clone());
        }
    }
}

impl TryFrom<HeaderMap> for ConnectHeaders {
    type Error = ConnectHeaderError;

    /// Every entry is validated as by [`ConnectHeaders::insert`]. A name that
    /// repeats in the map keeps its last value.
    fn try_from(map: HeaderMap) -> std::result::Result<Self, Self::Error> {
        let mut out = Self::new();
        for (name, value) in map.iter() {
            out.insert(name.clone(), value.clone())?;
        }
        Ok(out)
    }
}

impl TryFrom<&HeaderMap> for ConnectHeaders {
    type Error = ConnectHeaderError;

    fn try_from(map: &HeaderMap) -> std::result::Result<Self, Self::Error> {
        Self::try_from(map.clone())
    }
}

impl<T: EventMessage + Unpin> Connection<T> {
    pub async fn connect(debug_ws_url: impl AsRef<str>) -> Result<Self> {
        Self::connect_with_retries(debug_ws_url, DEFAULT_CONNECTION_RETRIES).await
    }

    pub async fn connect_with_retries(debug_ws_url: impl AsRef<str>, retries: u32) -> Result<Self> {
        Self::connect_with_retries_and_headers(debug_ws_url, retries, &ConnectHeaders::new()).await
    }

    /// Like [`Self::connect`], with `headers` added to the WebSocket upgrade
    /// request. Every attempt, including retries, carries them.
    pub async fn connect_with_headers(
        debug_ws_url: impl AsRef<str>,
        headers: &ConnectHeaders,
    ) -> Result<Self> {
        Self::connect_with_retries_and_headers(debug_ws_url, DEFAULT_CONNECTION_RETRIES, headers)
            .await
    }

    /// Like [`Self::connect_with_retries`], with `headers` added to the
    /// WebSocket upgrade request on every attempt.
    pub async fn connect_with_retries_and_headers(
        debug_ws_url: impl AsRef<str>,
        retries: u32,
        headers: &ConnectHeaders,
    ) -> Result<Self> {
        let mut config = WebSocketConfig::default();

        // Cap the internal write buffer so a slow receiver cannot cause
        // unbounded memory growth (default is usize::MAX).
        config.max_write_buffer_size = 4 * 1024 * 1024;

        if !*WEBSOCKET_DEFAULTS {
            config.max_message_size = None;
            config.max_frame_size = None;
        }

        let url = debug_ws_url.as_ref();
        let use_uring = crate::uring_fs::is_enabled();
        let mut last_err = None;

        for attempt in 0..=retries {
            let result = if use_uring {
                Self::connect_uring(url, config, headers).await
            } else {
                Self::connect_default(url, config, headers).await
            };

            match result {
                Ok(ws) => {
                    return Ok(Self {
                        pending_commands: Default::default(),
                        ws,
                        next_id: 0,
                        needs_flush: false,
                        _marker: Default::default(),
                    });
                }
                Err(e) => {
                    // Detect non-retriable errors early to avoid wasting time
                    // on connections that will never succeed.
                    let should_retry = match &e {
                        // Connection refused — nothing is listening on this port.
                        CdpError::Io(io_err)
                            if io_err.kind() == std::io::ErrorKind::ConnectionRefused =>
                        {
                            false
                        }
                        // HTTP response to a WebSocket upgrade (e.g. wrong path
                        // returns 404 / redirect) — retrying the same URL won't help.
                        CdpError::Ws(tungstenite_err) => !matches!(
                            tungstenite_err,
                            tokio_tungstenite::tungstenite::Error::Http(_)
                                | tokio_tungstenite::tungstenite::Error::HttpFormat(_)
                        ),
                        _ => true,
                    };

                    last_err = Some(e);

                    if !should_retry {
                        break;
                    }

                    if attempt < retries {
                        let backoff_ms =
                            (INITIAL_BACKOFF_MS * 3u64.saturating_pow(attempt)).min(MAX_BACKOFF_MS);
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| CdpError::msg("connection failed")))
    }

    /// Default path: let tokio-tungstenite handle TCP connect + WS handshake.
    ///
    /// For a plaintext `ws://` endpoint addressed by **hostname**, the resolution
    /// is served from [`crate::dns`] (a short-TTL cache) and the handshake runs
    /// over a pre-connected socket — the same shape the io_uring path uses — so
    /// repeated (re)connects skip `getaddrinfo`. The original request is passed
    /// to `client_async_with_config`, preserving the `Host` header. Everything
    /// else — `wss://` (TLS), IP-literal hosts (no DNS), or any URL parse issue —
    /// falls through to `connect_async_with_config` exactly as before, so TLS
    /// handling and behavior are unchanged. The cache only maps host→IP; the port
    /// and path always come from `url`, so it can never retarget a connection.
    ///
    /// `headers` go on the upgrade request before the handshake on both
    /// branches. When the set is empty the fallback still hands the bare `url`
    /// to tungstenite, so the request it writes is the one it always wrote.
    async fn connect_default(
        url: &str,
        config: WebSocketConfig,
        headers: &ConnectHeaders,
    ) -> Result<WebSocketStream<ConnectStream>> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        if let Ok(mut request) = url.into_client_request() {
            headers.apply(&mut request);
            let uri = request.uri();
            let is_plain_ws = uri.scheme_str() == Some("ws");
            let host = uri.host().map(str::to_string);
            let port = uri.port_u16().unwrap_or(9222);

            if is_plain_ws && crate::dns::enabled() {
                if let Some(h) = host {
                    // Hostname only — IP literals need no DNS and are left to the
                    // default path below.
                    if !crate::dns::is_ip_literal(&h) {
                        if let Ok(addrs) = crate::dns::resolve(&h, port).await {
                            match tokio::net::TcpStream::connect(&addrs[..]).await {
                                Ok(stream) => {
                                    if *DISABLE_NAGLE {
                                        let _ = stream.set_nodelay(true);
                                    }
                                    let (ws, _) = tokio_tungstenite::client_async_with_config(
                                        request,
                                        MaybeTlsStream::Plain(stream),
                                        Some(config),
                                    )
                                    .await?;
                                    return Ok(ws);
                                }
                                Err(e) => {
                                    // A cached address that won't connect may be
                                    // stale — drop it so the retry re-resolves.
                                    crate::dns::invalidate(&h, port);
                                    return Err(CdpError::Io(e));
                                }
                            }
                        }
                        // resolve failed → fall through to the default path, which
                        // surfaces the real connection error.
                    }
                }
            }
        }

        if headers.is_empty() {
            let (ws, _) =
                tokio_tungstenite::connect_async_with_config(url, Some(config), *DISABLE_NAGLE)
                    .await?;
            return Ok(ws);
        }

        let mut request = url.into_client_request()?;
        headers.apply(&mut request);
        let (ws, _) =
            tokio_tungstenite::connect_async_with_config(request, Some(config), *DISABLE_NAGLE)
                .await?;
        Ok(ws)
    }

    /// io_uring path: pre-connect the TCP socket via io_uring, then do WS
    /// handshake over the pre-connected stream.
    ///
    /// `headers` go on the upgrade request before the handshake, and travel
    /// with the fallback to [`Self::connect_default`].
    async fn connect_uring(
        url: &str,
        config: WebSocketConfig,
        headers: &ConnectHeaders,
    ) -> Result<WebSocketStream<ConnectStream>> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let mut request = url.into_client_request()?;
        headers.apply(&mut request);
        let host = request
            .uri()
            .host()
            .ok_or_else(|| CdpError::msg("no host in CDP WebSocket URL"))?;
        let port = request.uri().port_u16().unwrap_or(9222);

        // Resolve host → SocketAddr (CDP is always localhost, so this is fast).
        let addr_str = format!("{}:{}", host, port);
        let addr: std::net::SocketAddr = match addr_str.parse() {
            Ok(a) => a,
            Err(_) => {
                // Hostname needs DNS — fall back to default path.
                return Self::connect_default(url, config, headers).await;
            }
        };

        // TCP connect via io_uring.
        let std_stream = crate::uring_fs::tcp_connect(addr)
            .await
            .map_err(CdpError::Io)?;

        // Set non-blocking + Nagle.
        std_stream.set_nonblocking(true).map_err(CdpError::Io)?;
        if *DISABLE_NAGLE {
            let _ = std_stream.set_nodelay(true);
        }

        // Wrap in tokio TcpStream.
        let tokio_stream = tokio::net::TcpStream::from_std(std_stream).map_err(CdpError::Io)?;

        // WebSocket handshake over the pre-connected stream.
        let (ws, _) = tokio_tungstenite::client_async_with_config(
            request,
            MaybeTlsStream::Plain(tokio_stream),
            Some(config),
        )
        .await?;

        Ok(ws)
    }
}

impl<T: EventMessage> Connection<T> {
    fn next_call_id(&mut self) -> CallId {
        let id = CallId::new(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// Queue in the command to send over the socket and return the id for this
    /// command
    pub fn submit_command(
        &mut self,
        method: MethodId,
        session_id: Option<SessionId>,
        params: serde_json::Value,
    ) -> serde_json::Result<CallId> {
        let id = self.next_call_id();
        let call = MethodCall {
            id,
            method,
            session_id: session_id.map(Into::into),
            params,
        };
        self.pending_commands.push_back(call);
        Ok(id)
    }

    /// Buffer all queued commands into the WebSocket sink, then flush once.
    ///
    /// This batches multiple CDP commands into a single TCP write instead of
    /// flushing after every individual message.
    fn start_send_next(&mut self, cx: &mut Context<'_>) -> Result<()> {
        // Complete any pending flush from a previous poll first.
        if self.needs_flush {
            match self.ws.poll_flush_unpin(cx) {
                Poll::Ready(Ok(())) => self.needs_flush = false,
                Poll::Ready(Err(e)) => return Err(e.into()),
                Poll::Pending => return Ok(()),
            }
        }

        // Buffer as many queued commands as the sink will accept.
        let mut sent_any = false;
        while !self.pending_commands.is_empty() {
            match self.ws.poll_ready_unpin(cx) {
                Poll::Ready(Ok(())) => {
                    let Some(cmd) = self.pending_commands.pop_front() else {
                        break;
                    };
                    tracing::trace!("Sending {:?}", cmd);
                    let msg = serde_json::to_string(&cmd)?;
                    self.ws.start_send_unpin(msg.into())?;
                    sent_any = true;
                }
                _ => break,
            }
        }

        // Flush the entire batch in one write.
        if sent_any {
            match self.ws.poll_flush_unpin(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Err(e.into()),
                Poll::Pending => self.needs_flush = true,
            }
        }

        Ok(())
    }
}

/// Capacity of the bounded channel feeding the background WS writer task.
/// Large enough that bursts of CDP commands never block the handler, small
/// enough to apply back-pressure before memory grows without bound.
const WS_CMD_CHANNEL_CAPACITY: usize = 2048;

/// Capacity of the bounded channel from the background WS reader task to
/// the Handler. Keeps decoded CDP messages buffered so the reader task
/// can keep reading the socket while the Handler processes a backlog;
/// applies TCP-level back-pressure on Chrome when the Handler is slow
/// (the reader awaits channel capacity, stops draining the socket).
const WS_READ_CHANNEL_CAPACITY: usize = 1024;

/// Maximum number of in-flight decodes the reader pipeline holds at
/// once. While any of these is still running on the blocking pool,
/// the reader can keep draining the socket and starting new decodes,
/// up to this cap. Applies per-connection; the resulting decoded
/// messages are emitted to the Handler in strict WS arrival order
/// via a `FuturesOrdered` queue — no behavior change versus the
/// serial loop, just concurrent execution of independent decodes.
const MAX_IN_FLIGHT_DECODES: usize = 32;

/// Payload size at/above which `decode_message` runs via
/// `tokio::task::spawn_blocking` instead of inline on the reader task.
///
/// `serde_json::from_slice` is CPU-bound with no `.await` points, so
/// a multi-MB payload can occupy one tokio worker thread for tens of
/// milliseconds. Offloading to the blocking thread pool above a
/// threshold keeps the reader task cooperatively yielding — critical
/// on single-threaded runtimes where the reader shares its worker
/// with the Handler, user tasks, and timers.
///
/// The threshold is chosen so that typical CDP traffic (events,
/// responses, small evaluates) stays on the inline fast path and
/// doesn't pay the ~10-30 µs `spawn_blocking` hand-off cost, while
/// screenshot payloads, wide network events, and huge console
/// payloads take the offloaded path.
const LARGE_FRAME_THRESHOLD: usize = 256 * 1024; // 256 KiB

/// Split parts returned by [`Connection::into_async`].
#[derive(Debug)]
pub struct AsyncConnection<T: EventMessage> {
    /// Receive half for decoded CDP messages. Backed by a bounded mpsc
    /// fed by a dedicated background reader task — decode runs on that
    /// task, never on the Handler task, so large CDP responses (multi-MB
    /// screenshots, huge event payloads) cannot stall the Handler's
    /// event loop.
    pub reader: WsReader<T>,
    /// Sender half for submitting outgoing CDP commands.
    pub cmd_tx: mpsc::Sender<MethodCall>,
    /// Handle to the background writer task.
    pub writer_handle: tokio::task::JoinHandle<Result<()>>,
    /// Handle to the background reader task (reads + decodes WS frames).
    pub reader_handle: tokio::task::JoinHandle<()>,
    /// Next command-call-id counter (continue numbering from where Connection left off).
    pub next_id: usize,
}

impl<T: EventMessage + Unpin + Send + 'static> Connection<T> {
    /// Consume the connection and split into a background reader + writer
    /// pair, exposing the Handler-facing ends via `AsyncConnection`.
    ///
    /// Two `tokio::spawn`'d tasks are created:
    ///
    /// * `ws_write_loop` — batches outgoing commands and flushes them in
    ///   one write per wakeup.
    /// * `ws_read_loop`  — reads WS frames, decodes them to typed
    ///   `Message<T>`, and forwards them via a bounded mpsc to the
    ///   Handler. Ping/pong/malformed frames are skipped on this task
    ///   and never reach the Handler. Large-message decode (SerDe CPU
    ///   work) runs here, **not** on the Handler task, so the Handler's
    ///   poll loop never stalls for tens of milliseconds on a 10 MB
    ///   screenshot response.
    ///
    /// The design uses only `tokio::spawn` (cooperative async) — no
    /// `spawn_blocking` or blocking thread-pool — so it scales with the
    /// tokio runtime's worker threads on multi-threaded runtimes, and
    /// interleaves cleanly with the Handler task on single-threaded
    /// runtimes.
    pub fn into_async(self) -> AsyncConnection<T> {
        let (ws_sink, ws_stream) = self.ws.split();
        let (cmd_tx, cmd_rx) = mpsc::channel(WS_CMD_CHANNEL_CAPACITY);
        let (msg_tx, msg_rx) = mpsc::channel::<Result<Box<Message<T>>>>(WS_READ_CHANNEL_CAPACITY);

        // Replay any commands queued via `submit_command` before the
        // split — most notably the boot `Target.setDiscoverTargets`
        // pushed by `Handler::new`. Without this, real Chrome never
        // emits `Target.targetCreated` and `new_page` hangs forever.
        // Capacity is `WS_CMD_CHANNEL_CAPACITY`, so the boot batch fits
        // easily — `try_send` would only fail in a pathological case
        // and we'd lose those commands either way.
        for call in self.pending_commands {
            let _ = cmd_tx.try_send(call);
        }

        let writer_handle = tokio::spawn(ws_write_loop(ws_sink, cmd_rx));
        let reader_handle = tokio::spawn(ws_read_loop::<T, _>(ws_stream, msg_tx));

        let reader = WsReader {
            rx: msg_rx,
            _marker: PhantomData,
        };

        AsyncConnection {
            reader,
            cmd_tx,
            writer_handle,
            reader_handle,
            next_id: self.next_id,
        }
    }
}

/// An entry in the reader's decode pipeline.
///
/// Small frames have been decoded inline on the reader task and sit
/// in `Ready(Some(result))` waiting their turn to emit — zero
/// allocation beyond the `Option`. Large frames were offloaded to
/// `tokio::task::spawn_blocking`, so their entry is the
/// corresponding `JoinHandle`.
///
/// A single concrete enum means `FuturesOrdered<InFlightDecode<T>>`
/// can hold either kind without `Box<dyn Future>`, keeping the
/// pipeline cost-proportional to the workload.
enum InFlightDecode<T: EventMessage + Send + 'static> {
    /// Small-frame fast path: already decoded inline. `take()`'d
    /// exactly once when `FuturesOrdered` first polls it to Ready.
    Ready(Option<Result<Box<Message<T>>>>),
    /// Large-frame path: decoding on the blocking thread pool.
    Blocking(tokio::task::JoinHandle<Result<Box<Message<T>>>>),
}

impl<T: EventMessage + Send + 'static> Future for InFlightDecode<T> {
    type Output = Result<Box<Message<T>>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Safety: both variants are structurally pin-agnostic —
        // `Option<Result<..>>` is `Unpin`, and `tokio::task::JoinHandle`
        // is documented as `Unpin`. So we can project out a `&mut`
        // without unsafe.
        match self.get_mut() {
            InFlightDecode::Ready(slot) => Poll::Ready(
                slot.take()
                    .expect("InFlightDecode::Ready polled after completion"),
            ),
            InFlightDecode::Blocking(handle) => match Pin::new(handle).poll(cx) {
                Poll::Ready(Ok(res)) => Poll::Ready(res),
                Poll::Ready(Err(join_err)) => Poll::Ready(Err(CdpError::msg(format!(
                    "WS decode blocking task join error: {join_err}"
                )))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Emit a single decoded-frame result to the Handler, logging parse
/// errors. Returns `true` if the channel is still open, `false` if
/// the Handler has dropped the receiver (caller should exit).
async fn emit_decoded<T>(
    tx: &mpsc::Sender<Result<Box<Message<T>>>>,
    res: Result<Box<Message<T>>>,
) -> bool
where
    T: EventMessage + Send + 'static,
{
    match res {
        Ok(msg) => tx.send(Ok(msg)).await.is_ok(),
        Err(err) => {
            tracing::debug!(
                target: "chromiumoxide::conn::raw_ws::parse_errors",
                "Dropping malformed WS frame: {err}",
            );
            true
        }
    }
}

/// Background task that reads frames from the WebSocket, decodes them to
/// typed CDP `Message<T>`, and forwards them to the Handler over a
/// bounded mpsc.
///
/// Runs on a `tokio::spawn`'d task. Small-to-medium frames are
/// decoded inline (fast path); payloads at or above
/// [`LARGE_FRAME_THRESHOLD`] are offloaded to `spawn_blocking` so
/// multi-MB deserialization doesn't monopolise a tokio worker
/// thread — especially important on single-threaded runtimes where
/// the reader, Handler, and user tasks share the same worker.
///
/// Flow per frame:
///
/// * `Text` / `Binary` → [`decode_ws_frame`]; decoded `Ok(msg)` is
///   sent to the Handler. Decode errors are logged and the frame is
///   dropped (same behavior as the legacy inline decode path).
/// * `Close` → loop exits cleanly, dropping `tx`. The Handler's
///   `next_message().await` returns `None` on the next call.
/// * `Ping` / `Pong` / unexpected frame types → skipped silently; they
///   never cross the channel to the Handler.
/// * Transport error → forwarded as `Err(CdpError::Ws(..))`, then the
///   loop exits (the WS half is considered dead after an error).
///
/// Back-pressure: the outbound `tx` is bounded. If the Handler is busy
/// and the channel fills, `tx.send(..).await` parks this task, which
/// stops draining the WS socket. TCP flow control then applies
/// back-pressure to Chrome instead of letting memory grow without bound.
async fn ws_read_loop<T, S>(mut stream: S, tx: mpsc::Sender<Result<Box<Message<T>>>>)
where
    T: EventMessage + Send + 'static,
    S: Stream<Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    // Pipeline of decodes in strict arrival order. Small-frame decodes
    // are produced inline (zero allocation, borrowing the frame body);
    // large-frame decodes are offloaded to `spawn_blocking`. Both
    // variants share a single concrete `InFlightDecode<T>` so the
    // queue avoids `Box<dyn Future>` overhead.
    let mut in_flight: FuturesOrdered<InFlightDecode<T>> = FuturesOrdered::new();

    // Shutdown state. When the stream signals `Close`, transport
    // error, or end-of-stream, we stop reading new frames but keep
    // running the select loop so the emit arm can flush any still
    // in-flight decodes *interleaved with* whatever else the runtime
    // is doing. A pending transport error is surfaced to the Handler
    // only after the in-order flush completes.
    let mut stream_terminated = false;
    let mut pending_err: Option<CdpError> = None;

    loop {
        tokio::select! {
            // Bias: emit already-ready decodes before reading more
            // frames. Keeps the pipeline small in the steady state
            // while still allowing concurrency under burst, and —
            // critically during shutdown — drains the pipeline one
            // ready item at a time inside the select loop instead
            // of blocking in a dedicated drain helper.
            biased;

            // Emit the head of the pipeline as soon as it is ready.
            // `FuturesOrdered::next` preserves submit order, so
            // downstream delivery is byte-identical to the serial
            // loop's ordering guarantee.
            Some(res) = in_flight.next(), if !in_flight.is_empty() => {
                if !emit_decoded(&tx, res).await {
                    return;
                }
            }

            // Read the next frame if the pipeline has capacity and
            // the stream hasn't terminated. Disabled once the stream
            // signals end (Close / None / Err) so subsequent loop
            // iterations only do emit work.
            maybe_frame = stream.next(),
                if !stream_terminated && in_flight.len() < MAX_IN_FLIGHT_DECODES =>
            {
                match maybe_frame {
                    Some(Ok(WsMessage::Text(text))) => {
                        // Zero-copy enqueue. The small-frame fast
                        // path decodes inline *now* (borrowing
                        // `text`, keeping the `raw_text_for_logging`
                        // preview); the large-frame path moves the
                        // `Utf8Bytes` (`Send + 'static`) directly
                        // into `spawn_blocking` without an
                        // intermediate allocation.
                        if text.len() >= LARGE_FRAME_THRESHOLD {
                            in_flight.push_back(InFlightDecode::Blocking(
                                tokio::task::spawn_blocking(move || {
                                    decode_message::<T>(text.as_bytes(), None)
                                }),
                            ));
                        } else {
                            let res = decode_message::<T>(text.as_bytes(), Some(&text));
                            in_flight.push_back(InFlightDecode::Ready(Some(res)));
                        }
                    }
                    Some(Ok(WsMessage::Binary(buf))) => {
                        // Same shape as Text: move `Bytes`
                        // (`Send + 'static`) into `spawn_blocking`
                        // for large payloads, decode inline for
                        // small ones.
                        if buf.len() >= LARGE_FRAME_THRESHOLD {
                            in_flight.push_back(InFlightDecode::Blocking(
                                tokio::task::spawn_blocking(move || {
                                    decode_message::<T>(&buf, None)
                                }),
                            ));
                        } else {
                            let res = decode_message::<T>(&buf, None);
                            in_flight.push_back(InFlightDecode::Ready(Some(res)));
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        stream_terminated = true;
                    }
                    Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => {}
                    Some(Ok(msg)) => {
                        tracing::debug!(
                            target: "chromiumoxide::conn::raw_ws::parse_errors",
                            "Unexpected WS message type: {:?}",
                            msg
                        );
                    }
                    Some(Err(err)) => {
                        // Defer the error until after the already
                        // in-flight decodes have emitted — preserves
                        // the ordering contract that callers see
                        // frames up to the failure point before the
                        // error itself.
                        stream_terminated = true;
                        pending_err = Some(CdpError::Ws(err));
                    }
                    None => {
                        // Stream ended (connection closed without a
                        // `Close` frame). No more input, but
                        // in_flight may still hold pending decodes.
                        stream_terminated = true;
                    }
                }
            }

            // Both arms disabled: `in_flight` is empty AND
            // `stream_terminated`. We have nothing more to do.
            else => {
                break;
            }
        }
    }

    if let Some(err) = pending_err {
        let _ = tx.send(Err(err)).await;
    }
}

/// Background task that batches and flushes outgoing CDP commands.
async fn ws_write_loop(
    mut sink: SplitSink<WebSocketStream<ConnectStream>, WsMessage>,
    mut rx: mpsc::Receiver<MethodCall>,
) -> Result<()> {
    while let Some(call) = rx.recv().await {
        let msg = crate::serde_json::to_string(&call)?;
        sink.feed(WsMessage::Text(msg.into()))
            .await
            .map_err(CdpError::Ws)?;

        // Batch: drain all buffered commands without waiting.
        while let Ok(call) = rx.try_recv() {
            let msg = crate::serde_json::to_string(&call)?;
            sink.feed(WsMessage::Text(msg.into()))
                .await
                .map_err(CdpError::Ws)?;
        }

        // Flush the entire batch in one write.
        sink.flush().await.map_err(CdpError::Ws)?;
    }

    // Cmd channel closed → the Handler is shutting down. Send a graceful
    // WebSocket Close frame so the remote endpoint (esp. for
    // `Browser::connect()` to a remote DevTools URL, where there is no
    // child process whose exit closes the socket) tears the connection
    // down promptly instead of waiting for an idle timeout. Errors are
    // expected during shutdown (e.g. `AlreadyClosed` if Chrome closed
    // first) and are intentionally ignored.
    let _ = sink.close().await;
    Ok(())
}

/// Handler-facing read half of the split WebSocket connection.
///
/// Decoded CDP messages are produced by a dedicated background task
/// (see [`ws_read_loop`]) and forwarded over a bounded mpsc. `WsReader`
/// itself is a thin `Receiver` wrapper — calling `next_message()` does
/// a single `rx.recv().await` with no per-message decoding work on the
/// caller's task. This keeps the Handler's poll loop free of CPU-bound
/// deserialize time, which matters for large (multi-MB) CDP responses
/// such as screenshots and wide-header network events.
#[derive(Debug)]
pub struct WsReader<T: EventMessage> {
    rx: mpsc::Receiver<Result<Box<Message<T>>>>,
    _marker: PhantomData<T>,
}

impl<T: EventMessage + Unpin> WsReader<T> {
    /// Read the next CDP message from the WebSocket.
    ///
    /// Returns `None` when the background reader task has exited
    /// (connection closed or sender dropped). This call does only a
    /// channel `recv` — the actual WS read + JSON decode happens on
    /// the background `ws_read_loop` task.
    pub async fn next_message(&mut self) -> Option<Result<Box<Message<T>>>> {
        self.rx.recv().await
    }
}

impl<T: EventMessage + Unpin> Stream for Connection<T> {
    type Item = Result<Box<Message<T>>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        // Send and flush outgoing messages
        if let Err(err) = pin.start_send_next(cx) {
            return Poll::Ready(Some(Err(err)));
        }

        // Read from the websocket, skipping non-data frames (pings,
        // pongs, malformed messages) without yielding back to the
        // executor.  This avoids a full round-trip per skipped frame.
        //
        // Cap consecutive skips so a flood of non-data frames (many
        // pings, malformed/unexpected types) cannot starve the
        // runtime — yield Pending after `MAX_SKIPS_PER_POLL` and
        // self-wake so we resume on the next tick.
        const MAX_SKIPS_PER_POLL: u32 = 16;
        let mut skips: u32 = 0;
        loop {
            match ready!(pin.ws.poll_next_unpin(cx)) {
                Some(Ok(WsMessage::Text(text))) => {
                    match decode_message::<T>(text.as_bytes(), Some(&text)) {
                        Ok(msg) => return Poll::Ready(Some(Ok(msg))),
                        Err(err) => {
                            tracing::debug!(
                                target: "chromiumoxide::conn::raw_ws::parse_errors",
                                "Dropping malformed text WS frame: {err}",
                            );
                            skips += 1;
                        }
                    }
                }
                Some(Ok(WsMessage::Binary(buf))) => match decode_message::<T>(&buf, None) {
                    Ok(msg) => return Poll::Ready(Some(Ok(msg))),
                    Err(err) => {
                        tracing::debug!(
                            target: "chromiumoxide::conn::raw_ws::parse_errors",
                            "Dropping malformed binary WS frame: {err}",
                        );
                        skips += 1;
                    }
                },
                Some(Ok(WsMessage::Close(_))) => return Poll::Ready(None),
                Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => {
                    skips += 1;
                }
                Some(Ok(msg)) => {
                    tracing::debug!(
                        target: "chromiumoxide::conn::raw_ws::parse_errors",
                        "Unexpected WS message type: {:?}",
                        msg
                    );
                    skips += 1;
                }
                Some(Err(err)) => return Poll::Ready(Some(Err(CdpError::Ws(err)))),
                None => return Poll::Ready(None),
            }

            if skips >= MAX_SKIPS_PER_POLL {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
    }
}

/// Shared decode path for both text and binary WS frames.
/// `raw_text_for_logging` is only provided for textual frames so we can log the original
/// payload on parse failure if desired.
#[cfg(not(feature = "serde_stacker"))]
fn decode_message<T: EventMessage>(
    bytes: &[u8],
    raw_text_for_logging: Option<&str>,
) -> Result<Box<Message<T>>> {
    match serde_json::from_slice::<Box<Message<T>>>(bytes) {
        Ok(msg) => {
            tracing::trace!("Received {:?}", msg);
            Ok(msg)
        }
        Err(err) => {
            if let Some(txt) = raw_text_for_logging {
                let preview = &txt[..txt.len().min(512)];
                tracing::debug!(
                    target: "chromiumoxide::conn::raw_ws::parse_errors",
                    msg_len = txt.len(),
                    "Skipping unrecognized WS message {err} preview={preview}",
                );
            } else {
                tracing::debug!(
                    target: "chromiumoxide::conn::raw_ws::parse_errors",
                    "Skipping unrecognized binary WS message {err}",
                );
            }
            Err(err.into())
        }
    }
}

/// Shared decode path for both text and binary WS frames.
/// `raw_text_for_logging` is only provided for textual frames so we can log the original
/// payload on parse failure if desired.
#[cfg(feature = "serde_stacker")]
fn decode_message<T: EventMessage>(
    bytes: &[u8],
    raw_text_for_logging: Option<&str>,
) -> Result<Box<Message<T>>> {
    use serde::Deserialize;
    let mut de = serde_json::Deserializer::from_slice(bytes);

    de.disable_recursion_limit();

    let de = serde_stacker::Deserializer::new(&mut de);

    match Box::<Message<T>>::deserialize(de) {
        Ok(msg) => {
            tracing::trace!("Received {:?}", msg);
            Ok(msg)
        }
        Err(err) => {
            if let Some(txt) = raw_text_for_logging {
                let preview = &txt[..txt.len().min(512)];
                tracing::debug!(
                    target: "chromiumoxide::conn::raw_ws::parse_errors",
                    msg_len = txt.len(),
                    "Skipping unrecognized WS message {err} preview={preview}",
                );
            } else {
                tracing::debug!(
                    target: "chromiumoxide::conn::raw_ws::parse_errors",
                    "Skipping unrecognized binary WS message {err}",
                );
            }
            Err(err.into())
        }
    }
}

#[cfg(test)]
mod ws_read_loop_tests {
    //! Unit tests for the `ws_read_loop` background reader task.
    //!
    //! These tests feed a synthetic `Stream<Item = Result<WsMessage, _>>`
    //! into `ws_read_loop` — no real WebSocket, no Chrome — and observe
    //! what comes out the other side of the mpsc channel.
    //!
    //! The properties under test are the ones that make the reader-task
    //! decoupling safe: FIFO ordering, no-deadlock on a bounded channel
    //! under back-pressure, silent drop of non-data frames, graceful
    //! transport-error propagation, and clean exit on `Close`.
    //!
    //! The typed events are `chromiumoxide_cdp::cdp::CdpEventMessage` —
    //! the same instantiation the real Handler uses — so these tests
    //! exercise the actual decode path (`serde_json::from_slice`), not
    //! a simplified fake.
    use super::*;
    use chromiumoxide_cdp::cdp::CdpEventMessage;
    use chromiumoxide_types::CallId;
    use futures_util::stream;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// Build a CDP `Response` WS frame as text — the smallest valid CDP
    /// message. `id` tags the frame for ordering assertions.
    fn response_frame(id: u64) -> WsMessage {
        WsMessage::Text(
            format!(r#"{{"id":{id},"result":{{"ok":true}}}}"#)
                .to_string()
                .into(),
        )
    }

    /// Build a frame far larger than a typical socket chunk, to exercise
    /// the "large message" path that motivated this refactor. The blob
    /// field pushes serde_json through a big allocation even though the
    /// envelope is tiny.
    fn large_response_frame(id: u64, blob_bytes: usize) -> WsMessage {
        let blob = "x".repeat(blob_bytes);
        WsMessage::Text(
            format!(r#"{{"id":{id},"result":{{"blob":"{blob}"}}}}"#)
                .to_string()
                .into(),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_messages_in_stream_order() {
        let frames = vec![
            Ok(response_frame(1)),
            Ok(response_frame(2)),
            Ok(response_frame(3)),
        ];
        let stream = stream::iter(frames);
        let (tx, mut rx) = mpsc::channel::<Result<Box<Message<CdpEventMessage>>>>(8);
        let task = tokio::spawn(ws_read_loop::<CdpEventMessage, _>(stream, tx));

        for expected in [1u64, 2, 3] {
            let msg = rx.recv().await.expect("msg").expect("decode ok");
            if let Message::Response(resp) = *msg {
                assert_eq!(resp.id, CallId::new(expected as usize));
            } else {
                panic!("expected Response");
            }
        }
        assert!(rx.recv().await.is_none(), "channel must close on EOF");
        task.await.expect("reader task join");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pings_and_pongs_never_reach_the_handler() {
        let frames = vec![
            Ok(WsMessage::Ping(vec![1, 2, 3].into())),
            Ok(response_frame(7)),
            Ok(WsMessage::Pong(vec![].into())),
            Ok(response_frame(8)),
        ];
        let stream = stream::iter(frames);
        let (tx, mut rx) = mpsc::channel::<Result<Box<Message<CdpEventMessage>>>>(8);
        let task = tokio::spawn(ws_read_loop::<CdpEventMessage, _>(stream, tx));

        for expected in [7u64, 8] {
            let msg = rx.recv().await.expect("msg").expect("decode ok");
            if let Message::Response(resp) = *msg {
                assert_eq!(resp.id, CallId::new(expected as usize));
            }
        }
        assert!(rx.recv().await.is_none());
        task.await.expect("reader task join");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_frames_do_not_block_subsequent_valid_frames() {
        let frames = vec![
            Ok(WsMessage::Text("{not valid json".to_string().into())),
            Ok(response_frame(42)),
        ];
        let stream = stream::iter(frames);
        let (tx, mut rx) = mpsc::channel::<Result<Box<Message<CdpEventMessage>>>>(8);
        let task = tokio::spawn(ws_read_loop::<CdpEventMessage, _>(stream, tx));

        let msg = rx.recv().await.expect("msg").expect("decode ok");
        if let Message::Response(resp) = *msg {
            assert_eq!(resp.id, CallId::new(42));
        }
        assert!(rx.recv().await.is_none());
        task.await.expect("reader task join");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_frame_terminates_the_reader() {
        let frames = vec![
            Ok(response_frame(1)),
            Ok(WsMessage::Close(None)),
            Ok(response_frame(2)), // unreachable after Close
        ];
        let stream = stream::iter(frames);
        let (tx, mut rx) = mpsc::channel::<Result<Box<Message<CdpEventMessage>>>>(8);
        let task = tokio::spawn(ws_read_loop::<CdpEventMessage, _>(stream, tx));

        let msg = rx.recv().await.expect("msg").expect("decode ok");
        if let Message::Response(resp) = *msg {
            assert_eq!(resp.id, CallId::new(1));
        }
        assert!(
            rx.recv().await.is_none(),
            "reader must exit on Close; frames after Close must not appear"
        );
        task.await.expect("reader task join");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transport_error_is_forwarded_once_then_reader_exits() {
        let frames = vec![
            Ok(response_frame(1)),
            Err(tokio_tungstenite::tungstenite::Error::ConnectionClosed),
            Ok(response_frame(2)),
        ];
        let stream = stream::iter(frames);
        let (tx, mut rx) = mpsc::channel::<Result<Box<Message<CdpEventMessage>>>>(8);
        let task = tokio::spawn(ws_read_loop::<CdpEventMessage, _>(stream, tx));

        let msg = rx.recv().await.expect("msg").expect("ok");
        assert!(matches!(*msg, Message::Response(_)));
        match rx.recv().await {
            Some(Err(CdpError::Ws(_))) => {}
            other => panic!("expected forwarded Ws error, got {other:?}"),
        }
        assert!(rx.recv().await.is_none());
        task.await.expect("reader task join");
    }

    /// Back-pressure property: with the smallest possible channel and
    /// many frames, the reader task awaits capacity after each send and
    /// never deadlocks. This is the core "no deadlock" proof for the
    /// new design — if the reader held anything across its `.await` that
    /// the consumer needed, the consumer's `recv().await` would block
    /// forever. Completion under a 5s watchdog proves it doesn't.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounded_channel_does_not_deadlock_under_backpressure() {
        const N: u64 = 512;
        let frames: Vec<_> = (1..=N).map(|id| Ok(response_frame(id))).collect();
        let stream = stream::iter(frames);

        let (tx, mut rx) = mpsc::channel::<Result<Box<Message<CdpEventMessage>>>>(1);
        let task = tokio::spawn(ws_read_loop::<CdpEventMessage, _>(stream, tx));

        let deadline = std::time::Duration::from_secs(5);
        let collected = tokio::time::timeout(deadline, async {
            let mut seen = 0u64;
            while let Some(frame) = rx.recv().await {
                let msg = frame.expect("decode ok");
                if let Message::Response(resp) = *msg {
                    seen += 1;
                    assert_eq!(
                        resp.id,
                        CallId::new(seen as usize),
                        "back-pressure must preserve FIFO order"
                    );
                }
            }
            seen
        })
        .await
        .expect("reader must make forward progress despite cap-1 back-pressure");

        assert_eq!(collected, N, "all frames must arrive");
        task.await.expect("reader task join");
    }

    /// Large message (>1 MB) is decoded correctly on the background
    /// task. This is the specific scenario the reader-task refactor
    /// was built for — we don't measure time here (benches cover that),
    /// we just prove the end-to-end path works without corruption or
    /// deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_message_decodes_without_corruption() {
        let big = 2 * 1024 * 1024; // 2 MB payload
        let frames = vec![Ok(large_response_frame(100, big)), Ok(response_frame(101))];
        let stream = stream::iter(frames);
        let (tx, mut rx) = mpsc::channel::<Result<Box<Message<CdpEventMessage>>>>(4);
        let task = tokio::spawn(ws_read_loop::<CdpEventMessage, _>(stream, tx));

        let first = rx.recv().await.expect("msg").expect("ok");
        if let Message::Response(resp) = *first {
            assert_eq!(resp.id, CallId::new(100));
        }
        let second = rx.recv().await.expect("msg").expect("ok");
        if let Message::Response(resp) = *second {
            assert_eq!(resp.id, CallId::new(101));
        }
        assert!(rx.recv().await.is_none());
        task.await.expect("reader task join");
    }

    /// FIFO ordering under the pipelined reader when large-frame
    /// decodes run in parallel via `spawn_blocking`.
    ///
    /// This test submits an interleaved sequence of large and small
    /// frames. Large frames take the `spawn_blocking` path (decode
    /// on the blocking pool, variable completion order); small
    /// frames take the inline path (decode immediately). The
    /// pipeline's `FuturesOrdered` queue must emit them to the
    /// Handler in strict arrival order regardless of which
    /// blocking-pool thread finishes first.
    ///
    /// If the ordering guarantee were ever broken — e.g. by
    /// accidentally swapping `FuturesOrdered` for `FuturesUnordered`
    /// — id sequence checks here would catch it immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pipelined_large_and_small_frames_keep_fifo_order() {
        let big = 2 * 1024 * 1024; // 2 MB payload — forces spawn_blocking
        let frames = vec![
            Ok(large_response_frame(1, big)),
            Ok(response_frame(2)),
            Ok(response_frame(3)),
            Ok(large_response_frame(4, big)),
            Ok(response_frame(5)),
            Ok(large_response_frame(6, big)),
            Ok(response_frame(7)),
            Ok(response_frame(8)),
        ];
        let expected: Vec<usize> = (1..=8).collect();

        let stream = stream::iter(frames);
        let (tx, mut rx) = mpsc::channel::<Result<Box<Message<CdpEventMessage>>>>(16);
        let task = tokio::spawn(ws_read_loop::<CdpEventMessage, _>(stream, tx));

        let deadline = std::time::Duration::from_secs(10);
        let observed = tokio::time::timeout(deadline, async {
            let mut ids = Vec::with_capacity(expected.len());
            while let Some(frame) = rx.recv().await {
                let msg = frame.expect("decode ok");
                if let Message::Response(resp) = *msg {
                    ids.push(CallId::new(ids.len() + 1));
                    assert_eq!(
                        resp.id,
                        *ids.last().unwrap(),
                        "pipelined reader must emit frames in strict arrival order \
                         regardless of per-frame decode latency"
                    );
                }
            }
            ids
        })
        .await
        .expect("pipelined reader should make forward progress within 10s");

        assert_eq!(
            observed.len(),
            expected.len(),
            "all {} frames must reach the Handler",
            expected.len()
        );
        task.await.expect("reader task join");
    }
}

#[cfg(test)]
mod connect_headers_tests {
    //! Unit tests for `ConnectHeaders`: the size accounting, replacement,
    //! the reserved-name guard, and the promise that an empty set leaves an
    //! upgrade request exactly as tungstenite built it.

    use super::ConnectHeaderError;
    use super::ConnectHeaders;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderMap;

    #[test]
    fn empty_apply_is_a_no_op() {
        // Two `into_client_request` calls differ in `Sec-WebSocket-Key`, so
        // snapshot one request and compare it with itself after `apply`.
        let url = "ws://127.0.0.1:9222/devtools/browser/x";
        let mut req = url.into_client_request().unwrap();
        let before = req.headers().clone();
        ConnectHeaders::new().apply(&mut req);
        assert_eq!(&before, req.headers());
    }

    #[test]
    fn apply_adds_and_replaces_by_name() {
        let url = "ws://127.0.0.1:9222/devtools/browser/x";
        let mut req = url.into_client_request().unwrap();
        let set = ConnectHeaders::new()
            .with("x-a", "1")
            .unwrap()
            .with("x-a", "2")
            .unwrap()
            .with("x-b", "3")
            .unwrap();
        assert_eq!(set.len(), 2);
        set.apply(&mut req);
        assert_eq!(req.headers().get_all("x-a").iter().count(), 1);
        assert_eq!(req.headers()["x-a"], "2");
        assert_eq!(req.headers()["x-b"], "3");
    }

    #[test]
    fn total_bytes_tracks_replacement() {
        let mut set = ConnectHeaders::new();
        set.insert("x-a", "12345").unwrap();
        // "x-a: 12345\r\n"
        assert_eq!(set.total_bytes(), 3 + 2 + 5 + 2);
        set.insert("x-a", "1").unwrap();
        assert_eq!(set.total_bytes(), 3 + 2 + 1 + 2);
        set.insert("x-b", "").unwrap();
        assert_eq!(set.total_bytes(), 8 + 7);
    }

    #[test]
    fn limit_counts_the_whole_set() {
        let half = "x".repeat(ConnectHeaders::MAX_TOTAL_BYTES / 2);
        let mut set = ConnectHeaders::new();
        set.insert("x-a", half.as_str()).unwrap();
        let err = set.insert("x-b", half.as_str()).unwrap_err();
        assert!(matches!(err, ConnectHeaderError::TooLarge { .. }), "{err}");
        // A refused insert leaves the set as it was.
        assert_eq!(set.len(), 1);
        // Replacing the big one with a small one frees the room back up.
        set.insert("x-a", "1").unwrap();
        set.insert("x-b", half.as_str()).unwrap();
    }

    #[test]
    fn reserved_names_are_case_insensitive() {
        for name in ["Host", "CONNECTION", "Upgrade", "Sec-WebSocket-Protocol"] {
            let err = ConnectHeaders::new().with(name, "x").unwrap_err();
            assert!(
                matches!(err, ConnectHeaderError::Reserved(_)),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn obs_text_bytes_are_refused() {
        // `HeaderValue::from_bytes` accepts 0x80..=0xFF; the handshake writer
        // does not, so the set must not either.
        let value = tokio_tungstenite::tungstenite::http::HeaderValue::from_bytes(&[0xE9]).unwrap();
        let err = ConnectHeaders::new().with("x-a", value).unwrap_err();
        assert!(matches!(err, ConnectHeaderError::NonAscii(_)), "{err}");
    }

    #[test]
    fn header_map_conversion_validates_every_entry() {
        let mut map = HeaderMap::new();
        map.insert("x-a", "1".parse().unwrap());
        map.insert("host", "h".parse().unwrap());
        let err = ConnectHeaders::try_from(map).unwrap_err();
        assert!(matches!(err, ConnectHeaderError::Reserved(_)), "{err}");
    }
}
