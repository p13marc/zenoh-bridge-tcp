use crate::config::BridgeConfig;
use anyhow::Result;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span};
use zenoh::Session;

/// Run WebSocket import mode for a single service
///
/// This function:
/// 1. Binds a TCP listener on the specified address
/// 2. Accepts incoming connections and upgrades them to WebSocket
/// 3. For each connection, spawns a handler that bridges to Zenoh
pub(super) async fn run_ws_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = super::parse_import_spec(import_spec)?;

    info!(
        mode = "ws_import",
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting WebSocket import bridge"
    );

    // Start TCP listener
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "WebSocket import bridge ready");

    // Accept connections
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id = format!("wsclient_{}", uuid::Uuid::new_v4().as_simple());
                        info!(client_id = %client_id, remote_addr = %addr, "New WebSocket connection");

                        let session = session.clone();
                        let service_name = service_name.clone();
                        let client_id_clone = client_id.clone();
                        let config = config.clone();

                        let span = info_span!(
                            "ws_connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                            mode = "websocket"
                        );

                        tokio::spawn(
                            async move {
                                // Perform WebSocket upgrade
                                match tokio_tungstenite::accept_async(stream).await {
                                    Ok(ws_stream) => {
                                        let (ws_sender, ws_receiver) = ws_stream.split();
                                        let reader = crate::transport::WsReader::new(ws_receiver);
                                        let writer = crate::transport::WsWriter::new(ws_sender);
                                        if let Err(e) = super::bridge::bridge_import_connection(
                                            session,
                                            reader,
                                            writer,
                                            &service_name,
                                            &client_id_clone,
                                            "",
                                            None,
                                            config,
                                        )
                                        .await
                                        {
                                            error!(error = %e, "WebSocket connection error");
                                        }
                                        info!("WebSocket connection closed");
                                    }
                                    Err(e) => {
                                        error!(error = %e, "WebSocket handshake failed");
                                    }
                                }
                            }
                            .instrument(span),
                        );
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "WebSocket import bridge shutting down, no new connections");
                break;
            }
        }
    }

    info!(service = %service_name, "WebSocket import bridge stopped");
    Ok(())
}
