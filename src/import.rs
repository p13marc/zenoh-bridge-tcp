//! Import mode implementation for the Zenoh TCP Bridge.
//!
//! This module handles importing Zenoh services as TCP listeners.
//! Each TCP connection gets its own liveliness token and dedicated Zenoh pub/sub.
//!
//! Supports regular TCP mode, HTTP-aware mode with DNS-based routing,
//! HTTPS/TLS with SNI-based routing, and WebSocket mode.

use crate::http_parser::{http_400_response, http_502_response, parse_http_request};
use crate::tls_parser::{is_tls_handshake, parse_tls_client_hello};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
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
pub async fn run_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    buffer_size: usize,
    drain_timeout: Duration,
    shutdown_token: CancellationToken,
) -> Result<()> {
    run_import_mode_internal(
        session,
        import_spec,
        false,
        buffer_size,
        drain_timeout,
        shutdown_token,
    )
    .await
}

/// Run HTTP-aware import mode for a single service
///
/// This function:
/// 1. Binds a TCP listener on the specified address
/// 2. Accepts incoming HTTP connections
/// 3. Parses the Host header to determine DNS-based routing
/// 4. For each connection, spawns a handler that bridges to Zenoh with DNS keys
pub async fn run_http_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    buffer_size: usize,
    drain_timeout: Duration,
    shutdown_token: CancellationToken,
) -> Result<()> {
    run_import_mode_internal(
        session,
        import_spec,
        true,
        buffer_size,
        drain_timeout,
        shutdown_token,
    )
    .await
}

/// Internal implementation for both regular and HTTP import modes
async fn run_import_mode_internal(
    session: Arc<Session>,
    import_spec: &str,
    http_mode: bool,
    buffer_size: usize,
    drain_timeout: Duration,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = parse_import_spec(import_spec)?;

    let mode = if http_mode { "http_import" } else { "import" };
    info!(
        mode = mode,
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting import bridge"
    );

    // Start TCP listener
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "Import bridge ready");

    // Accept connections
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());
                        info!(
                            client_id = %client_id,
                            remote_addr = %addr,
                            "New connection"
                        );

                        let session = session.clone();
                        let service_name = service_name.clone();
                        let client_id_clone = client_id.clone();

                        let span = info_span!(
                            "connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                            mode = if http_mode { "http" } else { "tcp" }
                        );

                        tokio::spawn(
                            async move {
                                if let Err(e) = handle_import_connection(
                                    session,
                                    stream,
                                    &service_name,
                                    &client_id_clone,
                                    http_mode,
                                    buffer_size,
                                    drain_timeout,
                                )
                                .await
                                {
                                    error!(error = %e, "Connection error");
                                }
                                info!("Connection closed");
                            }
                            .instrument(span),
                        );
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "Import bridge shutting down, no new connections");
                break;
            }
        }
    }

    info!(service = %service_name, "Import bridge stopped");
    Ok(())
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
    buffer_size: usize,
    drain_timeout: Duration,
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

    let (tcp_reader, tcp_writer) = stream.into_split();

    bridge_import_connection(
        session,
        tcp_reader,
        tcp_writer,
        service_name,
        client_id,
        &dns_suffix,
        initial_buffer,
        buffer_size,
        drain_timeout,
    )
    .await
}

