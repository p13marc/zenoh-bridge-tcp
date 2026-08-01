use super::{CancellationSender, ExportBackend};
use crate::config::{BridgeConfig, ReliabilityMode};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
use zenoh::qos::CongestionControl;
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
            if let Some(client_id) = key.rsplit('/').next()
                && !client_id.is_empty()
            {
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
                        if let Some(client_id) = key.rsplit('/').next()
                            && !client_id.is_empty()
                        {
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
                // Collect senders and handles, then release the lock before awaiting
                let entries: Vec<(String, CancellationSender)> =
                    cancellation_senders.lock().await.drain().collect();

                // Send cancellation signals
                for (client_id, (tx, _)) in &entries {
                    let _ = tx.send(()).await;
                    debug!(client_id = %client_id, "Sent shutdown to client bridge");
                }

                // Wait for all task handles to drain
                for (client_id, (_, handle)) in entries {
                    match tokio::time::timeout(config.drain_timeout, handle).await {
                        Ok(Ok(())) => debug!(client_id = %client_id, "Client bridge drained"),
                        Ok(Err(e)) => warn!(client_id = %client_id, error = %e, "Client bridge task error during drain"),
                        Err(_) => warn!(client_id = %client_id, "Client bridge drain timeout"),
                    }
                }
                break;
            }
        }
    }

    // Explicitly undeclare liveliness subscriber
    if let Err(e) = liveliness_subscriber.undeclare().await {
        debug!(service = %service_name, "Error undeclaring liveliness subscriber: {:?}", e);
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
    let publisher_builder = session
        .declare_publisher(pub_key.clone())
        .cache(CacheConfig::default().max_samples(config.cache_size))
        .sample_miss_detection(MissDetectionConfig::default().heartbeat(config.heartbeat_interval))
        .publisher_detection();
    // Stream reliability: block on a full TX queue instead of Zenoh's default
    // `Drop`, which would silently drop payload bytes and corrupt the stream.
    let publisher = match config.reliability {
        ReliabilityMode::Stream => publisher_builder.congestion_control(CongestionControl::Block),
        ReliabilityMode::Telemetry => publisher_builder,
    }
    .await
    .map_err(|e| anyhow::anyhow!("Failed to declare publisher: {}", e))?;

    debug!(
        "Client {}: Declared AdvancedPublisher on {} with cache",
        client_id, pub_key_str
    );

    let client_id_for_reader = client_id.clone();
    let client_id_for_writer = client_id.clone();

    // Single abort token for the whole connection. A clean directional EOF ends
    // only its own direction (a half-close); a hard error, external teardown, or
    // an unrecoverable sample miss trips this token to reset both directions.
    let conn_cancel = CancellationToken::new();

    // Stream reliability: an unrecoverable sample miss means the byte stream has a
    // gap that cannot be delivered faithfully. Reset the connection rather than
    // hand corrupted bytes to the backend. The listener runs in the background for
    // the subscriber's lifetime.
    if config.reliability == ReliabilityMode::Stream {
        let miss_cancel = conn_cancel.clone();
        let miss_client = client_id.clone();
        subscriber
            .sample_miss_listener()
            .callback(move |miss| {
                warn!(
                    "Client {}: unrecoverable sample miss ({} sample(s)) — resetting connection",
                    miss_client,
                    miss.nb()
                );
                miss_cancel.cancel();
            })
            .background()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to register sample-miss listener: {:?}", e))?;
    }

    // Direction: backend -> Zenoh. A clean EOF publishes the empty EOF marker so
    // the import half-closes the client and ends this direction only. A read or
    // publish error resets the whole connection.
    let buffer_size = config.buffer_size;
    let b2z_cancel = conn_cancel.clone();
    let backend_to_zenoh_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = backend_reader.read_data(buffer_size) => {
                    match result {
                        Ok(data) if data.is_empty() => {
                            debug!("Backend half-close -> EOF for client {}", client_id_for_reader);
                            let _ = publisher.put(Vec::<u8>::new()).await;
                            break;
                        }
                        Ok(data) => {
                            if let Err(e) = publisher.put(&data[..]).await {
                                error!("Failed to publish for client {}: {:?}", client_id_for_reader, e);
                                b2z_cancel.cancel();
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Backend read error for client {}: {:?}", client_id_for_reader, e);
                            // C1: emit EOF so the import side doesn't hang, then reset.
                            let _ = publisher.put(Vec::<u8>::new()).await;
                            b2z_cancel.cancel();
                            break;
                        }
                    }
                }
                _ = b2z_cancel.cancelled() => break,
            }
        }
        // Return the publisher so it (and its cache) stays alive until the whole
        // connection ends, letting a late-joining import subscriber recover.
        publisher
    });

    // Direction: Zenoh -> backend. An empty payload is the client's half-close;
    // propagate it as a FIN on the backend's write side and end this direction only.
    let z2b_cancel = conn_cancel.clone();
    let zenoh_to_backend_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = subscriber.recv_async() => {
                    match result {
                        Ok(sample) => {
                            let payload = sample.payload().to_bytes().to_vec();
                            if payload.is_empty() {
                                debug!("Client {}: client half-close -> FIN to backend", client_id_for_writer);
                                let _ = backend_writer.send_eof().await;
                                break;
                            }
                            if let Err(e) = backend_writer.write_data(&payload).await {
                                error!("Failed to write to backend for client {}: {:?}", client_id_for_writer, e);
                                z2b_cancel.cancel();
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Subscriber error for client {}: {:?}", client_id_for_writer, e);
                            z2b_cancel.cancel();
                            break;
                        }
                    }
                }
                _ = z2b_cancel.cancelled() => {
                    let _ = backend_writer.shutdown().await;
                    break;
                }
            }
        }
        // Return the subscriber; the coordinator undeclares it after both ends.
        subscriber
    });

    // External teardown (liveliness delete / duplicate connect) resets the connection.
    let ext_cancel = conn_cancel.clone();
    let ext_task = tokio::spawn(async move {
        let _ = cancel_rx.recv().await;
        ext_cancel.cancel();
    });

    // Wait for BOTH directions. Each ends on its own EOF (half-close) or when the
    // connection is reset. A healthy half-open connection has no artificial timeout;
    // once reset, a watchdog gives the tasks the drain budget and then aborts them.
    let drain_timeout = config.drain_timeout;
    let watchdog_cancel = conn_cancel.clone();
    let b2z_abort = backend_to_zenoh_handle.abort_handle();
    let z2b_abort = zenoh_to_backend_handle.abort_handle();
    let watchdog = tokio::spawn(async move {
        watchdog_cancel.cancelled().await;
        tokio::time::sleep(drain_timeout).await;
        b2z_abort.abort();
        z2b_abort.abort();
    });

    let (b2z_res, z2b_res) = tokio::join!(backend_to_zenoh_handle, zenoh_to_backend_handle);
    watchdog.abort();
    ext_task.abort();
    let _ = ext_task.await;
    if let Ok(publisher) = b2z_res {
        let _ = publisher.undeclare().await;
    }
    if let Ok(subscriber) = z2b_res {
        let _ = subscriber.undeclare().await;
    }

    info!("Connection handler stopped for client: {}", client_id);

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
