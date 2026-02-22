use crate::config::BridgeConfig;
use crate::http_parser::parse_http_request;
use anyhow::Result;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span, warn};
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
    let tls_acceptor = TlsAcceptor::from(tls_config);

    info!(
        mode = "https_terminate",
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting HTTPS termination import bridge"
    );

    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "HTTPS termination import bridge ready");

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((tcp_stream, addr)) => {
                        let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());
                        info!(
                            client_id = %client_id,
                            remote_addr = %addr,
                            "New TLS connection"
                        );

                        let session = session.clone();
                        let service_name = service_name.clone();
                        let tls_acceptor = tls_acceptor.clone();
                        let config = config.clone();

                        let span = info_span!(
                            "tls_connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                        );

                        tokio::spawn(async move {
                            // Perform TLS handshake
                            match tls_acceptor.accept(tcp_stream).await {
                                Ok(tls_stream) => {
                                    if let Err(e) = handle_tls_terminated_connection(
                                        session,
                                        tls_stream,
                                        &service_name,
                                        &client_id,
                                        config,
                                    ).await {
                                        error!(error = %e, "TLS connection error");
                                    }
                                    info!("TLS connection closed");
                                }
                                Err(e) => {
                                    error!(error = %e, "TLS handshake failed");
                                }
                            }
                        }.instrument(span));
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "HTTPS terminate bridge shutting down");
                break;
            }
        }
    }

    info!(service = %service_name, "HTTPS terminate bridge stopped");
    Ok(())
}

/// Handle a single TLS-terminated connection.
///
/// After TLS termination, the decrypted stream is plain HTTP. Parse it for
/// Host-based routing, then bridge identically to the existing HTTP import.
async fn handle_tls_terminated_connection(
    session: Arc<Session>,
    tls_stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    let (mut tls_reader, tls_writer) = tokio::io::split(tls_stream);

    // After TLS termination, we have plaintext HTTP. Parse Host header for routing.
    let parsed = parse_http_request(&mut tls_reader).await.map_err(|e| {
        anyhow::anyhow!("Failed to parse HTTP request after TLS termination: {}", e)
    })?;

    let dns = parsed.dns.clone();
    let dns_suffix = format!("/{}", dns);
    info!(
        "Client {}: TLS-terminated HTTP routing to DNS: {}",
        client_id, dns
    );

    // Check backend availability
    let service_key = format!("{}/{}/available", service_name, dns);
    let liveliness_replies = session
        .liveliness()
        .get(&service_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query liveliness: {}", e))?;

    let backend_available = tokio::time::timeout(config.availability_timeout, async {
        liveliness_replies.recv_async().await.is_ok()
    })
    .await
    .unwrap_or(false);

    if !backend_available {
        warn!("Client {}: No backend for DNS: {}", client_id, dns);
        return Err(anyhow::anyhow!("No backend available for DNS: {}", dns));
    }

    info!("Client {}: Backend available for DNS: {}", client_id, dns);

    // Bridge the decrypted connection through Zenoh
    // Wrap TLS halves with transport traits (same as TCP — both implement AsyncReadExt/AsyncWriteExt)
    let reader = crate::transport::TcpReader::new(tls_reader, config.buffer_size);
    let writer = crate::transport::TcpWriter::new(tls_writer);

    super::bridge::bridge_import_connection(
        session,
        reader,
        writer,
        service_name,
        client_id,
        &dns_suffix,
        Some(parsed.buffer),
        config,
    )
    .await
}