/// Shared bidirectional bridging logic for import connections.
///
/// This function handles the Zenoh pub/sub setup and bidirectional data bridging
/// for any import connection, regardless of transport (TCP, TLS-terminated, etc.).
#[allow(clippy::too_many_arguments)]
async fn bridge_import_connection<R, W>(
    session: Arc<Session>,
    mut reader: R,
    mut writer: W,
    service_name: &str,
    client_id: &str,
    dns_suffix: &str,
    initial_buffer: Option<Vec<u8>>,
    buffer_size: usize,
    drain_timeout: Duration,
) -> Result<()>
where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
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
        .cache(CacheConfig::default().max_samples(64))
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
    let liveliness_token = session
        .liveliness()
        .declare_token(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare liveliness: {}", e))?;

    info!(
        "Client {} declared liveliness: {}",
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

    // Task: Zenoh subscriber -> writer
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

            if let Err(e) = writer.write_all(&payload).await {
                error!(
                    "Client {}: Failed to write to TCP: {:?}",
                    client_id_clone, e
                );
                break;
            }

            // Flush to ensure data is sent immediately
            if let Err(e) = writer.flush().await {
                error!("Client {}: Failed to flush TCP: {:?}", client_id_clone, e);
                break;
            }
        }

        // Shutdown the write side of the connection to signal EOF to the client
        if let Err(e) = writer.shutdown().await {
            debug!(
                "Client {}: Error shutting down write: {:?}",
                client_id_clone, e
            );
        }

        debug!("Client {}: Zenoh to TCP task completed", client_id_clone);
        // Explicitly undeclare subscriber before task exits
        if let Err(e) = subscriber.undeclare().await {
            debug!(
                "Client {}: Error undeclaring subscriber: {:?}",
                client_id_clone, e
            );
        }
    });

    // Task: reader -> Zenoh AdvancedPublisher
    let client_id_clone = client_id.to_string();
    let mut tcp_to_zenoh = tokio::spawn(async move {
        let mut buffer = vec![0u8; buffer_size];
        loop {
            match reader.read(&mut buffer).await {
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
        // Explicitly undeclare publisher before task exits
        if let Err(e) = publisher.undeclare().await {
            debug!(
                "Client {}: Error undeclaring publisher: {:?}",
                client_id_clone, e
            );
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
                warn!("Client {}: Backend unavailable, draining remaining data", client_id);

                // Stop sending to the dead backend
                tcp_to_zenoh.abort();

                // Give the zenoh_to_tcp task time to drain buffered samples
                match tokio::time::timeout(drain_timeout, &mut zenoh_to_tcp).await {
                    Ok(_) => {
                        info!("Client {}: Drain completed", client_id);
                    }
                    Err(_) => {
                        warn!("Client {}: Drain timed out after {:?}, aborting", client_id, drain_timeout);
                        zenoh_to_tcp.abort();
                    }
                }
            }
        },
    }

    // Explicitly undeclare liveliness token
    if let Err(e) = liveliness_token.undeclare().await {
        debug!(
            "Client {}: Error undeclaring liveliness: {:?}",
            client_id, e
        );
    }

    Ok(())
}

/// Run HTTP import mode with per-request routing.
///
/// Each HTTP request on a persistent connection is independently routed
/// based on its Host header. This allows a single client TCP connection
/// to reach multiple backends on a single port.
pub async fn run_http_multiroute_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    buffer_size: usize,
    drain_timeout: Duration,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = parse_import_spec(import_spec)?;

    info!(
        mode = "http_multiroute",
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting multi-route HTTP import bridge"
    );

    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "Multi-route HTTP import bridge ready");

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());
                        let session = session.clone();
                        let service_name = service_name.clone();

                        let span = info_span!(
                            "http_multiroute",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                        );

                        tokio::spawn(async move {
                            if let Err(e) = handle_multiroute_connection(
                                session, stream, &service_name, &client_id,
                                buffer_size, drain_timeout,
                            ).await {
                                error!(error = %e, "Multi-route connection error");
                            }
                            info!("Multi-route connection closed");
                        }.instrument(span));
                    }
                    Err(e) => error!("Failed to accept connection: {:?}", e),
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "Multi-route HTTP import bridge shutting down");
                break;
            }
        }
    }

    info!(service = %service_name, "Multi-route HTTP import bridge stopped");
    Ok(())
}

