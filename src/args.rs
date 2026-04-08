//! Command-line argument definitions for zenoh-bridge-tcp.

use crate::config::BridgeConfig;
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

    /// Export HTTP backend with DNS-based routing: 'service_name/dns/backend_addr'
    /// Example: --http-export 'http-service/api.example.com/127.0.0.1:8003'
    /// Registers backend for specific DNS name extracted from HTTP Host headers
    /// Can be specified multiple times for multiple DNS-based exports
    #[arg(long)]
    pub http_export: Vec<String>,

    /// Import HTTP service with DNS-based routing: 'service_name/listen_addr'
    /// Example: --http-import 'http-service/0.0.0.0:8080'
    /// Parses HTTP Host header to route requests to appropriate backends
    /// Can be specified multiple times for multiple HTTP listeners
    #[arg(long)]
    pub http_import: Vec<String>,

    /// Export WebSocket backend as Zenoh service: 'service_name/ws_url'
    /// Example: --ws-export 'myws/ws://127.0.0.1:9000'
    /// Connects to WebSocket backend when clients appear
    /// Can be specified multiple times for multiple WebSocket exports
    #[arg(long)]
    pub ws_export: Vec<String>,

    /// Import Zenoh service as WebSocket listener: 'service_name/listen_addr'
    /// Example: --ws-import 'myws/0.0.0.0:8080'
    /// Accepts WebSocket connections and bridges them to Zenoh
    /// Can be specified multiple times for multiple WebSocket imports
    #[arg(long)]
    pub ws_import: Vec<String>,

    /// Auto-detecting import: 'service_name/listen_addr'
    /// Example: --auto-import 'myservice/0.0.0.0:8080'
    /// Detects protocol (TLS/HTTPS, HTTP, WebSocket, raw TCP) from first bytes
    /// and dispatches to the appropriate handler automatically
    /// Can be specified multiple times for multiple auto-detect listeners
    #[arg(long)]
    pub auto_import: Vec<String>,

    /// Import HTTP service with per-request routing: 'service_name/listen_addr'
    /// Example: --http-multiroute-import 'http-service/0.0.0.0:8080'
    /// Each request's Host header is evaluated independently for routing,
    /// allowing persistent HTTP/1.1 connections to reach multiple backends
    #[arg(long)]
    pub http_multiroute_import: Vec<String>,

    /// Import HTTPS service with TLS termination: 'service_name/listen_addr'
    /// Example: --https-terminate 'https-service/0.0.0.0:8443'
    /// Terminates TLS at the bridge; backends receive plaintext HTTP
    /// Requires --tls-cert and --tls-key
    #[arg(long)]
    #[cfg(feature = "tls-termination")]
    pub https_terminate: Vec<String>,

    /// Path to PEM-encoded TLS certificate chain (required for --https-terminate)
    #[arg(long)]
    #[cfg(feature = "tls-termination")]
    pub tls_cert: Option<String>,

    /// Path to PEM-encoded TLS private key (required for --https-terminate)
    #[arg(long)]
    #[cfg(feature = "tls-termination")]
    pub tls_key: Option<String>,

    /// Zenoh configuration mode
    #[arg(short = 'm', long, default_value = "peer")]
    pub mode: String,

    /// Zenoh connect endpoint (e.g., tcp/localhost:7447)
    #[arg(short = 'e', long)]
    pub connect: Option<String>,

    /// Zenoh listen endpoint (e.g., tcp/0.0.0.0:7447)
    #[arg(short = 'l', long)]
    pub listen: Option<String>,

    /// Buffer size for TCP read/write operations in bytes
    #[arg(long, default_value = "65536")]
    pub buffer_size: usize,

    /// Timeout for reading HTTP/TLS headers in seconds
    #[arg(long, default_value = "10")]
    pub read_timeout: u64,

    /// Timeout in seconds for draining buffered data when a connection closes
    #[arg(long, default_value = "5")]
    pub drain_timeout: u64,

    /// Log level: trace, debug, info, warn, error
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Log format: pretty, compact, json
    #[arg(long, default_value = "pretty")]
    pub log_format: String,
}

