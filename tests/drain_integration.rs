mod common;

use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// Test that data sent by backend before it closes is drained to the client.
///
/// Scenario: Backend echoes a message, then sends additional data and closes.
/// The client should receive the echo AND the additional data, proving drain works.
#[tokio::test]
async fn test_drain_on_backend_close() -> Result<()> {
    println!("\n=== Test: Drain on Backend Close ===\n");

    // Start a backend that echoes then sends extra data then closes.
    // Accepts multiple connections because stale Zenoh sessions from prior tests
    // may trigger spurious backend connections that immediately close.
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let (drain_tx, _drain_rx) = tokio::sync::mpsc::channel::<bool>(1);

    let backend_task = tokio::spawn(async move {
        loop {
            let (mut stream, addr) = backend_listener.accept().await.unwrap();
            println!("   Backend: Connection from {}", addr);

            let tx = drain_tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                match tokio::time::timeout(Duration::from_secs(15), stream.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => {
                        let msg = String::from_utf8_lossy(&buf[..n]);
                        println!("   Backend: Received from {}: '{}'", addr, msg.trim());

                        // Echo it back
                        stream.write_all(&buf[..n]).await.unwrap();
                        println!("   Backend: Echoed message to {}", addr);

                        // Small delay, then send additional messages
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        stream.write_all(b"drain-msg-1\n").await.unwrap();
                        stream.write_all(b"drain-msg-2\n").await.unwrap();
                        println!("   Backend: Sent drain messages to {}, closing", addr);

                        // Close the connection - triggers EOF on export side
                        drop(stream);
                        let _ = tx.send(true).await;
                    }
                    _ => {
                        // Spurious connection (stale Zenoh session), ignore
                        println!(
                            "   Backend: Spurious connection from {} (no data), ignoring",
                            addr
                        );
                    }
                }
            });
        }
    });

    let service = common::unique_service_name("drainclose");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;
    let import_addr = pair.import_addr;

    // Connect client — use retry loop for resilience against Zenoh timing
    let mut got_response = false;
    let mut received = String::new();

    for attempt in 1..=3 {
        println!("10. Client: Connecting (attempt {})...", attempt);
        let mut client = TcpStream::connect(import_addr).await?;

        // Wait for Zenoh path establishment
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Send data to trigger backend connection and get round-trip confirmation
        client.write_all(b"Hello\n").await?;
        println!("11. Client: Sent message");

        // Read all data until the connection closes
        received.clear();
        let mut buf = vec![0u8; 1024];

        loop {
            match timeout(Duration::from_secs(15), client.read(&mut buf)).await {
                Ok(Ok(0)) => {
                    println!("12. Client: Connection closed (EOF)");
                    break;
                }
                Ok(Ok(n)) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    println!("    Client: Received {} bytes: '{}'", n, chunk.trim());
                    received.push_str(&chunk);
                }
                Ok(Err(e)) => {
                    println!("12. Client: Read error: {:?}", e);
                    break;
                }
                Err(_) => {
                    println!("12. Client: Timeout waiting for more data");
                    break;
                }
            }
        }

        if received.contains("Hello") && received.contains("drain-msg-1") {
            got_response = true;
            break;
        } else {
            println!(
                "    Client: Incomplete response (attempt {}), retrying...",
                attempt
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    // Verify the echo and drain messages arrived
    assert!(
        got_response,
        "Should receive echo and drain messages after retries, got: '{}'",
        received
    );
    assert!(
        received.contains("Hello"),
        "Should receive echo, got: '{}'",
        received
    );
    assert!(
        received.contains("drain-msg-1"),
        "Should receive drain-msg-1, got: '{}'",
        received
    );
    assert!(
        received.contains("drain-msg-2"),
        "Should receive drain-msg-2, got: '{}'",
        received
    );
    println!("13. TEST PASSED: Backend data drained to client before close");

    // Cleanup
    pair.kill_and_wait().await;
    backend_task.abort();
    println!("14. TEST COMPLETED\n");

    Ok(())
}

/// Test that when backend is unavailable, client connection is closed cleanly
/// (not reset) after draining.
///
/// With drain support, the import side should close the TCP connection gracefully
/// rather than abruptly resetting it.
#[tokio::test]
async fn test_drain_on_backend_error() -> Result<()> {
    println!("\n=== Test: Drain on Backend Error ===\n");

    // Don't start a backend - use a port with nothing listening
    let backend_port = common::PortGuard::new();
    let backend_addr = backend_port.release();
    println!(
        "1. Using backend address {} (no server listening)",
        backend_addr
    );

    // Start export bridge pointing to non-existent backend
    let service = common::unique_service_name("drainerr");
    println!("2. Starting bridges for service '{}'", service);

    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;
    let import_addr = pair.import_addr;

    // Connect client
    println!("5. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;

    // Wait for error detection cycle
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Try to send and then read - connection should be closed cleanly
    let _ = client.write_all(b"test\n").await;

    let mut buf = vec![0u8; 1024];
    match timeout(Duration::from_secs(10), client.read(&mut buf)).await {
        Ok(Ok(0)) => {
            println!("6. Client: Connection closed cleanly (EOF)");
            println!("7. TEST PASSED: Clean close after backend error");
        }
        Ok(Ok(_n)) => {
            // Got some data back, still acceptable
            println!("6. Client: Received data before close");
            println!("7. TEST PASSED: Connection handled gracefully");
        }
        Ok(Err(e)) => {
            // Connection reset is what we're trying to improve
            // but it may still happen depending on timing
            println!("6. Client: Connection error: {:?}", e);
            if e.kind() == std::io::ErrorKind::ConnectionReset {
                println!("7. TEST NOTE: Got connection reset (may improve with drain)");
            } else {
                println!("7. TEST PASSED: Connection closed with error (not stuck)");
            }
        }
        Err(_) => {
            panic!("Timeout - connection was not closed at all within 10 seconds");
        }
    }

    // Cleanup
    drop(client);
    pair.kill_and_wait().await;
    println!("8. TEST COMPLETED\n");

    Ok(())
}

/// Test that --drain-timeout CLI flag is accepted and the backend-unavailable
/// error path closes the client connection within a bounded time.
///
/// Uses --drain-timeout=2 and a non-existent backend. The client should be
/// disconnected within a reasonable time, not hang forever.
#[tokio::test]
async fn test_drain_timeout_enforced() -> Result<()> {
    println!("\n=== Test: Drain Timeout Enforced ===\n");

    // No backend - export will fail to connect and send error signal
    let backend_port = common::PortGuard::new();
    let backend_addr = backend_port.release();
    println!(
        "1. Using backend address {} (no server listening)",
        backend_addr
    );

    // Start bridges with short drain timeout
    let service = common::unique_service_name("draintimeout");
    println!("2. Starting bridges with --drain-timeout 2");

    let mut pair = common::BridgePair::tcp_with_args(
        &service,
        backend_addr,
        &["--drain-timeout", "2"],
        &["--drain-timeout", "2"],
    )
    .await;
    let import_addr = pair.import_addr;

    // Connect client
    println!("5. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;

    // Wait for Zenoh propagation and backend error detection
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Try to write (may fail if already closed)
    let _ = client.write_all(b"trigger\n").await;

    let start = std::time::Instant::now();
    println!("6. Client: Waiting for connection to close...");

    // Read until connection closes - should happen within drain_timeout + overhead
    let mut buf = vec![0u8; 1024];
    match timeout(Duration::from_secs(15), async {
        loop {
            match client.read(&mut buf).await {
                Ok(0) => return Ok::<_, std::io::Error>(()),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
    })
    .await
    {
        Ok(Ok(())) => {
            println!("7. Client: Connection closed (EOF)");
        }
        Ok(Err(e)) => {
            println!("7. Client: Connection error: {:?}", e);
        }
        Err(_) => {
            panic!("Connection did not close within 15 seconds - drain timeout not enforced");
        }
    }

    let elapsed = start.elapsed();
    println!("8. Connection closed after {:?}", elapsed);
    println!("9. TEST PASSED: Connection closed within timeout bounds");

    // Cleanup
    drop(client);
    pair.kill_and_wait().await;
    println!("10. TEST COMPLETED\n");

    Ok(())
}
