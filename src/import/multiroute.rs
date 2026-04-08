use crate::config::BridgeConfig;
use crate::http_parser::{http_400_response, parse_http_request};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
use zenoh_ext::{
    AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt, CacheConfig, HistoryConfig,
    MissDetectionConfig, RecoveryConfig,
};

/// Run HTTP import mode with per-request routing.
///
/// Each HTTP request on a persistent connection is independently routed
/// based on its Host header. This allows a single client TCP connection
/// to reach multiple backends on a single port.
pub(super) async fn run_http_multiroute_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = super::parse_import_spec(import_spec)?;

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

    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());
                        let session = session.clone();
                        let service_name = service_name.clone();
                        let config = config.clone();

                        let span = info_span!(
                            "http_multiroute",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                        );

                        tasks.spawn(async move {
                            if let Err(e) = handle_multiroute_connection(
                                session, stream, &service_name, &client_id,
                                config,
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

        // Reap completed tasks
        while tasks.try_join_next().is_some() {}
    }

    super::drain_tasks(&mut tasks, &service_name, config.drain_timeout).await;

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
async fn handle_multiroute_connection(
    session: Arc<Session>,
    mut stream: TcpStream,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
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
                MissDetectionConfig::default().heartbeat(config.heartbeat_interval),
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
        let mut response_buf = Vec::with_capacity(config.buffer_size);
        let mut body_framing: Option<crate::http_response_parser::ResponseBodyFraming> = None;
        let mut header_len: usize = 0;
        let mut response_complete = false;
        let mut bytes_written: usize = 0;

        let timeout_duration = Duration::from_secs(30).max(config.drain_timeout);
        let response_result = tokio::time::timeout(timeout_duration, async {
            while let Ok(sample) = rx_subscriber.recv_async().await {
                let payload = sample.payload().to_bytes();

                if payload.is_empty() {
                    // EOF from backend
                    debug!(request_id = %request_id, "Received EOF from backend");
                    break;
                }

                response_buf.extend_from_slice(&payload);

                if response_buf.len() > config.max_response_size {
                    warn!(request_id = %request_id, "Response exceeded max size ({} bytes)", config.max_response_size);
                    if bytes_written == 0 {
                        let error_resp = crate::http_parser::http_502_response("Response too large");
                        let _ = stream.write_all(&error_resp).await;
                    }
                    return Err(std::io::Error::other("response too large"));
                }

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
                bytes_written += payload.len();

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

        // Publish EOF so export side knows to clean up before we undeclare
        let _ = tx_publisher.put(Vec::<u8>::new()).await;

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
                // Only send error response if no data has been written yet;
                // writing a second HTTP response mid-stream would corrupt the connection.
                if bytes_written == 0 {
                    let _ = stream
                        .write_all(&crate::http_parser::http_504_response())
                        .await;
                }
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
pub(super) async fn check_backend_available(session: &Session, service_key: &str) -> bool {
    match session.liveliness().get(service_key).await {
        Ok(replies) => tokio::time::timeout(Duration::from_millis(1000), async {
            replies.recv_async().await.is_ok()
        })
        .await
        .unwrap_or(false),
        Err(_) => false,
    }
}
