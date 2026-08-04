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
