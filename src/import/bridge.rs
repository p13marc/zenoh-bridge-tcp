use crate::config::BridgeConfig;
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
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

    // Cancellation tokens for graceful shutdown of each direction
    let cancel_zenoh_to_client = CancellationToken::new();
    let cancel_client_to_zenoh = CancellationToken::new();

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

    let z2c_token = cancel_zenoh_to_client.clone();
    let mut zenoh_to_client = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = subscriber.recv_async() => {
                    let Ok(sample) = result else { break };
                    let payload = sample.payload().to_bytes().to_vec();

                    if payload.is_empty() {
                        debug!("Client {}: Received EOF signal from Zenoh", client_id_clone);
                        let _ = writer.send_eof().await;
                        break;
                    }

                    debug!("Client {}: ← {} bytes from Zenoh", client_id_clone, payload.len());

                    if let Err(e) = writer.write_data(&payload).await {
                        error!("Client {}: Failed to write to client: {:?}", client_id_clone, e);
                        break;
                    }
                }
                _ = z2c_token.cancelled() => {
                    debug!("Client {}: Zenoh-to-client cancelled", client_id_clone);
                    break;
                }
            }
        }

        if let Err(e) = writer.shutdown().await {
            debug!(
                "Client {}: Error shutting down write: {:?}",
                client_id_clone, e
            );
        }

        debug!("Client {}: Zenoh to client task completed", client_id_clone);
        if let Err(e) = subscriber.undeclare().await {
            debug!(
                "Client {}: Error undeclaring subscriber: {:?}",
                client_id_clone, e
            );
        }
    });

    // Task: reader -> Zenoh AdvancedPublisher
    let client_id_clone = client_id.to_string();
    let buffer_size = config.buffer_size;
    let drain_timeout = config.drain_timeout;
    let c2z_token = cancel_client_to_zenoh.clone();
    let mut client_to_zenoh = tokio::spawn(async move {
        loop {
            tokio::select! {
                result = reader.read_data(buffer_size) => {
                    match result {
                        Ok(data) if data.is_empty() => {
                            debug!("Client {}: Connection closed", client_id_clone);
                            break;
                        }
                        Ok(data) => {
                            debug!("Client {}: → {} bytes to Zenoh", client_id_clone, data.len());
                            if let Err(e) = publisher.put(&data[..]).await {
                                error!("Client {}: Failed to publish to Zenoh: {:?}", client_id_clone, e);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Client {}: Read error: {:?}", client_id_clone, e);
                            break;
                        }
                    }
                }
                _ = c2z_token.cancelled() => {
                    debug!("Client {}: Client-to-Zenoh cancelled", client_id_clone);
                    break;
                }
            }
        }
        if let Err(e) = publisher.undeclare().await {
            debug!(
                "Client {}: Error undeclaring publisher: {:?}",
                client_id_clone, e
            );
        }
    });

    // Wait for either task to complete or error signal
    tokio::select! {
        _ = &mut zenoh_to_client => {
            info!("Client {}: Zenoh to client task completed", client_id);
            cancel_client_to_zenoh.cancel();
            error_monitor.abort();
            match tokio::time::timeout(drain_timeout, &mut client_to_zenoh).await {
                Ok(_) => {}
                Err(_) => {
                    debug!("Client {}: Client-to-Zenoh drain timeout", client_id);
                    client_to_zenoh.abort();
                    let _ = client_to_zenoh.await;
                }
            }
        },
        _ = &mut client_to_zenoh => {
            info!("Client {}: Client to Zenoh task completed", client_id);
            cancel_zenoh_to_client.cancel();
            error_monitor.abort();
            match tokio::time::timeout(drain_timeout, &mut zenoh_to_client).await {
                Ok(_) => {}
                Err(_) => {
                    debug!("Client {}: Zenoh-to-client drain timeout", client_id);
                    zenoh_to_client.abort();
                    let _ = zenoh_to_client.await;
                }
            }
        },
        error_result = &mut error_monitor => {
            if let Ok(true) = error_result {
                warn!("Client {}: Backend unavailable, draining remaining data", client_id);

                cancel_client_to_zenoh.cancel();
                match tokio::time::timeout(drain_timeout, &mut client_to_zenoh).await {
                    Ok(_) => {}
                    Err(_) => {
                        debug!("Client {}: Client-to-Zenoh drain timeout after error", client_id);
                        client_to_zenoh.abort();
                        let _ = client_to_zenoh.await;
                    }
                }

                // Give the zenoh_to_client task time to drain buffered samples
                cancel_zenoh_to_client.cancel();
                match tokio::time::timeout(drain_timeout, &mut zenoh_to_client).await {
                    Ok(_) => {
                        info!("Client {}: Drain completed", client_id);
                    }
                    Err(_) => {
                        warn!("Client {}: Drain timed out after {:?}, aborting", client_id, drain_timeout);
                        zenoh_to_client.abort();
                        let _ = zenoh_to_client.await;
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
