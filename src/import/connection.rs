use crate::config::BridgeConfig;
use crate::http_parser::{http_400_response, http_502_response, parse_http_request};
use crate::tls_parser::{is_tls_handshake, parse_tls_client_hello};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
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
        // Peek at the first few bytes to detect HTTP vs HTTPS/TLS
        let mut peek_buffer = vec![0u8; 16];
        let peek_len = stream.peek(&mut peek_buffer).await.unwrap_or(0);

        if peek_len > 0 && is_tls_handshake(&peek_buffer[..peek_len]) {
            // This is a TLS/HTTPS connection - parse SNI
            info!("Client {}: Detected TLS/HTTPS connection", client_id);
            match parse_tls_client_hello(&mut stream).await {
                Ok(parsed) => {
                    let dns = parsed.dns.clone();
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
                    (format!("/{}", dns), Some(parsed.buffer))
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
