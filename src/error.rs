//! Error types for the Zenoh TCP Bridge.
//!
//! This module provides structured error types for all bridge operations,
//! enabling better error handling and more informative error messages.

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
}