/// Handle a persistent HTTP connection with per-request routing.
///
/// For each request:
/// 1. Parse the HTTP request to extract Host
/// 2. Check backend availability for that Host via Zenoh liveliness
/// 3. Create a per-request Zenoh pub/sub pair and liveliness token
/// 4. Forward the request and receive the full response
/// 5. Detect response completion via Content-Length/chunked/EOF
/// 6. Loop for the next request (or close on Connection: close)
#[allow(clippy::too_many_arguments)]
async fn handle_multiroute_connection(
    session: Arc<Session>,
    mut stream: TcpStream,
    service_name: &str,
    client_id: &str,
    buffer_size: usize,
    drain_timeout: Duration,
) -> Result<()> {
    let mut request_num = 0u64;

    loop {
        request_num += 1;
        let request_id = format!("{}:r{}", client_id, request_num);

        // 1. Parse the next HTTP request
        let parsed = match parse_http_request(&mut stream).await {
            Ok(p) => p,
            Err(e) => {
                if request_num > 1 {
                    debug!("Client {}: Persistent connection ended: {}", client_id, e);
                    break;
                }
                warn!("Client {}: First request parse failed: {}", client_id, e);
                let _ = stream.write_all(&http_400_response()).await;
                break;
            }
        };

        let dns = &parsed.dns;
        info!(
            request_id = %request_id,
            dns = %dns,
            "Routing HTTP request"
        );

        // 2. Check backend availability
        let service_key = format!("{}/{}/available", service_name, dns);
        let backend_available = check_backend_available(&session, &service_key).await;

        if !backend_available {
            warn!(request_id = %request_id, dns = %dns, "No backend available");
            let _ = stream
                .write_all(&crate::http_parser::http_502_response(dns))
                .await;
            // Don't close the persistent connection — client may retry with different Host
            continue;
        }

        // 3. Create a one-shot Zenoh exchange for this request
        let sub_request_id = format!("{}_{}", request_id, uuid::Uuid::new_v4().as_simple());
        let dns_suffix = format!("/{}", dns);

        let tx_key = format!("{}{}/tx/{}", service_name, dns_suffix, sub_request_id);
        let rx_key: KeyExpr<'static> =
            format!("{}{}/rx/{}", service_name, dns_suffix, sub_request_id)
                .try_into()
                .map_err(|e| anyhow::anyhow!("Invalid key expression: {}", e))?;

        // Declare liveliness for this sub-request (triggers export to connect to backend)
        let liveliness_key = format!("{}{}/clients/{}", service_name, dns_suffix, sub_request_id);
        let liveliness_token = session
            .liveliness()
            .declare_token(&liveliness_key)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to declare liveliness: {}", e))?;

        // Subscribe to response
        let rx_subscriber = session
            .declare_subscriber(&rx_key)
            .history(HistoryConfig::default().detect_late_publishers())
            .subscriber_detection()
            .recovery(RecoveryConfig::default())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to subscribe: {}", e))?;

        // Publish request
        let tx_key_expr: KeyExpr<'static> = tx_key
            .try_into()
            .map_err(|e| anyhow::anyhow!("Invalid key expression: {}", e))?;
        let tx_publisher = session
            .declare_publisher(tx_key_expr)
            .cache(CacheConfig::default().max_samples(64))
            .publisher_detection()
            .sample_miss_detection(
                MissDetectionConfig::default().heartbeat(Duration::from_millis(500)),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to declare publisher: {}", e))?;

        // Small delay for Zenoh subscriber/publisher to establish
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send the request through Zenoh
        tx_publisher
            .put(&parsed.buffer[..])
            .await
            .map_err(|e| anyhow::anyhow!("Failed to publish request: {}", e))?;

        debug!(request_id = %request_id, "Published request ({} bytes)", parsed.buffer.len());

        // 4. Receive response and forward to client
        let mut response_buf = Vec::with_capacity(buffer_size);
        let mut body_framing: Option<crate::http_response_parser::ResponseBodyFraming> = None;
        let mut header_len: usize = 0;
        let mut response_complete = false;

        let timeout_duration = Duration::from_secs(30).max(drain_timeout);
        let response_result = tokio::time::timeout(timeout_duration, async {
            while let Ok(sample) = rx_subscriber.recv_async().await {
                let payload = sample.payload().to_bytes();

                if payload.is_empty() {
                    // EOF from backend
                    debug!(request_id = %request_id, "Received EOF from backend");
                    break;
                }

                response_buf.extend_from_slice(&payload);

                // Parse response headers if not done yet
                if body_framing.is_none()
                    && let Ok(Some((hlen, framing))) =
                        crate::http_response_parser::parse_response_headers(&response_buf)
                {
                    header_len = hlen;
                    body_framing = Some(framing);
                }

                // Write to client
                stream.write_all(&payload).await?;

                // Check if response is complete
                if let Some(ref framing) = body_framing {
                    let body_received = response_buf.len().saturating_sub(header_len);
                    match framing {
                        crate::http_response_parser::ResponseBodyFraming::NoBody => {
                            response_complete = true;
                            break;
                        }
                        crate::http_response_parser::ResponseBodyFraming::ContentLength(
                            expected,
                        ) => {
                            if body_received >= *expected {
                                response_complete = true;
                                break;
                            }
                        }
                        crate::http_response_parser::ResponseBodyFraming::Chunked => {
                            if crate::http_response_parser::find_chunked_body_end(
                                &response_buf[header_len..],
                            )
                            .is_some()
                            {
                                response_complete = true;
                                break;
                            }
                        }
                        crate::http_response_parser::ResponseBodyFraming::UntilClose => {
                            // Must wait for EOF signal
                        }
                    }
                }
            }
            Ok::<(), std::io::Error>(())
        })
        .await;

        // Undeclare liveliness to signal export side to disconnect
        if let Err(e) = liveliness_token.undeclare().await {
            debug!(request_id = %request_id, "Error undeclaring liveliness: {:?}", e);
        }

        // Undeclare publisher and subscriber
        if let Err(e) = tx_publisher.undeclare().await {
            debug!(request_id = %request_id, "Error undeclaring publisher: {:?}", e);
        }
        if let Err(e) = rx_subscriber.undeclare().await {
            debug!(request_id = %request_id, "Error undeclaring subscriber: {:?}", e);
        }

        match response_result {
            Ok(Ok(())) => {
                debug!(request_id = %request_id, "Response forwarded ({} bytes)", response_buf.len());
            }
            Ok(Err(e)) => {
                error!(request_id = %request_id, "Error writing response: {}", e);
                break;
            }
            Err(_) => {
                warn!(request_id = %request_id, "Response timeout");
                let _ = stream
                    .write_all(&crate::http_parser::http_504_response())
                    .await;
                break;
            }
        }

        // 5. Check if we should keep the connection alive
        let should_close =
            !response_complete || crate::http_response_parser::is_connection_close(&response_buf);

        if should_close {
            debug!(request_id = %request_id, "Closing persistent connection");
            break;
        }

        debug!(request_id = %request_id, "Request complete, waiting for next request");
    }

    Ok(())
}

/// Check if a backend is available via liveliness query.
async fn check_backend_available(session: &Session, service_key: &str) -> bool {
    match session.liveliness().get(service_key).await {
        Ok(replies) => tokio::time::timeout(Duration::from_millis(1000), async {
            replies.recv_async().await.is_ok()
        })
        .await
        .unwrap_or(false),
        Err(_) => false,
    }
}

/// Run WebSocket import mode for a single service
///
/// This function:
/// 1. Binds a TCP listener on the specified address
/// 2. Accepts incoming connections and upgrades them to WebSocket
/// 3. For each connection, spawns a handler that bridges to Zenoh
pub async fn run_ws_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = parse_import_spec(import_spec)?;

    info!(
        mode = "ws_import",
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting WebSocket import bridge"
    );

    // Start TCP listener
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "WebSocket import bridge ready");

    // Accept connections
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id = format!("wsclient_{}", uuid::Uuid::new_v4().as_simple());
                        info!(client_id = %client_id, remote_addr = %addr, "New WebSocket connection");

                        let session = session.clone();
                        let service_name = service_name.clone();
                        let client_id_clone = client_id.clone();

                        let span = info_span!(
                            "ws_connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                            mode = "websocket"
                        );

                        tokio::spawn(
                            async move {
                                // Perform WebSocket upgrade
                                match tokio_tungstenite::accept_async(stream).await {
                                    Ok(ws_stream) => {
                                        if let Err(e) = handle_ws_import_connection(
                                            session,
                                            ws_stream,
                                            &service_name,
                                            &client_id_clone,
                                        )
                                        .await
                                        {
                                            error!(error = %e, "WebSocket connection error");
                                        }
                                        info!("WebSocket connection closed");
                                    }
                                    Err(e) => {
                                        error!(error = %e, "WebSocket handshake failed");
                                    }
                                }
                            }
                            .instrument(span),
                        );
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "WebSocket import bridge shutting down, no new connections");
                break;
            }
        }
    }

    info!(service = %service_name, "WebSocket import bridge stopped");
    Ok(())
}

