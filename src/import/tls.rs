use crate::config::BridgeConfig;
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use zenoh::Session;

/// Run HTTPS import mode with TLS termination.
///
/// This function:
/// 1. Binds a TCP listener
/// 2. Accepts TLS connections, terminating TLS at the bridge
/// 3. Parses the decrypted HTTP request for Host-based routing
/// 4. Bridges the plaintext data over Zenoh
///
/// Unlike `run_http_import_mode`, the backend receives plaintext HTTP —
/// all TLS is handled at the import bridge.
pub(super) async fn run_https_terminate_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    tls_config: Arc<rustls::ServerConfig>,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    use tokio_rustls::TlsAcceptor;

    let (service_name, listen_addr) = super::parse_import_spec(import_spec)?;
    // Built once; cloning per connection is an Arc clone.
    let tls_acceptor = TlsAcceptor::from(tls_config);

    super::accept::run_accept_loop(
        super::accept::AcceptLoopCfg {
            mode: "https_terminate",
            client_id_prefix: "client_",
        },
        session,
        service_name,
        listen_addr,
        config,
        shutdown_token,
        move |session, tcp_stream, service, client_id, config| {
            let tls_acceptor = tls_acceptor.clone();
            async move {
                let tls_stream = tls_acceptor
                    .accept(tcp_stream)
                    .await
                    .map_err(|e| anyhow::anyhow!("TLS handshake failed: {}", e))?;
                handle_tls_terminated_connection(session, tls_stream, &service, &client_id, config)
                    .await
            }
        },
    )
    .await
}

/// Handle a single TLS-terminated connection.
///
/// After TLS termination the decrypted stream is plaintext HTTP/1.1 or, when ALPN
/// negotiated `h2`, HTTP/2 (Phase C, #50). Either way we peek the decrypted head
/// for the routing key, then relay the bytes verbatim over Zenoh — the bridge
/// terminates TLS but never rewrites the application stream.
async fn handle_tls_terminated_connection(
    session: Arc<Session>,
    tls_stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    // Read the ALPN-negotiated protocol before the stream is split.
    let is_h2 = tls_stream
        .get_ref()
        .1
        .alpn_protocol()
        .is_some_and(|p| p == b"h2");

    let (mut tls_reader, tls_writer) = tokio::io::split(tls_stream);

    // Resolve the routing key from the decrypted head: for h2 the first request
    // stream's `:authority`, otherwise the HTTP/1.1 `Host`. The bytes read are
    // kept and relayed verbatim as the connection's initial payload.
    let (dns, buffer) = if is_h2 {
        super::connection::read_h2_head(&mut tls_reader, &config)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse HTTP/2 request after TLS termination: {}",
                    e
                )
            })?
    } else {
        super::connection::read_http_head(&mut tls_reader, &config)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to parse HTTP request after TLS termination: {}", e)
            })?
    };

    info!(
        "Client {}: TLS-terminated {} routing to DNS: {}",
        client_id,
        if is_h2 { "h2" } else { "HTTP/1.1" },
        dns
    );

    // Check backend availability
    if !super::connection::backend_available(&session, service_name, &dns, &config).await? {
        warn!("Client {}: No backend for DNS: {}", client_id, dns);
        return Err(anyhow::anyhow!("No backend available for DNS: {}", dns));
    }

    info!("Client {}: Backend available for DNS: {}", client_id, dns);

    // Bridge the decrypted connection through Zenoh
    // Wrap TLS halves with transport traits (same as TCP — both implement AsyncReadExt/AsyncWriteExt)
    let reader = crate::transport::TcpReader::new(tls_reader, config.buffer_size);
    let writer = crate::transport::TcpWriter::new(tls_writer);

    // For h2, observe the response stream to surface gRPC status trailers (#62).
    let response_tap = if is_h2 {
        Some(super::connection::h2_response_tap(service_name.to_string()))
    } else {
        None
    };

    super::bridge::bridge_import_connection(
        session,
        reader,
        writer,
        service_name,
        client_id,
        Some(&dns),
        Some(buffer),
        config,
        response_tap,
    )
    .await
}
