//! Export mode implementation for the Zenoh TCP Bridge.
//!
//! This module handles exporting TCP backend services as Zenoh services.
//! Each export creates lazy connections to the backend - one connection per importing client.
//!
//! Supports regular TCP mode, HTTP-aware mode with DNS-based routing, and WebSocket mode.
//!
//! ## Error handling strategy
//!
//! - **Startup errors** (parse, liveliness, subscribe): propagated via `?`, fatal to the service.
//! - **Per-client errors** (backend connect, read/write, publish): logged, close that connection
//!   only. Backend connect uses exponential backoff (100ms–5s, 5 retries) before notifying the
//!   import side via the error channel.
//! - **Shutdown**: each task direction has a `CancellationToken`; the outer select cancels the
//!   peer token and waits up to `drain_timeout` before a final `.abort()` fallback.

use crate::config::BridgeConfig;
use crate::http_parser::normalize_dns;
use anyhow::Result;
use backon::{ExponentialBuilder, Retryable};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::connect_async;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
use zenoh_ext::{
    AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt, CacheConfig, HistoryConfig,
    MissDetectionConfig, RecoveryConfig,
};

/// Type alias for cancellation sender and task handle
type CancellationSender = (mpsc::Sender<()>, tokio::task::JoinHandle<()>);

/// Backend type for export mode, determines how client connections are established
enum ExportBackend {
    Tcp {
        addr: SocketAddr,
        dns_suffix: Option<String>,
    },
    WebSocket(String),
}

/// Parse export specification in format 'service_name/backend_addr'
pub fn parse_export_spec(export_spec: &str) -> Result<(String, SocketAddr)> {
    let parts: Vec<&str> = export_spec.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!(
            "Invalid export format. Expected: 'service_name/backend_addr' (e.g., 'myservice/127.0.0.1:8003')"
        ));
    }

    let service_name = parts[0].to_string();
    let backend_addr: SocketAddr = parts[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid backend address: {}", e))?;

    Ok((service_name, backend_addr))
}

/// Parse HTTP export specification in format 'service_name/dns/backend_addr'
pub fn parse_http_export_spec(export_spec: &str) -> Result<(String, String, SocketAddr)> {
    let parts: Vec<&str> = export_spec.split('/').collect();
    if parts.len() != 3 {
        return Err(anyhow::anyhow!(
            "Invalid HTTP export format. Expected: 'service_name/dns/backend_addr' (e.g., 'http-service/api.example.com/127.0.0.1:8003')"
        ));
    }

    let service_name = parts[0].to_string();
    let dns = normalize_dns(parts[1]);
    let backend_addr: SocketAddr = parts[2]
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid backend address: {}", e))?;

    Ok((service_name, dns, backend_addr))
}

/// Run export mode for a single service
///
/// This function:
/// 1. Monitors client liveliness tokens
/// 2. Creates a backend connection when a client appears
/// 3. Bridges data between the backend and Zenoh
/// 4. Cleans up when clients disconnect
pub async fn run_export_mode(
    session: Arc<Session>,
    export_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, backend_addr) = parse_export_spec(export_spec)?;
    run_export_loop(
        session,
        &service_name,
        ExportBackend::Tcp {
            addr: backend_addr,
            dns_suffix: None,
        },
        config,
        shutdown_token,
    )
    .await
}

/// Run HTTP-aware export mode for a single service with DNS-based routing
///
/// This function:
/// 1. Registers a backend for a specific DNS name
/// 2. Monitors client liveliness tokens for that DNS
/// 3. Creates backend connections when clients appear
/// 4. Bridges data between the backend and Zenoh
/// 5. Cleans up when clients disconnect
pub async fn run_http_export_mode(
    session: Arc<Session>,
    export_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, dns, backend_addr) = parse_http_export_spec(export_spec)?;
    run_export_loop(
        session,
        &service_name,
        ExportBackend::Tcp {
            addr: backend_addr,
            dns_suffix: Some(dns),
        },
        config,
        shutdown_token,
    )
    .await
}

