use crate::config::BridgeConfig;
use crate::dns::normalize_dns;
use crate::http_parser::parse_http_request;
use crate::http_util::{http_400_response, http_502_response};
use anyhow::Result;
use flowscope::SessionParser;
use flowscope::Timestamp;
use flowscope::classify::{Classify, WireProtocol, classify_first_bytes};
use flowscope::tls::{TlsMessage, TlsParser};
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
            // This is a plain HTTP connection - parse HTTP
            info!("Client {}: Detected plain HTTP connection", client_id);
            match parse_http_request(&mut stream).await {
                Ok(parsed) => {
                    let dns = parsed.dns.clone();
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
                    (format!("/{}", dns), Some(parsed.buffer))
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
}
