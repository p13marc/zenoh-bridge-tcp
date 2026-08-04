//! Command-line argument definitions for zenoh-bridge-tcp.
//!
//! The 0.7.0 surface: routing is two flags — `--listen` (attachment points
//! that accept clients) and `--backend` (local services exposed onto the bus)
//! — with the grammar in [`crate::spec`]. Zenoh session flags carry a
//! `zenoh-` prefix. The route table itself is never configured: it lives on
//! the Zenoh key space (`docs/ROUTING-SIMPLIFICATION.md`).

use crate::config::BridgeConfig;
use crate::spec::{BackendSpec, ListenSpec};
use clap::Parser;
use std::net::SocketAddr;

/// Command-line arguments for the Zenoh TCP Bridge
#[derive(Parser, Debug)]
#[command(author, version, about = "TCP <-> Zenoh bridge", long_about = None)]
pub struct Args {
    /// Listener spec: '<service>/<addr>[,proto=raw][,cert=PATH,key=PATH][,route=request]'
    /// Default behavior auto-detects the protocol and routes TLS by SNI,
    /// HTTP/1 by Host, WebSocket upgrades transparently, anything else opaquely.
    /// 'proto=raw' skips detection (server-speaks-first protocols);
    /// 'cert='+'key=' terminate TLS at the bridge (cert implies termination);
    /// 'route=request' re-routes each HTTP/1.1 request independently.
    /// Can be specified multiple times.
    #[arg(long)]
    pub listen: Vec<String>,

    /// Backend spec: '<service>[@<host>]/<target>' where target is
    /// 'host:port' (TCP) or a 'ws://'/'wss://' URL (WebSocket).
    /// '@host' registers this backend for hostname routing at
    /// '{service}/{host}/available'. Can be specified multiple times.
    #[arg(long)]
    pub backend: Vec<String>,

    /// Path to a Zenoh configuration file (JSON5 format)
    /// If provided, the other zenoh-* options are ignored
    #[arg(long)]
    pub zenoh_config: Option<String>,

    /// Zenoh session mode: peer, client, or router
    #[arg(long, default_value = "peer")]
    pub zenoh_mode: String,

    /// Zenoh connect endpoint (e.g., tcp/localhost:7447)
    #[arg(long)]
    pub zenoh_connect: Option<String>,

    /// Zenoh listen endpoint (e.g., tcp/0.0.0.0:7447)
    #[arg(long)]
    pub zenoh_listen: Option<String>,

    /// Buffer size for TCP read/write operations in bytes
    #[arg(long, default_value = "65536")]
    pub buffer_size: usize,

    /// Timeout for reading HTTP/TLS headers in seconds
    #[arg(long, default_value = "10")]
    pub read_timeout: u64,

    /// Timeout in seconds for draining buffered data when a connection closes
    #[arg(long, default_value = "5")]
    pub drain_timeout: u64,

    /// Data-plane reliability posture: `stream` (default) blocks on backpressure
    /// and resets the connection on unrecoverable loss; `telemetry` tolerates drops.
    #[arg(long, default_value = "stream")]
    pub reliability: String,

    /// Maximum number of concurrent client connections per listener
    #[arg(long, default_value = "1024")]
    pub max_connections: usize,

    /// AdvancedPublisher cache depth in samples (late-joiner recovery window)
    #[arg(long, default_value = "256")]
    pub cache_size: usize,

    /// Maximum size for HTTP/TLS headers in bytes
    #[arg(long, default_value = "16384")]
    pub max_header_size: usize,

    /// Heartbeat interval in milliseconds for Zenoh publisher/subscriber recovery
    #[arg(long, default_value = "500")]
    pub heartbeat_interval_ms: u64,

    /// Timeout in milliseconds for checking backend availability
    #[arg(long, default_value = "1000")]
    pub availability_timeout_ms: u64,

    /// Maximum response size for route=request mode in bytes (exceeding -> HTTP 502)
    #[arg(long, default_value = "10485760")]
    pub max_response_size: usize,