/// Unified export loop for TCP, HTTP, and WebSocket backends
///
/// This function handles the liveliness monitoring loop shared by all export modes.
/// The `backend` parameter determines how client connections are established.
async fn run_export_loop(
    session: Arc<Session>,
    service_name: &str,
    backend: ExportBackend,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let dns_suffix = match &backend {
        ExportBackend::Tcp { dns_suffix, .. } => dns_suffix.clone(),
        ExportBackend::WebSocket(_) => None,
    };

    let mode = match &backend {
        ExportBackend::Tcp {
            dns_suffix: Some(_),
            ..
        } => "http_export",
        ExportBackend::Tcp { .. } => "export",
        ExportBackend::WebSocket(_) => "ws_export",
    };

    info!(
        mode = mode,
        service = %service_name,
        "Starting export bridge"
    );

    // Monitor client liveliness to create/destroy connections
    let liveliness_key = if let Some(ref dns) = dns_suffix {
        format!("{}/{}/clients/*", service_name, dns)
    } else {
        format!("{}/clients/*", service_name)
    };

    // Declare service availability liveliness token for HTTP mode
    let _service_liveliness = if let Some(ref dns) = dns_suffix {
        let service_key = format!("{}/{}/available", service_name, dns);
        let token = session
            .liveliness()
            .declare_token(&service_key)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to declare service liveliness: {}", e))?;
        debug!(service_key = %service_key, "Declared service availability");
        Some(token)
    } else {
        None
    };

    let liveliness_subscriber = session
        .liveliness()
        .declare_subscriber(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe to liveliness: {}", e))?;

    info!(liveliness_key = %liveliness_key, "Export bridge ready");

    // Track connection tasks and cancellation senders per client ID
    let cancellation_senders: Arc<Mutex<HashMap<String, CancellationSender>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Query existing clients that connected before this export started
    let existing_clients = session
        .liveliness()
        .get(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query existing clients: {}", e))?;

    while let Ok(reply) = existing_clients.recv_async().await {
        if let Ok(sample) = reply.into_result() {
            let key = sample.key_expr().as_str();
            if let Some(client_id) = key.rsplit('/').next() {
                let client_id = client_id.to_string();
                info!(client_id = %client_id, "Found existing client, connecting");
                dispatch_client_connect(
                    &session,
                    service_name,
                    &backend,
                    &client_id,
                    &cancellation_senders,
                    &config,
                )
                .await;
            }
        }
    }

    // Main loop: monitor liveliness and create/destroy connections
    loop {
        tokio::select! {
            result = liveliness_subscriber.recv_async() => {
                match result {
                    Ok(sample) => {
                        let key = sample.key_expr().as_str();
                        if let Some(client_id) = key.rsplit('/').next() {
                            let client_id = client_id.to_string();

                            match sample.kind() {
                                zenoh::sample::SampleKind::Put => {
                                    dispatch_client_connect(
                                        &session,
                                        service_name,
                                        &backend,
                                        &client_id,
                                        &cancellation_senders,
                                        &config,
                                    )
                                    .await;
                                }
                                zenoh::sample::SampleKind::Delete => {
                                    handle_client_disconnect(&client_id, &cancellation_senders, config.drain_timeout).await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Liveliness subscriber error (continuing): {:?}", e);
                        continue;
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "Export bridge shutting down");
                let senders = cancellation_senders.lock().await;
                for (client_id, (tx, _)) in senders.iter() {
                    let _ = tx.send(()).await;
                    debug!(client_id = %client_id, "Sent shutdown to client bridge");
                }
                break;
            }
        }
    }

    info!(service = %service_name, "Export bridge stopped");
    Ok(())
}

/// Dispatch a client connection to the appropriate backend handler
async fn dispatch_client_connect(
    session: &Arc<Session>,
    service_name: &str,
    backend: &ExportBackend,
    client_id: &str,
    cancellation_senders: &Arc<Mutex<HashMap<String, CancellationSender>>>,
    config: &Arc<BridgeConfig>,
) {
    match backend {
        ExportBackend::Tcp { addr, dns_suffix } => {
            handle_client_connect(
                session,
                service_name,
                *addr,
                client_id,
                cancellation_senders,
                dns_suffix.as_deref(),
                config,
            )
            .await;
        }
        ExportBackend::WebSocket(ws_url) => {
            handle_ws_client_connect(
                session,
                service_name,
                ws_url,
                client_id,
                cancellation_senders,
                config,
            )
            .await;
        }
    }
}

/// Handle a client connection event
async fn handle_client_connect(
    session: &Arc<Session>,
    service_name: &str,
    backend_addr: SocketAddr,
    client_id: &str,
    cancellation_senders: &Arc<Mutex<HashMap<String, CancellationSender>>>,
    dns_suffix: Option<&str>,
    config: &Arc<BridgeConfig>,
) {
    info!(client_id = %client_id, "Client connected, connecting to backend");

    // Retry backend connection with exponential backoff
    let client_id_for_log = client_id.to_string();
    let connect_result = (|| async { TcpStream::connect(backend_addr).await })
        .retry(
            ExponentialBuilder::default()
                .with_min_delay(Duration::from_millis(100))
                .with_max_delay(Duration::from_secs(5))
                .with_max_times(5),
        )
        .notify(move |err, dur| {
            warn!(
                client_id = %client_id_for_log,
                backend = %backend_addr,
                error = %err,
                retry_in = ?dur,
                "Backend connection failed, retrying"
            );
        })
        .await;

    match connect_result {
        Ok(backend_stream) => {
            info!(client_id = %client_id, backend = %backend_addr, "Backend connection established");

            let (backend_reader, backend_writer) = backend_stream.into_split();
            let reader = crate::transport::TcpReader::new(backend_reader, config.buffer_size);
            let writer = crate::transport::TcpWriter::new(backend_writer);

            let session_clone = session.clone();
            let service_name_clone = service_name.to_string();
            let client_id_str = client_id.to_string();
            let client_id_for_map = client_id.to_string();
            let dns_suffix_owned = dns_suffix.map(|s| s.to_string());
            let config = config.clone();

            // Create cancellation channel for graceful shutdown
            let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);

            let span = info_span!(
                "client_bridge",
                client_id = %client_id,
                service = %service_name,
                backend = %backend_addr,
                dns = dns_suffix.unwrap_or("-")
            );

            // Spawn dedicated task for this client connection
            let config_clone = config.clone();
            let main_handle = tokio::spawn(
                async move {
                    if let Err(e) = handle_client_bridge(
                        session_clone,
                        service_name_clone,
                        client_id_str,
                        reader,
                        writer,
                        cancel_rx,
                        dns_suffix_owned.as_deref(),
                        config_clone,
                    )
                    .await
                    {
                        error!(error = %e, "Client bridge error");
                    }
                }
                .instrument(span),
            );

            // Cancel any existing connection for this client ID before storing the new one
            {
                let mut senders = cancellation_senders.lock().await;
                if let Some((old_cancel_tx, old_handle)) = senders.remove(&client_id_for_map) {
                    warn!(client_id = %client_id, "Client already has active connection, cancelling old one");
                    drop(old_cancel_tx);
                    let _ = tokio::time::timeout(config.drain_timeout, old_handle).await;
                }
                senders.insert(client_id_for_map, (cancel_tx, main_handle));
            }
        }
        Err(e) => {
            error!(
                "Failed to connect to backend after retries for client {}: {:?}",
                client_id, e
            );

            // Publish error signal to notify import bridge
            let dns_part = dns_suffix.map(|d| format!("/{}", d)).unwrap_or_default();
            let error_key = format!("{}{}/error/{}", service_name, dns_part, client_id);
            if let Err(pub_err) = session.put(&error_key, "backend_unavailable").await {
                error!("Failed to publish error signal: {:?}", pub_err);
            }
            info!("Sent backend unavailable signal for client: {}", client_id);
        }
    }
}

/// Handle the bridge logic for a single client connection.
///
/// Generic over `TransportReader`/`TransportWriter` so the same function
/// serves both TCP and WebSocket export paths.
#[allow(clippy::too_many_arguments)]
async fn handle_client_bridge<R, W>(
    session: Arc<Session>,
    service_name: String,
    client_id: String,
    mut backend_reader: R,
    mut backend_writer: W,
    mut cancel_rx: mpsc::Receiver<()>,
    dns_suffix: Option<&str>,
    config: Arc<BridgeConfig>,
) -> Result<()>
where
    R: crate::transport::TransportReader,
    W: crate::transport::TransportWriter,
{
    let dns_part = dns_suffix.map(|d| format!("/{}", d)).unwrap_or_default();
    // Subscribe to messages from this specific client using AdvancedSubscriber
    // This enables late publisher detection and recovery of missed samples
    let sub_key = format!("{}{}/tx/{}", service_name, dns_part, client_id);
    let subscriber = session
        .declare_subscriber(&sub_key)
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default().periodic_queries(config.heartbeat_interval))
        .subscriber_detection()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe: {:?}", e))?;

    info!(
        "Client {} subscribed to {} with late publisher detection",
        client_id, sub_key
    );

    // Declare AdvancedPublisher with cache and publisher detection for RX channel
    // This allows the import bridge to detect when we're ready and recover any missed samples
    let pub_key_str = format!("{}{}/rx/{}", service_name, dns_part, client_id);
    let pub_key: KeyExpr<'static> = pub_key_str
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("Invalid key expression: {}", e))?;
    let publisher = session
        .declare_publisher(pub_key.clone())
        .cache(CacheConfig::default().max_samples(64))
        .sample_miss_detection(MissDetectionConfig::default().heartbeat(config.heartbeat_interval))
        .publisher_detection()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare publisher: {}", e))?;

    debug!(
        "Client {}: Declared AdvancedPublisher on {} with cache",
        client_id, pub_key_str
    );

    let client_id_for_reader = client_id.clone();
    let client_id_for_writer = client_id.clone();
    let client_id_for_final = client_id.clone();

    // Cancellation tokens for graceful shutdown of each direction
    let cancel_backend_to_zenoh = CancellationToken::new();
    let cancel_zenoh_to_backend = CancellationToken::new();

    // Task: read from backend and publish to Zenoh using AdvancedPublisher
    let buffer_size = config.buffer_size;
    let drain_timeout = config.drain_timeout;
    let b2z_token = cancel_backend_to_zenoh.clone();
    let mut backend_to_zenoh_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = backend_reader.read_data(buffer_size) => {
                    match result {
                        Ok(data) if data.is_empty() => {
                            info!("Backend closed connection for client: {}", client_id_for_reader);
                            if let Err(e) = publisher.put(Vec::<u8>::new()).await {
                                error!("Failed to send EOF signal for client {}: {:?}", client_id_for_reader, e);
                            }
                            break;
                        }
                        Ok(data) => {
                            debug!("← {} bytes from backend for client {}", data.len(), client_id_for_reader);
                            if let Err(e) = publisher.put(&data[..]).await {
                                error!("Failed to publish for client {}: {:?}", client_id_for_reader, e);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Backend read error for client {}: {:?}", client_id_for_reader, e);
                            break;
                        }
                    }
                }
                _ = b2z_token.cancelled() => {
                    debug!("Backend-to-Zenoh cancelled for client: {}", client_id_for_reader);
                    break;
                }
            }
        }
        if let Err(e) = publisher.undeclare().await {
            debug!("Error undeclaring publisher for {}: {:?}", client_id_for_reader, e);
        }
    });

    // Task: receive from Zenoh and write to backend
    let z2b_token = cancel_zenoh_to_backend.clone();
    let mut zenoh_to_backend_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = subscriber.recv_async() => {
                    match result {
                        Ok(sample) => {
                            let payload = sample.payload().to_bytes();
                            debug!("→ {} bytes to backend for client {}", payload.len(), client_id_for_writer);
                            if let Err(e) = backend_writer.write_data(&payload).await {
                                error!("Failed to write to backend for client {}: {:?}", client_id_for_writer, e);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Subscriber error for client {}: {:?}", client_id_for_writer, e);
                            break;
                        }
                    }
                }
                _ = z2b_token.cancelled() => {
                    debug!("Zenoh-to-backend cancelled for client: {}", client_id_for_writer);
                    break;
                }
            }
        }
        if let Err(e) = subscriber.undeclare().await {
            debug!("Error undeclaring subscriber for {}: {:?}", client_id_for_writer, e);
        }
    });

    // Wait for either task to complete or cancellation signal
    tokio::select! {
        _result = &mut backend_to_zenoh_handle => {
            info!("Backend closed for client: {}", client_id_for_final);
            cancel_zenoh_to_backend.cancel();
            match tokio::time::timeout(drain_timeout, &mut zenoh_to_backend_handle).await {
                Ok(_) => {}
                Err(_) => {
                    debug!("Zenoh-to-backend drain timeout for client: {}", client_id_for_final);
                    zenoh_to_backend_handle.abort();
                    let _ = zenoh_to_backend_handle.await;
                }
            }
        },
        _result = &mut zenoh_to_backend_handle => {
            info!("Zenoh closed for client: {}", client_id_for_final);
            cancel_backend_to_zenoh.cancel();
            match tokio::time::timeout(drain_timeout, &mut backend_to_zenoh_handle).await {
                Ok(_) => {}
                Err(_) => {
                    debug!("Backend-to-Zenoh drain timeout for client: {}", client_id_for_final);
                    backend_to_zenoh_handle.abort();
                    let _ = backend_to_zenoh_handle.await;
                }
            }
        },
        _ = cancel_rx.recv() => {
            info!("Cancellation received for client: {}", client_id_for_final);
            cancel_zenoh_to_backend.cancel();
            cancel_backend_to_zenoh.cancel();
            match tokio::time::timeout(drain_timeout, &mut backend_to_zenoh_handle).await {
                Ok(_) => {
                    debug!("Backend-to-Zenoh drained for client: {}", client_id_for_final);
                }
                Err(_) => {
                    debug!("Backend-to-Zenoh drain timeout for client: {}", client_id_for_final);
                    backend_to_zenoh_handle.abort();
                    let _ = backend_to_zenoh_handle.await;
                }
            }
            match tokio::time::timeout(drain_timeout, &mut zenoh_to_backend_handle).await {
                Ok(_) => {
                    debug!("Zenoh-to-backend drained for client: {}", client_id_for_final);
                }
                Err(_) => {
                    debug!("Zenoh-to-backend drain timeout for client: {}", client_id_for_final);
                    zenoh_to_backend_handle.abort();
                    let _ = zenoh_to_backend_handle.await;
                }
            }
        },
    }

    info!(
        "Connection handler stopped for client: {}",
        client_id_for_final
    );

    Ok(())
}

