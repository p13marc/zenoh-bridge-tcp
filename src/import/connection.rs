use crate::config::BridgeConfig;
use crate::dns::normalize_dns;
use crate::http_util::{http_400_response, http_502_response};
use anyhow::Result;
use bytes::Bytes;
use flowscope::SessionParser;
use flowscope::Timestamp;
use flowscope::classify::{Classify, WireProtocol, classify_first_bytes};
use flowscope::http::{HttpEvent, HttpProxyParser};
use flowscope::http2::{
    GrpcStatus, Http2Config, Http2Event, Http2Parser, StreamHead, grpc_call, grpc_status,
    grpc_status_of,
};
use flowscope::tls::{TlsMessage, TlsParser};
use flowscope::{FlowSide, http::RequestHead};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};
use zenoh::Session;

/// Handle a single import connection
///
/// This function bridges a TCP connection to a Zenoh service by:
/// 1. Optionally parsing HTTP to extract DNS (if http_mode is true)
/// 2. Subscribing to error signals (before declaring liveliness)
/// 3. Subscribing to the service's response channel
/// 4. Declaring a liveliness token to signal the export bridge
/// 5. Spawning tasks to bridge data in both directions
pub(super) async fn handle_import_connection(
    session: Arc<Session>,
    mut stream: TcpStream,
    service_name: &str,
    client_id: &str,
    http_mode: bool,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    // Parse HTTP/HTTPS request if in HTTP mode to extract DNS
    let (dns, initial_buffer) = if http_mode {
        // Peek at the first few bytes to detect HTTP vs HTTPS/TLS.
        // Bound the wait so an idle client cannot pin a task+fd forever (F4).
        let mut peek_buffer = vec![0u8; 16];
        let peek_len =
            match tokio::time::timeout(config.read_timeout, stream.peek(&mut peek_buffer)).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(anyhow::anyhow!("Failed to peek connection: {}", e)),
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "Client sent no data within the read timeout"
                    ));
                }
            };

        let is_tls = matches!(
            classify_first_bytes(&peek_buffer[..peek_len]),
            Classify::Decided(WireProtocol::Tls)
        );

        if is_tls {
            // This is a TLS/HTTPS connection - extract SNI (TLS is not
            // terminated; the ClientHello is forwarded verbatim).
            info!(proto = "tls", "Detected TLS/HTTPS connection");
            match read_tls_sni(&mut stream, &config).await {
                Ok((dns, buffer)) => {
                    // Record the routed host on the connection span so every
                    // later line in this connection carries it for free.
                    tracing::Span::current().record("dns", dns.as_str());
                    info!(dns = %dns, routed_by = "sni", "Routing TLS connection");

                    match resolve_backend(&session, service_name, Some(&dns), &config).await? {
                        BackendRoute::Host => {
                            debug!(dns = %dns, "Backend available");
                            (Some(dns), Some(buffer))
                        }
                        BackendRoute::Default => {
                            info!(dns = %dns, routed_by = "default", "Routing to the service default backend");
                            (None, Some(buffer))
                        }
                        BackendRoute::Unavailable => {
                            warn!(dns = %dns, "No backend available");
                            // For TLS, we can't send an HTTP error, just close the connection
                            return Err(anyhow::anyhow!("No backend available for DNS: {}", dns));
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to parse TLS ClientHello");
                    // For TLS, we can't send an HTTP error, just close the connection
                    return Err(anyhow::anyhow!("{}", e));
                }
            }
        } else {
            // This is a plain HTTP connection - parse the request head for Host.
            info!(proto = "http", "Detected plain HTTP connection");
            match read_http_head(&mut stream, &config).await {
                Ok((dns, buffer)) => {
                    tracing::Span::current().record("dns", dns.as_str());
                    info!(dns = %dns, routed_by = "host", "Routing HTTP connection");

                    match resolve_backend(&session, service_name, Some(&dns), &config).await? {
                        BackendRoute::Host => {
                            debug!(dns = %dns, "Backend available");
                            (Some(dns), Some(buffer))
                        }
                        BackendRoute::Default => {
                            info!(dns = %dns, routed_by = "default", "Routing to the service default backend");
                            (None, Some(buffer))
                        }
                        BackendRoute::Unavailable => {
                            warn!(dns = %dns, "No backend available");
                            // Send HTTP 502 Bad Gateway
                            let _ = stream.write_all(&http_502_response(&dns)).await;
                            return Err(anyhow::anyhow!("No backend available for DNS: {}", dns));
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to parse HTTP request");
                    // Send HTTP 400 Bad Request
                    let _ = stream.write_all(&http_400_response()).await;
                    return Err(anyhow::anyhow!("{}", e));
                }
            }
        }
    } else {
        (None, None)
    };

    let (tcp_reader, tcp_writer) = stream.into_split();
    let reader = crate::transport::TcpReader::new(tcp_reader, config.buffer_size);
    let writer = crate::transport::TcpWriter::new(tcp_writer);

    super::bridge::bridge_import_connection(
        session,
        reader,
        writer,
        service_name,
        client_id,
        dns.as_deref(),
        initial_buffer,
        config,
        None,
    )
    .await
}

/// How a [`read_until`] head-read ended, short of a hard failure.
///
/// Both variants carry every byte consumed from the socket so far — the head
/// readers relay, they never retain, so whatever was read must either be
/// replayed to a backend or the connection torn down. `Timeout` makes the
/// fallback possible: the h2c auto-detect path (#74) downgrades a client that
/// never sent its first HEADERS (RFC 9113 §3.4 lets it wait for the server's
/// SETTINGS) to an opaque relay of exactly these bytes.
pub(super) enum ReadHeadOutcome<T> {
    /// The step produced a value; the buffer is the verbatim replay payload.
    Parsed(T, Vec<u8>),
    /// `read_timeout` elapsed first; the buffer is what had been consumed.
    Timeout(Vec<u8>),
}

/// Shared shell of the head readers: read chunks off `stream`, mirror every
/// byte into a replay buffer, and offer each chunk to `step` until it yields a
/// value, errors, or the connection exceeds `max_header_size` / `read_timeout`.
///
/// `step` owns the incremental parser; it sees each newly read chunk exactly
/// once. `what` names the head being read in error messages.
async fn read_until<R, T, F>(
    stream: &mut R,
    config: &BridgeConfig,
    what: &'static str,
    mut step: F,
) -> Result<ReadHeadOutcome<T>>
where
    R: AsyncReadExt + Unpin,
    F: FnMut(&[u8]) -> Result<Option<T>>,
{
    let deadline = tokio::time::Instant::now() + config.read_timeout;
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut temp = vec![0u8; 4096];

    loop {
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut temp)).await {
            Err(_) => return Ok(ReadHeadOutcome::Timeout(buffer)),
            Ok(Err(e)) => {
                return Err(anyhow::anyhow!("read error while reading {}: {}", what, e));
            }
            Ok(Ok(0)) => {
                return Err(anyhow::anyhow!(
                    "connection closed before complete {}",
                    what
                ));
            }
            Ok(Ok(n)) => n,
        };
        buffer.extend_from_slice(&temp[..n]);

        if let Some(value) = step(&temp[..n])? {
            return Ok(ReadHeadOutcome::Parsed(value, buffer));
        }

        if buffer.len() >= config.max_header_size {
            return Err(anyhow::anyhow!(
                "{} exceeds maximum size of {} bytes",
                what,
                config.max_header_size
            ));
        }
    }
}

