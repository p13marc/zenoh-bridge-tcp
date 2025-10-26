//! Import mode implementation for the Zenoh TCP Bridge.
//!
//! This module handles importing Zenoh services as TCP listeners.
//! Each TCP connection gets its own liveliness token and dedicated Zenoh pub/sub.

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info};
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
    let (service_name, listen_addr) = parse_import_spec(import_spec)?;

    info!("🚀 IMPORT MODE");
    info!("   Service name: {}", service_name);
    info!("   TCP Listen: {}", listen_addr);
    info!("   Zenoh TX key: {}/tx/<client_id>", service_name);
    info!("   Zenoh RX key: {}/rx/<client_id>", service_name);
    info!("   Liveliness: {}/clients/<client_id>", service_name);

    // Start TCP listener
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!("✓ Listening for TCP connections on {}", listen_addr);
    info!("✓ Ready to forward to service '{}'", service_name);

    let mut connection_id = 0u64;

    // Accept connections
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                connection_id += 1;
                let client_id = format!("client_{}", connection_id);
                info!("✓ New TCP connection: {} from {}", client_id, addr);

                let session = session.clone();
                let service_name = service_name.clone();

                tokio::spawn(async move {
                    if let Err(e) =
                        handle_import_connection(session, stream, &service_name, &client_id).await
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
/// 1. Subscribing to error signals (before declaring liveliness)
/// 2. Subscribing to the service's response channel
/// 3. Declaring a liveliness token to signal the export bridge
/// 4. Spawning tasks to bridge data in both directions
async fn handle_import_connection(
    session: Arc<Session>,
    stream: TcpStream,
    service_name: &str,
    client_id: &str,
) -> Result<()> {
    let (mut tcp_reader, mut tcp_writer) = stream.into_split();

    // IMPORTANT: Subscribe to error channel FIRST, before declaring liveliness
    // This prevents race condition where export bridge publishes error before we're subscribed
    let error_key = format!("{}/error/{}", service_name, client_id);
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
    let sub_key = format!("{}/rx/{}", service_name, client_id);
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
    let pub_key_str = format!("{}/tx/{}", service_name, client_id);
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
    let liveliness_key = format!("{}/clients/{}", service_name, client_id);
    let _liveliness_token = session
        .liveliness()
        .declare_token(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare liveliness: {}", e))?;

    info!(
        "✓ Client {} declared liveliness: {}",
        client_id, liveliness_key
    );

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
        }
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
