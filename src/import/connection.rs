use crate::config::BridgeConfig;
use crate::dns::normalize_dns;
use crate::http_util::{http_400_response, http_502_response};
use anyhow::Result;
use bytes::Bytes;
use flowscope::SessionParser;
use flowscope::Timestamp;
use flowscope::classify::{Classify, WireProtocol, classify_first_bytes};
use flowscope::http::{HttpEvent, HttpProxyParser};
use flowscope::http2::{Http2Config, Http2Event, Http2Parser, StreamHead, grpc_call};
use flowscope::tls::{TlsMessage, TlsParser};
use flowscope::{FlowSide, http::RequestHead};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};
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
    let (dns_suffix, initial_buffer) = if http_mode {
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
            info!("Client {}: Detected TLS/HTTPS connection", client_id);
            match read_tls_sni(&mut stream, &config).await {
                Ok((dns, buffer)) => {
                    info!(
                        "Client {}: TLS SNI routing to DNS: {} (service: {})",
                        client_id, dns, service_name
                    );

                    // Check if backend is available by querying service liveliness
                    let service_key = format!("{}/{}/available", service_name, dns);
                    let liveliness_replies =
                        session.liveliness().get(&service_key).await.map_err(|e| {
                            anyhow::anyhow!("Failed to query service liveliness: {}", e)
                        })?;

                    // Check if any backend is alive
                    let backend_available =
                        tokio::time::timeout(config.availability_timeout, async {
                            liveliness_replies.recv_async().await.is_ok()
                        })
                        .await
                        .unwrap_or(false);

                    if !backend_available {
                        warn!(
                            "Client {}: No backend available for DNS: {}",
                            client_id, dns
                        );
                        // For TLS, we can't send an HTTP error, just close the connection
                        return Err(anyhow::anyhow!("No backend available for DNS: {}", dns));
                    }

                    info!("Client {}: Backend available for DNS: {}", client_id, dns);
                    (format!("/{}", dns), Some(buffer))
                }
                Err(e) => {
                    warn!(
                        "Client {}: Failed to parse TLS ClientHello: {}",
                        client_id, e
                    );
                    // For TLS, we can't send an HTTP error, just close the connection
                    return Err(anyhow::anyhow!("{}", e));
                }
            }
        } else {
            // This is a plain HTTP connection - parse the request head for Host.
            info!("Client {}: Detected plain HTTP connection", client_id);
            match read_http_head(&mut stream, &config).await {
                Ok((dns, buffer)) => {
                    info!(
                        "Client {}: HTTP routing to DNS: {} (service: {})",
                        client_id, dns, service_name
                    );

                    // Check if backend is available by querying service liveliness
                    let service_key = format!("{}/{}/available", service_name, dns);
                    let liveliness_replies =
                        session.liveliness().get(&service_key).await.map_err(|e| {
                            anyhow::anyhow!("Failed to query service liveliness: {}", e)
                        })?;

                    // Check if any backend is alive
                    let backend_available =
                        tokio::time::timeout(config.availability_timeout, async {
                            liveliness_replies.recv_async().await.is_ok()
                        })
                        .await
                        .unwrap_or(false);

                    if !backend_available {
                        warn!(
                            "Client {}: No backend available for DNS: {}",
                            client_id, dns
                        );
                        // Send HTTP 502 Bad Gateway
                        let _ = stream.write_all(&http_502_response(&dns)).await;
                        return Err(anyhow::anyhow!("No backend available for DNS: {}", dns));
                    }

                    info!("Client {}: Backend available for DNS: {}", client_id, dns);
                    (format!("/{}", dns), Some(buffer))
                }
                Err(e) => {
                    warn!("Client {}: Failed to parse HTTP request: {}", client_id, e);
                    // Send HTTP 400 Bad Request
                    let _ = stream.write_all(&http_400_response()).await;
                    return Err(anyhow::anyhow!("{}", e));
                }
            }
        }
    } else {
        (String::new(), None)
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
        &dns_suffix,
        initial_buffer,
        config,
    )
    .await
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
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut temp = vec![0u8; 4096];
    let max_size = config.max_header_size;

    let outcome = tokio::time::timeout(config.read_timeout, async {
        loop {
            let n = stream
                .read(&mut temp)
                .await
                .map_err(|e| anyhow::anyhow!("read error while reading TLS ClientHello: {}", e))?;
            if n == 0 {
                return Err(anyhow::anyhow!(
                    "connection closed before TLS ClientHello complete"
                ));
            }
            buffer.extend_from_slice(&temp[..n]);

            let mut msgs: Vec<TlsMessage> = Vec::new();
            parser.feed_initiator(&temp[..n], Timestamp::new(0, 0), &mut msgs);
            for msg in &msgs {
                if let TlsMessage::ClientHello(ch) = msg {
                    return match ch.sni() {
                        Some(sni) => Ok(normalize_dns(sni)),
                        None => Err(anyhow::anyhow!("TLS ClientHello has no SNI extension")),
                    };
                }
            }

            if buffer.len() >= max_size {
                return Err(anyhow::anyhow!(
                    "TLS ClientHello exceeds maximum size of {} bytes",
                    max_size
                ));
            }
        }
    })
    .await;

    match outcome {
        Ok(Ok(dns)) => Ok((dns, buffer)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!("timeout reading TLS ClientHello")),
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
pub(super) async fn read_http_head<R>(
    stream: &mut R,
    config: &BridgeConfig,
) -> Result<(String, Vec<u8>)>
where
    R: AsyncReadExt + Unpin,
{
    let mut parser = HttpProxyParser::new();
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut temp = vec![0u8; 4096];
    let max_size = config.max_header_size;

    let outcome = tokio::time::timeout(config.read_timeout, async {
        loop {
            let n = stream
                .read(&mut temp)
                .await
                .map_err(|e| anyhow::anyhow!("read error while reading HTTP request: {}", e))?;
            if n == 0 {
                return Err(anyhow::anyhow!(
                    "connection closed before complete HTTP request"
                ));
            }
            buffer.extend_from_slice(&temp[..n]);

            // Offer the new bytes to the parser, re-offering the tail on a short
            // count (the backpressure signal). The head fits well within the
            // parser's buffer, so this converges before any RequestHead.
            let mut pending = Bytes::copy_from_slice(&temp[..n]);
            while !pending.is_empty() {
                let accepted = parser.push(FlowSide::Initiator, &pending);
                pending = pending.slice(accepted..);
                if accepted == 0 {
                    break;
                }
            }

            while let Some(ev) = parser.next_event() {
                if let HttpEvent::RequestHead(head) = ev {
                    return routing_key_from_head(&head);
                }
            }

            if let Some(reason) = parser.poison() {
                return Err(anyhow::anyhow!(
                    "malformed HTTP request: {}",
                    reason.as_str()
                ));
            }

            if buffer.len() >= max_size {
                return Err(anyhow::anyhow!(
                    "HTTP headers exceed maximum size of {} bytes",
                    max_size
                ));
            }
        }
    })
    .await;

    match outcome {
        Ok(Ok(dns)) => Ok((dns, buffer)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!("timeout reading HTTP request")),
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

/// Read a terminated HTTP/2 connection's first request head and resolve its DNS
/// routing key from `:authority` (Phase C, #50).
///
/// Used after `--https-terminate` negotiates ALPN `h2`. The decrypted client
/// bytes are streamed into flowscope's [`Http2Parser`] purely to *peek* the first
/// request stream's `:authority`; the parser is read-only — every byte read is
/// also kept in `buffer` and relayed to the backend verbatim, exactly as the
/// HTTP/1.1 terminate path does. `require_preface = false` tolerates a client
/// that was pinned to h2 by ALPN yet still consumes the preface when present.
///
/// The connection is routed by the first stream's authority; its multiplexed
/// streams are then relayed opaquely (a single-authority h2 proxy, not a
/// per-stream demux). If the first head is a gRPC call it is logged.
pub(super) async fn read_h2_head<R>(
    stream: &mut R,
    config: &BridgeConfig,
) -> Result<(String, Vec<u8>)>
where
    R: AsyncReadExt + Unpin,
{
    let mut parser = Http2Parser::with_config(Http2Config::default().with_require_preface(false));
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut temp = vec![0u8; 4096];
    let max_size = config.max_header_size;

    let outcome = tokio::time::timeout(config.read_timeout, async {
        loop {
            let n = stream
                .read(&mut temp)
                .await
                .map_err(|e| anyhow::anyhow!("read error while reading HTTP/2 head: {}", e))?;
            if n == 0 {
                return Err(anyhow::anyhow!(
                    "connection closed before an HTTP/2 request head"
                ));
            }
            buffer.extend_from_slice(&temp[..n]);

            // Offer new bytes to the parser, re-offering the tail on a short count
            // (the backpressure signal), mirroring the HTTP/1.1 head reader.
            let mut pending = Bytes::copy_from_slice(&temp[..n]);
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
                            "Routing terminated gRPC call"
                        );
                    }
                    return h2_routing_key(&head);
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

            if buffer.len() >= max_size {
                return Err(anyhow::anyhow!(
                    "HTTP/2 head exceeds maximum size of {} bytes",
                    max_size
                ));
            }
        }
    })
    .await;

    match outcome {
        Ok(Ok(dns)) => Ok((dns, buffer)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!("timeout reading HTTP/2 head")),
    }
}

/// Detect a WebSocket upgrade from peeked (non-consumed) request bytes.
///
/// Feeds the peek into an [`HttpProxyParser`] and inspects the first request
/// head's `Upgrade` header. A partial head simply yields `false` (the caller
/// treats it as ordinary HTTP).
pub(super) fn peek_is_websocket(peek: &[u8]) -> bool {
    let mut parser = HttpProxyParser::new();
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
            return head
                .header("upgrade")
                .is_some_and(|v| v.eq_ignore_ascii_case(b"websocket"));
        }
    }
    false
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
        let (dns, buffer) = read_h2_head(&mut reader, &cfg).await.unwrap();
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
        let (dns, buffer) = read_h2_head(&mut reader, &cfg).await.unwrap();
        assert_eq!(dns, "grpc.internal:8443");
        assert_eq!(buffer, req);
    }

    #[tokio::test]
    async fn read_h2_head_default_port_normalized() {
        // :443 collapses just like the Host path, so keys match the export side.
        let req = build_h2_request("svc.example:443", "/svc/m", None);
        let mut reader = ChunkedReader::new(vec![req]);
        let cfg = BridgeConfig::default();
        let (dns, _) = read_h2_head(&mut reader, &cfg).await.unwrap();
        assert_eq!(dns, "svc.example");
    }
}