/// Read a TLS ClientHello from `stream` and extract its SNI for routing.
///
/// Bytes are streamed into flowscope's [`TlsParser`], which reassembles a
/// ClientHello split across multiple TCP segments — or inflated past a single
/// segment by post-quantum key shares — before emitting it (this closes G6,
/// where the previous single-record reader could not see a fragmented SNI).
///
/// TLS is *not* terminated here: the returned buffer is every byte read from the
/// socket so far, forwarded verbatim to the backend as the connection's initial
/// payload. Returns the normalized SNI plus those raw bytes.
async fn read_tls_sni<R>(stream: &mut R, config: &BridgeConfig) -> Result<(String, Vec<u8>)>
where
    R: AsyncReadExt + Unpin,
{
    let mut parser = TlsParser::default();
    let outcome = read_until(stream, config, "TLS ClientHello", |chunk| {
        let mut msgs: Vec<TlsMessage> = Vec::new();
        parser.feed_initiator(chunk, Timestamp::new(0, 0), &mut msgs);
        for msg in &msgs {
            if let TlsMessage::ClientHello(ch) = msg {
                // #80: surface the offered ALPN — it answers "is this
                // passthrough connection h2/gRPC or h1?" without decrypting
                // anything. Guess via the first offered token (client
                // preference order, RFC 7301 §3.1).
                if !ch.alpn.is_empty() {
                    let guess = ch
                        .alpn
                        .first()
                        .and_then(|t| flowscope::app_proto::classify_alpn_token(t))
                        .map(|p| p.as_str())
                        .unwrap_or("unknown");
                    info!(
                        sni = ch.sni().unwrap_or("-"),
                        alpn = ?ch.alpn,
                        proto_guess = %guess,
                        "TLS ClientHello ALPN offer"
                    );
                }
                return match ch.sni() {
                    Some(sni) => Ok(Some(normalize_dns(sni))),
                    None => Err(anyhow::anyhow!("TLS ClientHello has no SNI extension")),
                };
            }
        }
        Ok(None)
    })
    .await?;

    match outcome {
        ReadHeadOutcome::Parsed(dns, buffer) => Ok((dns, buffer)),
        ReadHeadOutcome::Timeout(partial) => Err(anyhow::anyhow!(
            "timeout reading TLS ClientHello ({} bytes consumed)",
            partial.len()
        )),
    }
}

