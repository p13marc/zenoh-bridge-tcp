//! Export mode implementation for the Zenoh TCP Bridge.
//!
//! This module handles exporting TCP backend services as Zenoh services.
//! Each export creates lazy connections to the backend - one connection per importing client.

use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};
use zenoh::key_expr::KeyExpr;
use zenoh::Session;
use zenoh_ext::{
    AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt, CacheConfig, HistoryConfig,
    MissDetectionConfig, RecoveryConfig,
};

/// Type alias for cancellation sender and task handle
type CancellationSender = (mpsc::Sender<()>, tokio::task::JoinHandle<()>);

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

/// Run export mode for a single service
///
/// This function:
/// 1. Monitors client liveliness tokens
/// 2. Creates a backend connection when a client appears
/// 3. Bridges data between the backend and Zenoh
/// 4. Cleans up when clients disconnect
pub async fn run_export_mode(session: Arc<Session>, export_spec: &str) -> Result<()> {
    let (service_name, backend_addr) = parse_export_spec(export_spec)?;

    info!("🚀 EXPORT MODE");
    info!("   Service name: {}", service_name);
    info!("   Backend: {}", backend_addr);
    info!("   Zenoh TX key: {}/tx/<client_id>", service_name);
    info!("   Zenoh RX key: {}/rx/<client_id>", service_name);
    info!("   Liveliness: {}/clients/*", service_name);

    // Monitor client liveliness to create/destroy connections
    let liveliness_key = format!("{}/clients/*", service_name);
    let liveliness_subscriber = session
        .liveliness()
        .declare_subscriber(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe to liveliness: {}", e))?;

    info!("✓ Monitoring client liveliness: {}", liveliness_key);
    info!("✓ Ready to create connections when clients appear");

    // Track connection tasks and cancellation senders per client ID
    let cancellation_senders: Arc<Mutex<HashMap<String, CancellationSender>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Main loop: monitor liveliness and create/destroy connections
    loop {
        match liveliness_subscriber.recv_async().await {
            Ok(sample) => {
                let key = sample.key_expr().as_str();
                if let Some(client_id) = key.rsplit('/').next() {
                    let client_id = client_id.to_string();

                    match sample.kind() {
                        zenoh::sample::SampleKind::Put => {
                            handle_client_connect(
                                &session,
                                &service_name,
                                backend_addr,
                                &client_id,
                                &cancellation_senders,
                            )
                            .await;
                        }
                        zenoh::sample::SampleKind::Delete => {
                            handle_client_disconnect(&client_id, &cancellation_senders).await;
                        }
                    }
                }
            }
            Err(e) => {
                error!("Liveliness subscriber error: {:?}", e);
                break Err(anyhow::anyhow!("Liveliness subscriber error: {:?}", e));
            }
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
) {
    info!("✓ Client connected: {}", client_id);

    // Create new backend connection for this client
    match TcpStream::connect(backend_addr).await {
        Ok(backend_stream) => {
            info!("✓ Created backend connection for client: {}", client_id);

            let (backend_reader, backend_writer) = backend_stream.into_split();

            let session_clone = session.clone();
            let service_name = service_name.to_string();
            let client_id_str = client_id.to_string();
            let client_id_for_map = client_id.to_string();

            // Create cancellation channel for graceful shutdown
            let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);

            // Spawn dedicated task for this client connection
            let main_handle = tokio::spawn(async move {
                if let Err(e) = handle_client_bridge(
                    session_clone,
                    service_name,
                    client_id_str,
                    backend_reader,
                    backend_writer,
                    cancel_rx,
                )
                .await
                {
                    error!("Client bridge error: {:?}", e);
                }
            });

            // Store the cancellation sender and task handle
            cancellation_senders
                .lock()
                .await
                .insert(client_id_for_map, (cancel_tx, main_handle));
        }
        Err(e) => {
            error!(
                "Failed to connect to backend for client {}: {:?}",
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

/// Handle the bridge logic for a single client connection
async fn handle_client_bridge(
    session: Arc<Session>,
    service_name: String,
    client_id: String,
    mut backend_reader: tokio::net::tcp::OwnedReadHalf,
    mut backend_writer: tokio::net::tcp::OwnedWriteHalf,
    mut cancel_rx: mpsc::Receiver<()>,
) -> Result<()> {
    // Subscribe to messages from this specific client using AdvancedSubscriber
    // This enables late publisher detection and recovery of missed samples
    let sub_key = format!("{}/tx/{}", service_name, client_id);
    let subscriber = session
        .declare_subscriber(&sub_key)
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default().periodic_queries(Duration::from_millis(500)))
        .subscriber_detection()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe: {:?}", e))?;

    info!(
        "✓ Client {} subscribed to {} with late publisher detection",
        client_id, sub_key
    );

    // Declare AdvancedPublisher with cache and publisher detection for RX channel
    // This allows the import bridge to detect when we're ready and recover any missed samples
    let pub_key_str = format!("{}/rx/{}", service_name, client_id);
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

    let client_id_for_reader = client_id.clone();
    let client_id_for_writer = client_id.clone();
    let client_id_for_final = client_id.clone();

    // Task: read from backend and publish to Zenoh using AdvancedPublisher
    let mut backend_to_zenoh_handle = tokio::spawn(async move {
        let mut buffer = vec![0u8; 65536];
        loop {
            match backend_reader.read(&mut buffer).await {
                Ok(0) => {
                    info!(
                        "Backend closed connection for client: {}",
                        client_id_for_reader
                    );
                    break;
                }
                Ok(n) => {
                    debug!(
                        "← {} bytes from backend for client {}",
                        n, client_id_for_reader
                    );
                    if let Err(e) = publisher.put(&buffer[..n]).await {
                        error!(
                            "Failed to publish for client {}: {:?}",
                            client_id_for_reader, e
                        );
                        break;
                    }
                }
                Err(e) => {
                    error!(
                        "Backend read error for client {}: {:?}",
                        client_id_for_reader, e
                    );
                    break;
                }
            }
        }
    });

    // Task: receive from Zenoh and write to backend
    let mut zenoh_to_backend_handle = tokio::spawn(async move {
        loop {
            match subscriber.recv_async().await {
                Ok(sample) => {
                    let payload = sample.payload().to_bytes();
                    debug!(
                        "→ {} bytes to backend for client {}",
                        payload.len(),
                        client_id_for_writer
                    );
                    if let Err(e) = backend_writer.write_all(&payload).await {
                        error!(
                            "Failed to write to backend for client {}: {:?}",
                            client_id_for_writer, e
                        );
                        break;
                    }
                }
                Err(e) => {
                    error!(
                        "Subscriber error for client {}: {:?}",
                        client_id_for_writer, e
                    );
                    break;
                }
            }
        }
    });

    // Wait for either task to complete or cancellation signal
    tokio::select! {
        _result = &mut backend_to_zenoh_handle => {
            info!("Backend closed for client: {}", client_id_for_final);
            // Abort the other task and wait for it
            zenoh_to_backend_handle.abort();
            let _ = zenoh_to_backend_handle.await;
        },
        _result = &mut zenoh_to_backend_handle => {
            info!("Zenoh closed for client: {}", client_id_for_final);
            // Abort the other task and wait for it
            backend_to_zenoh_handle.abort();
            let _ = backend_to_zenoh_handle.await;
        },
        _ = cancel_rx.recv() => {
            info!("Cancellation received for client: {}", client_id_for_final);
            // Abort both tasks and wait for them to finish
            backend_to_zenoh_handle.abort();
            zenoh_to_backend_handle.abort();
            let _ = backend_to_zenoh_handle.await;
            let _ = zenoh_to_backend_handle.await;
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
) {
    info!("✗ Client disconnected: {}", client_id);

    // Send cancellation signal and wait for task to complete
    if let Some((cancel_tx, task_handle)) = cancellation_senders.lock().await.remove(client_id) {
        // Send cancellation signal (ignore error if receiver already dropped)
        let _ = cancel_tx.send(()).await;
        info!(
            "  Sent shutdown signal to backend connection for: {}",
            client_id
        );

        // Wait for the task to complete with a timeout
        if tokio::time::timeout(std::time::Duration::from_secs(2), task_handle)
            .await
            .is_ok()
        {
            info!("  Backend connection closed for: {}", client_id);
        } else {
            warn!(
                "  Timeout waiting for backend connection to close for: {}",
                client_id
            );
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
}
