//! Import mode implementation for the Zenoh TCP Bridge.
//!
//! This module handles importing Zenoh services as TCP listeners.
//! Each TCP connection gets its own liveliness token and dedicated Zenoh pub/sub.
//!
//! Supports regular TCP mode, HTTP-aware mode with DNS-based routing,
//! HTTPS/TLS with SNI-based routing, and WebSocket mode.
//!
//! ## Error handling strategy
//!
//! - **Startup errors** (parse, bind, liveliness): propagated via `?`, fatal to the service.
//! - **Accept-loop errors**: logged at `error!`, loop continues (temporary failures like EMFILE
//!   should not crash the listener).
//! - **Per-connection errors** (read/write, publish, backend unavailable): logged, close that
//!   connection only; other connections are unaffected.
//! - **Multiroute mode**: response buffering is bounded by `config.max_response_size`; oversized
//!   responses get HTTP 502 (or connection close if data was already streamed).
//! - **Shutdown**: each task direction has a `CancellationToken`; the outer select cancels the
//!   peer token and waits up to `drain_timeout` before a final `.abort()` fallback.

mod auto;
mod bridge;
mod connection;
mod listener;
mod multiroute;
#[cfg(feature = "tls-termination")]
mod tls;
mod ws;

#[cfg(test)]
mod tests;

use crate::config::BridgeConfig;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use zenoh::Session;

/// Parse import specification in format 'service_name/listen_addr'
pub fn parse_import_spec(import_spec: &str) -> Result<(String, SocketAddr)> {
    let parts: Vec<&str> = import_spec.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!(
            "Invalid import format. Expected: 'service_name/listen_addr' (e.g., 'myservice/127.0.0.1:8002')"
        ));
    }

    let service_name = parts[0].to_string();
    let listen_addr: SocketAddr = parts[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid listen address: {}", e))?;

    Ok((service_name, listen_addr))
}

/// Drain active connection tasks on shutdown.
///
/// Waits up to `drain_timeout` for all tasks to complete, then aborts any remaining.
async fn drain_tasks(tasks: &mut JoinSet<()>, service_name: &str, drain_timeout: Duration) {
    if tasks.is_empty() {
        return;
    }
    info!(service = %service_name, count = tasks.len(), "Draining active connections");
    let deadline = tokio::time::Instant::now() + drain_timeout;
    while !tasks.is_empty() {
        match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(e))) => {
                warn!(service = %service_name, error = %e, "Connection task error during drain")
            }
            Ok(None) => break,
            Err(_) => {
                warn!(service = %service_name, remaining = tasks.len(), "Drain timeout, aborting remaining connections");
                tasks.abort_all();
                break;
            }
        }
    }
}

/// Run import mode for a single service
///
/// This function:
/// 1. Binds a TCP listener on the specified address
/// 2. Accepts incoming TCP connections
/// 3. For each connection, spawns a handler that bridges to Zenoh
pub async fn run_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    listener::run_import_mode_internal(session, import_spec, false, config, shutdown_token).await
}

/// Run HTTP-aware import mode for a single service
///
/// This function:
/// 1. Binds a TCP listener on the specified address
/// 2. Accepts incoming HTTP connections
/// 3. Parses the Host header to determine DNS-based routing
/// 4. For each connection, spawns a handler that bridges to Zenoh with DNS keys
pub async fn run_http_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    listener::run_import_mode_internal(session, import_spec, true, config, shutdown_token).await
}

/// Run HTTP import mode with per-request routing.
///
/// Each HTTP request on a persistent connection is independently routed
/// based on its Host header. This allows a single client TCP connection
/// to reach multiple backends on a single port.
pub async fn run_http_multiroute_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    multiroute::run_http_multiroute_import_mode(session, import_spec, config, shutdown_token).await
}

/// Run WebSocket import mode for a single service
///
/// This function:
/// 1. Binds a TCP listener on the specified address
/// 2. Accepts incoming connections and upgrades them to WebSocket
/// 3. For each connection, spawns a handler that bridges to Zenoh
pub async fn run_ws_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    ws::run_ws_import_mode(session, import_spec, config, shutdown_token).await
}

/// Run auto-detecting import mode for a single service.
///
/// This function:
/// 1. Binds a TCP listener
/// 2. Peeks at each incoming connection's first bytes
/// 3. Dispatches to the appropriate handler (TLS, HTTP, WebSocket, or raw TCP)
pub async fn run_auto_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    auto::run_auto_import_mode(session, import_spec, config, shutdown_token).await
}

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
#[cfg(feature = "tls-termination")]
pub async fn run_https_terminate_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    tls_config: Arc<rustls::ServerConfig>,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    tls::run_https_terminate_import_mode(session, import_spec, tls_config, config, shutdown_token)
        .await
}
