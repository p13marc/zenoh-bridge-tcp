//! Integration tests for HTTPS termination (TLS offloading)
//!
//! Architecture tested:
//!   HTTPS Client -> Import Bridge (TLS terminated) -> Zenoh -> Export Bridge -> Plaintext HTTP Backend
//!
//! Unlike HTTPS passthrough (where backend handles TLS), here the import bridge
//! terminates TLS and forwards plaintext HTTP over Zenoh.

#![cfg(feature = "tls-termination")]

mod common;

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

/// Test that --https-terminate without --tls-cert/--tls-key fails validation
#[tokio::test]
async fn test_https_terminate_requires_cert_and_key() {
    let child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--https-terminate", "svc/0.0.0.0:8443"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn bridge");

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("Timeout waiting for bridge exit")
        .expect("Failed to wait for bridge");

    assert!(
        !output.status.success(),
        "Bridge should fail without --tls-cert and --tls-key"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--tls-cert") || stderr.contains("--tls-key"),
        "Error should mention missing TLS cert/key, got: {}",
        stderr
    );
}

/// Test that --https-terminate with cert but missing key fails
#[tokio::test]
async fn test_https_terminate_requires_key() {
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_cert_only_integ.pem");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();

    let child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--https-terminate",
            "svc/0.0.0.0:8443",
            "--tls-cert",
            cert_path.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn bridge");

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("Timeout waiting for bridge exit")
        .expect("Failed to wait for bridge");

    assert!(
        !output.status.success(),
        "Bridge should fail without --tls-key"
    );

    std::fs::remove_file(&cert_path).unwrap();
}

/// Test that --https-terminate with valid cert and key starts successfully
/// (We can't do a full end-to-end test without a running Zenoh network,
/// but we verify the binary accepts the arguments and starts)
#[tokio::test]
async fn test_https_terminate_starts_with_valid_tls() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_integ_cert.pem");
    let key_path = dir.join("test_integ_key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    // Use port 0 style - find a free port first
    let port_guard = common::PortGuard::new();
    let addr = port_guard.release();

    let spec = format!("tls-test/{}", addr);

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--https-terminate",
            &spec,
            "--tls-cert",
            cert_path.to_str().unwrap(),
            "--tls-key",
            key_path.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn bridge");

    // Give it time to start (it should not crash immediately)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Check it's still running (hasn't crashed)
    let try_wait = child.try_wait().expect("Failed to check process status");
    assert!(
        try_wait.is_none(),
        "Bridge should still be running after 2s with valid TLS config, but exited: {:?}",
        try_wait
    );

    // Clean up
    let _ = child.kill().await;
    std::fs::remove_file(&cert_path).unwrap();
    std::fs::remove_file(&key_path).unwrap();
}

/// A client cert verifier that accepts anything — the bridge uses a self-signed
/// test cert, and these tests care only about ALPN negotiation, not trust.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_config_with_alpn(alpn: &[&[u8]]) -> rustls::ClientConfig {
    let mut cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    cfg
}

/// #46: the terminated listener advertises ALPN `http/1.1` only. A client that
/// offers `http/1.1` (alone or alongside `h2`) negotiates it cleanly; an h2-only
/// client fails the handshake with a `no_application_protocol` alert instead of
/// negotiating h2 and having its `PRI * HTTP/2.0` preface mis-parsed as HTTP/1.1.
#[tokio::test]
async fn test_https_terminate_alpn_negotiation() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_alpn_integ_cert.pem");
    let key_path = dir.join("test_alpn_integ_key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let port_guard = common::PortGuard::new();
    let addr = port_guard.release();
    let spec = format!("alpn-test/{}", addr);

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--https-terminate",
            &spec,
            "--tls-cert",
            cert_path.to_str().unwrap(),
            "--tls-key",
            key_path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn bridge");

    common::wait_for_port(addr, Duration::from_secs(10))
        .await
        .expect("terminate listener did not start");

    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();

    // 1. Client offering h2 + http/1.1 -> negotiates http/1.1.
    {
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config_with_alpn(&[
            b"h2",
            b"http/1.1",
        ])));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let tls = connector
            .connect(server_name.clone(), tcp)
            .await
            .expect("handshake with http/1.1 on offer should succeed");
        let negotiated = tls.get_ref().1.alpn_protocol().map(|p| p.to_vec());
        assert_eq!(
            negotiated,
            Some(b"http/1.1".to_vec()),
            "bridge must negotiate http/1.1"
        );
    }

    // 2. Client offering ONLY h2 -> handshake fails (no_application_protocol).
    {
        let connector =
            tokio_rustls::TlsConnector::from(Arc::new(client_config_with_alpn(&[b"h2"])));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let result = connector.connect(server_name.clone(), tcp).await;
        assert!(
            result.is_err(),
            "an h2-only client must fail ALPN negotiation, not mis-parse as HTTP/1.1"
        );
    }

    let _ = child.kill().await;
    std::fs::remove_file(&cert_path).unwrap();
    std::fs::remove_file(&key_path).unwrap();
}
