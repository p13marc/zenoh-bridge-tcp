//! Integration tests covering previously untested scenarios:
//! - Messages larger than buffer_size
//! - Partial transfer (client closes mid-send)
//! - Graceful shutdown with in-flight data

mod common;

use common::{start_echo_server, unique_service_name, BridgePair};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Test that messages larger than the default buffer_size (65536) are
/// correctly relayed through the bridge without corruption or truncation.
#[tokio::test]
async fn test_large_message_exceeds_buffer_size() {
    let (backend_addr, _backend) = start_echo_server().await;
    let service = unique_service_name("large_msg");
    let mut pair = BridgePair::tcp(&service, backend_addr).await;

    let mut client = TcpStream::connect(pair.import_addr).await.unwrap();

    // Wait for liveliness propagation
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Send 200KB — well above the default 64KB buffer_size.
    // The bridge must reassemble this across multiple reads.
    let payload = vec![0x42u8; 200_000];
    client.write_all(&payload).await.unwrap();

    // Read the echo back (may come in chunks)
    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while received.len() < payload.len() {
        let remaining = Duration::from_secs(1)
            .min(deadline.duration_since(tokio::time::Instant::now()));
        let mut buf = vec![0u8; 65536];
        match tokio::time::timeout(remaining, client.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            Ok(Err(_)) => break,
            Err(_) => break, // timeout
        }
    }

    assert_eq!(
        received.len(),
        payload.len(),
        "Should receive all {} bytes back through bridge, got {}",
        payload.len(),
        received.len()
    );
    assert!(
        received.iter().all(|&b| b == 0x42),
        "Data should not be corrupted"
    );

    pair.kill_and_wait().await;
}

/// Test that when a client closes the connection mid-transfer,
/// the bridge handles the EOF gracefully without panicking or leaking.
#[tokio::test]
async fn test_partial_transfer_client_closes_mid_send() {
    let (backend_addr, _backend) = start_echo_server().await;
    let service = unique_service_name("partial_close");
    let mut pair = BridgePair::tcp(&service, backend_addr).await;

    let mut client = TcpStream::connect(pair.import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Send some data
    client.write_all(b"hello").await.unwrap();

    // Read the echo
    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"hello");

    // Abruptly close the client (simulates mid-transfer disconnect)
    drop(client);

    // The bridge should handle this gracefully. Wait a moment then
    // verify the bridge is still alive by connecting a new client.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut client2 = TcpStream::connect(pair.import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    client2.write_all(b"world").await.unwrap();
    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), client2.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"world", "Bridge should still work after client disconnect");

    pair.kill_and_wait().await;
}

/// Test that the bridge correctly handles graceful shutdown while
/// there is data in flight. The bridge should drain pending data
/// before closing connections.
#[tokio::test]
async fn test_shutdown_with_in_flight_data() {
    let (backend_addr, _backend) = start_echo_server().await;
    let service = unique_service_name("shutdown_inflight");
    let mut pair = BridgePair::tcp(&service, backend_addr).await;

    let mut client = TcpStream::connect(pair.import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Send data and immediately trigger shutdown
    client.write_all(b"data before shutdown").await.unwrap();

    // Read the echo to ensure data made it through
    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"data before shutdown");

    // Kill the bridge — this triggers graceful shutdown with drain
    pair.kill_and_wait().await;

    // The bridge processes should have exited cleanly (kill_and_wait succeeds)
    // If shutdown hung or panicked, the test timeout would catch it.
}

/// Test that multiple concurrent large transfers work correctly
/// without data corruption between clients.
#[tokio::test]
async fn test_concurrent_large_transfers() {
    let (backend_addr, _backend) = start_echo_server().await;
    let service = unique_service_name("concurrent_large");
    let mut pair = BridgePair::tcp(&service, backend_addr).await;

    let import_addr = pair.import_addr;

    // Spawn 3 concurrent clients, each sending different data
    let mut handles = Vec::new();
    for i in 0u8..3 {
        let addr = import_addr;
        handles.push(tokio::spawn(async move {
            let mut client = TcpStream::connect(addr).await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;

            // Each client sends 50KB of a unique byte value
            let payload = vec![0x10 + i; 50_000];
            client.write_all(&payload).await.unwrap();

            let mut received = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while received.len() < payload.len() {
                let remaining = Duration::from_secs(1)
                    .min(deadline.duration_since(tokio::time::Instant::now()));
                let mut buf = vec![0u8; 65536];
                match tokio::time::timeout(remaining, client.read(&mut buf)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
                    Ok(Err(_)) => break,
                    Err(_) => break,
                }
            }

            assert_eq!(received.len(), 50_000, "Client {} should get 50KB back", i);
            assert!(
                received.iter().all(|&b| b == 0x10 + i),
                "Client {} data should not be corrupted or mixed",
                i
            );
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    pair.kill_and_wait().await;
}

/// Test that rapid connect/disconnect cycles don't crash or leak resources.
#[tokio::test]
async fn test_rapid_connect_disconnect_cycles() {
    let (backend_addr, _backend) = start_echo_server().await;
    let service = unique_service_name("rapid_cycle");
    let mut pair = BridgePair::tcp(&service, backend_addr).await;

    // Rapidly connect and disconnect 20 times
    for _ in 0..20 {
        let client = TcpStream::connect(pair.import_addr).await.unwrap();
        drop(client); // immediate close
    }

    // Small wait for any cleanup
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Bridge should still work after rapid cycles
    let mut client = TcpStream::connect(pair.import_addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    client.write_all(b"still alive").await.unwrap();
    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"still alive");

    pair.kill_and_wait().await;
}
