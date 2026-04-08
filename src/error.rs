//! Error types for the Zenoh TCP Bridge.
//!
//! This module provides structured error types for all bridge operations,
//! enabling better error handling and more informative error messages.
//!
//! ## Error Type Convention
//!
//! - **Public API** (`run_export_mode`, `run_import_mode`, spec parsers, config):
//!   Returns `anyhow::Result<T>` for ergonomic error propagation and context.
//!
//! - **Internal parsing** (`http_parser`, `tls_parser`):
//!   Uses `BridgeError` / `error::Result<T>` for structured, matchable errors.
//!
//! `BridgeError` implements `std::error::Error` via `thiserror`, so it converts
//! into `anyhow::Error` automatically through the `?` operator at module boundaries.

use std::net::SocketAddr;
use thiserror::Error;

/// The main error type for the Zenoh TCP Bridge.
#[derive(Error, Debug)]
pub enum BridgeError {
    /// Invalid export specification format.
    #[error("invalid export spec '{spec}': {reason}")]
    InvalidExportSpec { spec: String, reason: String },

    /// Invalid import specification format.
    #[error("invalid import spec '{spec}': {reason}")]
    InvalidImportSpec { spec: String, reason: String },

    /// Invalid WebSocket export specification format.
    #[error("invalid WebSocket export spec '{spec}': {reason}")]
    InvalidWsExportSpec { spec: String, reason: String },

