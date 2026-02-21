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