    /// Per-connection Zenoh reception buffer depth in samples. A slow client is
    /// isolated at this bound (Stream: reset; Telemetry: shed) so it cannot
    /// head-of-line-block others on the shared session (D2).
    #[arg(long, default_value = "256")]
    pub rx_channel_capacity: usize,

    /// Expose /healthz, /readyz, and /metrics on this address (e.g. 0.0.0.0:9100).
    /// Disabled when unset.
    #[arg(long)]
    pub metrics_addr: Option<SocketAddr>,

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
            listen: Vec::new(),
            backend: Vec::new(),
            zenoh_config: None,
            zenoh_mode: "peer".to_string(),
            zenoh_connect: None,
            zenoh_listen: None,
            buffer_size: 65536,
            read_timeout: 10,
            drain_timeout: 5,
            reliability: "stream".to_string(),
            max_connections: 1024,
            cache_size: 256,
            max_header_size: 16384,
            heartbeat_interval_ms: 500,
            availability_timeout_ms: 1000,
            max_response_size: 10 * 1024 * 1024,
            rx_channel_capacity: 256,
            metrics_addr: None,
            log_level: "info".to_string(),
            log_format: "pretty".to_string(),
        }
    }
}

impl Args {
    /// Parse every `--listen` spec, in flag order.
    pub fn listen_specs(&self) -> anyhow::Result<Vec<ListenSpec>> {
        self.listen.iter().map(|s| s.parse()).collect()
    }

    /// Parse every `--backend` spec, in flag order.
    pub fn backend_specs(&self) -> anyhow::Result<Vec<BackendSpec>> {
        self.backend.iter().map(|s| s.parse()).collect()
    }