/// Handle a client disconnection event
async fn handle_client_disconnect(
    client_id: &str,
    cancellation_senders: &Arc<Mutex<HashMap<String, CancellationSender>>>,
    drain_timeout: Duration,
) {
    info!("Client disconnected: {}", client_id);

    // Send cancellation signal and wait for task to complete
    if let Some((cancel_tx, task_handle)) = cancellation_senders.lock().await.remove(client_id) {
        // Send cancellation signal (ignore error if receiver already dropped)
        let _ = cancel_tx.send(()).await;
        info!(
            "  Sent shutdown signal to backend connection for: {}",
            client_id
        );

        // Wait for the task to drain and complete with a timeout
        match tokio::time::timeout(drain_timeout, task_handle).await {
            Ok(Ok(())) => {
                info!("  Backend connection drained and closed for: {}", client_id);
            }
            Ok(Err(e)) => {
                warn!(
                    "  Backend connection task error during drain for {}: {:?}",
                    client_id, e
                );
            }
            Err(_) => {
                warn!("  Drain timeout for backend connection: {}", client_id);
            }
        }
    }
}

/// Parse WebSocket export specification in format 'service_name/ws_url'
///
/// The ws_url should be a full WebSocket URL like 'ws://127.0.0.1:9000' or 'wss://example.com:443'
pub fn parse_ws_export_spec(export_spec: &str) -> Result<(String, String)> {
    // Split on first '/' only to preserve the ws:// or wss:// in the URL
    let slash_pos = export_spec
        .find('/')
        .ok_or_else(|| anyhow::anyhow!(
            "Invalid WebSocket export format. Expected: 'service_name/ws://host:port' (e.g., 'myws/ws://127.0.0.1:9000')"
        ))?;

    let service_name = export_spec[..slash_pos].to_string();
    let ws_url = export_spec[slash_pos + 1..].to_string();

    // Validate that the URL starts with ws:// or wss://
    if !ws_url.starts_with("ws://") && !ws_url.starts_with("wss://") {
        return Err(anyhow::anyhow!(
            "Invalid WebSocket URL: must start with 'ws://' or 'wss://'"
        ));
    }

    if service_name.is_empty() {
        return Err(anyhow::anyhow!("Service name cannot be empty"));
    }

    Ok((service_name, ws_url))
}

