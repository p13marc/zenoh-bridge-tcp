//! Behavior specific to the default (no `tls-termination`) build.
//!
//! This whole file compiles only without the feature, so it runs under the CI
//! `default-build` job and is skipped by `--all-features` runs.

#![cfg(not(feature = "tls-termination"))]

use std::process::Stdio;
use tokio::process::Command;

/// The `--https-terminate` / `--tls-cert` / `--tls-key` CLI fields are
/// `#[cfg(feature = "tls-termination")]`-gated, so a default build must not
/// accept them — clap rejects the unknown argument rather than silently ignoring
/// it (which would be a confusing no-op).
#[tokio::test]
async fn default_build_rejects_https_terminate() {
    let out = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--https-terminate", "svc/0.0.0.0:8443"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("failed to spawn bridge");

    assert!(
        !out.status.success(),
        "a default build must reject the gated --https-terminate flag"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("https-terminate"),
        "expected an unknown-argument error, got: {stderr}"
    );
}