#[cfg(test)]
impl Default for Args {
    fn default() -> Self {
        Self {
            config: None,
            export: Vec::new(),
            import: Vec::new(),
            http_export: Vec::new(),
            http_import: Vec::new(),
            ws_export: Vec::new(),
            ws_import: Vec::new(),
            auto_import: Vec::new(),
            http_multiroute_import: Vec::new(),
            #[cfg(feature = "tls-termination")]
            https_terminate: Vec::new(),
            #[cfg(feature = "tls-termination")]
            tls_cert: None,
            #[cfg(feature = "tls-termination")]
            tls_key: None,
            mode: "peer".to_string(),
            connect: None,
            listen: None,
            buffer_size: 65536,
            read_timeout: 10,
            drain_timeout: 5,
            log_level: "info".to_string(),
            log_format: "pretty".to_string(),
        }
    }
}

impl Args {
    /// Validate command-line arguments
    pub fn validate(&self) -> anyhow::Result<()> {
        let has_spec = !self.export.is_empty()
            || !self.import.is_empty()
            || !self.http_export.is_empty()
            || !self.http_import.is_empty()
            || !self.ws_export.is_empty()
            || !self.ws_import.is_empty()
            || !self.auto_import.is_empty()
            || !self.http_multiroute_import.is_empty();

        #[cfg(feature = "tls-termination")]
        let has_spec = has_spec || !self.https_terminate.is_empty();

        if !has_spec {
            return Err(anyhow::anyhow!(
                "Must specify at least one --export, --import, --http-export, --http-import, --ws-export, or --ws-import. Use --help for usage."
            ));
        }

        #[cfg(feature = "tls-termination")]
        if !self.https_terminate.is_empty() && (self.tls_cert.is_none() || self.tls_key.is_none()) {
            return Err(anyhow::anyhow!(
                "--https-terminate requires both --tls-cert and --tls-key"
            ));
        }

        // Validate buffer_size
        if self.buffer_size < 1024 {
            return Err(anyhow::anyhow!(
                "--buffer-size must be at least 1024 (got {})",
                self.buffer_size
            ));
        }

        // Validate drain_timeout
        if self.drain_timeout < 1 {
            return Err(anyhow::anyhow!(
                "--drain-timeout must be at least 1 second (got {})",
                self.drain_timeout
            ));
        }

        // Validate log_format
        match self.log_format.as_str() {
            "pretty" | "compact" | "json" => {}
            other => {
                return Err(anyhow::anyhow!(
                    "--log-format must be one of: pretty, compact, json (got '{}')",
                    other
                ));
            }
        }

        // Validate log_level
        match self.log_level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" | "off" => {}
            other => {
                return Err(anyhow::anyhow!(
                    "--log-level must be one of: trace, debug, info, warn, error, off (got '{}')",
                    other
                ));
            }
        }

        // Validate spec formats early to give clear error messages
        for spec in &self.export {
            crate::export::parse_export_spec(spec)?;
        }
        for spec in &self.import {
            crate::import::parse_import_spec(spec)?;
        }
        for spec in &self.http_export {
            crate::export::parse_http_export_spec(spec)?;
        }
        for spec in &self.http_import {
            crate::import::parse_import_spec(spec)?;
        }
        for spec in &self.ws_export {
            crate::export::parse_ws_export_spec(spec)?;
        }
        for spec in &self.ws_import {
            crate::import::parse_import_spec(spec)?;
        }
        for spec in &self.auto_import {
            crate::import::parse_import_spec(spec)?;
        }
        for spec in &self.http_multiroute_import {
            crate::import::parse_import_spec(spec)?;
        }

