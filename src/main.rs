//! Zenoh TCP Bridge - Main entry point
//!
//! This bridge allows TCP services to be exposed over Zenoh and vice versa.
//! Supports multiple simultaneous imports and exports.

use zenoh_bridge_tcp::{args, config, export, import};

use anyhow::Result;
use args::Args;
use clap::Parser;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Parse command-line arguments
    let args = Args::parse();

    // Validate arguments
    args.validate()?;

    // Configure Zenoh session
    let config = if let Some(config_file) = &args.config {
        // Load configuration from file
        info!("Loading Zenoh configuration from file: {}", config_file);
        config::create_zenoh_config_from_file(config_file)?
    } else {
        // Create configuration from command-line arguments
        config::create_zenoh_config(&args.mode, args.connect.as_ref(), args.listen.as_ref())?
    };

    // Open Zenoh session
    info!("Opening Zenoh session in {} mode...", args.mode);
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

    for export_spec in args.export {
        let export_spec_clone = export_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = export::run_export_mode(session_clone, &export_spec_clone).await {
                tracing::error!("Export '{}' failed: {:?}", export_spec_clone, e);
            }
        });
        tasks.push(task);
        info!("Started export task for: {}", export_spec);
    }

    // Spawn tasks for each import specification
    for import_spec in args.import {
        let import_spec_clone = import_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = import::run_import_mode(session_clone, &import_spec_clone).await {
                tracing::error!("Import '{}' failed: {:?}", import_spec_clone, e);
            }
        });
        tasks.push(task);
        info!("Started import task for: {}", import_spec);
    }

    // Spawn tasks for each HTTP export specification
    for export_spec in args.http_export {
        let export_spec_clone = export_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = export::run_http_export_mode(session_clone, &export_spec_clone).await {
                tracing::error!("HTTP export '{}' failed: {:?}", export_spec_clone, e);
            }
        });
        tasks.push(task);
        info!("Started HTTP export task for: {}", export_spec);
    }

    // Spawn tasks for each HTTP import specification
    for import_spec in args.http_import {
        let import_spec_clone = import_spec.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = import::run_http_import_mode(session_clone, &import_spec_clone).await {
                tracing::error!("HTTP import '{}' failed: {:?}", import_spec_clone, e);
            }
        });
        tasks.push(task);
        info!("Started HTTP import task for: {}", import_spec);
    }

    info!("✓ All tasks started successfully");
    info!("  Total exports: {}", export_count);
    info!("  Total imports: {}", import_count);
    info!("  Total HTTP exports: {}", http_export_count);
    info!("  Total HTTP imports: {}", http_import_count);

    // Wait for all tasks (they should run indefinitely)
    for task in tasks {
        let _ = task.await;
    }

    Ok(())
}