/// Handle a single WebSocket import connection
async fn handle_ws_import_connection(
    session: Arc<Session>,
    ws_stream: tokio_tungstenite::WebSocketStream<TcpStream>,
    service_name: &str,
    client_id: &str,
) -> Result<()> {
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Subscribe to responses from the service for this specific client
    let sub_key = format!("{}/rx/{}", service_name, client_id);
    let subscriber = session
        .declare_subscriber(&sub_key)
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default().periodic_queries(Duration::from_millis(500)))
        .subscriber_detection()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe: {:?}", e))?;

    debug!(
        "WebSocket client {}: Subscribed to {} with late publisher detection",
        client_id, sub_key
    );

    // Declare publisher for TX channel
    let pub_key_str = format!("{}/tx/{}", service_name, client_id);
    let pub_key: KeyExpr<'static> = pub_key_str
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("Invalid key expression: {}", e))?;
    let publisher = session
        .declare_publisher(pub_key.clone())
        .cache(CacheConfig::default().max_samples(64))
        .sample_miss_detection(MissDetectionConfig::default().heartbeat(Duration::from_millis(500)))
        .publisher_detection()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare publisher: {}", e))?;

    debug!(
        "WebSocket client {}: Declared AdvancedPublisher on {} with cache",
        client_id, pub_key_str
    );

    // Declare liveliness token - export bridge will detect this and connect
    let liveliness_key = format!("{}/clients/{}", service_name, client_id);
    let liveliness_token = session
        .liveliness()
        .declare_token(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare liveliness: {}", e))?;

    info!(
        "WebSocket client {} declared liveliness: {}",
        client_id, liveliness_key
    );

    let client_id_clone = client_id.to_string();
    let client_id_for_sender = client_id.to_string();

    // Task: Zenoh subscriber -> WebSocket sender
    let mut zenoh_to_ws = tokio::spawn(async move {
        while let Ok(sample) = subscriber.recv_async().await {
            let payload = sample.payload().to_bytes().to_vec();

            // Empty payload is EOF signal from export side
            if payload.is_empty() {
                debug!(
                    "WebSocket client {}: Received EOF signal from Zenoh",
                    client_id_clone
                );
                // Send close frame
                let _ = ws_sender.send(Message::Close(None)).await;
                break;
            }

            debug!(
                "WebSocket client {}: ← {} bytes from Zenoh",
                client_id_clone,
                payload.len()
            );

            if let Err(e) = ws_sender.send(Message::Binary(payload.into())).await {
                error!(
                    "WebSocket client {}: Failed to send: {:?}",
                    client_id_clone, e
                );
                break;
            }
        }

        debug!(
            "WebSocket client {}: Zenoh to WebSocket task completed",
            client_id_clone
        );
        // Explicitly undeclare subscriber before task exits
        if let Err(e) = subscriber.undeclare().await {
            debug!(
                "WebSocket client {}: Error undeclaring subscriber: {:?}",
                client_id_clone, e
            );
        }
    });

    // Task: WebSocket receiver -> Zenoh publisher
    let mut ws_to_zenoh = tokio::spawn(async move {
        while let Some(msg_result) = ws_receiver.next().await {
            match msg_result {
                Ok(msg) => {
                    let data = match msg {
                        Message::Binary(data) => data.to_vec(),
                        Message::Text(text) => text.as_bytes().to_vec(),
                        Message::Close(_) => {
                            debug!(
                                "WebSocket client {}: Connection closed by client",
                                client_id_for_sender
                            );
                            break;
                        }
                        Message::Ping(_) | Message::Pong(_) => continue,
                        Message::Frame(_) => continue,
                    };

                    debug!(
                        "WebSocket client {}: → {} bytes to Zenoh",
                        client_id_for_sender,
                        data.len()
                    );

                    if let Err(e) = publisher.put(&data).await {
                        error!(
                            "WebSocket client {}: Failed to publish to Zenoh: {:?}",
                            client_id_for_sender, e
                        );
                        break;
                    }
                }
                Err(e) => {
                    error!(
                        "WebSocket client {}: Receive error: {:?}",
                        client_id_for_sender, e
                    );
                    break;
                }
            }
        }
        // Explicitly undeclare publisher before task exits
        if let Err(e) = publisher.undeclare().await {
            debug!(
                "WebSocket client {}: Error undeclaring publisher: {:?}",
                client_id_for_sender, e
            );
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = &mut zenoh_to_ws => {
            info!("WebSocket client {}: Zenoh to WebSocket task completed", client_id);
            ws_to_zenoh.abort();
        },
        _ = &mut ws_to_zenoh => {
            info!("WebSocket client {}: WebSocket to Zenoh task completed", client_id);
            zenoh_to_ws.abort();
        },
    }

    // Explicitly undeclare liveliness token
    if let Err(e) = liveliness_token.undeclare().await {
        debug!(
            "WebSocket client {}: Error undeclaring liveliness: {:?}",
            client_id, e
        );
    }

    Ok(())
}

/// Run auto-detecting import mode for a single service.
///
/// This function:
/// 1. Binds a TCP listener
/// 2. Peeks at each incoming connection's first bytes
/// 3. Dispatches to the appropriate handler (TLS, HTTP, WebSocket, or raw TCP)
pub async fn run_auto_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    buffer_size: usize,
    drain_timeout: Duration,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = parse_import_spec(import_spec)?;

    info!(
        mode = "auto_import",
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting auto-detecting import bridge"
    );

    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "Auto-detect import bridge ready");

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());
                        info!(
                            client_id = %client_id,
                            remote_addr = %addr,
                            "New auto-detect connection"
                        );

                        let session = session.clone();
                        let service_name = service_name.clone();

                        let span = info_span!(
                            "auto_connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                        );

                        tokio::spawn(async move {
                            if let Err(e) = handle_auto_import_connection(
                                session, stream, &service_name, &client_id, buffer_size, drain_timeout,
                            ).await {
                                error!(error = %e, "Connection error");
                            }
                            info!("Connection closed");
                        }.instrument(span));
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "Auto-detect import bridge shutting down");
                break;
            }
        }
    }

    info!(service = %service_name, "Auto-detect import bridge stopped");
    Ok(())
}

