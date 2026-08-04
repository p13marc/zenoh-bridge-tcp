use crate::config::BridgeConfig;
use anyhow::Result;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::info;
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

    super::accept::run_accept_loop(
        super::accept::AcceptLoopCfg {
            mode: "auto_import",
            client_id_prefix: "client_",
        },
        session,
        service_name,
        listen_addr,
        config,
        shutdown_token,
        |session, stream, service, client_id, config| async move {
            handle_auto_import_connection(session, stream, &service, &client_id, config).await
        },
    )
    .await
}

/// Handle a single auto-detected import connection.
async fn handle_auto_import_connection(
    session: Arc<Session>,
    stream: TcpStream,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    use flowscope::classify::{Classify, WireProtocol, classify_first_bytes};

    // Peek at the first bytes to classify the protocol. `classify_first_bytes`
    // is prefix-stable: a short read returns `NeedMore` rather than a wrong
    // guess. 64 bytes comfortably covers the largest signature it needs (the
    // 24-byte HTTP/2 preface); anything real (TLS record header, HTTP request
    // line) decides from its first segment. Bound the wait so an idle client
    // cannot pin a task+fd forever (F4).
    let mut peek_buf = vec![0u8; 64];
    let peek_len = match tokio::time::timeout(config.read_timeout, stream.peek(&mut peek_buf)).await
    {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(anyhow::anyhow!("Failed to peek connection: {}", e)),
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Client sent no data within the read timeout"
            ));
        }
    };

    if peek_len == 0 {
        return Err(anyhow::anyhow!("Connection closed before sending any data"));
    }

    // `NeedMore` (a sub-signature fragment) falls back to opaque relay, matching
    // the previous "unknown protocol -> raw TCP" behavior.
    let protocol = match classify_first_bytes(&peek_buf[..peek_len]) {
        Classify::Decided(p) => p,
        _ => WireProtocol::Raw,
    };
    info!(
        client_id = %client_id,
        protocol = %protocol,
        "Auto-classified protocol"
    );

    match protocol {
        WireProtocol::Tls => {
            // TLS/HTTPS — route by SNI (connection handler detects + parses it).
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
        WireProtocol::Http1 => {
            // Could be regular HTTP or a WebSocket upgrade.
            handle_auto_http_connection(session, stream, service_name, client_id, config).await
        }
        WireProtocol::Http2Preface => {
            // Prior-knowledge HTTP/2 (h2c — plaintext gRPC's wire form): route
            // by the first stream's :authority, then relay the multiplexed
            // streams opaquely (#74). Same single-authority semantics as the
            // terminated-h2 path.
            handle_h2c_connection(session, stream, service_name, client_id, config).await
        }
        // SSH, raw, or an unrecognized first-byte class -> opaque passthrough,
        // no DNS routing.
        _ => {
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

/// Handle a prior-knowledge HTTP/2 (h2c) connection: peek the first request
/// stream's `:authority`, mint the routing key, and relay everything verbatim.
///
/// The classifier saw the client preface, so the parser runs with
/// `require_preface = true` (RFC 9113 §3.4 strictness). One trap, handled per
/// the same RFC: a conformant client may send preface+SETTINGS and then wait
/// for the *server's* SETTINGS before opening a stream — which never comes,
/// because the bridge writes nothing during the peek. On head-read timeout the
/// connection therefore falls back to an opaque relay of the bytes consumed so
/// far (routing un-keyed, like raw), mirroring the `NeedMore -> Raw`
/// classification fallback, instead of dropping the client.
async fn handle_h2c_connection(
    session: Arc<Session>,
    mut stream: TcpStream,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    use super::connection::ReadHeadOutcome;

    let (dns, buffer) = match super::connection::read_h2_head(&mut stream, &config, true).await? {
        ReadHeadOutcome::Parsed(dns, buffer) => {
            info!(
                client_id = %client_id,
                dns = %dns,
                "h2c routing by :authority"
            );
            if !super::connection::backend_available(&session, service_name, &dns, &config).await? {
                // No h2-level error is possible without becoming an h2
                // endpoint; close, and the client reports a connection error.
                return Err(anyhow::anyhow!("No backend available for DNS: {}", dns));
            }
            (Some(dns), buffer)
        }
        ReadHeadOutcome::Timeout(partial) => {
            info!(
                client_id = %client_id,
                consumed = partial.len(),
                "h2c client sent no request head; falling back to opaque relay"
            );
            (None, partial)
        }
    };

    let (tcp_reader, tcp_writer) = stream.into_split();
    let reader = crate::transport::TcpReader::new(tcp_reader, config.buffer_size);
    let writer = crate::transport::TcpWriter::new(tcp_writer);

    // Surface gRPC status trailers from the response stream (#62) only when
    // the connection actually routed as h2.
    let response_tap = dns
        .is_some()
        .then(|| super::connection::h2_response_tap(service_name.to_string()));

    super::bridge::bridge_import_connection(
        session,
        reader,
        writer,
        service_name,
        client_id,
        dns.as_deref(),
        Some(buffer),
        config,
        response_tap,
    )
    .await
}

/// Handle an auto-detected HTTP connection, which might be a WebSocket upgrade.
async fn handle_auto_http_connection(
    session: Arc<Session>,
    stream: TcpStream,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    // Peek enough to detect WebSocket upgrade via proper HTTP parsing
    let mut peek_buf = vec![0u8; 4096];
    let peek_len = stream.peek(&mut peek_buf).await.unwrap_or(0);

    if peek_len > 0
        && let Some(head) = super::connection::peek_websocket_head(&peek_buf[..peek_len])
    {
        // Host-routed WS (#75): if a backend announced itself for this
        // upgrade's Host, scope the connection to it; otherwise fall back
        // to the bare service key (a plain `--backend svc/ws://…`).
        let dns = match super::connection::routing_key_from_head(&head) {
            Ok(dns)
                if super::connection::backend_available(&session, service_name, &dns, &config)
                    .await
                    .unwrap_or(false) =>
            {
                Some(dns)
            }
            _ => None,
        };
        info!(
            client_id = %client_id,
            dns = %dns.as_deref().unwrap_or("-"),
            "Detected WebSocket upgrade request"
        );

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
                    dns.as_deref(),
                    None,
                    config,
                    None,
                )
                .await;
            }
            Err(e) => {
                return Err(anyhow::anyhow!("WebSocket upgrade failed: {}", e));
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
