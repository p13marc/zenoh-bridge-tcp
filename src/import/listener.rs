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

    // Cap concurrent connections: hold a permit before accepting so the loop
    // applies backpressure at the limit instead of spawning without bound (D3).
    let conn_limit = Arc::new(tokio::sync::Semaphore::new(config.max_connections));

    // Accept connections
    loop {
        let permit = tokio::select! {
            p = conn_limit.clone().acquire_owned() => {
                p.expect("connection semaphore is never closed")
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "Import bridge shutting down, no new connections");
                break;
            }
        };

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
                                // Hold the permit for the connection's lifetime;
                                // dropping it on completion frees a slot.
                                let _permit = permit;
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
                        // Accept failed; release the permit we were holding.
                        drop(permit);
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
            reaped = tasks.join_next(), if !tasks.is_empty() => {
                // D5: reap completed connection tasks promptly, even while the
                // listener is otherwise idle waiting for the next accept.
                if let Some(Err(e)) = reaped {
                    error!(error = %e, "Connection task panicked");
                }
                drop(permit);
                continue;
            }
            _ = shutdown_token.cancelled() => {
                drop(permit);
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