/// Handle a single auto-detected import connection.
async fn handle_auto_import_connection(
    session: Arc<Session>,
    stream: TcpStream,
    service_name: &str,
    client_id: &str,
    buffer_size: usize,
    drain_timeout: Duration,
) -> Result<()> {
    use crate::protocol_detect::{DetectedProtocol, detect_protocol};

    // Peek at the first bytes to detect the protocol
    let mut peek_buf = vec![0u8; 16];
    let peek_len = stream.peek(&mut peek_buf).await.unwrap_or(0);

    if peek_len == 0 {
        return Err(anyhow::anyhow!("Connection closed before sending any data"));
    }

    let protocol = detect_protocol(&peek_buf[..peek_len]);
    info!(
        client_id = %client_id,
        protocol = ?protocol,
        "Auto-detected protocol"
    );

    match protocol {
        DetectedProtocol::Tls => {
            // Delegate to HTTP mode which already handles TLS detection
            // via is_tls_handshake + parse_tls_client_hello
            handle_import_connection(
                session,
                stream,
                service_name,
                client_id,
                true,
                buffer_size,
                drain_timeout,
            )
            .await
        }
        DetectedProtocol::Http => {
            // Could be regular HTTP or WebSocket upgrade
            handle_auto_http_connection(
                session,
                stream,
                service_name,
                client_id,
                buffer_size,
                drain_timeout,
            )
            .await
        }
        DetectedProtocol::RawTcp => {
            // Raw TCP passthrough — no DNS routing
            handle_import_connection(
                session,
                stream,
                service_name,
                client_id,
                false,
                buffer_size,
                drain_timeout,
            )
            .await
        }
    }
}

