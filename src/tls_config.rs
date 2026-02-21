//! TLS configuration loading for TLS termination.
//!
//! Feature-gated behind `tls-termination`.

use anyhow::Result;
use rustls::ServerConfig;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

/// Load TLS server configuration from certificate and key files.
///
/// # Arguments
/// * `cert_path` - Path to PEM-encoded certificate chain file
/// * `key_path` - Path to PEM-encoded private key file
///
/// # Returns
/// An Arc-wrapped ServerConfig ready for use with TlsAcceptor
pub fn load_tls_config<P1: AsRef<Path>, P2: AsRef<Path>>(
    cert_path: P1,
    key_path: P2,
) -> Result<Arc<ServerConfig>> {
    let cert_file = File::open(cert_path.as_ref()).map_err(|e| {
        anyhow::anyhow!(
            "Failed to open certificate file '{}': {}",
            cert_path.as_ref().display(),
            e
        )
    })?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Failed to parse certificates: {}", e))?;

    if certs.is_empty() {
        return Err(anyhow::anyhow!(
            "No certificates found in '{}'",
            cert_path.as_ref().display()
        ));
    }

    let key_file = File::open(key_path.as_ref()).map_err(|e| {
        anyhow::anyhow!(
            "Failed to open key file '{}': {}",
            key_path.as_ref().display(),
            e
        )
    })?;
    let mut key_reader = BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| anyhow::anyhow!("Failed to parse private key: {}", e))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No private key found in '{}'",
                key_path.as_ref().display()
            )
        })?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("Failed to build TLS config: {}", e))?;

    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn test_load_tls_config_missing_cert_file() {
        install_crypto_provider();
        let result = load_tls_config("/nonexistent/cert.pem", "/nonexistent/key.pem");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to open certificate")
        );
    }

    #[test]
    fn test_load_tls_config_empty_cert() {
        install_crypto_provider();
        let dir = std::env::temp_dir();
        let cert_path = dir.join("test_empty_cert.pem");
        let key_path = dir.join("test_empty_key.pem");
        std::fs::write(&cert_path, "").unwrap();
        std::fs::write(&key_path, "").unwrap();

        let result = load_tls_config(&cert_path, &key_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No certificates"));

        std::fs::remove_file(&cert_path).unwrap();
        std::fs::remove_file(&key_path).unwrap();
    }

    #[test]
    fn test_load_tls_config_missing_key_file() {
        install_crypto_provider();
        // Generate a real cert so the cert parsing succeeds
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir();
        let cert_path = dir.join("test_cert_only_valid.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();

        let result = load_tls_config(&cert_path, "/nonexistent/key.pem");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to open key file"),
        );

        std::fs::remove_file(&cert_path).unwrap();
    }

    #[test]
    fn test_load_tls_config_valid() {
        install_crypto_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.key_pair.serialize_pem();

        let dir = std::env::temp_dir();
        let cert_path = dir.join("test_tls_valid_cert.pem");
        let key_path = dir.join("test_tls_valid_key.pem");
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();

        let result = load_tls_config(&cert_path, &key_path);
        assert!(
            result.is_ok(),
            "Valid cert/key should load: {:?}",
            result.err()
        );

        std::fs::remove_file(&cert_path).unwrap();
        std::fs::remove_file(&key_path).unwrap();
    }
}