    /// Failed to connect to a backend server.
    #[error("failed to connect to backend {addr}")]
    BackendConnection {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    /// Failed to bind to a listen address.
    #[error("failed to bind to {addr}")]
    BindFailed {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    /// Zenoh operation failed.
    #[error("zenoh error: {0}")]
    Zenoh(String),

    /// HTTP request parsing failed.
    #[error("HTTP parse error: {0}")]
    HttpParse(String),

    /// TLS/SNI parsing failed.
    #[error("TLS/SNI parse error: {0}")]
    TlsParse(String),

    /// Operation timed out.
    #[error("timeout: {0}")]
    Timeout(String),

    /// No backend available for the requested DNS.
    #[error("no backend available for '{dns}'")]
    NoBackend { dns: String },

    /// WebSocket connection failed.
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    /// Generic I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A specialized Result type for bridge operations.
pub type Result<T> = std::result::Result<T, BridgeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invalid_export_spec_display() {
        let err = BridgeError::InvalidExportSpec {
            spec: "bad".to_string(),
            reason: "missing backend address".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "invalid export spec 'bad': missing backend address"
        );
    }

    #[test]
    fn test_no_backend_display() {
        let err = BridgeError::NoBackend {
            dns: "api.example.com".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "no backend available for 'api.example.com'"
        );
    }

    #[test]
    fn test_io_error_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let bridge_err: BridgeError = io_err.into();
        assert!(matches!(bridge_err, BridgeError::Io(_)));
    }

    #[test]
    fn test_invalid_import_spec_display() {
        let err = BridgeError::InvalidImportSpec {
            spec: "bad".to_string(),
            reason: "missing address".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "invalid import spec 'bad': missing address"
        );
    }

    #[test]
    fn test_invalid_ws_export_spec_display() {
        let err = BridgeError::InvalidWsExportSpec {
            spec: "bad".to_string(),
            reason: "not a ws:// URL".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "invalid WebSocket export spec 'bad': not a ws:// URL"
        );
    }

    #[test]
    fn test_backend_connection_display() {
        let err = BridgeError::BackendConnection {
            addr: "127.0.0.1:9999".parse().unwrap(),
            source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        };
        assert_eq!(
            err.to_string(),
            "failed to connect to backend 127.0.0.1:9999"
        );
    }

    #[test]
    fn test_bind_failed_display() {
        let err = BridgeError::BindFailed {
            addr: "0.0.0.0:80".parse().unwrap(),
            source: std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use"),
        };
        assert_eq!(err.to_string(), "failed to bind to 0.0.0.0:80");
    }

    #[test]
    fn test_zenoh_error_display() {
        let err = BridgeError::Zenoh("session closed".to_string());
        assert_eq!(err.to_string(), "zenoh error: session closed");
    }

    #[test]
    fn test_http_parse_display() {
        let err = BridgeError::HttpParse("invalid method".to_string());
        assert_eq!(err.to_string(), "HTTP parse error: invalid method");
    }

    #[test]
    fn test_tls_parse_display() {
        let err = BridgeError::TlsParse("no SNI".to_string());
        assert_eq!(err.to_string(), "TLS/SNI parse error: no SNI");
    }

    #[test]
    fn test_timeout_display() {
        let err = BridgeError::Timeout("reading headers".to_string());
        assert_eq!(err.to_string(), "timeout: reading headers");
    }

    #[test]
    fn test_websocket_display() {
        let err = BridgeError::WebSocket("handshake failed".to_string());
        assert_eq!(err.to_string(), "WebSocket error: handshake failed");
    }

    // --- Source chain tests ---

    #[test]
    fn test_backend_connection_has_source() {
        use std::error::Error;
        let err = BridgeError::BackendConnection {
            addr: "127.0.0.1:9999".parse().unwrap(),
            source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        };
        let source = err.source().expect("should have source");
        assert!(source.to_string().contains("refused"));
    }

    #[test]
    fn test_bind_failed_has_source() {
        use std::error::Error;
        let err = BridgeError::BindFailed {
            addr: "0.0.0.0:80".parse().unwrap(),
            source: std::io::Error::new(std::io::ErrorKind::AddrInUse, "address in use"),
        };
        let source = err.source().expect("should have source");
        assert!(source.to_string().contains("address in use"));
    }

    #[test]
    fn test_io_error_has_source() {
        use std::error::Error;
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let bridge_err: BridgeError = io_err.into();
        let source = bridge_err.source().expect("Io variant should have source");
        assert!(source.to_string().contains("broken pipe"));
    }

    #[test]
    fn test_simple_variants_have_no_source() {
        use std::error::Error;
        let cases: Vec<BridgeError> = vec![
            BridgeError::Zenoh("test".into()),
            BridgeError::HttpParse("test".into()),
            BridgeError::TlsParse("test".into()),
            BridgeError::Timeout("test".into()),
            BridgeError::NoBackend { dns: "test".into() },
            BridgeError::WebSocket("test".into()),
        ];
        for err in cases {
            assert!(err.source().is_none(), "{} should have no source", err);
        }
    }

    // --- From<io::Error> conversion ---

    #[test]
    fn test_io_error_kind_preserved() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let bridge_err: BridgeError = io_err.into();
        match bridge_err {
            BridgeError::Io(ref e) => assert_eq!(e.kind(), std::io::ErrorKind::TimedOut),
            _ => panic!("expected Io variant"),
        }
    }

    // --- anyhow conversion ---

    #[test]
    fn test_bridge_error_into_anyhow() {
        let bridge_err = BridgeError::NoBackend {
            dns: "test.com".into(),
        };
        let anyhow_err: anyhow::Error = bridge_err.into();
        assert!(anyhow_err.to_string().contains("test.com"));
    }

    // --- Debug formatting ---

    #[test]
    fn test_all_variants_debug() {
        let variants: Vec<BridgeError> = vec![
            BridgeError::InvalidExportSpec { spec: "s".into(), reason: "r".into() },
            BridgeError::InvalidImportSpec { spec: "s".into(), reason: "r".into() },
            BridgeError::InvalidWsExportSpec { spec: "s".into(), reason: "r".into() },
            BridgeError::Zenoh("z".into()),
            BridgeError::HttpParse("h".into()),
            BridgeError::TlsParse("t".into()),
            BridgeError::Timeout("t".into()),
            BridgeError::NoBackend { dns: "d".into() },
            BridgeError::WebSocket("w".into()),
            BridgeError::Io(std::io::Error::new(std::io::ErrorKind::Other, "e")),
        ];
        for err in variants {
            let debug = format!("{:?}", err);
            assert!(!debug.is_empty());
        }
    }
}