/// Handle an auto-detected HTTP connection, which might be a WebSocket upgrade.
async fn handle_auto_http_connection(
    session: Arc<Session>,
    stream: TcpStream,
    service_name: &str,
    client_id: &str,
    buffer_size: usize,
    drain_timeout: Duration,
) -> Result<()> {
    // Peek enough to detect "Upgrade: websocket" in headers
    let mut peek_buf = vec![0u8; 4096];
    let peek_len = stream.peek(&mut peek_buf).await.unwrap_or(0);

    if peek_len > 0 {
        let peek_lower = String::from_utf8_lossy(&peek_buf[..peek_len]).to_lowercase();
        let looks_like_ws =
            peek_lower.contains("upgrade: websocket") || peek_lower.contains("upgrade:websocket");

        if looks_like_ws {
            info!(client_id = %client_id, "Detected WebSocket upgrade request");
            match tokio_tungstenite::accept_async(stream).await {
                Ok(ws_stream) => {
                    return handle_ws_import_connection(
                        session,
                        ws_stream,
                        service_name,
                        client_id,
                    )
                    .await;
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("WebSocket upgrade failed: {}", e));
                }
            }
        }
    }

    // Regular HTTP — delegate to HTTP import handler
    handle_import_connection(
        session,
        stream,
        service_name,
        client_id,
        true,
        buffer_size,
        drain_timeout,
    )
    .await
}

