//! The one accept loop behind every import listener (#73).
//!
//! All listener flavors share the same lifecycle: bind, cap concurrency with a
//! semaphore held across the accept (D3), spawn a per-connection task that
//! owns its permit, reap completed tasks promptly even while idle (D5), and
//! drain on shutdown. What *differs* — protocol detection, TLS handshakes,
//! WebSocket upgrades, per-request routing — lives entirely inside the
//! per-connection handler, so it is a closure parameter, not a loop variant.

use crate::config::BridgeConfig;
use anyhow::Result;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span};
use zenoh::Session;

/// The per-listener constants of an accept loop.
pub(super) struct AcceptLoopCfg {
    /// Labels logs and the connection span.
    pub mode: &'static str,
    /// Seeds generated client ids (`client_`, `wsclient_`).
    pub client_id_prefix: &'static str,
}

/// Run the accept loop for one listener.
///
/// The handler is invoked inside the spawned task — handshakes and
/// per-connection errors belong to it; the loop only logs its `Err` and
/// moves on.
pub(super) async fn run_accept_loop<H, Fut>(
    cfg: AcceptLoopCfg,
    session: Arc<Session>,
    service_name: String,
    listen_addr: SocketAddr,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
    handler: H,
) -> Result<()>
where
    H: Fn(Arc<Session>, TcpStream, String, String, Arc<BridgeConfig>) -> Fut
        + Clone
        + Send
        + Sync
        + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let AcceptLoopCfg {
        mode,
        client_id_prefix,
    } = cfg;
    info!(
        mode = mode,
        service = %service_name,
        listen_addr = %listen_addr,
        "Starting import bridge"
    );

    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", listen_addr, e))?;

    info!(listen_addr = %listen_addr, service = %service_name, mode = mode, "Import bridge ready");

    let mut tasks = JoinSet::new();

    // Cap concurrent connections: hold a permit before accepting so the loop
    // applies backpressure at the limit instead of spawning without bound (D3).
    let conn_limit = Arc::new(tokio::sync::Semaphore::new(config.max_connections));

    loop {
        let permit = tokio::select! {
            p = conn_limit.clone().acquire_owned() => {
                p.expect("connection semaphore is never closed")
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, mode = mode, "Import bridge shutting down, no new connections");
                break;
            }
        };

        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let client_id =
                            format!("{}{}", client_id_prefix, uuid::Uuid::new_v4().as_simple());
                        info!(
                            client_id = %client_id,
                            remote_addr = %addr,
                            mode = mode,
                            "New connection"
                        );

                        let session = session.clone();
                        let service_name = service_name.clone();
                        let config = config.clone();
                        let handler = handler.clone();

                        let span = info_span!(
                            "connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                            mode = mode,
                        );

                        tasks.spawn(
                            async move {
                                // Hold the permit for the connection's lifetime;
                                // dropping it on completion frees a slot.
                                let _permit = permit;
                                if let Err(e) =
                                    handler(session, stream, service_name, client_id, config).await
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
                info!(service = %service_name, mode = mode, "Import bridge shutting down, no new connections");
                break;
            }
        }

        // Reap completed tasks to prevent unbounded growth
        while tasks.try_join_next().is_some() {}
    }

    super::drain_tasks(&mut tasks, &service_name, config.drain_timeout).await;

    info!(service = %service_name, mode = mode, "Import bridge stopped");
    Ok(())
}
