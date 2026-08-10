//! G7 (#43): the /healthz, /readyz, /metrics observability endpoints.
//!
//! Starts a real bridge process with `--metrics-addr` and verifies the health
//! and metrics HTTP surface responds as expected.

mod common;

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let mut child = common::bridge_command()
        .args([
            "--backend",
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

/// D5 (#25): a listener reaps a completed connection task while otherwise idle
/// (the `tasks.join_next()` arm in the accept select) and stays healthy for the
/// next connection. Observably: after a connection completes and an idle gap,
/// `active_connections` returns to 0 and a fresh connection still round-trips.
#[tokio::test]
async fn idle_listener_reaps_and_stays_healthy() {
    use tokio::net::TcpStream;

    let (backend_addr, _echo) = common::start_echo_server().await;
    let metrics_port = common::PortGuard::new();
    let metrics_addr = metrics_port.release();
    let service = common::unique_service_name("d5_idle");
    let bridge = common::BridgePair::tcp_with_args(
        &service,
        backend_addr,
        &[],
        &["--metrics-addr", &metrics_addr.to_string()],
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // First connection: round-trip, then close.
    {
        let mut c = TcpStream::connect(bridge.import_addr).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
            .await
            .expect("read timed out")
            .expect("read failed");
        assert_eq!(&buf[..n], b"ping");
    }

    // While the listener sits idle in accept(), the join_next arm reaps the
    // completed task and the connection's guard drops active back to 0.
    common::wait_for(
        || {
            let url = format!("http://{metrics_addr}/metrics");
            let needle = format!("zbridge_active_connections{{service=\"{service}\"}} 0");
            async move {
                match reqwest::get(url).await {
                    Ok(r) => r.text().await.map(|b| b.contains(&needle)).unwrap_or(false),
                    Err(_) => false,
                }
            }
        },
        Duration::from_secs(10),
        "active_connections returns to 0 while idle",
    )
    .await
    .expect("active_connections did not return to 0 after the connection closed");

    // The listener is still healthy after reaping while idle: a second connection
    // round-trips.
    let mut c2 = TcpStream::connect(bridge.import_addr).await.unwrap();
    c2.write_all(b"pong").await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), c2.read(&mut buf))
        .await
        .expect("second read timed out")
        .expect("second read failed");
    assert_eq!(&buf[..n], b"pong");
}

/// B4 regression: an idle client must not be able to pin the observability
/// server's resources, and must not be able to starve it.
///
/// `handle_conn` used to read until end-of-headers with **no timeout**, and
/// connections were spawned with **no cap** — so a peer that connected and then
/// said nothing held a task and a file descriptor indefinitely, and enough of
/// them would exhaust the process. Every other reader in this codebase is
/// bounded (the data plane calls this out explicitly as F4/D3); this server was
/// simply missed.
///
/// The test parks a batch of silent connections and asserts (a) the server keeps
/// answering, and (b) the silent connections are hung up on rather than held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_server_hangs_up_on_idle_clients_and_stays_available() {
    let port = common::PortGuard::new();
    let metrics_addr = port.release();
    let metrics_addr_str = metrics_addr.to_string();

    let mut child = common::bridge_command()
        .args([
            "--backend",
            "idlesvc/127.0.0.1:1",
            "--metrics-addr",
            &metrics_addr_str,
            // Keep the test quick: the read bound is what is under test.
            "--read-timeout",
            "1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn bridge");

    common::wait_for_port(metrics_addr, Duration::from_secs(10))
        .await
        .expect("metrics server did not start");

    // Park connections that connect and then say nothing at all.
    let mut idle = Vec::new();
    for _ in 0..16 {
        match tokio::net::TcpStream::connect(metrics_addr).await {
            Ok(s) => idle.push(s),
            Err(_) => break,
        }
    }
    assert!(!idle.is_empty(), "could not open any idle connections");

    // The server must still serve a real request while they are parked.
    let resp = reqwest::Client::new()
        .get(format!("http://{metrics_addr}/healthz"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("/healthz must still answer while idle clients are parked");
    assert_eq!(resp.status(), 200);

    // And each idle connection must be closed by the server rather than held
    // open forever. Reading returns 0 (clean close) once it hangs up.
    let closed = tokio::time::timeout(Duration::from_secs(30), async {
        let mut closed = 0usize;
        for mut s in idle {
            let mut buf = [0u8; 64];
            // The server sends nothing, so a clean EOF is the close signal.
            if let Ok(Ok(0)) = tokio::time::timeout(Duration::from_secs(25), s.read(&mut buf)).await
            {
                closed += 1;
            }
        }
        closed
    })
    .await
    .expect("timed out waiting for the server to hang up on idle clients");

    assert!(
        closed > 0,
        "the server held every idle connection open; it must bound the read"
    );

    let _ = child.kill().await;
}