    /// Validate command-line arguments
    pub fn validate(&self) -> anyhow::Result<()> {
        // Parse specs early for clear startup errors.
        let listens = self.listen_specs()?;
        let _backends = self.backend_specs()?;

        if self.listen.is_empty() && self.backend.is_empty() {
            return Err(anyhow::anyhow!(
                "Must specify at least one --listen or --backend. Use --help for usage."
            ));
        }

        // The grammar accepts cert=/key= unconditionally; whether this build
        // can act on them is a feature question, answered here by name.
        #[cfg(not(feature = "tls-termination"))]
        for spec in &listens {
            if matches!(spec.tls, crate::spec::TlsMode::Terminate { .. }) {
                return Err(anyhow::anyhow!(
                    "--listen '{spec}' requests TLS termination (cert=/key=), but this \
                     binary was built without the 'tls-termination' feature; rebuild with \
                     `cargo build --features tls-termination`"
                ));
            }
        }
        #[cfg(feature = "tls-termination")]
        let _ = listens;

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

        // Validate reliability posture
        self.reliability
            .parse::<crate::config::ReliabilityMode>()
            .map_err(|e| anyhow::anyhow!("--reliability: {}", e))?;

        // Validate tunables
        if self.max_connections < 1 {
            return Err(anyhow::anyhow!("--max-connections must be at least 1"));
        }
        if self.cache_size < 1 {
            return Err(anyhow::anyhow!("--cache-size must be at least 1"));
        }
        if self.rx_channel_capacity < 1 {
            return Err(anyhow::anyhow!("--rx-channel-capacity must be at least 1"));
        }
        if self.max_header_size < 1024 {
            return Err(anyhow::anyhow!(
                "--max-header-size must be at least 1024 (got {})",
                self.max_header_size
            ));
        }
        if self.heartbeat_interval_ms < 1 {
            return Err(anyhow::anyhow!(
                "--heartbeat-interval-ms must be at least 1"
            ));
        }
        if self.availability_timeout_ms < 1 {
            return Err(anyhow::anyhow!(
                "--availability-timeout-ms must be at least 1"
            ));
        }
        if self.max_response_size < self.max_header_size {
            return Err(anyhow::anyhow!(
                "--max-response-size ({}) must be >= --max-header-size ({})",
                self.max_response_size,
                self.max_header_size
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

        Ok(())
    }

    /// Build a BridgeConfig from command-line arguments
    pub fn bridge_config(&self) -> BridgeConfig {
        let mut config = BridgeConfig::new(self.buffer_size, self.read_timeout, self.drain_timeout);
        // Already validated in `validate()`; fall back to the default posture.
        config.reliability = self.reliability.parse().unwrap_or_default();
        config.max_connections = self.max_connections;
        config.cache_size = self.cache_size;
        config.max_header_size = self.max_header_size;
        config.heartbeat_interval = std::time::Duration::from_millis(self.heartbeat_interval_ms);
        config.availability_timeout =
            std::time::Duration::from_millis(self.availability_timeout_ms);
        config.max_response_size = self.max_response_size;
        config.rx_channel_capacity = self.rx_channel_capacity;
        config
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
    fn test_validate_with_listen_passes() {
        let args = Args {
            listen: vec!["svc/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_with_backend_passes() {
        let args = Args {
            backend: vec!["svc/127.0.0.1:8000".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_listen_and_backend_passes() {
        let args = Args {
            listen: vec![
                "svc1/127.0.0.1:8001".into(),
                "svc2/[::1]:8002,proto=raw".into(),
            ],
            backend: vec![
                "svc1@api.example.com/127.0.0.1:9001".into(),
                "chat/ws://127.0.0.1:9000".into(),
            ],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_bad_listen_spec() {
        let args = Args {
            listen: vec!["invalid-no-slash".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_bad_backend_spec() {
        let args = Args {
            backend: vec!["svc/http://not-supported".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_unknown_listen_option() {
        // The 0.7.0 grammar has no tls= keyword: cert=/key= imply termination.
        let args = Args {
            listen: vec!["svc/127.0.0.1:8000,tls=terminate".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_mixed_good_and_bad_specs() {
        let args = Args {
            listen: vec!["good/127.0.0.1:8001".into(), "bad-no-slash".into()],
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[cfg(not(feature = "tls-termination"))]
    #[test]
    fn test_validate_default_build_rejects_termination_by_feature_name() {
        let args = Args {
            listen: vec!["svc/127.0.0.1:8443,cert=/c.pem,key=/k.pem".into()],
            ..Default::default()
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("tls-termination"), "{err}");
    }

    #[cfg(feature = "tls-termination")]
    #[test]
    fn test_validate_tls_build_accepts_termination_spec() {
        // Cert/key files need not exist at validate() time — they are opened
        // when the listener starts.
        let args = Args {
            listen: vec!["svc/127.0.0.1:8443,cert=/c.pem,key=/k.pem".into()],
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_bridge_config_maps_fields_correctly() {
        let args = Args {
            listen: vec!["svc/127.0.0.1:8000".into()],
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
            listen: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 1024,
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_buffer_size_below_minimum() {
        let args = Args {
            listen: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 1023,
            ..Default::default()
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("1024"));
    }

    #[test]
    fn test_validate_buffer_size_zero() {
        let args = Args {
            listen: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 0,
            ..Default::default()
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_buffer_size_large() {
        let args = Args {
            listen: vec!["svc/127.0.0.1:8000".into()],
            buffer_size: 10 * 1024 * 1024, // 10 MiB
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    // --- Drain timeout validation ---

    #[test]
    fn test_validate_drain_timeout_minimum() {
        let args = Args {
            listen: vec!["svc/127.0.0.1:8000".into()],
            drain_timeout: 1,
            ..Default::default()
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_drain_timeout_zero() {
        let args = Args {
            listen: vec!["svc/127.0.0.1:8000".into()],
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
                listen: vec!["svc/127.0.0.1:8000".into()],
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
            listen: vec!["svc/127.0.0.1:8000".into()],
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
                listen: vec!["svc/127.0.0.1:8000".into()],
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
            listen: vec!["svc/127.0.0.1:8000".into()],
            log_level: "verbose".into(),
            ..Default::default()
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("log-level"));
        assert!(err.contains("verbose"));
    }
}