/// Run HTTPS import mode with TLS termination.
///
/// This function:
/// 1. Binds a TCP listener
/// 2. Accepts TLS connections, terminating TLS at the bridge
/// 3. Parses the decrypted HTTP request for Host-based routing
/// 4. Bridges the plaintext data over Zenoh
///
/// Unlike `run_http_import_mode`, the backend receives plaintext HTTP —
/// all TLS is handled at the import bridge.
#[cfg(feature = "tls-termination")]
pub async fn run_https_terminate_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    tls_config: Arc<rustls::ServerConfig>,
    buffer_size: usize,
    drain_timeout: Duration,
    shutdown_token: CancellationToken,
) -> Result<()> {
    use tokio_rustls::TlsAcceptor;

    let (service_name, listen_addr) = parse_import_spec(import_spec)?;
    let tls_acceptor = TlsAcceptor::from(tls_config);

    info!(
        mode = "https_terminate",
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting HTTPS termination import bridge"
    );

    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "HTTPS termination import bridge ready");

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((tcp_stream, addr)) => {
                        let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());
                        info!(
                            client_id = %client_id,
                            remote_addr = %addr,
                            "New TLS connection"
                        );

                        let session = session.clone();
                        let service_name = service_name.clone();
                        let tls_acceptor = tls_acceptor.clone();

                        let span = info_span!(
                            "tls_connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                        );

                        tokio::spawn(async move {
                            // Perform TLS handshake
                            match tls_acceptor.accept(tcp_stream).await {
                                Ok(tls_stream) => {
                                    if let Err(e) = handle_tls_terminated_connection(
                                        session,
                                        tls_stream,
                                        &service_name,
                                        &client_id,
                                        buffer_size,
                                        drain_timeout,
                                    ).await {
                                        error!(error = %e, "TLS connection error");
                                    }
                                    info!("TLS connection closed");
                                }
                                Err(e) => {
                                    error!(error = %e, "TLS handshake failed");
                                }
                            }
                        }.instrument(span));
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "HTTPS terminate bridge shutting down");
                break;
            }
        }
    }

    info!(service = %service_name, "HTTPS terminate bridge stopped");
    Ok(())
}

