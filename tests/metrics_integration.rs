//! G7 (#43): the /healthz, /readyz, /metrics observability endpoints.
//!
//! Starts a real bridge process with `--metrics-addr` and verifies the health
//! and metrics HTTP surface responds as expected.

mod common;

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Extract the value of the first Prometheus line starting with `prefix`.
fn metric_value(body: &str, prefix: &str) -> Option<u64> {
    body.lines()
        .find(|l| l.starts_with(prefix))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.trim().parse().ok())
}

#[tokio::test]
async fn metrics_endpoints_respond() {
    let port = common::PortGuard::new();
    let metrics_addr = port.release();
    let metrics_addr_str = metrics_addr.to_string();

    // A dummy export gives the process a bridge task to keep it alive; the
    // backend need not exist (it is only dialed when a client appears).
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--export",
            "metricsvc/127.0.0.1:1",
            "--metrics-addr",
            &metrics_addr_str,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn bridge");

    common::wait_for_port(metrics_addr, Duration::from_secs(10))
        .await
        .expect("metrics server did not start");

    let base = format!("http://{metrics_addr}");

    // /healthz — liveness, always ok.
    let resp = reqwest::get(format!("{base}/healthz")).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.text().await.unwrap().contains("ok"));

    // /readyz — becomes ready shortly after bridge tasks start.
    let ready_base = base.clone();
    common::wait_for(
        || {
            let url = format!("{ready_base}/readyz");
            async move {
                reqwest::get(url)
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }
        },
        Duration::from_secs(10),
        "/readyz returns 200",
    )
    .await
    .expect("bridge never became ready");

    // /metrics — Prometheus exposition.
    let resp = reqwest::get(format!("{base}/metrics")).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("zbridge_ready 1"), "metrics body: {body}");
    assert!(body.contains("# TYPE zbridge_active_connections gauge"));
    assert!(body.contains("# TYPE zbridge_bytes_total counter"));

    // Unknown path -> 404.
    let resp = reqwest::get(format!("{base}/nope")).await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    let _ = child.kill().await;
}

/// Driving real traffic through an import bridge moves the per-service counters.
#[tokio::test]
async fn metrics_count_real_traffic() {
    let (backend_addr, _echo) = common::start_echo_server().await;

    let metrics_port = common::PortGuard::new();
    let metrics_addr = metrics_port.release();
    let metrics_str = metrics_addr.to_string();

    let service = common::unique_service_name("metricsflow");
    let bridge = common::BridgePair::tcp_with_args(
        &service,
        backend_addr,
        &[],
        &["--metrics-addr", &metrics_str],
    )
    .await;

    // Liveliness propagation: import declares -> export connects to backend.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Round-trip a payload through the echo backend.
    let payload = b"hello-metrics";
    let mut stream = tokio::net::TcpStream::connect(bridge.import_addr)
        .await
        .unwrap();
    stream.write_all(payload).await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed");
    assert_eq!(&buf[..n], payload);

    // Scrape the import bridge's metrics and confirm the counters moved.
    let body = reqwest::get(format!("http://{metrics_addr}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let total = metric_value(
        &body,
        &format!("zbridge_connections_total{{service=\"{service}\"}} "),
    )
    .unwrap_or(0);
    assert!(total >= 1, "connections_total should be >= 1\n{body}");

    let up = metric_value(
        &body,
        &format!("zbridge_bytes_total{{service=\"{service}\",direction=\"up\"}} "),
    )
    .unwrap_or(0);
    let down = metric_value(
        &body,
        &format!("zbridge_bytes_total{{service=\"{service}\",direction=\"down\"}} "),
    )
    .unwrap_or(0);
    assert!(
        up >= payload.len() as u64,
        "bytes up ({up}) should be >= {}\n{body}",
        payload.len()
    );
    assert!(
        down >= payload.len() as u64,
        "bytes down ({down}) should be >= {}\n{body}",
        payload.len()
    );

    drop(stream);
}
