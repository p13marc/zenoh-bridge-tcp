use super::CancellationSender;
use super::bridge::handle_client_bridge;
use crate::config::BridgeConfig;
use backon::{ExponentialBuilder, Retryable};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::connect_async;
use tracing::{Instrument, error, info, info_span, warn};
use zenoh::Session;

/// Handle a WebSocket client connection event
pub(super) async fn handle_ws_client_connect(
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

            // Cancel any existing connection for this client ID BEFORE spawning new task
            {
                let mut senders = cancellation_senders.lock().await;
                if let Some((old_cancel_tx, old_handle)) = senders.remove(client_id) {
                    warn!(client_id = %client_id, "WS client already has active connection, cancelling old one");
                    let _ = old_cancel_tx.send(()).await;
                    let _ = tokio::time::timeout(config.drain_timeout, old_handle).await;
                }
            }

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

            // Store the new task handle
            cancellation_senders
                .lock()
                .await
                .insert(client_id_for_map, (cancel_tx, main_handle));
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
