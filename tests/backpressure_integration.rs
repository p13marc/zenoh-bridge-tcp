//! D2 (#22): a slow client must not head-of-line-block others on the shared
//! Zenoh session.
//!
//! Before the per-connection backpressure fix, a subscriber's default FIFO
//! handler blocked the shared session's reception thread once full, so one
//! client that stopped reading its socket stalled every other client. These
//! tests drive a real export+import bridge pair over a flooding backend and
//! assert that a paused (non-reading) client does not prevent a concurrent
//! client from making progress.

mod common;

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use common::{BridgePair, unique_service_name};

/// A backend that floods every accepted connection with data as fast as it can,
/// ignoring anything the peer sends. Used to build reception backpressure.
async fn start_flooding_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let chunk = vec![0xABu8; 64 * 1024];
                while stream.write_all(&chunk).await.is_ok() {}
            });
        }
    });
    (addr, handle)
}

/// A slow client stuck not reading must not stop a fast client from receiving
/// the backend flood over the shared session.
#[tokio::test]
async fn slow_client_does_not_stall_fast_client() {
    let (backend_addr, _backend) = start_flooding_backend().await;
    let service = unique_service_name("d2_backpressure");

    // Small reception buffer so the slow client overflows quickly. Stream mode
    // (the default) resets the slow connection on overflow.
    let args = ["--reliability", "stream", "--rx-channel-capacity", "8"];
    let bridge = BridgePair::tcp_with_args(&service, backend_addr, &args, &args).await;

    // Let liveliness propagate (import declares -> export connects -> flood).
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Slow client: connect and then never read. Its reception buffer + socket
    // buffer fill and, in Stream mode, its connection is reset — but crucially
    // the shared session must keep serving everyone else.
    let mut slow = TcpStream::connect(bridge.import_addr).await.unwrap();
    // Nudge the connection open; the backend floods regardless.
    let _ = slow.write_all(b"hello").await;

    // Give the slow client time to back up and trip the reset.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Fast client: connect and read. It must accumulate a healthy amount of the
    // flood promptly, proving the session is not head-of-line-blocked.
    let mut fast = TcpStream::connect(bridge.import_addr).await.unwrap();
    let _ = fast.write_all(b"hello").await;

    const TARGET: usize = 256 * 1024;
    let mut received = 0usize;
    let mut buf = vec![0u8; 64 * 1024];

    let read_target = async {
        while received < TARGET {
            match fast.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => received += n,
                Err(_) => break,
            }
        }
        received
    };

    let got = timeout(Duration::from_secs(15), read_target)
        .await
        .unwrap_or(received);

    // Keep the slow client alive until the assertion so it is genuinely
    // competing for the session the whole time.
    drop(slow);

    assert!(
        got >= TARGET,
        "fast client should keep progressing while a slow client is stuck; \
         only received {} of {} bytes",
        got,
        TARGET
    );
}

/// D2 in `Telemetry` reliability mode: on overflow the reception callback SHEDS
/// the sample (drops it) instead of resetting the connection — but the shared
/// session must still not block, so a concurrent client keeps progressing.
/// (The shed-vs-reset decision itself is unit-tested in `backpressure.rs`.)
#[tokio::test]
async fn telemetry_mode_does_not_stall_fast_client() {
    let (backend_addr, _backend) = start_flooding_backend().await;
    let service = unique_service_name("d2_telemetry");

    let args = ["--reliability", "telemetry", "--rx-channel-capacity", "8"];
    let bridge = BridgePair::tcp_with_args(&service, backend_addr, &args, &args).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut slow = TcpStream::connect(bridge.import_addr).await.unwrap();
    let _ = slow.write_all(b"hello").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut fast = TcpStream::connect(bridge.import_addr).await.unwrap();
    let _ = fast.write_all(b"hello").await;

    const TARGET: usize = 256 * 1024;
    let mut received = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    let read_target = async {
        while received < TARGET {
            match fast.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => received += n,
                Err(_) => break,
            }
        }
        received
    };
    let got = timeout(Duration::from_secs(15), read_target)
        .await
        .unwrap_or(received);
    drop(slow);

    assert!(
        got >= TARGET,
        "in Telemetry mode a slow client must not stall others; got {} of {}",
        got,
        TARGET
    );
}

/// A backend that answers any HTTP request with a large HTTP/1.1 response, used
/// to build reception backpressure on the multiroute path.
async fn start_flooding_http_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await; // consume the request head
                let body = vec![b'x'; 8 * 1024 * 1024];
                let header = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    (addr, handle)
}

fn http_get(host: &str) -> Vec<u8> {
    format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").into_bytes()
}

/// The multiroute path must honor D2 too (#63 follow-up): a slow multiroute
/// client whose huge response backs up must not head-of-line-block a concurrent
/// client on the shared session. Before the fix, `run_exchange`'s response
/// subscriber used the default blocking handler and this stalled everyone.
#[tokio::test]
async fn slow_multiroute_client_does_not_stall_fast_client() {
    let (backend_addr, _backend) = start_flooding_http_backend().await;
    let service = unique_service_name("d2_multiroute");

    let export_spec = format!("{service}/host.test/{backend_addr}");
    let _export = common::BridgeProcess::new(&["--http-export", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let import_spec = format!("{service}/{import_addr}");
    let _import = common::BridgeProcess::new(&[
        "--http-multiroute-import",
        &import_spec,
        "--reliability",
        "stream",
        "--rx-channel-capacity",
        "8",
    ])
    .await;
    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("multiroute import did not start");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Slow client: request, then never read the 8 MiB response.
    let mut slow = TcpStream::connect(import_addr).await.unwrap();
    let _ = slow.write_all(&http_get("host.test")).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Fast client: request and read promptly — must progress despite the slow one.
    // The discriminator is strong: WITHOUT the fix the blocked session delivers
    // *zero* bytes to the fast client, so a modest target + generous timeout stays
    // robust under heavy CI load while still failing hard on a regression.
    let mut fast = TcpStream::connect(import_addr).await.unwrap();
    fast.write_all(&http_get("host.test")).await.unwrap();

    const TARGET: usize = 64 * 1024;
    let mut received = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    let read_target = async {
        while received < TARGET {
            match fast.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => received += n,
                Err(_) => break,
            }
        }
        received
    };
    let got = timeout(Duration::from_secs(30), read_target)
        .await
        .unwrap_or(received);
    drop(slow);

    assert!(
        got >= TARGET,
        "fast multiroute client should progress while a slow one is stuck; \
         only received {} of {} bytes",
        got,
        TARGET
    );
}
