use super::{CancellationSender, ExportBackend};
use crate::config::BridgeConfig;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
use zenoh_ext::{
    AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt, CacheConfig, HistoryConfig,
    MissDetectionConfig, RecoveryConfig,
};

/// Unified export loop for TCP, HTTP, and WebSocket backends
///
/// This function handles the liveliness monitoring loop shared by all export modes.
/// The `backend` parameter determines how client connections are established.
pub(super) async fn run_export_loop(
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
            super::tcp::handle_client_connect(
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
            super::ws::handle_ws_client_connect(
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

/// Handle the bridge logic for a single client connection.
///
/// Generic over `TransportReader`/`TransportWriter` so the same function
/// serves both TCP and WebSocket export paths.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_client_bridge<R, W>(
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
            debug!(
                "Error undeclaring publisher for {}: {:?}",
                client_id_for_reader, e
            );
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
            debug!(
                "Error undeclaring subscriber for {}: {:?}",
                client_id_for_writer, e
            );
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
pub(super) async fn handle_client_disconnect(
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
