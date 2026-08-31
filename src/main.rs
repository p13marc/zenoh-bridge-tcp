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

    // Spawn the health/metrics server if requested (G7), on a CHILD token that
    // main cancels only AFTER the drain: during shutdown /readyz must answer
    // 503 (drain in progress) rather than connection-refused, or a load
    // balancer keeps routing to a port that no longer answers.
    let metrics_token = CancellationToken::new();
    let mut metrics_task = None;
    if let Some(metrics_addr) = args.metrics_addr {
        // Bind here, fatally: a taken metrics port must fail startup the way a
        // taken data port does — not leave the process "ready" with its health
        // endpoint connection-refused.
        let listener = metrics::bind(metrics_addr)
            .await
            .map_err(|e| anyhow::anyhow!("failed to bind --metrics-addr {metrics_addr}: {e}"))?;
        let token = metrics_token.clone();
        // Same budget the data-plane head readers use, so an idle client cannot
        // pin a task+fd on the observability port either.
        let read_timeout = std::time::Duration::from_secs(args.read_timeout);
        metrics_task = Some(tokio::spawn(async move {
            if let Err(e) = metrics::serve_on(listener, read_timeout, token).await {
                tracing::error!(addr = %metrics_addr, error = %e, "Metrics server failed");
            }
        }));
    }

    // Spawn signal handler (tracked so it is aborted on exit).
    let signal_token = shutdown_token.clone();
    let signal_task = tokio::spawn(async move {
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
    // Each listener reports through a oneshot once its socket is ACCEPTING;
    // readiness is only declared when all of them have.
    let mut bound_rxs = Vec::with_capacity(listener_count);
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        for spec in listens {
            let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
            bound_rxs.push((spec.to_string(), bound_rx));
            let token = shutdown_token.child_token();
            let session = session.clone();
            let config = bridge_config.clone();
            debug!(mode = "listen", spec = %spec, "Spawning task");
            tasks.push(tokio::spawn(async move {
                let label = spec.to_string();
                if let Err(e) = import::run_listener_with_readiness(
                    session,
                    spec,
                    config,
                    token,
                    Some(bound_tx),
                )
                .await
                {
                    tracing::error!(mode = "listen", spec = %label, error = %e, "Task failed");
                }
            }));
        }
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

    // Readiness truth (A5): wait for every listener to actually bind before
    // /readyz says 200. A dropped sender means that listener FAILED to bind
    // (EADDRINUSE, bad cert, ...) — a configuration error; fail the process
    // rather than run partially deaf with readiness green.
    for (spec, bound_rx) in bound_rxs {
        match tokio::time::timeout(std::time::Duration::from_secs(30), bound_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                shutdown_token.cancel();
                metrics_token.cancel();
                signal_task.abort();
                return Err(anyhow::anyhow!(
                    "listener '{spec}' failed to start; aborting (see the error above)"
                ));
            }
            Err(_) => {
                shutdown_token.cancel();
                metrics_token.cancel();
                signal_task.abort();
                return Err(anyhow::anyhow!(
                    "listener '{spec}' did not become ready within 30s; aborting"
                ));
            }
        }
    }

    info!(
        listeners = listener_count,
        backends = backend_count,
        "All bridge tasks started"
    );

    // Every listener is accepting: report readiness on /readyz.
    metrics::metrics().set_ready(true);

    // Outer budget exceeds the accept loops' own drain_timeout so their
    // orderly drain (cancel connections, wait, abort leftovers) actually gets
    // to run — with equal budgets the outer timer always fired first and the
    // session was closed under still-relaying tasks.
    let drain_budget =
        tokio::time::Duration::from_secs(args.drain_timeout) + tokio::time::Duration::from_secs(2);

    // Wait for a shutdown signal — or detect that every bridge task exited on
    // its own first (bridges run until cancelled; all of them finishing with no
    // shutdown requested means nothing remains to serve).
    let drain_all = async move {
        for task in tasks {
            let _ = task.await;
        }
    };
    tokio::pin!(drain_all);

    let clean = tokio::select! {
        _ = shutdown_token.cancelled() => {
            // Stop advertising readiness the moment shutdown begins: the LB
            // must stop sending new work while existing connections drain.
            metrics::metrics().set_ready(false);
            info!(
                drain_timeout_s = args.drain_timeout,
                "Waiting for tasks to drain"
            );
            let _ = tokio::time::timeout(drain_budget, &mut drain_all).await;
            true
        }
        _ = &mut drain_all => {
            tracing::error!(
                "All bridge tasks exited before a shutdown signal; no listeners remain — exiting"
            );
            metrics::metrics().set_ready(false);
            false
        }
    };

    // Close Zenoh session explicitly, stop the observability server last, and
    // return through main so the log guards flush (process::exit here used to
    // discard the very error being reported when a file sink was buffered).
    if let Err(e) = session.close().await {
        warn!(error = %e, "Error closing Zenoh session");
    }
    metrics_token.cancel();
    if let Some(handle) = metrics_task {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }
    signal_task.abort();

    if clean {
        info!("Shutdown complete");
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "all bridge tasks exited unexpectedly; nothing left to serve"
        ))
    }
}