/// Handle a single TLS-terminated connection.
///
/// After TLS termination, the decrypted stream is plain HTTP. Parse it for
/// Host-based routing, then bridge identically to the existing HTTP import.
#[cfg(feature = "tls-termination")]
async fn handle_tls_terminated_connection(
    session: Arc<Session>,
    tls_stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    service_name: &str,
    client_id: &str,
    buffer_size: usize,
    drain_timeout: Duration,
) -> Result<()> {
    let (mut tls_reader, tls_writer) = tokio::io::split(tls_stream);

    // After TLS termination, we have plaintext HTTP. Parse Host header for routing.
    let parsed = parse_http_request(&mut tls_reader).await.map_err(|e| {
        anyhow::anyhow!("Failed to parse HTTP request after TLS termination: {}", e)
    })?;

    let dns = parsed.dns.clone();
    let dns_suffix = format!("/{}", dns);
    info!(
        "Client {}: TLS-terminated HTTP routing to DNS: {}",
        client_id, dns
    );

    // Check backend availability
    let service_key = format!("{}/{}/available", service_name, dns);
    let liveliness_replies = session
        .liveliness()
        .get(&service_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query liveliness: {}", e))?;

    let backend_available = tokio::time::timeout(Duration::from_millis(1000), async {
        liveliness_replies.recv_async().await.is_ok()
    })
    .await
    .unwrap_or(false);

    if !backend_available {
        warn!("Client {}: No backend for DNS: {}", client_id, dns);
        return Err(anyhow::anyhow!("No backend available for DNS: {}", dns));
    }

    info!("Client {}: Backend available for DNS: {}", client_id, dns);

    // Bridge the decrypted connection through Zenoh
    // The tls_writer half comes from the split TLS stream
    bridge_import_connection(
        session,
        tls_reader,
        tls_writer,
        service_name,
        client_id,
        &dns_suffix,
        Some(parsed.buffer),
        buffer_size,
        drain_timeout,
    )
    .await
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

    #[test]
    fn test_parse_import_spec_empty_service_name() {
        let result = parse_import_spec("/127.0.0.1:8080");
        assert!(result.is_ok());
        let (service, _) = result.unwrap();
        assert_eq!(service, "");
    }

    #[test]
    fn test_parse_import_spec_empty_string() {
        let result = parse_import_spec("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_import_spec_nested_service_name() {
        let result = parse_import_spec("my/nested/service/127.0.0.1:8080");
        assert!(
            result.is_err(),
            "Nested service names should be rejected by spec parser"
        );
    }

    #[test]
    fn test_parse_import_spec_ipv4_all_interfaces() {
        let result = parse_import_spec("myservice/0.0.0.0:8080");
        assert!(result.is_ok());
        let (_, addr) = result.unwrap();
        assert_eq!(addr.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn test_client_ids_are_unique() {
        let id1 = format!("client_{}", uuid::Uuid::new_v4().as_simple());
        let id2 = format!("client_{}", uuid::Uuid::new_v4().as_simple());
        assert_ne!(id1, id2);
        // Verify format is valid for Zenoh key expressions (no slashes, wildcards)
        assert!(!id1.contains('/'));
        assert!(!id1.contains('*'));
        assert!(!id1.contains('?'));
    }
}
