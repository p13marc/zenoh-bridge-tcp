use super::CancellationSender;
use crate::config::BridgeConfig;
use backon::{ExponentialBuilder, Retryable};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{error, info, info_span, warn};
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

            let span = info_span!(
                "client_bridge",
                client_id = %client_id,
                service = %service_name,
                backend = %backend_addr,
                dns = dns_suffix.unwrap_or("-")
            );

            // Cancel any prior connection, spawn the bridge, and track it so it
            // frees its own map entry on completion (D1).
            super::bridge::spawn_and_track(
                session.clone(),
                service_name.to_string(),
                client_id.to_string(),
                reader,
                writer,
                dns_suffix.map(|s| s.to_string()),
                config.clone(),
                cancellation_senders,
                span,
            )
            .await;
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