/// Resolve the DNS routing key from a request head's authority.
///
/// flowscope's [`RequestHead::authority`] applies RFC 9112 §3.2 rules — an
/// absolute-form request-target beats the `Host` header, a duplicate `Host` is
/// rejected, and the host is ASCII-folded (rejecting non-ASCII authorities that
/// could otherwise desync routing, F3). We then run it through [`normalize_dns`]
/// so the default 80/443 ports collapse exactly as on the export side.
pub(super) fn routing_key_from_head(head: &RequestHead) -> Result<String> {
    let authority = head
        .authority()
        .map_err(|p| anyhow::anyhow!("unroutable request target: {}", p.as_str()))?;
    let with_port = match authority.port {
        Some(port) => format!("{}:{}", authority.host, port),
        None => authority.host,
    };
    let dns = normalize_dns(&with_port);
    if dns.is_empty() {
        return Err(anyhow::anyhow!("request has no Host/authority"));
    }
    Ok(dns)
}

/// Read an HTTP/1.x request head from `stream` and resolve its DNS routing key.
///
/// Bytes are streamed into flowscope's [`HttpProxyParser`]; the routing key is
/// taken from the first [`HttpEvent::RequestHead`], before any body byte. The
/// returned buffer is every byte read from the socket so far (head plus any body
/// bytes that arrived in the same reads), forwarded verbatim to the backend as
/// the connection's initial payload — the bridge relays, it does not rewrite.
/// An [`HttpProxyParser`] whose caps honour `--max-header-size` — without
/// this, flowscope's own 64 KiB head default silently overrides a larger
/// configured cap (and a smaller one is enforced later than it could be).
pub(super) fn http_head_parser(config: &BridgeConfig) -> HttpProxyParser {
    HttpProxyParser::with_config(
        flowscope::http::HttpProxyConfig::default()
            .with_max_head_bytes(config.max_header_size)
            .with_max_buffered_bytes(config.max_header_size.max(256 * 1024)),
    )
}

pub(super) async fn read_http_head<R>(
    stream: &mut R,
    config: &BridgeConfig,
) -> Result<(String, Vec<u8>)>
where
    R: AsyncReadExt + Unpin,
{
    let mut parser = http_head_parser(config);
    let outcome = read_until(stream, config, "HTTP request head", |chunk| {
        // Offer the new bytes to the parser, re-offering the tail on a short
        // count (the backpressure signal). The head fits well within the
        // parser's buffer, so this converges before any RequestHead.
        let mut pending = Bytes::copy_from_slice(chunk);
        while !pending.is_empty() {
            let accepted = parser.push(FlowSide::Initiator, &pending);
            pending = pending.slice(accepted..);
            if accepted == 0 {
                break;
            }
        }

        while let Some(ev) = parser.next_event() {
            if let HttpEvent::RequestHead(head) = ev {
                return routing_key_from_head(&head).map(Some);
            }
        }

        if let Some(reason) = parser.poison() {
            return Err(anyhow::anyhow!(
                "malformed HTTP request: {}",
                reason.as_str()
            ));
        }
        // flowscope 0.24: push returns 0 once a protocol switch tunnelled the
        // connection. A switch before any routable request head means this
        // reader cannot make progress — fail now with the reason, instead of
        // accumulating refused bytes until the size cap trips.
        if parser.is_tunnelled() {
            return Err(anyhow::anyhow!(
                "connection switched protocols before a routable request head"
            ));
        }
        Ok(None)
    })
    .await?;

    match outcome {
        ReadHeadOutcome::Parsed(dns, buffer) => Ok((dns, buffer)),
        ReadHeadOutcome::Timeout(partial) => Err(anyhow::anyhow!(
            "timeout reading HTTP request ({} bytes consumed)",
            partial.len()
        )),
    }
}

/// Resolve the DNS routing key from an HTTP/2 request head's `:authority`.
///
/// `:authority` is the h2 equivalent of the `Host` header (RFC 9113 §8.3.1);
/// [`normalize_dns`] collapses default 80/443 ports so keys match the export side.
fn h2_routing_key(head: &StreamHead) -> Result<String> {
    let authority = head
        .authority()
        .ok_or_else(|| anyhow::anyhow!("HTTP/2 request has no :authority"))?;
    let dns = normalize_dns(authority);
    if dns.is_empty() {
        return Err(anyhow::anyhow!("HTTP/2 request has an empty :authority"));
    }
    Ok(dns)
}

