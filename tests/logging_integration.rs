//! The logging surface end to end: sinks, JSON shape, and the access log.
//!
//! These drive real bridge processes rather than an in-process subscriber,
//! because the things most likely to break are the ones only a real process
//! exercises — CLI parsing, sink construction, background writer flushing on
//! shutdown, and whether the span fields actually reach the sink.

mod common;

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Read a log file as JSON lines, skipping anything unparseable.
fn json_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// A trivial TCP echo backend; returns its address and a shutdown handle.
async fn echo_backend() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    (addr, handle)
}

/// A `file=` sink must produce parseable JSON containing the startup events,
/// and must not leak ANSI escapes into the file.
#[tokio::test]
async fn file_sink_writes_ansi_free_json() {
    let dir = tempdir("file-sink");
    let log_path = dir.join("bridge.log");
    let metrics_port = common::PortGuard::new();
    let metrics_addr = metrics_port.release();

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--backend",
            "logsvc/127.0.0.1:1",
            "--metrics-addr",
            &metrics_addr.to_string(),
            "--log-format",
            "json",
            "--log-target",
            &format!("file={}", log_path.display()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn bridge");

    // The metrics port coming up means the process is past logging init.
    common::wait_for_port(metrics_addr, Duration::from_secs(10))
        .await
        .expect("bridge did not start");

    let path = log_path.clone();
    common::wait_for(
        || {
            let path = path.clone();
            async move { !json_lines(&path).is_empty() }
        },
        Duration::from_secs(10),
        "the file sink to receive a JSON line",
    )
    .await
    .expect("nothing was written to the file sink");

    let _ = child.kill().await;

    let raw = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        !raw.contains('\u{1b}'),
        "ANSI escapes leaked into the file sink: {raw:?}"
    );

    let lines = json_lines(&log_path);
    // Every line must be a real JSON object with the expected envelope, not
    // just "some JSON happened to parse".
    for line in &lines {
        assert!(line["timestamp"].is_string(), "no timestamp in {line}");
        assert!(line["level"].is_string(), "no level in {line}");
        assert!(line["fields"]["message"].is_string(), "no message in {line}");
    }
    assert!(
        lines
            .iter()
            .any(|l| l["fields"]["message"] == "Zenoh session established"),
        "startup events missing from the file sink: {lines:?}"
    );
    // The bridge's own target must survive the dependency damping default --
    // `zenoh=warn` is a string-prefix match that also matches
    // `zenoh_bridge_tcp`, which once silenced everything.
    assert!(
        lines
            .iter()
            .any(|l| l["target"].as_str().is_some_and(|t| t.starts_with("zenoh_bridge_tcp"))),
        "no events from the bridge's own target: {lines:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The headline record: one queryable event per connection carrying the
/// outcome, byte counts and duration, with identity inherited from the
/// connection span. Byte counts must agree with what `/metrics` reports.
#[tokio::test]
async fn access_log_reports_outcome_bytes_and_duration() {
    let dir = tempdir("access-log");
    let export_log = dir.join("export.log");
    let import_log = dir.join("import.log");

    let (backend_addr, backend) = echo_backend().await;

    let listen_port = common::PortGuard::new();
    let listen_addr = listen_port.release();
    let metrics_port = common::PortGuard::new();
    let metrics_addr = metrics_port.release();

    let mut exporter = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--backend",
            &format!("accesssvc/{backend_addr}"),
            "--log-format",
            "json",
            "--log-target",
            &format!("file={}", export_log.display()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn exporter");

    let mut importer = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--listen",
            &format!("accesssvc/{listen_addr},proto=raw"),
            "--metrics-addr",
            &metrics_addr.to_string(),
            "--log-format",
            "json",
            "--log-target",
            &format!("file={}", import_log.display()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn importer");

    common::wait_for_port(metrics_addr, Duration::from_secs(10))
        .await
        .expect("metrics server did not start");

    // Deliberately NOT wait_for_port on the listener: a probe connection is a
    // real client as far as the bridge is concerned, and would earn its own
    // zero-byte access record. Retry the connect instead, so the first
    // connection that succeeds is the one under test.
    const PAYLOAD: &[u8] = b"access log payload";
    let mut client = {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::net::TcpStream::connect(listen_addr).await {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("importer never accepted a connection: {e}"),
            }
        }
    };
    // Liveliness has to reach the exporter before it dials the backend.
    tokio::time::sleep(Duration::from_secs(2)).await;
    client.write_all(PAYLOAD).await.unwrap();
    let mut buf = vec![0u8; PAYLOAD.len()];
    client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, PAYLOAD, "echo did not round-trip");
    drop(client);

    // Wait for the access record rather than sleeping a fixed amount.
    let path = import_log.clone();
    common::wait_for(
        || {
            let path = path.clone();
            async move {
                json_lines(&path)
                    .iter()
                    .any(|l| l["fields"]["message"] == "connection closed")
            }
        },
        Duration::from_secs(15),
        "the import access-log record",
    )
    .await
    .expect("no access-log record was written");

    // Read /metrics before killing the process.
    let body = reqwest::get(format!("http://{metrics_addr}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let import_records: Vec<_> = json_lines(&import_log)
        .into_iter()
        .filter(|l| l["fields"]["message"] == "connection closed")
        .collect();
    assert_eq!(
        import_records.len(),
        1,
        "expected exactly one access record, got {import_records:?}"
    );
    let rec = &import_records[0];

    assert_eq!(rec["target"], "zenoh_bridge_tcp::access");
    assert_eq!(rec["fields"]["outcome"], "completed");
    assert_eq!(rec["fields"]["bytes_up"], PAYLOAD.len());
    assert_eq!(rec["fields"]["bytes_down"], PAYLOAD.len());
    // The connection was held open ~2s before the payload, so the duration is
    // real elapsed time and not a zero placeholder.
    let duration_ms = rec["fields"]["duration_ms"].as_u64().expect("duration_ms");
    assert!(duration_ms >= 1000, "implausible duration: {duration_ms}ms");

    // Identity comes from the connection span, not from the message text.
    let span = &rec["span"];
    assert_eq!(span["service"], "accesssvc");
    assert_eq!(span["mode"], "import");
    let client_id = span["client_id"].as_str().expect("client_id on the span");
    assert!(!client_id.is_empty());
    assert!(span["remote_addr"].is_string(), "no remote_addr: {span}");
    // The message is a static literal, so nothing is interpolated into it.
    assert!(
        !rec["fields"]["message"]
            .as_str()
            .unwrap()
            .contains(client_id),
        "identity leaked into the message text: {rec}"
    );

    // The access log and Prometheus must not disagree about the same bytes.
    let metric_line = body
        .lines()
        .find(|l| l.starts_with("zbridge_bytes_total") && l.contains("direction=\"up\""))
        .unwrap_or_else(|| panic!("no up-byte metric in: {body}"));
    let metric_bytes: u64 = metric_line
        .rsplit(' ')
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        metric_bytes,
        PAYLOAD.len() as u64,
        "access log and /metrics disagree; metrics body: {body}"
    );
    assert!(
        body.contains("zbridge_connections_outcome_total")
            && body.contains("outcome=\"completed\""),
        "outcome metric missing: {body}"
    );

    // The exporter records the same connection under the same client_id, which
    // is what makes the two sides correlatable in an aggregator.
    let export_path = export_log.clone();
    let wanted = client_id.to_string();
    common::wait_for(
        || {
            let path = export_path.clone();
            let wanted = wanted.clone();
            async move {
                json_lines(&path).iter().any(|l| {
                    l["fields"]["message"] == "connection closed"
                        && l["span"]["client_id"] == wanted.as_str()
                })
            }
        },
        Duration::from_secs(15),
        "the matching export access-log record",
    )
    .await
    .expect("export side did not log the same connection");

    let _ = importer.kill().await;
    let _ = exporter.kill().await;
    backend.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A bad `--log-target` must be rejected at startup with a message naming the
/// flag and the offending value, not accepted and silently ignored.
#[tokio::test]
async fn invalid_log_target_is_rejected_at_startup() {
    let out = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--backend",
            "svc/127.0.0.1:1",
            "--log-target",
            "elasticsearch",
        ])
        .output()
        .await
        .expect("Failed to run bridge");

    assert!(!out.status.success(), "an unknown sink should not start");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("log-target"), "got: {stderr}");
    assert!(stderr.contains("elasticsearch"), "got: {stderr}");
}

/// `--log-target stderr` must leave stdout empty — the point of the flag for
/// setups that reserve stdout for data.
#[tokio::test]
async fn stderr_sink_leaves_stdout_empty() {
    let metrics_port = common::PortGuard::new();
    let metrics_addr = metrics_port.release();

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--backend",
            "stderrsvc/127.0.0.1:1",
            "--metrics-addr",
            &metrics_addr.to_string(),
            "--log-target",
            "stderr",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn bridge");

    common::wait_for_port(metrics_addr, Duration::from_secs(10))
        .await
        .expect("bridge did not start");
    let _ = child.kill().await;
    let out = child.wait_with_output().await.expect("collecting output");

    assert!(
        out.stdout.is_empty(),
        "stdout should be empty, got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Zenoh session established"),
        "logs did not reach stderr: {stderr}"
    );
    // Piped output is not a terminal, so `--log-color auto` must not colour it.
    assert!(
        !stderr.contains('\u{1b}'),
        "ANSI escapes written to a non-terminal: {stderr:?}"
    );
}

/// A unique temp directory per test, so the suite can run in parallel.
fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zbridge-logging-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("creating the temp log directory");
    dir
}
