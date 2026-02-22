use super::CancellationSender;
use super::bridge::handle_client_bridge;
use crate::config::BridgeConfig;
use backon::{ExponentialBuilder, Retryable};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tracing::{Instrument, error, info, info_span, warn};
use zenoh::Session;

/// Handle a client connection event for TCP backends
pub(super) async fn handle_client_connect(
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