/// Read an HTTP/2 connection's first request head and resolve its DNS routing
/// key from `:authority` (Phase C #50 for terminated h2; #74 for plaintext h2c).
///
/// Two doors lead here: a terminating listener whose ALPN negotiated `h2`
/// (`require_preface = false` — ALPN pinned the protocol, tolerate a missing
/// preface but consume one when present), and the auto-detect door after
/// `classify_first_bytes` saw the client preface (`require_preface = true` —
/// RFC 9113 §3.4 strictness is correct, the preface is known to be there).
///
/// The bytes are streamed into flowscope's [`Http2Parser`] purely to *peek*
/// the first request stream's `:authority`; the parser is read-only — every
/// byte read is also kept in the outcome's buffer and relayed to the backend
/// verbatim. The connection is routed by the first stream's authority; its
/// multiplexed streams are then relayed opaquely (a single-authority h2 proxy,
/// not a per-stream demux). If the first head is a gRPC call it is logged.
///
/// Returns the [`ReadHeadOutcome`] so a caller can turn a timeout into an
/// opaque relay of the consumed bytes (a conformant client may wait for the
/// server's SETTINGS before opening a stream, and the bridge sends nothing
/// during this peek) instead of dropping them.
pub(super) async fn read_h2_head<R>(
    stream: &mut R,
    config: &BridgeConfig,
    require_preface: bool,
) -> Result<ReadHeadOutcome<String>>
where
    R: AsyncReadExt + Unpin,
{
    let mut parser =
        Http2Parser::with_config(Http2Config::default().with_require_preface(require_preface));
    read_until(stream, config, "HTTP/2 head", |chunk| {
        // Offer new bytes to the parser, re-offering the tail on a short count
        // (the backpressure signal), mirroring the HTTP/1.1 head reader.
        let mut pending = Bytes::copy_from_slice(chunk);
        while !pending.is_empty() {
            let accepted = parser.push(FlowSide::Initiator, &pending);
            pending = pending.slice(accepted..);
            if accepted == 0 {
                break;
            }
        }

        while let Some(ev) = parser.next_event() {
            if let Http2Event::Head(head) = ev
                && head.dir == FlowSide::Initiator
            {
                if let Some(call) = grpc_call(&head) {
                    info!(
                        authority = head.authority().unwrap_or("-"),
                        service = call.service,
                        method = call.method,
                        "Routing gRPC call"
                    );
                }
                return h2_routing_key(&head).map(Some);
            }
        }

        if parser.is_failed() {
            return Err(anyhow::anyhow!(
                "malformed HTTP/2 connection: {}",
                parser
                    .error()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        Ok(None)
    })
    .await
}

/// Scans an h2 **response** (Responder) byte stream for each stream's
/// gRPC completion status (#62), read-only.
struct GrpcStatusScanner {
    parser: Http2Parser,
    seen: std::collections::HashSet<u32>,
}

impl GrpcStatusScanner {
    fn new() -> Self {
        Self {
            parser: Http2Parser::with_config(Http2Config::default().with_require_preface(false)),
            seen: std::collections::HashSet::new(),
        }
    }

    /// Feed a response chunk; call `on_status` once per stream as its gRPC status
    /// becomes known — from `Trailers`, or a Trailers-Only response `Head`
    /// (a `HEADERS` block that ends the stream, carrying `grpc-status`).
    fn feed(&mut self, chunk: &[u8], mut on_status: impl FnMut(u32, GrpcStatus)) {
        let mut pending = Bytes::copy_from_slice(chunk);
        while !pending.is_empty() {
            let accepted = self.parser.push(FlowSide::Responder, &pending);
            pending = pending.slice(accepted..);
            if accepted == 0 {
                break;
            }
        }
        while let Some(ev) = self.parser.next_event() {
            let found = match ev {
                Http2Event::Trailers {
                    stream_id,
                    ref fields,
                    ..
                } => grpc_status(fields).map(|s| (stream_id, s)),
                Http2Event::Head(ref head) if head.end_stream => {
                    grpc_status_of(head).map(|s| (head.stream_id, s))
                }
                _ => None,
            };
            if let Some((stream_id, status)) = found
                && self.seen.insert(stream_id)
            {
                on_status(stream_id, status);
            }
        }
    }
}

/// Build a [`ResponseTap`](super::bridge::ResponseTap) for a terminated-h2
/// connection that surfaces gRPC completion status (#62): each call is recorded
/// to the `zbridge_grpc_status_total{service,code}` metric and logged (a non-OK
/// code at `warn`, since a failed gRPC call still carries HTTP 200).
pub(super) fn h2_response_tap(service: String) -> super::bridge::ResponseTap {
    let mut scanner = GrpcStatusScanner::new();
    Box::new(move |chunk: &[u8]| {
        scanner.feed(chunk, |stream_id, status| {
            crate::metrics::metrics().record_grpc_status(&service, status.code);
            if status.is_ok() {
                debug!(stream_id, code = status.code, "terminated gRPC call OK");
            } else {
                warn!(
                    stream_id,
                    code = status.code,
                    name = status.name().unwrap_or("?"),
                    "terminated gRPC call returned an error status"
                );
            }
        });
    })
}

/// Parse the first request head out of peeked (non-consumed) bytes, if one is
/// complete. A partial head yields `None` — callers re-peek with more bytes.
pub(super) fn peek_http_head(peek: &[u8], config: &BridgeConfig) -> Option<RequestHead> {
    let mut parser = http_head_parser(config);
    let mut pending = Bytes::copy_from_slice(peek);
    while !pending.is_empty() {
        let accepted = parser.push(FlowSide::Initiator, &pending);
        pending = pending.slice(accepted..);
        if accepted == 0 {
            break;
        }
    }
    while let Some(ev) = parser.next_event() {
        if let HttpEvent::RequestHead(head) = ev {
            return Some(head);
        }
    }
    None
}

/// Whether a request head asks for a WebSocket upgrade, per RFC 9110 §7.8 +
/// RFC 6455 §4.1 (flowscope 0.24, #77): the `Connection` header must name the
/// `upgrade` option, `websocket` must appear among the offered tokens (comma
/// lists and duplicate header instances handled), the method must be GET, and
/// `Sec-WebSocket-Key`/`-Version` must be present. A POST with a stray
/// `Upgrade: websocket`, or an `Upgrade` header that `Connection` does not
/// name, now correctly routes as plain HTTP.
pub(super) fn head_is_websocket_upgrade(head: &RequestHead) -> bool {
    head.is_websocket_upgrade()
}

/// Where a host-keyed connection goes after consulting liveliness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackendRoute {
    /// `{service}/{dns}/available` is alive — scope the keys to the host.
    Host,
    /// No host token, but the default `{service}/available` is alive — use
    /// bare service keys (a plain backend serves as the service's catch-all).
    Default,
    /// Neither token is alive — refuse the connection (502 / close).
    Unavailable,
}

/// One-shot liveliness probe: any reply for `key` within `timeout` means alive.
async fn token_alive(session: &Session, key: &str, timeout: std::time::Duration) -> Result<bool> {
    let replies = session
        .liveliness()
        .get(key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query service liveliness: {}", e))?;
    Ok(tokio::time::timeout(timeout, async {
        replies.recv_async().await.is_ok()
    })
    .await
    .unwrap_or(false))
}

/// Resolve which backend serves a connection: an `@host` backend matching
/// `dns` wins, else the service's default (plain) backend, else nobody.
/// `dns = None` (no routable hostname) consults only the default token.
/// Each probe is bounded by `config.availability_timeout`.
pub(super) async fn resolve_backend(
    session: &Session,
    service_name: &str,
    dns: Option<&str>,
    config: &BridgeConfig,
) -> Result<BackendRoute> {
    let t = config.availability_timeout;
    let default_key = format!("{}/available", service_name);
    let Some(dns) = dns else {
        return Ok(if token_alive(session, &default_key, t).await? {
            BackendRoute::Default
        } else {
            BackendRoute::Unavailable
        });
    };

    // Probe both tokens concurrently, but return the moment the host probe is
    // positive: making the host-routed happy path wait for the default probe
    // would tax every connection with up to a full availability timeout.
    let host_key = format!("{}/{}/available", service_name, dns);
    let mut host_fut = std::pin::pin!(token_alive(session, &host_key, t));
    let mut default_fut = std::pin::pin!(token_alive(session, &default_key, t));
    let mut default_done: Option<bool> = None;
    let host_alive = loop {
        tokio::select! {
            h = &mut host_fut => break h?,
            d = &mut default_fut, if default_done.is_none() => default_done = Some(d?),
        }
    };
    if host_alive {
        return Ok(BackendRoute::Host);
    }
    let default_alive = match default_done {
        Some(d) => d,
        None => default_fut.await?,
    };
    Ok(if default_alive {
        BackendRoute::Default
    } else {
        BackendRoute::Unavailable
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    /// A mock reader that yields its bytes in preset chunks, one per `read` call,
    /// so a test can force a TLS record to arrive across multiple TCP segments.
    struct ChunkedReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl ChunkedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
            }
        }
    }

    impl tokio::io::AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if let Some(mut chunk) = self.chunks.pop_front() {
                let n = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.chunks.push_front(chunk.split_off(n));
                }
            }
            // Empty queue -> a zero-byte read, which read_tls_sni treats as EOF.
            Poll::Ready(Ok(()))
        }
    }

    /// Yields its chunks, then stays pending forever — a client that sent a
    /// partial head and went quiet, for exercising the read timeout.
    struct StallingReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl tokio::io::AsyncRead for StallingReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            match self.chunks.pop_front() {
                Some(mut chunk) => {
                    let n = chunk.len().min(buf.remaining());
                    buf.put_slice(&chunk[..n]);
                    if n < chunk.len() {
                        self.chunks.push_front(chunk.split_off(n));
                    }
                    Poll::Ready(Ok(()))
                }
                None => Poll::Pending,
            }
        }
    }

    // #76: on read timeout the shared reader surfaces the bytes it consumed,
    // so a caller can fall back to relaying them opaquely (the h2c path, #74)
    // instead of silently dropping them on the floor.
    // #76: a client that closes mid-head is a hard error naming the head kind,
    // not a hang or a silent empty route.
    #[tokio::test]
    async fn read_until_eof_mid_head_errors() {
        let hello = build_client_hello_with_sni("eof.example.com");
        let half = hello[..hello.len() / 2].to_vec();
        // ChunkedReader yields a zero-byte read once its queue empties -> EOF.
        let mut reader = ChunkedReader::new(vec![half]);
        let cfg = BridgeConfig::default();
        let err = read_tls_sni(&mut reader, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed before complete"), "{err}");
    }

    // #76: a head that never completes within max_header_size trips the size
    // cap instead of buffering without bound.
    #[tokio::test]
    async fn read_until_oversized_head_errors() {
        // A TLS record header declaring a large handshake, then filler that
        // never completes a ClientHello.
        let mut chunks = vec![vec![0x16, 0x03, 0x01, 0x40, 0x00, 0x01]];
        for _ in 0..8 {
            chunks.push(vec![0u8; 512]);
        }
        let mut reader = ChunkedReader::new(chunks);
        let cfg = BridgeConfig {
            max_header_size: 2048,
            ..Default::default()
        };
        let err = read_tls_sni(&mut reader, &cfg)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds maximum size"), "{err}");
    }

    // #77: WS-upgrade detection delegates to flowscope 0.24's RFC-strict
    // check — the shapes the old single-header check got wrong must now
    // resolve correctly.
    #[test]
    fn ws_upgrade_detection_is_rfc_strict() {
        let cfg = BridgeConfig::default();
        let ws = |wire: &[u8]| head_is_websocket_upgrade(&peek_http_head(wire, &cfg).unwrap());

        // Comma-listed Connection token + case variants: an upgrade.
        assert!(ws(
            b"GET /c HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, Upgrade\r\nUpgrade: WebSocket\r\nSec-WebSocket-Key: k\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ));
        // POST with full WS headers: not the RFC 6455 handshake shape.
        assert!(!ws(
            b"POST /c HTTP/1.1\r\nHost: a\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k\r\nSec-WebSocket-Version: 13\r\nContent-Length: 0\r\n\r\n"
        ));
        // Upgrade header the Connection header does not name: not an offer.
        assert!(!ws(
            b"GET /c HTTP/1.1\r\nHost: a\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ));
        // Missing Sec-WebSocket-Key: not a handshake.
        assert!(!ws(
            b"GET /c HTTP/1.1\r\nHost: a\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn read_until_timeout_returns_partial_buffer() {
        let partial = b"PRI * HT".to_vec();
        let mut reader = StallingReader {
            chunks: vec![partial.clone()].into(),
        };
        let cfg = BridgeConfig::default();
        let outcome = read_until(&mut reader, &cfg, "test head", |_chunk| Ok(None::<()>))
            .await
            .unwrap();
        match outcome {
            ReadHeadOutcome::Timeout(buffer) => assert_eq!(buffer, partial),
            ReadHeadOutcome::Parsed(..) => panic!("nothing should have parsed"),
        }
    }

    /// Build a minimal but well-formed TLS 1.2 ClientHello record carrying an
    /// SNI extension for `hostname`.
    fn build_client_hello_with_sni(hostname: &str) -> Vec<u8> {
        let name_bytes = hostname.as_bytes();

        let sni_ext_len = 2 + 1 + 2 + name_bytes.len();
        let ext_len = sni_ext_len;
        let sni_ext: Vec<u8> = [
            &[0x00, 0x00],                                  // Extension type: SNI
            &((ext_len as u16).to_be_bytes())[..],          // Extension data length
            &(((ext_len - 2) as u16).to_be_bytes())[..],    // SNI list length
            &[0x00],                                        // Name type: host_name
            &((name_bytes.len() as u16).to_be_bytes())[..], // Name length
            name_bytes,
        ]
        .concat();

        let extensions_total_len = sni_ext.len();

        let client_hello_body: Vec<u8> = [
            &[0x03, 0x03],     // TLS 1.2
            &[0x00u8; 32][..], // Random
            &[0x00],           // Session ID length: 0
            &[0x00, 0x02],     // Cipher suites length: 2
            &[0x00, 0xFF],     // Cipher suite: TLS_EMPTY_RENEGOTIATION_INFO_SCSV
            &[0x01, 0x00],     // Compression methods: 1, null
            &((extensions_total_len as u16).to_be_bytes())[..],
            &sni_ext,
        ]
        .concat();

        let hs_len = client_hello_body.len();
        let mut handshake = Vec::with_capacity(4 + hs_len);
        handshake.push(0x01); // ClientHello
        handshake.extend_from_slice(&[0x00, ((hs_len >> 8) & 0xFF) as u8, (hs_len & 0xFF) as u8]);
        handshake.extend_from_slice(&client_hello_body);

        let record_len = handshake.len();
        [
            &[0x16, 0x03, 0x01], // Handshake, TLS 1.0 (legacy record version)
            &((record_len as u16).to_be_bytes())[..],
            &handshake,
        ]
        .concat()
    }

    #[tokio::test]
    async fn read_tls_sni_single_read() {
        let hello = build_client_hello_with_sni("example.com");
        let mut reader = ChunkedReader::new(vec![hello.clone()]);
        let cfg = BridgeConfig::default();
        let (dns, buffer) = read_tls_sni(&mut reader, &cfg).await.unwrap();
        assert_eq!(dns, "example.com");
        // The ClientHello is forwarded verbatim.
        assert_eq!(buffer, hello);
    }

    // G6 (#42): a ClientHello split across two TCP segments must still yield its
    // SNI. The previous single-record reader could not reassemble this and would
    // fall through, letting an HTTP 400 be written onto a TLS socket.
    #[tokio::test]
    async fn read_tls_sni_split_clienthello_reassembles() {
        let hello = build_client_hello_with_sni("split.example.com");
        // Split mid-record so neither half is a complete ClientHello on its own.
        let mid = hello.len() / 2;
        let first = hello[..mid].to_vec();
        let second = hello[mid..].to_vec();
        let mut reader = ChunkedReader::new(vec![first, second]);
        let cfg = BridgeConfig::default();
        let (dns, buffer) = read_tls_sni(&mut reader, &cfg).await.unwrap();
        assert_eq!(dns, "split.example.com");
        assert_eq!(buffer, hello);
    }

    #[tokio::test]
    async fn read_tls_sni_split_into_many_bytes_reassembles() {
        let hello = build_client_hello_with_sni("bytewise.example.com");
        // One byte per read — the pathological segmentation case.
        let chunks: Vec<Vec<u8>> = hello.iter().map(|b| vec![*b]).collect();
        let mut reader = ChunkedReader::new(chunks);
        let cfg = BridgeConfig::default();
        let (dns, _buffer) = read_tls_sni(&mut reader, &cfg).await.unwrap();
        assert_eq!(dns, "bytewise.example.com");
    }

    #[tokio::test]
    async fn read_tls_sni_missing_sni_errors() {
        // A ClientHello with no extensions -> no SNI -> error (not a panic, and
        // not a silent empty route).
        let body: Vec<u8> = [
            &[0x03, 0x03],     // TLS 1.2
            &[0x00u8; 32][..], // Random
            &[0x00],           // Session ID length: 0
            &[0x00, 0x02],     // Cipher suites length
            &[0x00, 0xFF],     // one suite
            &[0x01, 0x00],     // compression
            &[0x00, 0x00],     // extensions length: 0
        ]
        .concat();
        let hs_len = body.len();
        let mut handshake = vec![
            0x01,
            0x00,
            ((hs_len >> 8) & 0xFF) as u8,
            (hs_len & 0xFF) as u8,
        ];
        handshake.extend_from_slice(&body);
        let record_len = handshake.len();
        let mut record = vec![0x16, 0x03, 0x01];
        record.extend_from_slice(&(record_len as u16).to_be_bytes());
        record.extend_from_slice(&handshake);

        let mut reader = ChunkedReader::new(vec![record]);
        let cfg = BridgeConfig::default();
        assert!(read_tls_sni(&mut reader, &cfg).await.is_err());
    }

    // --- HTTP/2 head reading (Phase C, #50) ---

    /// Build a real on-the-wire h2 client request (preface + a HEADERS frame)
    /// via flowscope's own HPACK encoder, so the test exercises the actual parse.
    fn build_h2_request(authority: &str, path: &str, content_type: Option<&str>) -> Vec<u8> {
        use flowscope::http2::{HpackEncoder, PREFACE, write_headers};

        let mut enc = HpackEncoder::new();
        let mut fields = vec![
            (Bytes::from_static(b":method"), Bytes::from_static(b"POST")),
            (Bytes::from_static(b":scheme"), Bytes::from_static(b"https")),
            (
                Bytes::from_static(b":authority"),
                Bytes::copy_from_slice(authority.as_bytes()),
            ),
            (
                Bytes::from_static(b":path"),
                Bytes::copy_from_slice(path.as_bytes()),
            ),
        ];
        if let Some(ct) = content_type {
            fields.push((
                Bytes::from_static(b"content-type"),
                Bytes::copy_from_slice(ct.as_bytes()),
            ));
        }
        let block = enc.encode(&fields).expect("encodable");
        let frames = write_headers(1, &block, true, 16_384).expect("framable");

        let mut out = Vec::with_capacity(PREFACE.len() + frames.len());
        out.extend_from_slice(PREFACE);
        out.extend_from_slice(&frames);
        out
    }

    fn chunkify(data: &[u8], size: usize) -> Vec<Vec<u8>> {
        data.chunks(size).map(|c| c.to_vec()).collect()
    }

    #[tokio::test]
    async fn read_h2_head_routes_by_authority() {
        let req = build_h2_request(
            "api.example.com",
            "/pkg.Svc/Method",
            Some("application/grpc"),
        );
        let mut reader = ChunkedReader::new(vec![req.clone()]);
        let cfg = BridgeConfig::default();
        let ReadHeadOutcome::Parsed(dns, buffer) =
            read_h2_head(&mut reader, &cfg, true).await.unwrap()
        else {
            panic!("expected a parsed head")
        };
        assert_eq!(dns, "api.example.com");
        // Every byte read is relayed verbatim.
        assert_eq!(buffer, req);
    }

    #[tokio::test]
    async fn read_h2_head_reassembles_split_frames() {
        // Preface + HEADERS split into tiny 5-byte segments must still reassemble.
        let req = build_h2_request("grpc.internal:8443", "/svc/m", None);
        let mut reader = ChunkedReader::new(chunkify(&req, 5));
        let cfg = BridgeConfig::default();
        let ReadHeadOutcome::Parsed(dns, buffer) =
            read_h2_head(&mut reader, &cfg, true).await.unwrap()
        else {
            panic!("expected a parsed head")
        };
        assert_eq!(dns, "grpc.internal:8443");
        assert_eq!(buffer, req);
    }

    #[tokio::test]
    async fn read_h2_head_default_port_normalized() {
        // :443 collapses just like the Host path, so keys match the export side.
        let req = build_h2_request("svc.example:443", "/svc/m", None);
        let mut reader = ChunkedReader::new(vec![req]);
        let cfg = BridgeConfig::default();
        let ReadHeadOutcome::Parsed(dns, _) = read_h2_head(&mut reader, &cfg, true).await.unwrap()
        else {
            panic!("expected a parsed head")
        };
        assert_eq!(dns, "svc.example");
    }

    // --- gRPC status trailer surfacing (#62) ---

    /// Encode a response HEADERS/Trailers field block into HEADERS frame(s).
    fn h2_headers_frame(fields: &[(&[u8], &[u8])], end_stream: bool) -> Vec<u8> {
        use flowscope::http2::{HpackEncoder, write_headers};
        let owned: Vec<(Bytes, Bytes)> = fields
            .iter()
            .map(|(n, v)| (Bytes::copy_from_slice(n), Bytes::copy_from_slice(v)))
            .collect();
        let block = HpackEncoder::new().encode(&owned).expect("encodable");
        write_headers(1, &block, end_stream, 16_384).expect("framable")
    }

    fn h2_data_frame(payload: &[u8], end_stream: bool) -> Vec<u8> {
        let len = payload.len();
        let mut f = vec![
            (len >> 16) as u8,
            (len >> 8) as u8,
            len as u8,
            0x00,                                 // type: DATA
            if end_stream { 0x01 } else { 0x00 }, // END_STREAM
        ];
        f.extend_from_slice(&1u32.to_be_bytes()); // stream id 1
        f.extend_from_slice(payload);
        f
    }

    fn scan(bytes: &[&[u8]]) -> Vec<(u32, u32)> {
        let mut scanner = GrpcStatusScanner::new();
        let mut got = Vec::new();
        for chunk in bytes {
            scanner.feed(chunk, |sid, st| got.push((sid, st.code)));
        }
        got
    }

    #[test]
    fn grpc_status_scanner_reads_trailers_only() {
        // A gRPC error is commonly a single HEADERS block (END_STREAM) with the
        // status and no body — flowscope reports it as a Head, so grpc_status_of.
        let resp = h2_headers_frame(
            &[
                (b":status", b"200"),
                (b"content-type", b"application/grpc"),
                (b"grpc-status", b"5"),
            ],
            true,
        );
        assert_eq!(scan(&[&resp]), vec![(1, 5)]);
    }

    #[test]
    fn grpc_status_scanner_reads_headers_data_trailers() {
        // The full-success shape: HEADERS(200) + DATA + Trailers(grpc-status: 0).
        let head = h2_headers_frame(
            &[(b":status", b"200"), (b"content-type", b"application/grpc")],
            false,
        );
        let data = h2_data_frame(b"\x00\x00\x00\x00\x00", false);
        let trailers = h2_headers_frame(&[(b"grpc-status", b"0")], true);
        let full: Vec<u8> = [head, data, trailers].concat();
        assert_eq!(scan(&[&full]), vec![(1, 0)]);
    }

    #[test]
    fn grpc_status_scanner_reassembles_split_stream() {
        let resp = h2_headers_frame(
            &[
                (b":status", b"200"),
                (b"content-type", b"application/grpc"),
                (b"grpc-status", b"9"),
            ],
            true,
        );
        let chunks: Vec<&[u8]> = resp.chunks(3).collect();
        assert_eq!(scan(&chunks), vec![(1, 9)]);
    }
}
