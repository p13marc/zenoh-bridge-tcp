//! Regression safety net (issue #44).
//!
//! Characterization tests that pin observable behavior of the raw TCP relay
//! before/after the hardening work. They use the subprocess `BridgePair`
//! harness and a retry-until-ready round trip so they do not depend on a
//! fixed liveliness-propagation sleep.
//!
//! Coverage added here:
//! - golden byte-stream round trip (opaque relay is byte-exact),
//! - large transfer (>buffer_size, exercises chunking + cache),
//! - backend close propagates to the client as EOF,
//! - client half-close (currently a documented, ignored regression target —
//!   propagation is deferred to the framing-layer work, issue #20).

mod common;

use common::{BridgePair, start_echo_server, unique_service_name};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Connect through the import bridge and round-trip `payload`, retrying the
/// whole connect+write+read until the backend echoes or `budget` elapses.
///
/// Retrying absorbs Zenoh liveliness propagation (import declares liveliness →
/// export detects it → export connects the backend) without a fixed sleep.
async fn round_trip_with_retry(
    import_addr: std::net::SocketAddr,
    payload: &[u8],
    budget: Duration,
) -> Vec<u8> {
    let start = std::time::Instant::now();
    let mut last_err = String::from("never attempted");
    while start.elapsed() < budget {
        match try_round_trip(import_addr, payload).await {
            Ok(got) => return got,
            Err(e) => {
                last_err = e;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    panic!("round trip did not succeed within {budget:?}: {last_err}");
}

async fn try_round_trip(
    import_addr: std::net::SocketAddr,
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect(import_addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    stream
        .write_all(payload)
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .map_err(|_| "read timed out (backend not wired yet)".to_string())?
        .map_err(|e| format!("read: {e}"))?;
    Ok(buf)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_round_trip_is_byte_exact() {
    let (backend, _backend) = start_echo_server().await;
    let service = unique_service_name("regnet_rt");
    let mut pair = BridgePair::tcp(&service, backend).await;

    let payload = b"hello-zenoh-bridge-tcp-golden-round-trip";
    let got = round_trip_with_retry(pair.import_addr, payload, Duration::from_secs(20)).await;
    assert_eq!(got, payload, "opaque relay must be byte-exact");

    pair.kill_and_wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_transfer_is_byte_exact() {
    let (backend, _backend) = start_echo_server().await;
    let service = unique_service_name("regnet_large");
    let mut pair = BridgePair::tcp(&service, backend).await;

    // 1 MiB — well past the default 64 KiB buffer, so it exercises chunking.
    let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let got = round_trip_with_retry(pair.import_addr, &payload, Duration::from_secs(30)).await;
    assert_eq!(got.len(), payload.len(), "length must match");
    assert_eq!(got, payload, "large transfer must be byte-exact");

    pair.kill_and_wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_close_propagates_to_client_as_eof() {
    // A backend that, for every connection, echoes exactly one read then drops
    // the connection (FIN). Loop-accept so liveliness-propagation retries during
    // warm-up are each served (and closed), including the final kept connection.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    let _backend = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                if let Ok(n) = stream.read(&mut buf).await
                    && n > 0
                {
                    let _ = stream.write_all(&buf[..n]).await;
                }
                // Drop `stream` -> FIN to the backend side of the bridge.
            });
        }
    });

    let service = unique_service_name("regnet_close");
    let mut pair = BridgePair::tcp(&service, backend).await;

    // Establish the connection and get the one echo (retry for liveliness).
    let start = std::time::Instant::now();
    let mut stream = loop {
        if start.elapsed() > Duration::from_secs(20) {
            panic!("bridge/backend never became ready");
        }
        let mut s = match TcpStream::connect(pair.import_addr).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        if s.write_all(b"ping").await.is_err() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        let mut echo = [0u8; 4];
        match tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut echo)).await {
            Ok(Ok(_)) => break s,
            _ => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };

    // The backend has closed; the client's next read must observe EOF (0 bytes),
    // not hang. This pins the clean-EOF propagation path.
    let mut tail = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tail))
        .await
        .expect("client read must not hang after backend close")
        .expect("read after backend close");
    assert_eq!(n, 0, "client must see EOF after the backend closes");

    pair.kill_and_wait().await;
}

/// Client half-close: after the client shuts down its write half, the response
/// direction must still deliver the backend's reply.
///
/// This is the desired behavior per issues #13/#14 (B1/B2). It is currently
/// broken — the response direction is cancelled on client half-close — and the
/// fix is deferred to the framing-layer work (#20). Kept as an ignored,
/// documented regression target so it is not forgotten.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "B1/B2: half-close propagation deferred to the framing layer (#20)"]
async fn client_half_close_still_delivers_response() {
    let (backend, _backend) = start_echo_server().await;
    let service = unique_service_name("regnet_halfclose");
    let mut pair = BridgePair::tcp(&service, backend).await;

    // Wait until the path is wired by doing one successful round trip first.
    let _ = round_trip_with_retry(pair.import_addr, b"warmup", Duration::from_secs(20)).await;

    let mut stream = TcpStream::connect(pair.import_addr).await.unwrap();
    let payload = b"request-then-half-close";
    stream.write_all(payload).await.unwrap();
    stream.shutdown().await.unwrap(); // half-close the write side

    let mut buf = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf))
        .await
        .expect("response must still arrive after client half-close")
        .expect("read response");
    assert_eq!(&buf, payload);

    pair.kill_and_wait().await;
}
