//! Import mode implementation for the Zenoh TCP Bridge.
//!
//! This module handles importing Zenoh services as TCP listeners.
//! Each TCP connection gets its own liveliness token and dedicated Zenoh pub/sub.
//!
//! Supports both regular TCP mode and HTTP-aware mode with DNS-based routing.
//! Also supports HTTPS/TLS with SNI-based routing.

use crate::http_parser::{http_400_response, http_502_response, parse_http_request};
use crate::tls_parser::{is_tls_handshake, parse_tls_client_hello};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};
use zenoh::key_expr::KeyExpr;
use zenoh::Session;
use zenoh_ext::{
    AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt, CacheConfig, HistoryConfig,
    MissDetectionConfig, RecoveryConfig,
};

/// Parse import specification in format 'service_name/listen_addr'
pub fn parse_import_spec(import_spec: &str) -> Result<(String, SocketAddr)> {
    let parts: Vec<&str> = import_spec.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!(
            "Invalid import format. Expected: 'service_name/listen_addr' (e.g., 'myservice/127.0.0.1:8002')"
        ));
    }

    let service_name = parts[0].to_string();
    let listen_addr: SocketAddr = parts[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid listen address: {}", e))?;

    Ok((service_name, listen_addr))
}

/// Run import mode for a single service
///
/// This function:
/// 1. Binds a TCP listener on the specified address
/// 2. Accepts incoming TCP connections
/// 3. For each connection, spawns a handler that bridges to Zenoh
pub async fn run_import_mode(session: Arc<Session>, import_spec: &str) -> Result<()> {
    run_import_mode_internal(session, import_spec, false).await
}

/// Run HTTP-aware import mode for a single service
///
/// This function:
/// 1. Binds a TCP listener on the specified address
/// 2. Accepts incoming HTTP connections
/// 3. Parses the Host header to determine DNS-based routing
/// 4. For each connection, spawns a handler that bridges to Zenoh with DNS keys
pub async fn run_http_import_mode(session: Arc<Session>, import_spec: &str) -> Result<()> {
    run_import_mode_internal(session, import_spec, true).await
}

/// Internal implementation for both regular and HTTP import modes
async fn run_import_mode_internal(
    session: Arc<Session>,
    import_spec: &str,
    http_mode: bool,
) -> Result<()> {
    let (service_name, listen_addr) = parse_import_spec(import_spec)?;

    if http_mode {
        info!("🚀 HTTP IMPORT MODE");
        info!("   Service name: {}", service_name);
        info!("   TCP Listen: {}", listen_addr);
        info!("   Zenoh TX key: {}/{{dns}}/tx/<client_id>", service_name);
        info!("   Zenoh RX key: {}/{{dns}}/rx/<client_id>", service_name);
        info!(
            "   Liveliness: {}/{{dns}}/clients/<client_id>",
            service_name
        );
        info!("   DNS routing: Extracts Host header from HTTP requests");
    } else {
        info!("🚀 IMPORT MODE");
        info!("   Service name: {}", service_name);
        info!("   TCP Listen: {}", listen_addr);
        info!("   Zenoh TX key: {}/tx/<client_id>", service_name);
        info!("   Zenoh RX key: {}/rx/<client_id>", service_name);
        info!("   Liveliness: {}/clients/<client_id>", service_name);
    }

    // Start TCP listener
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    if http_mode {
        info!("✓ Listening for HTTP connections on {}", listen_addr);
        info!(
            "✓ Ready to route to backends under service '{}'",
            service_name
        );
    } else {
        info!("✓ Listening for TCP connections on {}", listen_addr);
        info!("✓ Ready to forward to service '{}'", service_name);
    }

    let mut connection_id = 0u64;

    // Accept connections
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                connection_id += 1;
                let client_id = format!("client_{}", connection_id);
                info!(
                    "✓ New {} connection: {} from {}",
                    if http_mode { "HTTP" } else { "TCP" },
                    client_id,
                    addr
                );

                let session = session.clone();
                let service_name = service_name.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_import_connection(
                        session,
                        stream,
                        &service_name,
                        &client_id,
                        http_mode,
                    )
                    .await
                    {
                        error!("Connection {} error: {:?}", client_id, e);
                    }
                    info!("✗ Connection {} closed", client_id);
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {:?}", e);
            }
        }
    }
}

