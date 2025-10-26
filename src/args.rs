//! Command-line argument definitions for zenoh-bridge-tcp.

use clap::Parser;

/// Command-line arguments for the Zenoh TCP Bridge
#[derive(Parser, Debug)]
#[command(author, version, about = "Simple TCP to Zenoh Bridge", long_about = None)]
pub struct Args {
    /// Path to a Zenoh configuration file (JSON5 format)
    /// If provided, this configuration will be used instead of mode/connect/listen options
    #[arg(short = 'c', long)]
    pub config: Option<String>,

    /// Export a TCP backend as Zenoh service: 'service_name/backend_addr'
    /// Example: --export 'myservice/127.0.0.1:8003'
    /// Creates lazy connections: one per importing client
    /// Can be specified multiple times for multiple exports
    #[arg(long)]
    pub export: Vec<String>,

    /// Import a Zenoh service as TCP listener: 'service_name/listen_addr'
    /// Example: --import 'myservice/127.0.0.1:8002'
    /// Listens for TCP connections and connects them to the exported service
    /// Can be specified multiple times for multiple imports
    #[arg(long)]
    pub import: Vec<String>,

    /// Zenoh configuration mode
    #[arg(short = 'm', long, default_value = "peer")]
    pub mode: String,

    /// Zenoh connect endpoint (e.g., tcp/localhost:7447)
    #[arg(short = 'e', long)]
    pub connect: Option<String>,

    /// Zenoh listen endpoint (e.g., tcp/0.0.0.0:7447)
    #[arg(short = 'l', long)]
    pub listen: Option<String>,
}

impl Args {
    /// Validate that at least one export or import is specified
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.export.is_empty() && self.import.is_empty() {
            return Err(anyhow::anyhow!(
                "Must specify at least one --export or --import. Use --help for usage."
            ));
        }
        Ok(())
    }
}
