//! WebSocket backend dialing for the export side.
//!
//! Just the dial (see `tcp.rs` for the division of labour). The per-attempt
//! bound matters even more here: `connect_async` is a TCP connect **plus** a
//! full HTTP upgrade round-trip, so a peer that accepts and then stalls the
//! handshake would otherwise pin an attempt indefinitely.

use crate::config::BridgeConfig;
use crate::transport::{WsReader, WsWriter};
use anyhow::Result;
use backon::{ExponentialBuilder, Retryable};
use futures_util::StreamExt;
use futures_util::stream::{SplitSink, SplitStream};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{info, warn};

type WsSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Dial a `ws://` / `wss://` backend with retries, each attempt bounded by
/// `config.connect_timeout`.
pub(super) async fn dial(
    ws_url: &str,
    client_id: &str,
    config: &BridgeConfig,
) -> Result<(
    WsReader<SplitStream<WsSocket>>,
    WsWriter<SplitSink<WsSocket, Message>>,
)> {
    let connect_timeout = config.connect_timeout;
    let ws_url_owned = ws_url.to_string();
    let client_id_for_log = client_id.to_string();
    let ws_url_for_log = ws_url.to_string();
    let (ws_stream, _response) = (|| {
        let url = ws_url_owned.clone();
        async move {
            tokio::time::timeout(connect_timeout, connect_async(&url))
                .await
                .map_err(|_| {
                    tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "WebSocket backend connect timed out",
                    ))
                })?
        }
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
    .await?;

    info!(ws_url = %ws_url, "WebSocket backend connection established");
    let (sender, receiver) = ws_stream.split();
    Ok((WsReader::new(receiver), WsWriter::new(sender)))
}
