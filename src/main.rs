//! Zenoh TCP Bridge - Main entry point
//!
//! This bridge allows TCP services to be exposed over Zenoh and vice versa.
//! Supports multiple simultaneous imports and exports.

use zenoh_bridge_tcp::{args, config, export, import, logging, metrics};

use anyhow::Result;
use args::Args;
use clap::Parser;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Wait for SIGINT or SIGTERM
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Spawn bridge tasks from a list of parsed specs using a factory closure
fn spawn_bridge_tasks<S, F, Fut>(
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    specs: Vec<S>,
    mode: &'static str,
    shutdown_token: &CancellationToken,
    factory: F,
) where
    S: std::fmt::Display + Clone + Send + 'static,
    F: Fn(S, CancellationToken) -> Fut + Send + 'static + Clone,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    for spec in specs {
        let token = shutdown_token.child_token();
        let factory = factory.clone();
        debug!(mode = mode, spec = %spec, "Spawning task");
        tasks.push(tokio::spawn(async move {
            let label = spec.to_string();
            if let Err(e) = factory(spec, token).await {
                tracing::error!(mode = %mode, spec = %label, error = %e, "Task failed");
            }
        }));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Validate before installing the subscriber: validation emits no logs, and
    // doing it the other way round meant a bad --log-format silently installed
    // a fallback subscriber before being rejected.
    args.validate()?;

    // Held for the whole process: dropping the guards stops the background
    // file writers and truncates whatever they had buffered.
    let _log_guards = logging::init(&args.log_options()?)?;

    // Configure Zenoh session
    let config = if let Some(config_file) = &args.zenoh_config {
        if args.zenoh_mode != "peer" || args.zenoh_connect.is_some() || args.zenoh_listen.is_some()
        {
            warn!(
                "Zenoh config file provided; --zenoh-mode, --zenoh-connect, and --zenoh-listen will be ignored"
            );
        }
        info!(config_file = %config_file, "Loading Zenoh configuration from file");
        config::create_zenoh_config_from_file(config_file)?
    } else {
        config::create_zenoh_config(
            &args.zenoh_mode,
            args.zenoh_connect.as_ref(),
            args.zenoh_listen.as_ref(),
        )?
    };

    // Open Zenoh session
    info!(mode = %args.zenoh_mode, "Opening Zenoh session");
    let session = Arc::new(
        zenoh::open(config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to open Zenoh session: {}", e))?,
    );
    info!("Zenoh session established");

    // Create a global cancellation token
    let shutdown_token = CancellationToken::new();

    // Spawn the health/metrics server if requested (G7). Liveness is up
    // immediately; readiness is flipped on once all bridge tasks are started.
    if let Some(metrics_addr) = args.metrics_addr {
        let token = shutdown_token.clone();
        tokio::spawn(async move {
            if let Err(e) = metrics::serve(metrics_addr, token).await {
                tracing::error!(addr = %metrics_addr, error = %e, "Metrics server failed to bind");
            }
        });
    }

    // Spawn signal handler
    let signal_token = shutdown_token.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received, initiating graceful shutdown");
        signal_token.cancel();
    });

    // Spawn tasks for each attachment point. Specs were validated already;
    // parse them into their structured form.
    let listens = args.listen_specs()?;
    let backends = args.backend_specs()?;
    let listener_count = listens.len();
    let backend_count = backends.len();

    let mut tasks = Vec::new();
    let bridge_config = Arc::new(args.bridge_config());

    // Listener tasks: each --listen dispatches on its parsed options
    // (auto/raw, passthrough/terminate, per-connection/per-request). TLS
    // material, if any, is loaded lazily inside run_listener, per listener.
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(&mut tasks, listens, "listen", &shutdown_token, {
            move |spec, token| {
                let session = session.clone();
                let config = bridge_config.clone();
                async move { import::run_listener(session, spec, config, token).await }
            }
        });
    }

    // Backend tasks: each --backend exposes a local target onto the bus.
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(&mut tasks, backends, "backend", &shutdown_token, {
            move |spec, token| {
                let session = session.clone();
                let config = bridge_config.clone();
                async move { export::run_backend(session, spec, config, token).await }
            }
        });
    }

    info!(
        listeners = listener_count,
        backends = backend_count,
        "All bridge tasks started"
    );

    // Bridges are up: report readiness on /readyz.
    metrics::metrics().set_ready(true);

    let drain_timeout = tokio::time::Duration::from_secs(args.drain_timeout);

    // Wait for a shutdown signal — or detect that every bridge task exited on its
    // own first. Bridges run until cancelled, so if they all finish while no
    // shutdown was requested, no listeners remain (e.g. every bind failed with
    // EADDRINUSE); fail fast rather than linger as a live process doing nothing.
    let drain_all = async move {
        for task in tasks {
            let _ = task.await;
        }
    };
    tokio::pin!(drain_all);

    tokio::select! {
        _ = shutdown_token.cancelled() => {
            info!(
                drain_timeout_s = args.drain_timeout,
                "Waiting for tasks to drain"
            );
            let _ = tokio::time::timeout(drain_timeout, &mut drain_all).await;
        }
        _ = &mut drain_all => {
            tracing::error!(
                "All bridge tasks exited before a shutdown signal; no listeners remain — exiting"
            );
            if let Err(e) = session.close().await {
                warn!(error = %e, "Error closing Zenoh session");
            }
            std::process::exit(1);
        }
    }

    // Close Zenoh session explicitly
    if let Err(e) = session.close().await {
        warn!(error = %e, "Error closing Zenoh session");
    }

    info!("Shutdown complete");
    Ok(())
}
