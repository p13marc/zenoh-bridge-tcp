//! Protocol-switch behavior of the `route=request` (per-request HTTP) plane:
//! the flowscope-0.24 tunnel contract regressions found by the verification
//! audit — an h2 preface must close the connection (not busy-spin a worker),
//! and a 101 flushed together with the first WebSocket frame must not lose
//! that frame (tunnel residue, flowscope 0.24.1).

mod common;

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// A plaintext gRPC/h2 client hitting a route=request listener sends the h2
/// preface where a request line is expected. flowscope tunnels the parser;
/// with 0.24's push-returns-0 contract the old code busy-spun a worker thread
/// forever on the unshrinkable leftover. The connection must close promptly,
/// and the listener must stay healthy for the next client.
#[tokio::test]
async fn test_multiroute_h2_preface_closes_and_listener_survives() {
    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let service = common::unique_service_name("mrh2");
    let listen_spec = format!("{service}/{import_addr},route=request");
    let _import = common::BridgeProcess::new(&["--listen", &listen_spec]).await;
    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .unwrap();

    // Preface + SETTINGS + PING: the PING is the byte the tunnelled parser
    // refuses, which is what used to spin.
    let mut h2 = TcpStream::connect(import_addr).await.unwrap();
    h2.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .await
        .unwrap();
    h2.write_all(b"\x00\x00\x08\x06\x00\x00\x00\x00\x00pingpong")
        .await
        .unwrap();

    // The bridge must close this connection promptly (EOF), not hang.
    let mut buf = vec![0u8; 256];
    loop {
        match timeout(Duration::from_secs(8), h2.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(_)) => continue, // tolerate any error bytes before close
            Err(_) => panic!("h2-preface connection must be closed, not left hanging"),
        }
    }

    // And the listener still serves: a normal HTTP request on a NEW connection
    // gets the 502 for its unknown host (no backend exported) — proving no
    // worker thread was pinned by the h2 client.
    let mut http = TcpStream::connect(import_addr).await.unwrap();
    http.write_all(concat!("GET / HTTP/1.1\r\n", "Host: nobody.test\r\n", "\r\n").as_bytes())
        .await
        .unwrap();
    let n = timeout(Duration::from_secs(8), http.read(&mut buf))
        .await
        .expect("listener must still answer after an h2-preface client")
        .unwrap();
    assert!(
        buf[..n].starts_with(b"HTTP/1.1 502"),
        "expected a 502 for an unknown host, got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
}

/// A backend that flushes the `101` and the first WebSocket frame in ONE
/// write: the frame rides in the same Zenoh sample as the head, lands in the
/// parser alongside the switch, and before flowscope 0.24.1 was destroyed.
/// The client must receive it (tunnel residue splice), and bytes must keep
/// flowing both ways afterwards.
#[tokio::test]
async fn test_multiroute_upgrade_keeps_coalesced_first_frame() {
    // Backend: reads the upgrade request head, replies 101 + frame in one
    // write, then echoes whatever arrives, verbatim.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let Ok(n) = s.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                // 101 + the first frame, coalesced deliberately (one write,
                // hence one Zenoh sample through the export side).
                let mut coalesced = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n".to_vec();
                coalesced.extend_from_slice(b"\x82\x0bFIRST-FRAME");
                if s.write_all(&coalesced).await.is_err() {
                    return;
                }
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let host = "wsmr.test";
    let service = common::unique_service_name("mrws");
    let export_spec = format!("{service}@{host}/{backend_addr}");
    let _export = common::BridgeProcess::new(&["--backend", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let listen_spec = format!("{service}/{import_addr},route=request");
    let _import = common::BridgeProcess::new(&["--listen", &listen_spec]).await;
    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut client = TcpStream::connect(import_addr).await.unwrap();
    client
        .write_all(
            concat!(
                "GET /chat HTTP/1.1\r\n",
                "Host: wsmr.test\r\n",
                "Upgrade: websocket\r\n",
                "Connection: Upgrade\r\n",
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
                "Sec-WebSocket-Version: 13\r\n",
                "\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    // Expect the 101 AND the coalesced first frame.
    let mut got = Vec::new();
    let mut buf = vec![0u8; 1024];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !got.windows(11).any(|w| w == b"FIRST-FRAME") {
        let n = tokio::time::timeout_at(deadline, client.read(&mut buf))
            .await
            .expect("the coalesced first frame must reach the client")
            .unwrap();
        assert!(n > 0, "connection closed before the first frame arrived");
        got.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&got);
    assert!(text.starts_with("HTTP/1.1 101"), "got: {text:.60}");

    // The tunnel still splices both ways after the residue.
    client.write_all(b"\x82\x04echo").await.unwrap();
    let mut echoed = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !echoed.windows(4).any(|w| w == b"echo") {
        let n = tokio::time::timeout_at(deadline, client.read(&mut buf))
            .await
            .expect("post-switch splice must keep relaying")
            .unwrap();
        assert!(n > 0, "connection closed before the echo arrived");
        echoed.extend_from_slice(&buf[..n]);
    }
}
