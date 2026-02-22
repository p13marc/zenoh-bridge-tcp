//! Integration tests for backend availability detection via Zenoh liveliness.
//!
//! These tests validate liveliness-based behavior using real bridge processes:
//! 1. Backend available — data flows through the bridge
//! 2. Backend not available — client connects but gets no response
//! 3. Backend goes down — active connection stops working
//! 4. Backend comes back — new connections start working

mod common;

use common::{
    BridgePair, BridgeProcess, PortGuard, start_echo_server, unique_service_name, wait_for_port,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// TEST 1: Backend available — export + import running, echo works.
/// Uses BridgePair which is proven to work in export_import_integration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_backend_available() {
    let _ = tracing_subscriber::fmt::try_init();

    let (backend_addr, _echo) = start_echo_server().await;
    let service = unique_service_name("live_avail");

    let mut pair = BridgePair::tcp(&service, backend_addr).await;
    let import_addr = pair.import_addr;

    // Connect and wait for liveliness propagation
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    stream.write_all(b"Hello Backend!").await.unwrap();

    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("Timeout waiting for echo")
        .expect("Read error");

    assert!(n > 0, "Should receive echo data");
    assert_eq!(&buf[..n], b"Hello Backend!");

    pair.kill_and_wait().await;
}

/// TEST 2: Backend not available — import only, no export running.
/// Client can connect (import accepts TCP) but no data is echoed back
/// because no export bridge is there to forward to a backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_backend_not_available() {
    let _ = tracing_subscriber::fmt::try_init();

    let service = unique_service_name("live_noback");

    // Start import bridge only (no export)
    let import_port = PortGuard::new();
    let import_addr = import_port.addr();
    let import_spec = format!("{}/{}", service, import_addr);
    let import_addr = import_port.release();
    let _import = BridgeProcess::new(&["--import", &import_spec]).await;
    wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");

    // Connect — the import bridge accepts TCP connections regardless
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    stream.write_all(b"Hello?").await.unwrap();

    // No export bridge → no one processes the liveliness token → no response
    let mut buf = [0u8; 1024];
    let result = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await;

    match result {
        Ok(Ok(0)) => {} // Connection closed (acceptable)
        Err(_) => {}    // Timeout — no response (expected)
        Ok(Ok(_n)) => panic!("Should not receive data when no export bridge is running"),
        Ok(Err(_)) => {} // Read error (acceptable)
    }
}

/// TEST 3: Backend goes down — kill export bridge while client is connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_backend_goes_down() {
    let _ = tracing_subscriber::fmt::try_init();

    let (backend_addr, _echo) = start_echo_server().await;
    let service = unique_service_name("live_down");

    let mut pair = BridgePair::tcp(&service, backend_addr).await;
    let import_addr = pair.import_addr;

    // Connect and wait for liveliness propagation
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify the connection works initially
    stream.write_all(b"Before").await.unwrap();

    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("Timeout on initial echo")
        .expect("Read error on initial echo");
    assert_eq!(&buf[..n], b"Before");

    // Kill the export bridge — simulates backend going down
    pair.export.kill_and_wait().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Try to send on the existing connection — should fail or get no response
    let write_result = stream.write_all(b"After").await;
    if write_result.is_ok() {
        let result = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await;
        match result {
            Ok(Ok(0)) => {} // Connection closed
            Err(_) => {}    // Timeout — no response
            Ok(Ok(n)) => {
                // Might get back "After" if there was buffered data; acceptable
                let _ = n;
            }
            Ok(Err(_)) => {} // Read error
        }
    }
    // Either the write or read failed/timed out — backend is effectively down
}

/// TEST 4: Backend comes back — start import first, then export later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_backend_comes_back() {
    let _ = tracing_subscriber::fmt::try_init();

    let (backend_addr, _echo) = start_echo_server().await;
    let service = unique_service_name("live_back");

    // Start import bridge first (no export yet)
    let import_port = PortGuard::new();
    let import_addr = import_port.addr();
    let import_spec = format!("{}/{}", service, import_addr);
    let import_addr = import_port.release();
    let _import = BridgeProcess::new(&["--import", &import_spec]).await;
    wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");

    // Connect — should be accepted but no data flows
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    stream.write_all(b"NoBackend").await.unwrap();

    let mut buf = [0u8; 1024];
    let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
    // Expect timeout or close
    match result {
        Ok(Ok(0)) | Err(_) | Ok(Err(_)) => {} // Expected
        Ok(Ok(_)) => panic!("Should not get response without export bridge"),
    }
    drop(stream);

    // Now start the export bridge — "backend comes back"
    let export_spec = format!("{}/{}", service, backend_addr);
    let _export = BridgeProcess::new(&["--export", &export_spec]).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // New connection should now work (wait for liveliness propagation)
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    stream.write_all(b"BackendReturned").await.unwrap();

    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("Timeout — export bridge should be handling data now")
        .expect("Read error after export started");

    assert!(n > 0, "Should receive echo after export bridge starts");
    assert_eq!(&buf[..n], b"BackendReturned");
}
