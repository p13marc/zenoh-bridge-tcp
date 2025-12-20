//! Zenoh TCP Bridge - Main entry point
//!
//! This bridge allows TCP services to be exposed over Zenoh and vice versa.
//! Supports multiple simultaneous imports and exports.

use zenoh_bridge_tcp::{args, config, export, import};

use anyhow::Result;
use args::Args;
use clap::Parser;
use std::sync::Arc;
use tracing::{debug, info};
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

    // Spawn tasks for each export specification
    let mut tasks = Vec::new();
    let export_count = args.export.len();
    let import_count = args.import.len();
    let http_export_count = args.http_export.len();
    let http_import_count = args.http_import.len();
    let ws_export_count = args.ws_export.len();
    let ws_import_count = args.ws_import.len();

    for export_spec in args.export {
        let export_spec_clone = export_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = export::run_export_mode(session_clone, &export_spec_clone).await {
                tracing::error!(spec = %export_spec_clone, error = %e, "Export task failed");
            }
        });
        tasks.push(task);
        debug!(mode = "export", spec = %export_spec, "Spawned task");
    }

    // Spawn tasks for each import specification
    for import_spec in args.import {
        let import_spec_clone = import_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = import::run_import_mode(session_clone, &import_spec_clone).await {
                tracing::error!(spec = %import_spec_clone, error = %e, "Import task failed");
            }
        });
        tasks.push(task);
        debug!(mode = "import", spec = %import_spec, "Spawned task");
    }

    // Spawn tasks for each HTTP export specification
    for export_spec in args.http_export {
        let export_spec_clone = export_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = export::run_http_export_mode(session_clone, &export_spec_clone).await {
                tracing::error!(spec = %export_spec_clone, error = %e, "HTTP export task failed");
            }
        });
        tasks.push(task);
        debug!(mode = "http_export", spec = %export_spec, "Spawned task");
    }

    // Spawn tasks for each HTTP import specification
    for import_spec in args.http_import {
        let import_spec_clone = import_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = import::run_http_import_mode(session_clone, &import_spec_clone).await {
                tracing::error!(spec = %import_spec_clone, error = %e, "HTTP import task failed");
            }
        });
        tasks.push(task);
        debug!(mode = "http_import", spec = %import_spec, "Spawned task");
    }

    // Spawn tasks for each WebSocket export specification
    for export_spec in args.ws_export {
        let export_spec_clone = export_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = export::run_ws_export_mode(session_clone, &export_spec_clone).await {
                tracing::error!(spec = %export_spec_clone, error = %e, "WebSocket export task failed");
            }
        });
        tasks.push(task);
        debug!(mode = "ws_export", spec = %export_spec, "Spawned task");
    }

    // Spawn tasks for each WebSocket import specification
    for import_spec in args.ws_import {
        let import_spec_clone = import_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = import::run_ws_import_mode(session_clone, &import_spec_clone).await {
                tracing::error!(spec = %import_spec_clone, error = %e, "WebSocket import task failed");
            }
        });
        tasks.push(task);
        debug!(mode = "ws_import", spec = %import_spec, "Spawned task");
    }

    info!(
        exports = export_count,
        imports = import_count,
        http_exports = http_export_count,
        http_imports = http_import_count,
        ws_exports = ws_export_count,
        ws_imports = ws_import_count,
        "All bridge tasks started"
    );

    // Wait for all tasks (they should run indefinitely)
    for task in tasks {
        let _ = task.await;
    }

    Ok(())
}