/// Handle a single import connection
///
/// This function bridges a TCP connection to a Zenoh service by:
/// 1. Optionally parsing HTTP to extract DNS (if http_mode is true)
/// 2. Subscribing to error signals (before declaring liveliness)
/// 3. Subscribing to the service's response channel
/// 4. Declaring a liveliness token to signal the export bridge
/// 5. Spawning tasks to bridge data in both directions
async fn handle_import_connection(
    session: Arc<Session>,
    mut stream: TcpStream,
    service_name: &str,
    client_id: &str,
    http_mode: bool,
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
                        tokio::time::timeout(Duration::from_millis(1000), async {
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
                        tokio::time::timeout(Duration::from_millis(1000), async {
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

    let (mut tcp_reader, mut tcp_writer) = stream.into_split();

    // IMPORTANT: Subscribe to error channel FIRST, before declaring liveliness
    // This prevents race condition where export bridge publishes error before we're subscribed
    let error_key = format!("{}{}/error/{}", service_name, dns_suffix, client_id);
    let error_subscriber = session
        .declare_subscriber(&error_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe to error channel: {}", e))?;

    debug!(
        "Client {}: Subscribed to error channel {}",
        client_id, error_key
    );

    // Subscribe to responses from the service for this specific client using AdvancedSubscriber
    // This allows late publisher detection and recovery of missed samples
    let sub_key = format!("{}{}/rx/{}", service_name, dns_suffix, client_id);
    let subscriber = session
        .declare_subscriber(&sub_key)
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default().periodic_queries(Duration::from_millis(500)))
        .subscriber_detection()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe: {}", e))?;

    debug!(
        "Client {}: Subscribed to {} with late publisher detection",
        client_id, sub_key
    );

    // Declare AdvancedPublisher with cache and publisher detection
    // This allows the export bridge to detect when we're ready and recover any missed samples
    let pub_key_str = format!("{}{}/tx/{}", service_name, dns_suffix, client_id);
    let pub_key: KeyExpr<'static> = pub_key_str
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("Invalid key expression: {}", e))?;
    let publisher = session
        .declare_publisher(pub_key.clone())
        .cache(CacheConfig::default().max_samples(10))
        .sample_miss_detection(MissDetectionConfig::default().heartbeat(Duration::from_millis(500)))
        .publisher_detection()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare publisher: {}", e))?;

    debug!(
        "Client {}: Declared AdvancedPublisher on {} with cache",
        client_id, pub_key_str
    );

    // NOW declare liveliness token - export bridge will detect this and try to connect
    let liveliness_key = format!("{}{}/clients/{}", service_name, dns_suffix, client_id);
    let _liveliness_token = session
        .liveliness()
        .declare_token(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare liveliness: {}", e))?;

    info!(
        "✓ Client {} declared liveliness: {}",
        client_id, liveliness_key
    );

    // Send the initial HTTP request if we buffered it
    if let Some(buffer) = initial_buffer {
        debug!(
            "Client {}: Forwarding initial HTTP request ({} bytes)",
            client_id,
            buffer.len()
        );
        if let Err(e) = publisher.put(&buffer).await {
            error!(
                "Client {}: Failed to publish initial request: {:?}",
                client_id, e
            );
            return Err(anyhow::anyhow!("Failed to publish initial request: {}", e));
        }
    }

    // No sleep needed! The AdvancedPublisher/Subscriber with cache and history
    // handle synchronization automatically through publisher detection and late joiner support

    // Task: Zenoh subscriber -> TCP writer
    let client_id_clone = client_id.to_string();
    let client_id_for_error = client_id.to_string();

    // Task: Monitor error signals from export bridge
    let mut error_monitor = tokio::spawn(async move {
        if let Ok(sample) = error_subscriber.recv_async().await {
            let error_msg = sample.payload().to_bytes();
            error!(
                "Client {}: Backend error: {}",
                client_id_for_error,
                String::from_utf8_lossy(&error_msg)
            );
            return true; // Signal error occurred
        }
        false
    });

    let mut zenoh_to_tcp = tokio::spawn(async move {
        while let Ok(sample) = subscriber.recv_async().await {
            let payload = sample.payload().to_bytes().to_vec();

            // Empty payload is EOF signal from export side
            if payload.is_empty() {
                debug!("Client {}: Received EOF signal from Zenoh", client_id_clone);
                break;
            }

            debug!(
                "Client {}: ← {} bytes from Zenoh",
                client_id_clone,
                payload.len()
            );

            if let Err(e) = tcp_writer.write_all(&payload).await {
                error!(
                    "Client {}: Failed to write to TCP: {:?}",
                    client_id_clone, e
                );
                break;
            }

            // Flush to ensure data is sent immediately
            if let Err(e) = tcp_writer.flush().await {
                error!("Client {}: Failed to flush TCP: {:?}", client_id_clone, e);
                break;
            }
        }

        // Shutdown the write side of the TCP connection to signal EOF to the client
        // This is important so clients using read_to_string() or similar will complete
        if let Err(e) = tcp_writer.shutdown().await {
            debug!(
                "Client {}: Error shutting down TCP write: {:?}",
                client_id_clone, e
            );
        }

        debug!("Client {}: Zenoh to TCP task completed", client_id_clone);
    });

    // Task: TCP reader -> Zenoh AdvancedPublisher
    let client_id_clone = client_id.to_string();
    let mut tcp_to_zenoh = tokio::spawn(async move {
        let mut buffer = vec![0u8; 65536];
        loop {
            match tcp_reader.read(&mut buffer).await {
                Ok(0) => {
                    debug!("Client {}: TCP connection closed", client_id_clone);
                    break;
                }
                Ok(n) => {
                    debug!("Client {}: → {} bytes to Zenoh", client_id_clone, n);
                    if let Err(e) = publisher.put(&buffer[..n]).await {
                        error!(
                            "Client {}: Failed to publish to Zenoh: {:?}",
                            client_id_clone, e
                        );
                        break;
                    }
                }
                Err(e) => {
                    error!("Client {}: TCP read error: {:?}", client_id_clone, e);
                    break;
                }
            }
        }
    });

    // Wait for either task to complete or error signal
    tokio::select! {
        _ = &mut zenoh_to_tcp => {
            info!("Client {}: Zenoh to TCP task completed", client_id);
            tcp_to_zenoh.abort();
            error_monitor.abort();
        },
        _ = &mut tcp_to_zenoh => {
            info!("Client {}: TCP to Zenoh task completed", client_id);
            zenoh_to_tcp.abort();
            error_monitor.abort();
        },
        error_result = &mut error_monitor => {
            if let Ok(true) = error_result {
                error!("Client {}: Backend unavailable, closing connection", client_id);
                // Abort both tasks to immediately close the TCP connection
                zenoh_to_tcp.abort();
                tcp_to_zenoh.abort();
            }
        },
    }

    // Liveliness token is automatically dropped here, signaling disconnection

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_import_spec_valid() {
        let result = parse_import_spec("myservice/127.0.0.1:8080");
        assert!(result.is_ok());
        let (service, addr) = result.unwrap();
        assert_eq!(service, "myservice");
        assert_eq!(addr.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn test_parse_import_spec_invalid_format() {
        let result = parse_import_spec("invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_import_spec_invalid_addr() {
        let result = parse_import_spec("myservice/invalid:addr");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_import_spec_too_many_parts() {
        let result = parse_import_spec("service/addr/extra");
        assert!(result.is_err());
    }
}
