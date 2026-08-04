use crate::config::BridgeConfig;
use anyhow::Result;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
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

    super::accept::run_accept_loop(
        super::accept::AcceptLoopCfg {
            mode: "ws_import",
            client_id_prefix: "wsclient_",
        },
        session,
        service_name,
        listen_addr,
        config,
        shutdown_token,
        handle_ws_import_connection,
    )
    .await
}

/// Upgrade the connection to WebSocket, then bridge it over Zenoh.
async fn handle_ws_import_connection(
    session: Arc<Session>,
    stream: TcpStream,
    service_name: String,
    client_id: String,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    let ws_stream = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| anyhow::anyhow!("WebSocket handshake failed: {}", e))?;

    let (ws_sender, ws_receiver) = ws_stream.split();
    let reader = crate::transport::WsReader::new(ws_receiver);
    let writer = crate::transport::WsWriter::new(ws_sender);

    super::bridge::bridge_import_connection(
        session,
        reader,
        writer,
        &service_name,
        &client_id,
        "",
        None,
        config,
        None,
    )
    .await
}
