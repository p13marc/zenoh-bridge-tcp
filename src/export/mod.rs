//! Export mode implementation for the Zenoh TCP Bridge.
//!
//! This module handles exporting TCP backend services as Zenoh services.
//! Each export creates lazy connections to the backend - one connection per importing client.
//!
//! Supports regular TCP mode, HTTP-aware mode with DNS-based routing, and WebSocket mode.
//!
//! ## Error handling strategy
//!
//! - **Startup errors** (parse, liveliness, subscribe): propagated via `?`, fatal to the service.
//! - **Per-client errors** (backend connect, read/write, publish): logged, close that connection
//!   only. Backend connect uses exponential backoff (100ms-5s, 5 retries) before notifying the
//!   import side via the error channel.
//! - **Shutdown**: each task direction has a `CancellationToken`; the outer select cancels the
//!   peer token and waits up to `drain_timeout` before a final `.abort()` fallback.

mod bridge;
mod tcp;
mod ws;

#[cfg(test)]
mod tests;

use crate::config::BridgeConfig;
use crate::http_parser::normalize_dns;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zenoh::Session;

/// Type alias for cancellation sender and task handle
pub(super) type CancellationSender = (mpsc::Sender<()>, tokio::task::JoinHandle<()>);

/// Backend type for export mode, determines how client connections are established
pub(super) enum ExportBackend {
    Tcp {
        addr: SocketAddr,
        dns_suffix: Option<String>,
    },
    WebSocket(String),
}

/// Parse export specification in format 'service_name/backend_addr'
pub fn parse_export_spec(export_spec: &str) -> Result<(String, SocketAddr)> {
    let parts: Vec<&str> = export_spec.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!(
            "Invalid export format. Expected: 'service_name/backend_addr' (e.g., 'myservice/127.0.0.1:8003')"
        ));
    }

    let service_name = parts[0].to_string();
    let backend_addr: SocketAddr = parts[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid backend address: {}", e))?;

    Ok((service_name, backend_addr))
}

/// Parse HTTP export specification in format 'service_name/dns/backend_addr'
pub fn parse_http_export_spec(export_spec: &str) -> Result<(String, String, SocketAddr)> {
    let parts: Vec<&str> = export_spec.split('/').collect();
    if parts.len() != 3 {
        return Err(anyhow::anyhow!(
            "Invalid HTTP export format. Expected: 'service_name/dns/backend_addr' (e.g., 'http-service/api.example.com/127.0.0.1:8003')"
        ));
    }

    let service_name = parts[0].to_string();
    let dns = normalize_dns(parts[1]);
    let backend_addr: SocketAddr = parts[2]
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid backend address: {}", e))?;

    Ok((service_name, dns, backend_addr))
}

/// Parse WebSocket export specification in format 'service_name/ws_url'
///
/// The ws_url should be a full WebSocket URL like 'ws://127.0.0.1:9000' or 'wss://example.com:443'
pub fn parse_ws_export_spec(export_spec: &str) -> Result<(String, String)> {
    // Split on first '/' only to preserve the ws:// or wss:// in the URL
    let slash_pos = export_spec
        .find('/')
        .ok_or_else(|| anyhow::anyhow!(
            "Invalid WebSocket export format. Expected: 'service_name/ws://host:port' (e.g., 'myws/ws://127.0.0.1:9000')"
        ))?;

    let service_name = export_spec[..slash_pos].to_string();
    let ws_url = export_spec[slash_pos + 1..].to_string();

    // Validate that the URL starts with ws:// or wss://
    if !ws_url.starts_with("ws://") && !ws_url.starts_with("wss://") {
        return Err(anyhow::anyhow!(
            "Invalid WebSocket URL: must start with 'ws://' or 'wss://'"
        ));
    }

    if service_name.is_empty() {
        return Err(anyhow::anyhow!("Service name cannot be empty"));
    }

    Ok((service_name, ws_url))
}

/// Run export mode for a single service
///
/// This function:
/// 1. Monitors client liveliness tokens
/// 2. Creates a backend connection when a client appears
/// 3. Bridges data between the backend and Zenoh
/// 4. Cleans up when clients disconnect
pub async fn run_export_mode(
    session: Arc<Session>,
    export_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, backend_addr) = parse_export_spec(export_spec)?;
    bridge::run_export_loop(
        session,
        &service_name,
        ExportBackend::Tcp {
            addr: backend_addr,
            dns_suffix: None,
        },
        config,
        shutdown_token,
    )
    .await
}

/// Run HTTP-aware export mode for a single service with DNS-based routing
///
/// This function:
/// 1. Registers a backend for a specific DNS name
/// 2. Monitors client liveliness tokens for that DNS
/// 3. Creates backend connections when clients appear
/// 4. Bridges data between the backend and Zenoh
/// 5. Cleans up when clients disconnect
pub async fn run_http_export_mode(
    session: Arc<Session>,
    export_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, dns, backend_addr) = parse_http_export_spec(export_spec)?;
    bridge::run_export_loop(
        session,
        &service_name,
        ExportBackend::Tcp {
            addr: backend_addr,
            dns_suffix: Some(dns),
        },
        config,
        shutdown_token,
    )
    .await
}

/// Run WebSocket export mode for a single service
///
/// This function:
/// 1. Monitors client liveliness tokens
/// 2. Creates a WebSocket connection to the backend when a client appears
/// 3. Bridges data between the WebSocket backend and Zenoh
/// 4. Cleans up when clients disconnect
pub async fn run_ws_export_mode(
    session: Arc<Session>,
    export_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, ws_url) = parse_ws_export_spec(export_spec)?;
    bridge::run_export_loop(
        session,
        &service_name,
        ExportBackend::WebSocket(ws_url),
        config,
        shutdown_token,
    )
    .await
}
