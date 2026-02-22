//! Zenoh TCP Bridge - Main entry point
//!
//! This bridge allows TCP services to be exposed over Zenoh and vice versa.
//! Supports multiple simultaneous imports and exports.

use zenoh_bridge_tcp::{args, config, export, import};

use anyhow::Result;
use args::Args;
use clap::Parser;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber based on CLI arguments
fn init_tracing(log_level: &str, log_format: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    match log_format {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        "compact" => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .compact()
                .init();
        }
        _ => {
            // "pretty" or default
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}

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

/// Spawn bridge tasks from a list of specs using a factory closure
fn spawn_bridge_tasks<F, Fut>(
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    specs: &[String],
    mode: &'static str,
    shutdown_token: &CancellationToken,
    factory: F,
) where
    F: Fn(String, CancellationToken) -> Fut + Send + 'static + Clone,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    for spec in specs {
        let token = shutdown_token.child_token();
        let spec_clone = spec.clone();
        let factory = factory.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = factory(spec_clone.clone(), token).await {
                tracing::error!(mode = %mode, spec = %spec_clone, error = %e, "Task failed");
            }
        }));
        debug!(mode = mode, spec = %spec, "Spawned task");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments first (before tracing init)
    let args = Args::parse();

    // Initialize tracing with configured level and format
    init_tracing(&args.log_level, &args.log_format);

    // Validate arguments
    args.validate()?;

    // Configure Zenoh session
    let config = if let Some(config_file) = &args.config {
        // Load configuration from file
        info!(config_file = %config_file, "Loading Zenoh configuration from file");
        config::create_zenoh_config_from_file(config_file)?
    } else {
        // Create configuration from command-line arguments
        config::create_zenoh_config(&args.mode, args.connect.as_ref(), args.listen.as_ref())?
    };

    // Open Zenoh session
    info!(mode = %args.mode, "Opening Zenoh session");
    let session = Arc::new(
        zenoh::open(config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to open Zenoh session: {}", e))?,
    );
    info!("Zenoh session established");

    // Create a global cancellation token
    let shutdown_token = CancellationToken::new();

    // Spawn signal handler
    let signal_token = shutdown_token.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received, initiating graceful shutdown...");
        signal_token.cancel();
    });

    // Spawn tasks for each specification
    let mut tasks = Vec::new();
    let export_count = args.export.len();
    let import_count = args.import.len();
    let http_export_count = args.http_export.len();
    let http_import_count = args.http_import.len();
    let ws_export_count = args.ws_export.len();
    let ws_import_count = args.ws_import.len();
    let auto_import_count = args.auto_import.len();
    let http_multiroute_import_count = args.http_multiroute_import.len();

    let bridge_config = Arc::new(args.bridge_config());

    // TCP export tasks
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(&mut tasks, &args.export, "export", &shutdown_token, {
            move |spec, token| {
                let session = session.clone();
                let config = bridge_config.clone();
                async move { export::run_export_mode(session, &spec, config, token).await }
            }
        });
    }

    // TCP import tasks
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(&mut tasks, &args.import, "import", &shutdown_token, {
            move |spec, token| {
                let session = session.clone();
                let config = bridge_config.clone();
                async move { import::run_import_mode(session, &spec, config, token).await }
            }
        });
    }

    // HTTP export tasks
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(
            &mut tasks,
            &args.http_export,
            "http_export",
            &shutdown_token,
            {
                move |spec, token| {
                    let session = session.clone();
                    let config = bridge_config.clone();
                    async move {
                        export::run_http_export_mode(session, &spec, config, token).await
                    }
                }
            },
        );
    }

    // HTTP import tasks
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(
            &mut tasks,
            &args.http_import,
            "http_import",
            &shutdown_token,
            {
                move |spec, token| {
                    let session = session.clone();
                    let config = bridge_config.clone();
                    async move {
                        import::run_http_import_mode(session, &spec, config, token).await
                    }
                }
            },
        );
    }

    // WebSocket export tasks
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(&mut tasks, &args.ws_export, "ws_export", &shutdown_token, {
            move |spec, token| {
                let session = session.clone();
                let config = bridge_config.clone();
                async move { export::run_ws_export_mode(session, &spec, config, token).await }
            }
        });
    }

    // WebSocket import tasks
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(&mut tasks, &args.ws_import, "ws_import", &shutdown_token, {
            move |spec, token| {
                let session = session.clone();
                let config = bridge_config.clone();
                async move { import::run_ws_import_mode(session, &spec, config, token).await }
            }
        });
    }

    // Auto-detecting import tasks
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(
            &mut tasks,
            &args.auto_import,
            "auto_import",
            &shutdown_token,
            {
                move |spec, token| {
                    let session = session.clone();
                    let config = bridge_config.clone();
                    async move {
                        import::run_auto_import_mode(session, &spec, config, token).await
                    }
                }
            },
        );
    }

    // HTTP multiroute import tasks
    {
        let session = session.clone();
        let bridge_config = bridge_config.clone();
        spawn_bridge_tasks(
            &mut tasks,
            &args.http_multiroute_import,
            "http_multiroute_import",
            &shutdown_token,
            {
                move |spec, token| {
                    let session = session.clone();
                    let config = bridge_config.clone();
                    async move {
                        import::run_http_multiroute_import_mode(session, &spec, config, token)
                            .await
                    }
                }
            },
        );
    }

    // HTTPS termination import tasks (feature-gated)
    #[cfg(feature = "tls-termination")]
    let https_terminate_count = args.https_terminate.len();
    #[cfg(not(feature = "tls-termination"))]
    let https_terminate_count = 0;

    #[cfg(feature = "tls-termination")]
    if !args.https_terminate.is_empty() {
        let tls_config = zenoh_bridge_tcp::tls_config::load_tls_config(
            args.tls_cert.as_ref().unwrap(),
            args.tls_key.as_ref().unwrap(),
        )?;

        for spec in &args.https_terminate {
            let session = session.clone();
            let spec_clone = spec.clone();
            let tls_config = tls_config.clone();
            let config = bridge_config.clone();
            let token = shutdown_token.child_token();
            tasks.push(tokio::spawn(async move {
                if let Err(e) = import::run_https_terminate_import_mode(
                    session,
                    &spec_clone,
                    tls_config,
                    config,
                    token,
                )
                .await
                {
                    tracing::error!(mode = "https_terminate", spec = %spec_clone, error = %e, "Task failed");
                }
            }));
            debug!(mode = "https_terminate", spec = %spec, "Spawned task");
        }
    }

    info!(
        exports = export_count,
        imports = import_count,
        http_exports = http_export_count,
        http_imports = http_import_count,
        ws_exports = ws_export_count,
        ws_imports = ws_import_count,
        auto_imports = auto_import_count,
        http_multiroute_imports = http_multiroute_import_count,
        https_terminates = https_terminate_count,
        "All bridge tasks started"
    );

    // Wait for shutdown signal
    shutdown_token.cancelled().await;

    info!("Waiting for tasks to drain (max 10 seconds)...");
    let drain_timeout = tokio::time::Duration::from_secs(10);

    // Wait for all tasks to finish with timeout
    let _ = tokio::time::timeout(drain_timeout, async {
        for task in tasks {
            let _ = task.await;
        }
    })
    .await;

    // Close Zenoh session explicitly
    if let Err(e) = session.close().await {
        warn!("Error closing Zenoh session: {}", e);
    }

    info!("Shutdown complete");
    Ok(())
}
