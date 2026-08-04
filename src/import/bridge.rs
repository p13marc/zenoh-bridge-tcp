use crate::config::{BridgeConfig, ReliabilityMode};
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
use zenoh::qos::CongestionControl;
use zenoh_ext::{
    AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt, CacheConfig, HistoryConfig,
    MissDetectionConfig, RecoveryConfig,
};

/// Shared bidirectional bridging logic for import connections.
///
/// This function handles the Zenoh pub/sub setup and bidirectional data bridging
/// for any import connection, regardless of transport (TCP, TLS-terminated, WebSocket).
///
/// Generic over `TransportReader`/`TransportWriter` so the same function serves
/// TCP, TLS-terminated, and WebSocket import paths.
#[allow(clippy::too_many_arguments)]
pub(super) async fn bridge_import_connection<R, W>(
    session: Arc<Session>,
    mut reader: R,
    mut writer: W,
    service_name: &str,
    client_id: &str,
    dns_suffix: &str,
    initial_buffer: Option<Vec<u8>>,
    config: Arc<BridgeConfig>,
) -> Result<()>
where
    R: crate::transport::TransportReader,
    W: crate::transport::TransportWriter,
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
        .recovery(RecoveryConfig::default().periodic_queries(config.heartbeat_interval))
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

    let client_id_for_error = client_id.to_string();

    // Single abort token for the whole connection. A clean directional EOF ends
    // only its own direction (a half-close); a hard error, external teardown, or
    // an unrecoverable sample miss trips this token to reset both directions.
    let conn_cancel = CancellationToken::new();

    // Stream reliability: reset the connection on an unrecoverable sample miss
    // rather than deliver a corrupted byte stream to the client.
    if config.reliability == ReliabilityMode::Stream {
        let miss_cancel = conn_cancel.clone();
        let miss_client = client_id.to_string();
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

    // Monitor error signals from the export bridge. An error means the backend
    // side is gone, so reset the whole connection.
    let error_cancel = conn_cancel.clone();
    let error_monitor = tokio::spawn(async move {
        if let Ok(sample) = error_subscriber.recv_async().await {
            let error_msg = sample.payload().to_bytes();
            warn!(
                "Client {}: Backend error: {} — resetting connection",
                client_id_for_error,
                String::from_utf8_lossy(&error_msg)
            );
            error_cancel.cancel();
        }
    });

    // Direction: Zenoh -> client. An empty payload is the backend's half-close;
    // propagate it as a FIN on the client's write side and end this direction
    // only. The reverse direction keeps flowing.
    let z2c_client = client_id.to_string();
    let z2c_cancel = conn_cancel.clone();
    let zenoh_to_client = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = subscriber.recv_async() => {
                    match result {
                        Ok(sample) => {
                            let payload = sample.payload().to_bytes().to_vec();
                            if payload.is_empty() {
                                debug!("Client {}: backend half-close -> FIN to client", z2c_client);
                                let _ = writer.send_eof().await;
                                break;
                            }
                            if let Err(e) = writer.write_data(&payload).await {
                                error!("Client {}: Failed to write to client: {:?}", z2c_client, e);
                                z2c_cancel.cancel();
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Client {}: Subscriber error: {:?}", z2c_client, e);
                            z2c_cancel.cancel();
                            break;
                        }
                    }
                }
                _ = z2c_cancel.cancelled() => {
                    let _ = writer.shutdown().await;
                    break;
                }
            }
        }
        // Return the subscriber so the coordinator undeclares it only after the
        // whole connection ends (keeps it alive across a half-open connection).
        subscriber
    });

    // Direction: client -> Zenoh. An empty read is the client's half-close;
    // publish the EOF marker so the export half-closes the backend, then end this
    // direction only. A read error resets the whole connection.
    let c2z_client = client_id.to_string();
    let c2z_cancel = conn_cancel.clone();
    let client_to_zenoh = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = reader.read_data() => {
                    match result {
                        Ok(data) if data.is_empty() => {
                            debug!("Client {}: client half-close -> EOF to Zenoh", c2z_client);
                            let _ = publisher.put(Vec::<u8>::new()).await;
                            break;
                        }
                        Ok(data) => {
                            // Zero-copy: `data` is `Bytes`, published via Zenoh's
                            // `From<bytes::Bytes>` without a further copy.
                            if let Err(e) = publisher.put(data).await {
                                error!("Client {}: Failed to publish to Zenoh: {:?}", c2z_client, e);
                                c2z_cancel.cancel();
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Client {}: Read error: {:?}", c2z_client, e);
                            let _ = publisher.put(Vec::<u8>::new()).await;
                            c2z_cancel.cancel();
                            break;
                        }
                    }
                }
                _ = c2z_cancel.cancelled() => break,
            }
        }
        // Return the publisher (and its cache) so it stays alive until the whole
        // connection ends — a late-joining export subscriber can then still
        // recover the buffered samples (critical for a fast half-close).
        publisher
    });

    // Wait for BOTH directions to finish. Each ends on its own EOF (half-close)
    // or when the connection is reset. A healthy half-open connection keeps the
    // still-open direction running until its own EOF, so there is no artificial
    // timeout on the happy path. If the connection is reset, a watchdog gives the
    // tasks the drain budget to exit and then aborts them.
    let drain_timeout = config.drain_timeout;
    let watchdog_cancel = conn_cancel.clone();
    let z2c_abort = zenoh_to_client.abort_handle();
    let c2z_abort = client_to_zenoh.abort_handle();
    let watchdog = tokio::spawn(async move {
        watchdog_cancel.cancelled().await;
        tokio::time::sleep(drain_timeout).await;
        z2c_abort.abort();
        c2z_abort.abort();
    });

    let (z2c_res, c2z_res) = tokio::join!(zenoh_to_client, client_to_zenoh);
    watchdog.abort();
    error_monitor.abort();
    let _ = error_monitor.await;
    // Both directions have ended: now undeclare the Zenoh entities.
    if let Ok(subscriber) = z2c_res {
        let _ = subscriber.undeclare().await;
    }
    if let Ok(publisher) = c2z_res {
        let _ = publisher.undeclare().await;
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