        Ok(())
    }

    /// Build a BridgeConfig from command-line arguments
    pub fn bridge_config(&self) -> BridgeConfig {
        BridgeConfig::new(self.buffer_size, self.read_timeout, self.drain_timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_no_specs_fails() {
        let args = Args::default();
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_with_export_passes() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_with_import_passes() {
        let args = Args {
            import: vec!["svc/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_with_http_export_passes() {
        let args = Args {
            http_export: vec!["svc/dns.test/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_with_http_import_passes() {
        let args = Args {
            http_import: vec!["svc/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_with_ws_export_passes() {
        let args = Args {
            ws_export: vec!["svc/ws://127.0.0.1:9000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_with_ws_import_passes() {
        let args = Args {
            ws_import: vec!["svc/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_with_auto_import_passes() {
        let args = Args {
            auto_import: vec!["svc/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_with_http_multiroute_import_passes() {
        let args = Args {
            http_multiroute_import: vec!["svc/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_bridge_config_maps_fields_correctly() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 1024,
            read_timeout: 30,
            drain_timeout: 15,
            ..Default::default()
        };
        let config = args.bridge_config();
        assert_eq!(config.buffer_size, 1024);
        assert_eq!(config.read_timeout, std::time::Duration::from_secs(30));
        assert_eq!(config.drain_timeout, std::time::Duration::from_secs(15));
    }

    // --- Buffer size validation ---

    #[test]
    fn test_validate_buffer_size_minimum_boundary() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 1024,
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_buffer_size_below_minimum() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 1023,
            ..Default::default()
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("1024"));
    }

    #[test]
    fn test_validate_buffer_size_zero() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 0,
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_buffer_size_large() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 10 * 1024 * 1024, // 10 MiB
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    // --- Drain timeout validation ---

    #[test]
    fn test_validate_drain_timeout_minimum() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            drain_timeout: 1,
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_drain_timeout_zero() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            drain_timeout: 0,
            ..Default::default()
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("drain-timeout"));
    }

    // --- Log format validation ---

    #[test]
    fn test_validate_all_log_formats() {
        for fmt in &["pretty", "compact", "json"] {
            let args = Args {
                export: vec!["svc/127.0.0.1:8000".into()],
                log_format: fmt.to_string(),
                ..Default::default()
            };
            assert!(
                args.validate().is_ok(),
                "log_format '{}' should be valid",
                fmt
            );
        }
    }

    #[test]
    fn test_validate_invalid_log_format() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            log_format: "xml".into(),
            ..Default::default()
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("log-format"));
        assert!(err.contains("xml"));
    }

    // --- Log level validation ---

    #[test]
    fn test_validate_all_log_levels() {
        for level in &["trace", "debug", "info", "warn", "error", "off"] {
            let args = Args {
                export: vec!["svc/127.0.0.1:8000".into()],
                log_level: level.to_string(),
                ..Default::default()
            };
            assert!(
                args.validate().is_ok(),
                "log_level '{}' should be valid",
                level
            );
        }
    }

    #[test]
    fn test_validate_invalid_log_level() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8000".into()],
            log_level: "verbose".into(),
            ..Default::default()
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("log-level"));
        assert!(err.contains("verbose"));
    }

    // --- Spec format validation through validate() ---

    #[test]
    fn test_validate_rejects_bad_export_spec() {
        let args = Args {
            export: vec!["invalid-no-slash".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_bad_import_spec() {
        let args = Args {
            import: vec!["invalid-no-slash".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_bad_http_export_spec() {
        let args = Args {
            http_export: vec!["only/two-parts".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_bad_ws_export_spec() {
        let args = Args {
            ws_export: vec!["svc/http://not-ws".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    // --- Multiple spec combinations ---

    #[test]
    fn test_validate_mixed_export_import() {
        let args = Args {
            export: vec!["svc1/127.0.0.1:8001".into()],
            import: vec!["svc2/127.0.0.1:8002".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_all_spec_types_at_once() {
        let args = Args {
            export: vec!["svc/127.0.0.1:8001".into()],
            import: vec!["svc/127.0.0.1:8002".into()],
            http_export: vec!["http/dns.test/127.0.0.1:8003".into()],
            http_import: vec!["http/127.0.0.1:8004".into()],
            ws_export: vec!["ws/ws://127.0.0.1:9000".into()],
            ws_import: vec!["ws/127.0.0.1:8005".into()],
            auto_import: vec!["auto/127.0.0.1:8006".into()],
            http_multiroute_import: vec!["mr/127.0.0.1:8007".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    // --- First bad spec in list fails validation ---

    #[test]
    fn test_validate_mixed_good_and_bad_specs() {
        let args = Args {
            export: vec!["good/127.0.0.1:8001".into(), "bad-no-slash".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }
}
