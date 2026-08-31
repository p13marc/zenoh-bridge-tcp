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
use tracing::{Instrument, debug, error, info, info_span};
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
#[allow(clippy::too_many_arguments)] // internal, named call sites in 5 flavor modules
pub(super) async fn run_accept_loop<H, Fut>(
    cfg: AcceptLoopCfg,
    session: Arc<Session>,
    service_name: String,
    listen_addr: SocketAddr,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
    on_bound: Option<tokio::sync::oneshot::Sender<()>>,
    handler: H,
) -> Result<()>
where
    H: Fn(Arc<Session>, TcpStream, String, String, Arc<BridgeConfig>, CancellationToken) -> Fut
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

    // Readiness truth (A5): /readyz must not say 200 before this port can
    // actually accept. The sender is dropped un-sent on a bind failure, which
    // main treats as fatal.
    if let Some(tx) = on_bound {
        let _ = tx.send(());
    }

    // The requested and the bound address differ when the spec says port 0;
    // log the real one so an ephemeral port is discoverable.
    let bound_addr = listener.local_addr().unwrap_or(listen_addr);
    info!(listen_addr = %bound_addr, service = %service_name, mode = mode, "Import bridge ready");

    let mut tasks = JoinSet::new();

    // Cancelled by the drain when its voluntary window expires: every
    // connection handler receives a child of this token, so a drain can tear
    // data planes down cleanly (EOF markers, undeclares, access logs) instead
    // of aborting coordinator tasks and orphaning their relay spawns.
    let conn_shutdown = CancellationToken::new();

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
                        let conn_token = conn_shutdown.child_token();

                        // `dns` is empty until the handler resolves a routing
                        // key; recording it on the span means every later line
                        // in this connection carries the routed host without
                        // repeating it at each call site.
                        let span = info_span!(
                            "connection",
                            client_id = %client_id,
                            service = %service_name,
                            remote_addr = %addr,
                            mode = mode,
                            dns = tracing::field::Empty,
                        );

                        tasks.spawn(
                            async move {
                                // Hold the permit for the connection's lifetime;
                                // dropping it on completion frees a slot.
                                let _permit = permit;
                                if let Err(e) =
                                    handler(session, stream, service_name, client_id, config, conn_token)
                                        .await
                                {
                                    error!(error = %e, "Connection error");
                                }
                                // The access log (metrics::ConnGuard::finish)
                                // carries the outcome, byte counts and duration
                                // for connections that reached a data plane.
                                debug!("Connection task finished");
                            }
                            .instrument(span),
                        );
                    }
                    Err(e) => {
                        // Accept failed; release the permit we were holding.
                        drop(permit);
                        error!(error = %e, "Failed to accept connection");
                        // Back off: EMFILE/ENFILE do not consume the pending
                        // connection, so accept() fails again immediately — an
                        // unthrottled loop burned 100% CPU and flooded the log
                        // exactly when the process was already fd-starved.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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

    super::drain_tasks(
        &mut tasks,
        &conn_shutdown,
        &service_name,
        config.drain_timeout,
    )
    .await;

    info!(service = %service_name, mode = mode, "Import bridge stopped");
    Ok(())
}
