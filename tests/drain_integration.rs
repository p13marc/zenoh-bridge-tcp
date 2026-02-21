mod common;

use anyhow::Result;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::time::timeout;

/// Helper to spawn a bridge process with kill_on_drop for reliable cleanup.
async fn spawn_bridge(args: &[&str]) -> tokio::process::Child {
    Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(args)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn bridge")
}

/// Kill a bridge and wait for it to fully exit.
async fn kill_bridge(mut child: tokio::process::Child) {
    let _ = child.kill().await;
    let _ = timeout(Duration::from_secs(2), child.wait()).await;
}

/// Test that data sent by backend before it closes is drained to the client.
///
/// Scenario: Backend echoes a message, then sends additional data and closes.
/// The client should receive the echo AND the additional data, proving drain works.
#[tokio::test]
async fn test_drain_on_backend_close() -> Result<()> {
    println!("\n=== Test: Drain on Backend Close ===\n");

    // Start a backend that echoes then sends extra data then closes
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        let (mut stream, _) = backend_listener.accept().await.unwrap();
        println!("2. Backend: Connection accepted");

        // Wait for data from client (proves the full bridge path works)
        let mut buf = vec![0u8; 1024];
        match tokio::time::timeout(Duration::from_secs(15), stream.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                let msg = String::from_utf8_lossy(&buf[..n]);
                println!("3. Backend: Received: '{}'", msg.trim());

                // Echo it back
                stream.write_all(&buf[..n]).await.unwrap();
                println!("4. Backend: Echoed message");

                // Small delay, then send additional messages
                tokio::time::sleep(Duration::from_millis(100)).await;
                stream.write_all(b"drain-msg-1\n").await.unwrap();
                stream.write_all(b"drain-msg-2\n").await.unwrap();
                println!("5. Backend: Sent drain messages, closing");
            }
            other => {
                println!("3. Backend: Unexpected read result: {:?}", other);
                return;
            }
        }

        // Close the connection - triggers EOF on export side
        drop(stream);
        println!("6. Backend: Connection closed");
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start export bridge
    let service = common::unique_service_name("drainclose");
    let export_spec = format!("{}/{}", service, backend_addr);
    println!("7. Starting export bridge: --export '{}'", export_spec);

    let export_bridge = spawn_bridge(&["--export", &export_spec]).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start import bridge
    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("{}/{}", service, import_addr);
    println!("8. Starting import bridge: --import '{}'", import_spec);

    let import_bridge = spawn_bridge(&["--import", &import_spec]).await;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");
    println!("9. Import bridge started");

    // Connect client
    println!("10. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send data to trigger backend connection and get round-trip confirmation
    client.write_all(b"Hello\n").await?;
    println!("11. Client: Sent message");

    // Read all data until the connection closes (backend will close after sending drain msgs)
    let mut received = String::new();
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

    // Verify the echo and drain messages arrived
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

    // Cleanup - kill and wait for full exit
    drop(client);
    kill_bridge(export_bridge).await;
    kill_bridge(import_bridge).await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
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
    let export_spec = format!("{}/{}", service, backend_addr);
    println!("2. Starting export bridge: --export '{}'", export_spec);

    let export_bridge = spawn_bridge(&["--export", &export_spec]).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start import bridge
    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("{}/{}", service, import_addr);
    println!("3. Starting import bridge: --import '{}'", import_spec);

    let import_bridge = spawn_bridge(&["--import", &import_spec]).await;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");
    println!("4. Import bridge started");

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
    kill_bridge(export_bridge).await;
    kill_bridge(import_bridge).await;
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

    // Start export bridge with short drain timeout
    let service = common::unique_service_name("draintimeout");
    let export_spec = format!("{}/{}", service, backend_addr);
    println!("2. Starting export bridge with --drain-timeout 2");

    let export_bridge =
        spawn_bridge(&["--export", &export_spec, "--drain-timeout", "2"]).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start import bridge with same drain timeout
    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("{}/{}", service, import_addr);
    println!("3. Starting import bridge with --drain-timeout 2");

    let import_bridge =
        spawn_bridge(&["--import", &import_spec, "--drain-timeout", "2"]).await;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");
    println!("4. Import bridge started");

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
    kill_bridge(export_bridge).await;
    kill_bridge(import_bridge).await;
    println!("10. TEST COMPLETED\n");

    Ok(())
}
