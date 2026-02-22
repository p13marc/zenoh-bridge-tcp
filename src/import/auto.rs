use crate::config::BridgeConfig;
use anyhow::Result;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span};
use zenoh::Session;

/// Run auto-detecting import mode for a single service.
///
/// This function:
/// 1. Binds a TCP listener
/// 2. Peeks at each incoming connection's first bytes
/// 3. Dispatches to the appropriate handler (TLS, HTTP, WebSocket, or raw TCP)
pub(super) async fn run_auto_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = super::parse_import_spec(import_spec)?;

    info!(
        mode = "auto_import",
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting auto-detecting import bridge"
    );

    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "Auto-detect import bridge ready");

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());
                        info!(
                            client_id = %client_id,
                            remote_addr = %addr,
                            "New auto-detect connection"
                        );

                        let session = session.clone();
                        let service_name = service_name.clone();
                        let config = config.clone();

                        let span = info_span!(
                            "auto_connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                        );

                        tokio::spawn(async move {
                            if let Err(e) = handle_auto_import_connection(
                                session, stream, &service_name, &client_id, config,
                            ).await {
                                error!(error = %e, "Connection error");
                            }
                            info!("Connection closed");
                        }.instrument(span));
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "Auto-detect import bridge shutting down");
                break;
            }
        }
    }

    info!(service = %service_name, "Auto-detect import bridge stopped");
    Ok(())
}

/// Handle a single auto-detected import connection.
async fn handle_auto_import_connection(
    session: Arc<Session>,
    stream: TcpStream,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    use crate::protocol_detect::{DetectedProtocol, detect_protocol};

    // Peek at the first bytes to detect the protocol
    let mut peek_buf = vec![0u8; 16];
    let peek_len = stream.peek(&mut peek_buf).await.unwrap_or(0);

    if peek_len == 0 {
        return Err(anyhow::anyhow!("Connection closed before sending any data"));
    }

    let protocol = detect_protocol(&peek_buf[..peek_len]);
    info!(
        client_id = %client_id,
        protocol = ?protocol,
        "Auto-detected protocol"
    );

    match protocol {
        DetectedProtocol::Tls => {
            // Delegate to HTTP mode which already handles TLS detection
            // via is_tls_handshake + parse_tls_client_hello
            super::connection::handle_import_connection(
                session,
                stream,
                service_name,
                client_id,
                true,
                config,
            )
            .await
        }
        DetectedProtocol::Http => {
            // Could be regular HTTP or WebSocket upgrade
            handle_auto_http_connection(session, stream, service_name, client_id, config).await
        }
        DetectedProtocol::RawTcp => {
            // Raw TCP passthrough — no DNS routing
            super::connection::handle_import_connection(
                session,
                stream,
                service_name,
                client_id,
                false,
                config,
            )
            .await
        }
    }
}

/// Handle an auto-detected HTTP connection, which might be a WebSocket upgrade.
async fn handle_auto_http_connection(
    session: Arc<Session>,
    stream: TcpStream,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    // Peek enough to detect "Upgrade: websocket" in headers
    let mut peek_buf = vec![0u8; 4096];
    let peek_len = stream.peek(&mut peek_buf).await.unwrap_or(0);

    if peek_len > 0 {
        let peek_lower = String::from_utf8_lossy(&peek_buf[..peek_len]).to_lowercase();
        let looks_like_ws =
            peek_lower.contains("upgrade: websocket") || peek_lower.contains("upgrade:websocket");

        if looks_like_ws {
            info!(client_id = %client_id, "Detected WebSocket upgrade request");
            match tokio_tungstenite::accept_async(stream).await {
                Ok(ws_stream) => {
                    let (ws_sender, ws_receiver) = ws_stream.split();
                    let reader = crate::transport::WsReader::new(ws_receiver);
                    let writer = crate::transport::WsWriter::new(ws_sender);
                    return super::bridge::bridge_import_connection(
                        session,
                        reader,
                        writer,
                        service_name,
                        client_id,
                        "",
                        None,
                        config,
                    )
                    .await;
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("WebSocket upgrade failed: {}", e));
                }
            }
        }
    }

    // Regular HTTP — delegate to HTTP import handler
    super::connection::handle_import_connection(
        session,
        stream,
        service_name,
        client_id,
        true,
        config,
    )
    .await
}
