use super::CancellationSender;
use crate::config::BridgeConfig;
use backon::{ExponentialBuilder, Retryable};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tracing::{error, info, info_span, warn};
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

            let (ws_sender, ws_receiver) = ws_stream.split();
            let reader = crate::transport::WsReader::new(ws_receiver);
            let writer = crate::transport::WsWriter::new(ws_sender);

            let span = info_span!(
                "ws_client_bridge",
                client_id = %client_id,
                service = %service_name,
                ws_url = %ws_url
            );

            // Cancel any prior connection, spawn the bridge, and track it so it
            // frees its own map entry on completion (D1).
            super::bridge::spawn_and_track(
                session.clone(),
                service_name.to_string(),
                client_id.to_string(),
                reader,
                writer,
                None,
                config.clone(),
                cancellation_senders,
                span,
            )
            .await;
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