/// Run WebSocket export mode for a single service
///
/// This function:
/// 1. Monitors client liveliness tokens
/// 2. Creates a WebSocket connection to the backend when a client appears
/// 3. Bridges data between the WebSocket backend and Zenoh
/// 4. Cleans up when clients disconnect
pub async fn run_ws_export_mode(
    session: Arc<Session>,
    export_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, ws_url) = parse_ws_export_spec(export_spec)?;
    run_export_loop(
        session,
        &service_name,
        ExportBackend::WebSocket(ws_url),
        config,
        shutdown_token,
    )
    .await
}

/// Handle a WebSocket client connection event
async fn handle_ws_client_connect(
    session: &Arc<Session>,
    service_name: &str,
    ws_url: &str,
    client_id: &str,
    cancellation_senders: &Arc<Mutex<HashMap<String, CancellationSender>>>,
    config: &Arc<BridgeConfig>,
) {
    info!(client_id = %client_id, "WebSocket client connected, connecting to backend");

    // Retry WebSocket backend connection with exponential backoff
    let ws_url_owned = ws_url.to_string();
    let client_id_for_log = client_id.to_string();
    let ws_url_for_log = ws_url.to_string();
    let connect_result = (|| {
        let url = ws_url_owned.clone();
        async move { connect_async(&url).await }
    })
    .retry(
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(100))
            .with_max_delay(Duration::from_secs(5))
            .with_max_times(5),
    )
    .notify(move |err, dur| {
        warn!(
            client_id = %client_id_for_log,
            ws_url = %ws_url_for_log,
            error = %err,
            retry_in = ?dur,
            "WebSocket backend connection failed, retrying"
        );
    })
    .await;

    match connect_result {
        Ok((ws_stream, _response)) => {
            info!(client_id = %client_id, ws_url = %ws_url, "WebSocket backend connection established");

            let (ws_sender, ws_receiver) = ws_stream.split();
            let reader = crate::transport::WsReader::new(ws_receiver);
            let writer = crate::transport::WsWriter::new(ws_sender);

            let session_clone = session.clone();
            let service_name_clone = service_name.to_string();
            let client_id_str = client_id.to_string();
            let client_id_for_map = client_id.to_string();

            // Create cancellation channel for graceful shutdown
            let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);

            let span = info_span!(
                "ws_client_bridge",
                client_id = %client_id,
                service = %service_name,
                ws_url = %ws_url
            );

            // Spawn dedicated task for this client connection
            let config_clone = config.clone();
            let main_handle = tokio::spawn(
                async move {
                    if let Err(e) = handle_client_bridge(
                        session_clone,
                        service_name_clone,
                        client_id_str,
                        reader,
                        writer,
                        cancel_rx,
                        None,
                        config_clone,
                    )
                    .await
                    {
                        error!(error = %e, "WebSocket client bridge error");
                    }
                }
                .instrument(span),
            );

            // Cancel any existing connection for this client ID before storing the new one
            {
                let mut senders = cancellation_senders.lock().await;
                if let Some((old_cancel_tx, old_handle)) = senders.remove(&client_id_for_map) {
                    warn!(client_id = %client_id, "WS client already has active connection, cancelling old one");
                    drop(old_cancel_tx);
                    let _ = tokio::time::timeout(config.drain_timeout, old_handle).await;
                }
                senders.insert(client_id_for_map, (cancel_tx, main_handle));
            }
        }
        Err(e) => {
            error!(
                "Failed to connect to WebSocket backend after retries for client {}: {:?}",
                client_id, e
            );

            // Publish error signal to notify import bridge
            let error_key = format!("{}/error/{}", service_name, client_id);
            if let Err(pub_err) = session.put(&error_key, "backend_unavailable").await {
                error!("Failed to publish error signal: {:?}", pub_err);
            }
            info!("Sent backend unavailable signal for client: {}", client_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_export_spec_valid() {
        let result = parse_export_spec("myservice/127.0.0.1:8080");
        assert!(result.is_ok());
        let (service, addr) = result.unwrap();
        assert_eq!(service, "myservice");
        assert_eq!(addr.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn test_parse_export_spec_invalid_format() {
        let result = parse_export_spec("invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_export_spec_invalid_addr() {
        let result = parse_export_spec("myservice/invalid:addr");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_export_spec_too_many_parts() {
        let result = parse_export_spec("service/addr/extra");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_http_export_spec_valid() {
        let result = parse_http_export_spec("http-service/api.example.com/127.0.0.1:8080");
        assert!(result.is_ok());
        let (service, dns, addr) = result.unwrap();
        assert_eq!(service, "http-service");
        assert_eq!(dns, "api.example.com");
        assert_eq!(addr.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn test_parse_http_export_spec_dns_normalization() {
        let result = parse_http_export_spec("http-service/Example.COM:80/127.0.0.1:8080");
        assert!(result.is_ok());
        let (service, dns, addr) = result.unwrap();
        assert_eq!(service, "http-service");
        assert_eq!(dns, "example.com"); // Normalized: lowercase + port 80 stripped
        assert_eq!(addr.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn test_parse_http_export_spec_invalid_format() {
        let result = parse_http_export_spec("invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_http_export_spec_missing_dns() {
        let result = parse_http_export_spec("service/127.0.0.1:8080");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_http_export_spec_invalid_addr() {
        let result = parse_http_export_spec("service/example.com/invalid:addr");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ws_export_spec_valid() {
        let result = parse_ws_export_spec("myws/ws://127.0.0.1:9000");
        assert!(result.is_ok());
        let (service, url) = result.unwrap();
        assert_eq!(service, "myws");
        assert_eq!(url, "ws://127.0.0.1:9000");
    }

    #[test]
    fn test_parse_ws_export_spec_wss() {
        let result = parse_ws_export_spec("secure-ws/wss://example.com:443/path");
        assert!(result.is_ok());
        let (service, url) = result.unwrap();
        assert_eq!(service, "secure-ws");
        assert_eq!(url, "wss://example.com:443/path");
    }

    #[test]
    fn test_parse_ws_export_spec_invalid_no_slash() {
        let result = parse_ws_export_spec("invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ws_export_spec_invalid_not_ws() {
        let result = parse_ws_export_spec("myws/http://example.com");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ws_export_spec_empty_service() {
        let result = parse_ws_export_spec("/ws://example.com");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_export_spec_empty_service_name() {
        let result = parse_export_spec("/127.0.0.1:8080");
        // Empty service name should succeed at parsing but is an edge case
        assert!(result.is_ok());
        let (service, _) = result.unwrap();
        assert_eq!(service, "");
    }

    #[test]
    fn test_parse_export_spec_empty_string() {
        let result = parse_export_spec("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_export_spec_nested_service_name() {
        // "my/nested/service/127.0.0.1:8080" has 4 parts, should fail
        let result = parse_export_spec("my/nested/service/127.0.0.1:8080");
        assert!(
            result.is_err(),
            "Nested service names should be rejected by spec parser"
        );
    }

    #[test]
    fn test_parse_export_spec_with_wildcards() {
        // Zenoh wildcards in service names: parsing succeeds but usage would be invalid
        let result = parse_export_spec("my*service/127.0.0.1:8080");
        assert!(
            result.is_ok(),
            "Parser accepts wildcards (validation is at Zenoh level)"
        );
    }

    #[test]
    fn test_parse_http_export_spec_dns_port_443_stripped() {
        let result = parse_http_export_spec("svc/example.com:443/127.0.0.1:8080");
        assert!(result.is_ok());
        let (_, dns, _) = result.unwrap();
        assert_eq!(dns, "example.com", "Port 443 should be stripped from DNS");
    }

    #[test]
    fn test_parse_http_export_spec_dns_custom_port_kept() {
        let result = parse_http_export_spec("svc/example.com:8443/127.0.0.1:8080");
        assert!(result.is_ok());
        let (_, dns, _) = result.unwrap();
        assert_eq!(dns, "example.com:8443", "Non-standard ports should be kept");
    }

    #[test]
    fn test_parse_ws_export_spec_with_path() {
        let result = parse_ws_export_spec("myws/ws://127.0.0.1:9000/path/to/ws");
        assert!(result.is_ok());
        let (service, url) = result.unwrap();
        assert_eq!(service, "myws");
        assert_eq!(url, "ws://127.0.0.1:9000/path/to/ws");
    }

    #[tokio::test]
    async fn test_cancellation_token_propagation() {
        let parent = CancellationToken::new();
        let child = parent.child_token();

        let task = tokio::spawn({
            let child = child.clone();
            async move {
                child.cancelled().await;
                true
            }
        });

        // Child should not be cancelled yet
        assert!(!child.is_cancelled());

        // Cancel parent
        parent.cancel();

        // Child task should complete
        let result = tokio::time::timeout(Duration::from_secs(1), task).await;
        assert!(result.is_ok());
        assert!(result.unwrap().unwrap());
    }
}
