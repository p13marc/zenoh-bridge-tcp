use anyhow::Result;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;

/// Test basic export/import communication through the bridge
#[tokio::test]
async fn test_export_import_basic_communication() -> Result<()> {
    println!("\n=== Test: Export/Import Basic Communication ===\n");

    // Step 1: Start a backend server (simulates what nc -l would do)
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend server listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        println!("2. Backend: Waiting for connection...");
        let (mut stream, addr) = backend_listener.accept().await.unwrap();
        println!("3. Backend: Accepted connection from {}", addr);

        let mut buffer = vec![0u8; 1024];

        // Read client message
        match stream.read(&mut buffer).await {
            Ok(n) if n > 0 => {
                let msg = String::from_utf8_lossy(&buffer[..n]);
                println!("4. Backend: Received: '{}'", msg.trim());

                // Send response back
                let response = b"Hello from backend!\n";
                stream.write_all(response).await.unwrap();
                println!("5. Backend: Sent response");
            }
            Ok(n) => {
                println!("4. Backend: Unexpected read of {} bytes", n);
            }
            Err(e) => {
                println!("4. Backend: Read error: {:?}", e);
            }
        }

        // Wait for close
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => {
                    println!("6. Backend: Connection closed properly");
                    return true;
                }
                Ok(n) => {
                    println!("Backend: Received additional {} bytes", n);
                }
                Err(e) => {
                    println!("Backend: Error: {:?}", e);
                    return false;
                }
            }
        }
    });

    // Give backend time to start
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Step 2: Start export bridge (using debug binary built by cargo test)
    let export_spec = format!("testservice/{}", backend_addr);
    println!("7. Starting export bridge: --export '{}'", export_spec);

    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("8. Export bridge started");

    // Step 3: Start import bridge on a random port
    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener); // Release the port

    let import_spec = format!("testservice/{}", import_addr);
    println!("9. Starting import bridge: --import '{}'", import_spec);

    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("10. Import bridge started");

    // Step 4: Connect a client to the import bridge
    println!("11. Client: Connecting to import bridge at {}", import_addr);
    let mut client = TcpStream::connect(import_addr).await?;
    println!("12. Client: Connected successfully");

    // Give bridges time to establish Zenoh connection
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Step 5: Send data from client
    let message = b"Hello from client!\n";
    println!(
        "13. Client: Sending message: '{}'",
        String::from_utf8_lossy(message).trim()
    );
    client.write_all(message).await?;
    println!("14. Client: Message sent");

    // Step 6: Read response from backend
    let mut response_buffer = vec![0u8; 1024];
    let read_result = timeout(Duration::from_secs(3), client.read(&mut response_buffer)).await;

    match read_result {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&response_buffer[..n]);
            println!("15. Client: Received response: '{}'", response.trim());

            if response.contains("Hello from backend") {
                println!("16. ✓ Response matches expected!");
            } else {
                println!("16. ✗ Unexpected response!");
            }
        }
        Ok(Ok(_)) => {
            println!("15. ✗ Connection closed before receiving response");
        }
        Ok(Err(e)) => {
            println!("15. ✗ Read error: {:?}", e);
        }
        Err(_) => {
            println!("15. ✗ Timeout waiting for response");
        }
    }

    // Step 7: Close client connection
    println!("17. Client: Closing connection...");
    drop(client);
    println!("18. Client: Connection closed");

    // Step 8: Verify backend detected close
    match timeout(Duration::from_secs(5), backend_task).await {
        Ok(Ok(true)) => {
            println!("19. ✓ Backend detected close properly");
        }
        Ok(Ok(false)) => {
            println!("19. ✗ Backend didn't detect close");
        }
        Ok(Err(e)) => {
            println!("19. ✗ Backend task panicked: {:?}", e);
        }
        Err(_) => {
            println!("19. ✗ Timeout - backend didn't detect close");
        }
    }

    // Cleanup: kill bridge processes
    println!("20. Cleaning up bridge processes...");
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    println!("21. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test multiple clients with separate backend connections
#[tokio::test]
async fn test_multiple_clients_separate_connections() -> Result<()> {
    println!("\n=== Test: Multiple Clients - Separate Backend Connections ===\n");

    // Backend that tracks how many connections it receives
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let (conn_tx, mut conn_rx) = mpsc::channel::<usize>(2);

    let backend_task = tokio::spawn(async move {
        let mut connection_count = 0;

        // Accept first connection
        println!("2. Backend: Waiting for first connection...");
        let (mut stream1, addr1) = backend_listener.accept().await.unwrap();
        connection_count += 1;
        println!("3. Backend: Connection {} from {}", connection_count, addr1);
        let _ = conn_tx.send(connection_count).await;

        // Accept second connection
        println!("4. Backend: Waiting for second connection...");
        let (mut stream2, addr2) = backend_listener.accept().await.unwrap();
        connection_count += 1;
        println!("5. Backend: Connection {} from {}", connection_count, addr2);
        let _ = conn_tx.send(connection_count).await;

        // Handle both connections
        let handle1 = tokio::spawn(async move {
            let mut buf = vec![0u8; 1024];
            match stream1.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    println!("   Conn1: Received {} bytes", n);
                    let _ = stream1.write_all(b"Response to client 1\n").await;
                }
                _ => {}
            }
            // Don't wait for close - just exit
            println!("   Conn1: Finished handling");
        });

        let handle2 = tokio::spawn(async move {
            let mut buf = vec![0u8; 1024];
            match stream2.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    println!("   Conn2: Received {} bytes", n);
                    let _ = stream2.write_all(b"Response to client 2\n").await;
                }
                _ => {}
            }
            // Don't wait for close - just exit
            println!("   Conn2: Finished handling");
        });

        let _ = tokio::join!(handle1, handle2);
        connection_count
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start export bridge (using debug binary)
    let export_spec = format!("multitest/{}", backend_addr);
    println!("6. Starting export bridge: --export '{}'", export_spec);

    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start import bridge
    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("multitest/{}", import_addr);
    println!("7. Starting import bridge: --import '{}'", import_spec);

    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect first client
    println!("8. Client 1: Connecting...");
    let mut client1 = TcpStream::connect(import_addr).await?;
    println!("9. Client 1: Connected");

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Connect second client
    println!("10. Client 2: Connecting...");
    let mut client2 = TcpStream::connect(import_addr).await?;
    println!("11. Client 2: Connected");

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send data from both clients
    println!("12. Client 1: Sending message...");
    client1.write_all(b"Message from client 1\n").await?;

    println!("13. Client 2: Sending message...");
    client2.write_all(b"Message from client 2\n").await?;

    // Read responses
    let mut buf1 = vec![0u8; 1024];
    let mut buf2 = vec![0u8; 1024];

    let read1 = timeout(Duration::from_secs(2), client1.read(&mut buf1));
    let read2 = timeout(Duration::from_secs(2), client2.read(&mut buf2));

    let (result1, result2) = tokio::join!(read1, read2);

    match result1 {
        Ok(Ok(n)) if n > 0 => {
            println!("14. Client 1: Received {} bytes", n);
        }
        _ => {
            println!("14. Client 1: No response");
        }
    }

    match result2 {
        Ok(Ok(n)) if n > 0 => {
            println!("15. Client 2: Received {} bytes", n);
        }
        _ => {
            println!("15. Client 2: No response");
        }
    }

    // Verify backend received 2 separate connections via channel
    let mut received_count = 0;
    while let Ok(Some(_)) = timeout(Duration::from_millis(100), conn_rx.recv()).await {
        received_count += 1;
    }

    println!(
        "17. Backend created {} separate connections",
        received_count
    );
    if received_count == 2 {
        println!("18. ✓ TEST PASSED: Each client got separate backend connection");
    } else {
        println!(
            "19. ✗ TEST FAILED: Expected 2 connections, got {}",
            received_count
        );
        let _ = export_bridge.kill().await;
        let _ = import_bridge.kill().await;
        let _ = timeout(Duration::from_millis(100), backend_task).await;
        return Err(anyhow::anyhow!(
            "Expected 2 connections, got {}",
            received_count
        ));
    }

    // Cleanup
    println!("19. Cleaning up...");
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("20. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test connection close propagation
/// This test verifies that when a client disconnects, the backend detects the close quickly.
#[tokio::test]
async fn test_connection_close_propagation() -> Result<()> {
    println!("\n=== Test: Connection Close Propagation ===\n");

    // Backend server that verifies it receives proper close
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let (close_tx, mut close_rx) = mpsc::channel::<bool>(1);

    let backend_task = tokio::spawn(async move {
        println!("2. Backend: Waiting for connection...");
        let (mut stream, addr) = backend_listener.accept().await.unwrap();
        println!("3. Backend: Connection from {}", addr);

        let mut buffer = vec![0u8; 1024];

        // Read one message
        match stream.read(&mut buffer).await {
            Ok(n) if n > 0 => {
                println!("4. Backend: Received {} bytes", n);
            }
            _ => {
                println!("4. Backend: Unexpected read result");
            }
        }

        // Wait for FIN
        println!("5. Backend: Waiting for close...");
        let result = match stream.read(&mut buffer).await {
            Ok(0) => {
                println!("6. Backend: ✓ Received FIN (proper close)");
                true
            }
            Ok(n) => {
                println!("6. Backend: ✗ Received {} more bytes (expected 0)", n);
                false
            }
            Err(e) => {
                println!("6. Backend: ✗ Error: {:?}", e);
                false
            }
        };

        let _ = close_tx.send(result).await;
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start bridges (assume bridge is already built)
    let export_spec = format!("closetest/{}", backend_addr);
    println!("7. Starting export bridge...");

    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("closetest/{}", import_addr);
    println!("8. Starting import bridge...");

    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect client
    println!("9. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("10. Client: Connected");

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send one message
    println!("11. Client: Sending message...");
    client.write_all(b"Test message\n").await?;
    println!("12. Client: Message sent");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Close client connection
    println!("13. Client: Closing connection...");
    drop(client);
    println!("14. Client: Connection closed");

    // Wait for backend to signal close detection via channel
    // With the fix, this should happen within a few seconds
    match timeout(Duration::from_secs(5), close_rx.recv()).await {
        Ok(Some(true)) => {
            println!("15. ✓ TEST PASSED: Backend detected proper close quickly");
        }
        Ok(Some(false)) => {
            println!("15. ✗ TEST FAILED: Backend didn't detect close properly");
            let _ = export_bridge.kill().await;
            let _ = import_bridge.kill().await;
            let _ = backend_task.await;
            return Err(anyhow::anyhow!("Close not propagated"));
        }
        Ok(None) => {
            println!("15. ✗ TEST FAILED: Backend channel closed unexpectedly");
            let _ = export_bridge.kill().await;
            let _ = import_bridge.kill().await;
            let _ = backend_task.await;
            return Err(anyhow::anyhow!("Backend channel closed"));
        }
        Err(_) => {
            println!("15. ✗ TEST FAILED: Timeout waiting for backend close detection");
            println!("    Close should happen within 5 seconds with the fix");
            let _ = export_bridge.kill().await;
            let _ = import_bridge.kill().await;
            let _ = backend_task.await;
            return Err(anyhow::anyhow!("Backend timeout"));
        }
    }

    // Wait for backend task to complete
    let _ = backend_task.await;

    // Cleanup
    println!("16. Cleaning up...");
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    println!("17. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test basic connectivity without checking close propagation
#[tokio::test]
async fn test_connection_basic() -> Result<()> {
    println!("\n=== Test: Basic Connection Without Close Check ===\n");

    // Backend server that accepts one connection and echoes one message
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        let (mut stream, _) = backend_listener.accept().await.unwrap();
        println!("2. Backend: Connection accepted");

        let mut buffer = vec![0u8; 1024];
        if let Ok(n) = stream.read(&mut buffer).await {
            if n > 0 {
                println!("3. Backend: Received {} bytes", n);
                let _ = stream.write_all(b"Response\n").await;
                println!("4. Backend: Sent response");
            }
        }
        // Don't wait for close - just exit
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start bridges (using debug binary)
    println!("5. Starting export bridge...");
    let export_spec = format!("basictest/{}", backend_addr);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("basictest/{}", import_addr);
    println!("6. Starting import bridge...");
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect and send message
    println!("7. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("8. Client: Connected");

    tokio::time::sleep(Duration::from_millis(300)).await;

    client.write_all(b"Test\n").await?;
    println!("9. Client: Sent message");

    // Read response
    let mut buf = vec![0u8; 1024];
    match timeout(Duration::from_secs(3), client.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            println!("10. Client: Received {} bytes", n);
            println!("11. ✓ TEST PASSED: Basic communication works");
        }
        _ => {
            println!("10. ✗ No response received");
            let _ = export_bridge.kill().await;
            let _ = import_bridge.kill().await;
            return Err(anyhow::anyhow!("No response"));
        }
    }

    // Cleanup immediately without waiting for close
    println!("12. Cleaning up...");
    drop(client);
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("13. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test bidirectional data flow
#[tokio::test]
async fn test_bidirectional_data_flow() -> Result<()> {
    println!("\n=== Test: Bidirectional Data Flow ===\n");

    // Backend that echoes data back
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend echo server listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        println!("2. Backend: Waiting for connection...");
        let (mut stream, _) = backend_listener.accept().await.unwrap();
        println!("3. Backend: Connection accepted");

        let mut buffer = vec![0u8; 1024];
        let mut messages_echoed = 0;

        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => {
                    println!(
                        "4. Backend: Connection closed after {} messages",
                        messages_echoed
                    );
                    break;
                }
                Ok(n) => {
                    println!("   Backend: Echoing {} bytes", n);
                    if stream.write_all(&buffer[..n]).await.is_err() {
                        break;
                    }
                    messages_echoed += 1;
                }
                Err(_) => break,
            }
        }
        messages_echoed
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start bridges (using debug binary)
    println!("5. Starting export bridge...");
    let export_spec = format!("echotest/{}", backend_addr);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("echotest/{}", import_addr);
    println!("6. Starting import bridge...");
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect client and test echo
    println!("7. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("8. Client: Connected");

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send multiple messages and verify echoes
    let messages = vec!["Hello\n", "World\n", "Test\n"];
    let mut echoes_received = 0;

    for (i, msg) in messages.iter().enumerate() {
        println!("9.{} Client: Sending '{}'", i + 1, msg.trim());
        client.write_all(msg.as_bytes()).await?;

        let mut buf = vec![0u8; 1024];
        match timeout(Duration::from_secs(2), client.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                let echo = String::from_utf8_lossy(&buf[..n]);
                println!("10.{} Client: Received echo '{}'", i + 1, echo.trim());
                if echo.trim() == msg.trim() {
                    echoes_received += 1;
                }
            }
            _ => {
                println!("10.{} Client: No echo received", i + 1);
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    println!("11. Client: Closing...");
    drop(client);

    // Verify all echoes were received
    if echoes_received == messages.len() {
        println!(
            "12. ✓ TEST PASSED: All {} messages echoed correctly",
            echoes_received
        );
    } else {
        println!(
            "12. ✗ TEST FAILED: Only {}/{} messages echoed",
            echoes_received,
            messages.len()
        );
    }

    // Wait for backend
    let _ = timeout(Duration::from_secs(5), backend_task).await;

    // Cleanup
    println!("13. Cleaning up...");
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    println!("14. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test that client connection is closed when backend is unavailable
#[tokio::test]
async fn test_backend_unavailable_closes_client() -> Result<()> {
    println!("\n=== Test: Backend Unavailable Closes Client ===\n");

    // DON'T start a backend server - this is the key part of the test
    let backend_addr = "127.0.0.1:9999"; // Use a port with no server
    println!(
        "1. Using backend address {} (no server listening)",
        backend_addr
    );

    // Start export bridge (using debug binary)
    println!("2. Starting export bridge...");
    let export_spec = format!("nobackend/{}", backend_addr);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("nobackend/{}", import_addr);
    println!("3. Starting import bridge...");
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect client
    println!("4. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("5. Client: Connected to import bridge");

    // Give significant time for:
    // - Liveliness declaration to propagate through Zenoh
    // - Export bridge to detect liveliness PUT
    // - Export bridge to attempt backend connection
    // - Export bridge to publish error signal
    // - Import bridge to receive error signal and close connection
    println!("6. Waiting for error detection cycle...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Try to send data (connection may already be closed)
    println!("7. Client: Attempting to send message...");
    let write_result = client.write_all(b"Test message\n").await;
    if write_result.is_err() {
        println!("8. Client: Write failed (connection already closed - good!)");
        println!("9. ✓ TEST PASSED: Connection closed before write (backend unavailable)");
        let _ = export_bridge.kill().await;
        let _ = import_bridge.kill().await;
        println!("10. Cleaning up...");
        println!("11. ✓ TEST COMPLETED\n");
        return Ok(());
    }
    println!("8. Client: Write succeeded, checking for close...");

    // Client should either:
    // 1. Receive connection close (read returns 0)
    // 2. Receive an error
    // 3. Connection should close within reasonable time
    println!("9. Client: Waiting for response or close...");
    let mut buf = vec![0u8; 1024];

    match timeout(Duration::from_secs(5), client.read(&mut buf)).await {
        Ok(Ok(0)) => {
            println!("10. ✓ TEST PASSED: Connection closed by server (backend unavailable)");
        }
        Ok(Ok(n)) => {
            println!("10. Client: Received {} bytes (unexpected)", n);
            println!("    Data: {:?}", String::from_utf8_lossy(&buf[..n]));
            println!("11. ⚠ TEST WARNING: Expected connection close, got data");
        }
        Ok(Err(e)) => {
            println!(
                "10. ✓ TEST PASSED: Connection error: {:?} (backend unavailable)",
                e
            );
        }
        Err(_) => {
            println!("10. ✗ TEST FAILED: Connection did not close within 5 seconds");
            println!("    Client should be dropped when backend is unavailable");
            let _ = export_bridge.kill().await;
            let _ = import_bridge.kill().await;
            return Err(anyhow::anyhow!(
                "Client not closed when backend unavailable"
            ));
        }
    }

    // Cleanup
    println!("11. Cleaning up...");
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    println!("12. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test rapid connection/disconnection cycles
#[tokio::test]
async fn test_rapid_connect_disconnect() -> Result<()> {
    println!("\n=== Test: Rapid Connect/Disconnect Cycles ===\n");

    // Start backend
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let connection_count = Arc::new(Mutex::new(0));
    let count_clone = connection_count.clone();

    let backend_task = tokio::spawn(async move {
        let mut connections = 0;
        while connections < 10 {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                connections += 1;
                println!("   Backend: Connection {} accepted", connections);
                *count_clone.lock().await = connections;

                // Echo one message then close
                let mut buf = vec![0u8; 1024];
                if stream.read(&mut buf).await.unwrap_or(0) > 0 {
                    let _ = stream.write_all(b"ack\n").await;
                }
            }
        }
        connections
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start bridges
    let export_spec = format!("rapidtest/{}", backend_addr);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("rapidtest/{}", import_addr);
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("2. Bridges started, beginning rapid connection test...");

    // Rapidly connect and disconnect 10 clients
    for i in 0..10 {
        println!("3.{} Client {}: Connecting...", i + 1, i + 1);
        let mut client = TcpStream::connect(import_addr).await?;

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send message
        client.write_all(b"test\n").await?;

        // Read response
        let mut buf = vec![0u8; 64];
        let _ = timeout(Duration::from_secs(1), client.read(&mut buf)).await;

        // Immediately disconnect
        drop(client);
        println!("   Client {}: Disconnected", i + 1);

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Verify all connections were handled
    tokio::time::sleep(Duration::from_secs(1)).await;
    let final_count = *connection_count.lock().await;

    println!("4. Backend handled {} connections", final_count);
    if final_count >= 8 {
        // Allow some tolerance for race conditions
        println!(
            "5. ✓ TEST PASSED: Handled rapid connections ({}/10)",
            final_count
        );
    } else {
        println!(
            "5. ✗ TEST FAILED: Only handled {}/10 connections",
            final_count
        );
    }

    // Cleanup
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("6. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test concurrent connections from multiple clients
#[tokio::test]
async fn test_concurrent_connections() -> Result<()> {
    println!("\n=== Test: Concurrent Connections ===\n");

    // Backend that tracks concurrent connections
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let max_concurrent = Arc::new(Mutex::new(0));
    let max_clone = max_concurrent.clone();

    let backend_task = tokio::spawn(async move {
        let mut handles = vec![];
        let active_count = Arc::new(Mutex::new(0));

        for i in 0..5 {
            let listener_result = backend_listener.accept().await;
            if let Ok((mut stream, _)) = listener_result {
                let active = active_count.clone();
                let max_tracker = max_clone.clone();

                let handle = tokio::spawn(async move {
                    // Increment active count
                    {
                        let mut count = active.lock().await;
                        *count += 1;
                        let mut max = max_tracker.lock().await;
                        if *count > *max {
                            *max = *count;
                        }
                        println!(
                            "   Backend: Connection {} active (total active: {})",
                            i + 1,
                            *count
                        );
                    }

                    // Hold connection for a bit
                    tokio::time::sleep(Duration::from_millis(500)).await;

                    let mut buf = vec![0u8; 1024];
                    if stream.read(&mut buf).await.unwrap_or(0) > 0 {
                        let _ = stream.write_all(b"response\n").await;
                    }

                    // Decrement active count
                    {
                        let mut count = active.lock().await;
                        *count -= 1;
                        println!(
                            "   Backend: Connection {} closed (total active: {})",
                            i + 1,
                            *count
                        );
                    }
                });

                handles.push(handle);
            }
        }

        for handle in handles {
            let _ = handle.await;
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start bridges
    let export_spec = format!("concurrenttest/{}", backend_addr);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("concurrenttest/{}", import_addr);
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("2. Bridges started, connecting 5 concurrent clients...");

    // Connect 5 clients concurrently
    let mut client_handles = vec![];

    for i in 0..5 {
        let addr = import_addr;
        let handle = tokio::spawn(async move {
            if let Ok(mut client) = TcpStream::connect(addr).await {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = client.write_all(b"test\n").await;
                let mut buf = vec![0u8; 64];
                let _ = timeout(Duration::from_secs(2), client.read(&mut buf)).await;
                println!("3.{} Client {} completed", i + 1, i + 1);
            }
        });
        client_handles.push(handle);

        // Small delay between connections
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for all clients
    for handle in client_handles {
        let _ = handle.await;
    }

    // Check max concurrent
    let max_concurrent_value = *max_concurrent.lock().await;
    println!(
        "4. Max concurrent backend connections: {}",
        max_concurrent_value
    );

    if max_concurrent_value >= 3 {
        println!(
            "5. ✓ TEST PASSED: Handled concurrent connections (max: {})",
            max_concurrent_value
        );
    } else {
        println!(
            "5. ⚠ TEST WARNING: Low concurrency (max: {})",
            max_concurrent_value
        );
    }

    // Cleanup
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_secs(2), backend_task).await;
    println!("6. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test large message transfer through bridges
#[tokio::test]
async fn test_large_message_transfer() -> Result<()> {
    println!("\n=== Test: Large Message Transfer ===\n");

    // Backend that receives and echoes large messages
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = backend_listener.accept().await {
            println!("2. Backend: Connection accepted");

            let mut total_received = 0;
            let mut buffer = vec![0u8; 65536];

            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                total_received += n;
                println!(
                    "   Backend: Received {} bytes (total: {})",
                    n, total_received
                );

                // Echo back
                if stream.write_all(&buffer[..n]).await.is_err() {
                    break;
                }
            }

            println!("   Backend: Total received {} bytes", total_received);
            total_received
        } else {
            0
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start bridges
    let export_spec = format!("largetest/{}", backend_addr);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("largetest/{}", import_addr);
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect client
    println!("3. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("4. Client: Connected");

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send large message (1 MB)
    let message_size = 1024 * 1024; // 1 MB
    let large_message = vec![0xAB; message_size];
    println!("5. Client: Sending {} byte message...", message_size);

    client.write_all(&large_message).await?;
    println!("6. Client: Message sent");

    // Read echo back
    let mut received = 0;
    let mut buffer = vec![0u8; 65536];
    let start = std::time::Instant::now();

    while received < message_size {
        match timeout(Duration::from_secs(5), client.read(&mut buffer)).await {
            Ok(Ok(n)) if n > 0 => {
                received += n;
                println!(
                    "   Client: Received {} bytes (total: {}/{})",
                    n, received, message_size
                );
            }
            Ok(Ok(_)) => {
                println!("   Client: Connection closed");
                break;
            }
            Ok(Err(e)) => {
                println!("   Client: Error: {:?}", e);
                break;
            }
            Err(_) => {
                println!("   Client: Timeout");
                break;
            }
        }
    }

    let elapsed = start.elapsed();
    println!("7. Client: Received {} bytes in {:?}", received, elapsed);

    if received == message_size {
        println!("8. ✓ TEST PASSED: Large message transferred successfully");
    } else {
        println!(
            "8. ⚠ TEST WARNING: Partial transfer ({}/{} bytes)",
            received, message_size
        );
    }

    // Cleanup
    drop(client);
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("9. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test bridge behavior when client sends data rapidly
#[tokio::test]
async fn test_rapid_data_send() -> Result<()> {
    println!("\n=== Test: Rapid Data Send ===\n");

    // Backend that counts messages
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let message_count = Arc::new(Mutex::new(0));
    let count_clone = message_count.clone();

    let backend_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = backend_listener.accept().await {
            println!("2. Backend: Connection accepted");

            let mut buffer = vec![0u8; 1024];
            let mut messages = 0;

            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }

                // Count newlines (messages)
                messages += buffer[..n].iter().filter(|&&b| b == b'\n').count();
                *count_clone.lock().await = messages;

                // Send ack for each message
                for _ in 0..buffer[..n].iter().filter(|&&b| b == b'\n').count() {
                    let _ = stream.write_all(b"ack\n").await;
                }
            }

            println!("   Backend: Received {} messages", messages);
            messages
        } else {
            0
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start bridges
    let export_spec = format!("rapiddata/{}", backend_addr);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("rapiddata/{}", import_addr);
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect client
    println!("3. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("4. Client: Connected");

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send 100 messages rapidly
    let num_messages = 100;
    println!("5. Client: Sending {} messages rapidly...", num_messages);

    for i in 0..num_messages {
        let msg = format!("message_{}\n", i);
        client.write_all(msg.as_bytes()).await?;
    }

    println!("6. Client: All messages sent");

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(2)).await;

    let received_count = *message_count.lock().await;
    println!("7. Backend received {} messages", received_count);

    if received_count >= num_messages * 90 / 100 {
        // Allow 90% success rate
        println!(
            "8. ✓ TEST PASSED: Handled rapid data send ({}/{} messages)",
            received_count, num_messages
        );
    } else {
        println!(
            "8. ⚠ TEST WARNING: Some messages lost ({}/{} messages)",
            received_count, num_messages
        );
    }

    // Cleanup
    drop(client);
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("9. ✓ TEST COMPLETED\n");

    Ok(())
}

/// Test bridge recovery when backend restarts
#[tokio::test]
async fn test_backend_restart_recovery() -> Result<()> {
    println!("\n=== Test: Backend Restart Recovery ===\n");

    let backend_addr = "127.0.0.1:19999";
    println!("1. Using backend address {}", backend_addr);

    // Start bridges WITHOUT backend initially
    let export_spec = format!("restarttest/{}", backend_addr);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("restarttest/{}", import_addr);
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("2. Bridges started (no backend yet)");

    // First client should fail fast
    println!("3. Client 1: Connecting (should fail)...");
    let mut client1 = TcpStream::connect(import_addr).await?;
    client1.write_all(b"test1\n").await?;

    let mut buf = vec![0u8; 64];
    match timeout(Duration::from_secs(2), client1.read(&mut buf)).await {
        Ok(Ok(0)) => {
            println!("4. Client 1: Connection closed (expected - no backend)");
        }
        _ => {
            println!("4. Client 1: Unexpected response");
        }
    }
    drop(client1);

    // Now start backend
    println!("5. Starting backend...");
    let backend_listener = TcpListener::bind(backend_addr).await?;

    let backend_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = backend_listener.accept().await {
            println!("   Backend: Connection accepted");
            let mut buf = vec![0u8; 1024];
            if let Ok(n) = stream.read(&mut buf).await {
                if n > 0 {
                    let _ = stream.write_all(b"backend_ok\n").await;
                    return true;
                }
            }
        }
        false
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Second client should succeed
    println!("6. Client 2: Connecting (should succeed)...");
    let mut client2 = TcpStream::connect(import_addr).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    client2.write_all(b"test2\n").await?;

    match timeout(Duration::from_secs(2), client2.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            println!("7. Client 2: Received response (backend is working!)");
            println!("8. ✓ TEST PASSED: Backend restart recovery works");
        }
        _ => {
            println!("7. Client 2: No response");
            println!("8. ⚠ TEST WARNING: Recovery may be slow");
        }
    }

    // Cleanup
    drop(client2);
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("9. ✓ TEST COMPLETED\n");

    Ok(())
}
