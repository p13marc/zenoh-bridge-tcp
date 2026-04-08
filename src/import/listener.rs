use crate::config::BridgeConfig;
use anyhow::Result;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span};
use zenoh::Session;

/// Internal implementation for both regular and HTTP import modes
pub(super) async fn run_import_mode_internal(
    session: Arc<Session>,
    import_spec: &str,
    http_mode: bool,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = super::parse_import_spec(import_spec)?;

    let mode = if http_mode { "http_import" } else { "import" };
    info!(
        mode = mode,
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting import bridge"
    );

    // Start TCP listener
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, "Import bridge ready");

    let mut tasks = JoinSet::new();

    // Accept connections
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());
                        info!(
                            client_id = %client_id,
                            remote_addr = %addr,
                            "New connection"
                        );

                        let session = session.clone();
                        let service_name = service_name.clone();
                        let client_id_clone = client_id.clone();
                        let config = config.clone();

                        let span = info_span!(
                            "connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                            mode = if http_mode { "http" } else { "tcp" }
                        );

                        tasks.spawn(
                            async move {
                                if let Err(e) = super::connection::handle_import_connection(
                                    session,
                                    stream,
                                    &service_name,
                                    &client_id_clone,
                                    http_mode,
                                    config,
                                )
                                .await
                                {
                                    error!(error = %e, "Connection error");
                                }
                                info!("Connection closed");
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
                info!(service = %service_name, "Import bridge shutting down, no new connections");
                break;
            }
        }

        // Reap completed tasks to prevent unbounded growth
        while tasks.try_join_next().is_some() {}
    }

    super::drain_tasks(&mut tasks, &service_name, config.drain_timeout).await;

    info!(service = %service_name, "Import bridge stopped");
    Ok(())
}
