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
async fn default_build_rejects_terminating_listen_by_feature_name() {
    // cert=/key= parse in every build (the grammar is feature-independent);
    // a default build must reject them at validation, naming the feature the
    // user has to enable rather than a cryptic unknown-argument error.
    let out = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--listen", "svc/0.0.0.0:8443,cert=/c.pem,key=/k.pem"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("failed to spawn bridge");

    assert!(
        !out.status.success(),
        "a default build must reject a terminating --listen spec"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("tls-termination"),
        "expected the error to name the tls-termination feature, got: {stderr}"
    );
}
