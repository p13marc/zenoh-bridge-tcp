//! Stress tests for Zenoh TCP Bridge
//!
//! These tests are designed to find race conditions, crashes, and reliability issues
//! by hammering the bridge with many concurrent connections and edge cases.

use anyhow::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

/// Helper to start export bridge
async fn start_export_bridge(
    backend_addr: &str,
    service_name: &str,
) -> Result<tokio::process::Child> {
    let export_spec = format!("{}/{}", service_name, backend_addr);
    let child = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--export", &export_spec])
        .kill_on_drop(true)
        .spawn()?;

    sleep(Duration::from_millis(500)).await;
    Ok(child)
}

/// Helper to start import bridge
async fn start_import_bridge(
    listen_addr: &str,
    service_name: &str,
) -> Result<tokio::process::Child> {
    let import_spec = format!("{}/{}", service_name, listen_addr);
    let child = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--import", &import_spec])
        .kill_on_drop(true)
        .spawn()?;

    sleep(Duration::from_millis(500)).await;
    Ok(child)
}

/// Test: Many concurrent connections at once
#[tokio::test]
#[ignore] // Run with: cargo test --test stress_test -- --ignored --nocapture
async fn stress_test_many_concurrent_connections() -> Result<()> {
    println!("\n=== Stress Test: Many Concurrent Connections ===\n");

    let num_connections = 50;
    let messages_per_connection = 10;

    // Start echo server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let connection_counter = Arc::new(AtomicU64::new(0));
    let message_counter = Arc::new(AtomicU64::new(0));

    let conn_counter_clone = connection_counter.clone();
    let msg_counter_clone = message_counter.clone();

    // Echo server that handles multiple connections
    let backend_task = tokio::spawn(async move {
        while let Ok((mut stream, _addr)) = backend_listener.accept().await {
            let _conn_num = conn_counter_clone.fetch_add(1, Ordering::SeqCst) + 1;
            let msg_counter = msg_counter_clone.clone();

            tokio::spawn(async move {
                let mut buffer = vec![0u8; 1024];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(n) => {
                            msg_counter.fetch_add(1, Ordering::SeqCst);
                            if stream.write_all(&buffer[..n]).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    // Start bridges
    let _export_bridge = start_export_bridge(&backend_addr.to_string(), "stress_test").await?;
    println!("2. Export bridge started");

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let _import_bridge = start_import_bridge(&import_addr.to_string(), "stress_test").await?;
    println!("3. Import bridge started");

    sleep(Duration::from_secs(2)).await;
    println!("4. Launching {} concurrent connections...", num_connections);

    // Launch many concurrent clients
    let mut tasks = Vec::new();
    let success_count = Arc::new(AtomicU64::new(0));

    for i in 0..num_connections {
        let import_addr_clone = import_addr;
        let success_counter = success_count.clone();

        let task = tokio::spawn(async move {
            match timeout(
                Duration::from_secs(10),
                test_client_session(import_addr_clone, i, messages_per_connection),
            )
            .await
            {
                Ok(Ok(())) => {
                    success_counter.fetch_add(1, Ordering::SeqCst);
                    true
                }
                Ok(Err(e)) => {
                    eprintln!("Client {} error: {:?}", i, e);
                    false
                }
                Err(_) => {
                    eprintln!("Client {} timeout", i);
                    false
                }
            }
        });
        tasks.push(task);
    }

    // Wait for all clients
    for task in tasks {
        let _ = task.await;
    }

    let successes = success_count.load(Ordering::SeqCst);
    let backend_connections = connection_counter.load(Ordering::SeqCst);
    let total_messages = message_counter.load(Ordering::SeqCst);

    println!("\n=== Results ===");
    println!("Connections attempted: {}", num_connections);
    println!("Connections succeeded: {}", successes);
    println!("Backend connections: {}", backend_connections);
    println!("Total messages echoed: {}", total_messages);
    println!(
        "Expected messages: {}",
        num_connections * messages_per_connection
    );

    backend_task.abort();

    assert!(
        successes >= (num_connections as u64 * 90 / 100),
        "Too many failures: {}/{}",
        successes,
        num_connections
    );

    Ok(())
}

/// Test client session: connect, send messages, verify echoes
async fn test_client_session(
    addr: std::net::SocketAddr,
    client_id: usize,
    num_messages: usize,
) -> Result<()> {
    let mut stream = TcpStream::connect(addr).await?;

    for i in 0..num_messages {
        let message = format!("Client {} message {}\n", client_id, i);
        stream.write_all(message.as_bytes()).await?;

        let mut response = vec![0u8; 1024];
        let n = stream.read(&mut response).await?;

        if n == 0 {
            return Err(anyhow::anyhow!("Connection closed unexpectedly"));
        }

        let response_str = String::from_utf8_lossy(&response[..n]);
        if response_str != message {
            return Err(anyhow::anyhow!(
                "Echo mismatch: sent '{}', got '{}'",
                message.trim(),
                response_str.trim()
            ));
        }
    }

    Ok(())
}

/// Test: Rapid connect/disconnect
#[tokio::test]
#[ignore]
async fn stress_test_rapid_connect_disconnect() -> Result<()> {
    println!("\n=== Stress Test: Rapid Connect/Disconnect ===\n");

    let num_iterations = 100;

    // Start echo server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        let _ = stream.write_all(&buffer[..n]).await;
                    }
                });
            }
        }
    });

    // Start bridges
    let _export_bridge = start_export_bridge(&backend_addr.to_string(), "rapid_test").await?;
    println!("2. Export bridge started");

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let _import_bridge = start_import_bridge(&import_addr.to_string(), "rapid_test").await?;
    println!("3. Import bridge started");

    sleep(Duration::from_secs(2)).await;

    println!(
        "4. Running {} rapid connect/disconnect cycles...",
        num_iterations
    );

    let mut success = 0;
    let mut failures = 0;

    for i in 0..num_iterations {
        match timeout(Duration::from_secs(2), async {
            let mut stream = TcpStream::connect(import_addr).await?;
            stream.write_all(b"test\n").await?;

            let mut buf = vec![0u8; 16];
            let n = stream.read(&mut buf).await?;

            drop(stream); // Immediate disconnect

            anyhow::Ok(n > 0)
        })
        .await
        {
            Ok(Ok(true)) => success += 1,
            _ => {
                failures += 1;
                if failures < 10 {
                    eprintln!("Iteration {} failed", i);
                }
            }
        }

        // Small delay between iterations
        sleep(Duration::from_millis(10)).await;
    }

    println!("\n=== Results ===");
    println!("Iterations: {}", num_iterations);
    println!("Successes: {}", success);
    println!("Failures: {}", failures);

    backend_task.abort();

    assert!(
        success >= (num_iterations as u64 * 90 / 100),
        "Too many failures: {}/{}",
        success,
        num_iterations
    );

    Ok(())
}

/// Test: Large message transfer
#[tokio::test]
#[ignore]
async fn stress_test_large_messages() -> Result<()> {
    println!("\n=== Stress Test: Large Messages ===\n");

    let message_sizes = vec![1024, 10 * 1024, 100 * 1024, 500 * 1024];

    // Start echo server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = backend_listener.accept().await {
            let mut buffer = vec![0u8; 1024 * 1024]; // 1MB buffer
            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                if stream.write_all(&buffer[..n]).await.is_err() {
                    break;
                }
            }
        }
    });

    // Start bridges
    let _export_bridge = start_export_bridge(&backend_addr.to_string(), "large_test").await?;
    println!("2. Export bridge started");

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let _import_bridge = start_import_bridge(&import_addr.to_string(), "large_test").await?;
    println!("3. Import bridge started");

    sleep(Duration::from_secs(2)).await;

    println!("4. Testing various message sizes...");

    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(import_addr)).await??;

    for size in message_sizes {
        println!("   Testing {} byte message...", size);

        // Create message with predictable content
        let message: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

        timeout(Duration::from_secs(10), stream.write_all(&message)).await??;

        // Read back the echo
        let mut response = vec![0u8; size];
        let mut total_read = 0;

        while total_read < size {
            let n = timeout(
                Duration::from_secs(10),
                stream.read(&mut response[total_read..]),
            )
            .await??;

            if n == 0 {
                return Err(anyhow::anyhow!("Connection closed at {} bytes", total_read));
            }

            total_read += n;
        }

        // Verify content
        if response != message {
            return Err(anyhow::anyhow!(
                "Data corruption at size {}: response mismatch",
                size
            ));
        }

        println!("   ✓ {} bytes OK", size);
    }

    backend_task.abort();

    println!("\n✅ All large message tests passed");

    Ok(())
}

/// Test: Connection timeout handling
#[tokio::test]
#[ignore]
async fn stress_test_slow_backend() -> Result<()> {
    println!("\n=== Stress Test: Slow Backend Response ===\n");

    // Start slow backend
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Slow backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = backend_listener.accept().await {
            let mut buffer = vec![0u8; 1024];
            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                // Simulate slow processing
                sleep(Duration::from_millis(500)).await;
                let _ = stream.write_all(&buffer[..n]).await;
            }
        }
    });

    // Start bridges
    let _export_bridge = start_export_bridge(&backend_addr.to_string(), "slow_test").await?;
    println!("2. Export bridge started");

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let _import_bridge = start_import_bridge(&import_addr.to_string(), "slow_test").await?;
    println!("3. Import bridge started");

    sleep(Duration::from_secs(2)).await;

    println!("4. Testing with slow backend (500ms delay per message)...");

    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(import_addr)).await??;

    for i in 0..5 {
        let message = format!("slow message {}\n", i);
        println!("   Sending message {}...", i);

        timeout(Duration::from_secs(2), stream.write_all(message.as_bytes())).await??;

        let mut buf = vec![0u8; 1024];
        let n = timeout(Duration::from_secs(2), stream.read(&mut buf)).await??;

        let response = String::from_utf8_lossy(&buf[..n]);
        println!("   Received: {}", response.trim());

        assert_eq!(
            response.trim(),
            message.trim(),
            "Message mismatch at iteration {}",
            i
        );
    }

    backend_task.abort();

    println!("\n✅ Slow backend test passed");

    Ok(())
}

/// Test: Backend crashes and recovers
#[tokio::test]
#[ignore]
async fn stress_test_backend_crash_recovery() -> Result<()> {
    println!("\n=== Stress Test: Backend Crash and Recovery ===\n");

    let backend_addr = "127.0.0.1:19999";

    // Start bridges first (backend not running yet)
    let _export_bridge = start_export_bridge(backend_addr, "crash_test").await?;
    println!("1. Export bridge started (backend not running)");

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let _import_bridge = start_import_bridge(&import_addr.to_string(), "crash_test").await?;
    println!("2. Import bridge started");

    sleep(Duration::from_secs(2)).await;

    println!("3. Trying to connect (backend offline - should fail)...");

    // Try to connect - should fail or timeout
    let result = timeout(Duration::from_secs(3), async {
        let mut stream = TcpStream::connect(import_addr).await?;
        stream.write_all(b"test\n").await?;
        let mut buf = vec![0u8; 16];
        stream.read(&mut buf).await
    })
    .await;

    println!(
        "   Connection result (backend offline): {:?}",
        result.is_err()
    );

    // Now start the backend
    println!("4. Starting backend...");
    let backend_listener = TcpListener::bind(backend_addr).await?;

    let backend_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = backend_listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 1024];
                while let Ok(n) = stream.read(&mut buffer).await {
                    if n == 0 {
                        break;
                    }
                    let _ = stream.write_all(&buffer[..n]).await;
                }
            });
        }
    });

    sleep(Duration::from_secs(1)).await;

    println!("5. Trying to connect (backend online - should succeed)...");

    // Now it should work
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(import_addr)).await??;

    stream.write_all(b"recovery test\n").await?;
    let mut buf = vec![0u8; 1024];
    let n = timeout(Duration::from_secs(5), stream.read(&mut buf)).await??;

    let response = String::from_utf8_lossy(&buf[..n]);
    println!("   Received: {}", response.trim());

    backend_task.abort();

    assert_eq!(response.trim(), "recovery test");

    println!("\n✅ Backend recovery test passed");

    Ok(())
}

/// Test: Interleaved connections
#[tokio::test]
#[ignore]
async fn stress_test_interleaved_connections() -> Result<()> {
    println!("\n=== Stress Test: Interleaved Connection Lifecycle ===\n");

    // Start echo server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        let _ = stream.write_all(&buffer[..n]).await;
                    }
                });
            }
        }
    });

    // Start bridges
    let _export_bridge = start_export_bridge(&backend_addr.to_string(), "interleave_test").await?;
    println!("2. Export bridge started");

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let _import_bridge = start_import_bridge(&import_addr.to_string(), "interleave_test").await?;
    println!("3. Import bridge started");

    sleep(Duration::from_secs(2)).await;

    println!("4. Testing interleaved connection lifecycles...");

    // Open 10 connections but don't close them immediately
    let mut streams = Vec::new();

    for i in 0..10 {
        let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(import_addr)).await??;

        // Send initial message
        let msg = format!("connection {}\n", i);
        stream.write_all(msg.as_bytes()).await?;

        streams.push(stream);
        println!("   Opened connection {}", i);

        sleep(Duration::from_millis(50)).await;
    }

    println!("5. All connections open. Sending interleaved messages...");

    // Send messages on all connections
    for (i, stream) in streams.iter_mut().enumerate() {
        for j in 0..3 {
            let msg = format!("conn {} msg {}\n", i, j);
            timeout(Duration::from_secs(5), stream.write_all(msg.as_bytes())).await??;
        }
    }

    println!("6. Reading all responses (handling buffered reads)...");

    // Read all responses - they may be buffered together
    for (i, stream) in streams.iter_mut().enumerate() {
        let mut total_received = String::new();
        let mut buf = vec![0u8; 4096];

        // Keep reading until we get all expected messages
        let expected_lines = 4; // 1 initial + 3 messages

        for _ in 0..10 {
            // Max 10 reads
            match timeout(Duration::from_millis(500), stream.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    total_received.push_str(&String::from_utf8_lossy(&buf[..n]));
                    let line_count = total_received.matches('\n').count();
                    if line_count >= expected_lines {
                        break;
                    }
                }
                Ok(Ok(_)) => break, // 0 or any other value - connection closed
                Ok(Err(_)) => break,
                Err(_) => break, // Timeout is OK if we got all data
            }
        }

        let lines: Vec<&str> = total_received.lines().collect();
        println!("   Conn {} received {} lines", i, lines.len());

        if lines.len() < expected_lines {
            return Err(anyhow::anyhow!(
                "Connection {} only received {} lines, expected {}",
                i,
                lines.len(),
                expected_lines
            ));
        }
    }

    println!("7. Closing all connections...");

    for stream in streams {
        drop(stream);
    }

    backend_task.abort();

    println!("\n✅ Interleaved connection test passed");

    Ok(())
}

/// Test: Aggressive crash finder - rapid operations with random timing
#[tokio::test]
#[ignore]
async fn stress_test_aggressive_crash_finder() -> Result<()> {
    println!("\n=== Stress Test: Aggressive Crash Finder ===\n");

    let num_iterations = 200;
    let max_concurrent = 20;

    // Start echo server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = backend_listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        let _ = stream.write_all(&buffer[..n]).await;
                    }
                });
            }
        }
    });

    // Start bridges
    let _export_bridge = start_export_bridge(&backend_addr.to_string(), "crash_finder").await?;
    println!("2. Export bridge started");

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let _import_bridge = start_import_bridge(&import_addr.to_string(), "crash_finder").await?;
    println!("3. Import bridge started");

    sleep(Duration::from_secs(2)).await;

    println!(
        "4. Running {} aggressive operations with {} max concurrent...",
        num_iterations, max_concurrent
    );

    let success_count = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));

    for batch in 0..(num_iterations / max_concurrent) {
        let mut tasks = Vec::new();

        for i in 0..max_concurrent {
            let import_addr_clone = import_addr;
            let success_counter = success_count.clone();
            let error_counter = error_count.clone();
            let iter = batch * max_concurrent + i;

            let task = tokio::spawn(async move {
                // Random behavior: some send data, some just connect/disconnect
                let behavior = iter % 5;

                match timeout(Duration::from_secs(3), async {
                    let mut stream = TcpStream::connect(import_addr_clone).await?;

                    match behavior {
                        0 => {
                            // Just connect and disconnect immediately
                            anyhow::Ok(())
                        }
                        1 => {
                            // Send one message and disconnect
                            stream.write_all(b"quick\n").await?;
                            anyhow::Ok(())
                        }
                        2 => {
                            // Send and receive
                            stream.write_all(b"echo\n").await?;
                            let mut buf = vec![0u8; 16];
                            let _ = stream.read(&mut buf).await?;
                            anyhow::Ok(())
                        }
                        3 => {
                            // Send multiple messages rapidly
                            for _ in 0..5 {
                                stream.write_all(b"rapid\n").await?;
                            }
                            anyhow::Ok(())
                        }
                        _ => {
                            // Send and wait a bit
                            stream.write_all(b"wait\n").await?;
                            sleep(Duration::from_millis(50)).await;
                            let mut buf = vec![0u8; 16];
                            let _ = stream.read(&mut buf).await;
                            anyhow::Ok(())
                        }
                    }
                })
                .await
                {
                    Ok(Ok(())) => {
                        success_counter.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {
                        error_counter.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });

            tasks.push(task);
        }

        // Wait for this batch
        for task in tasks {
            let _ = task.await;
        }

        if (batch + 1) % 5 == 0 {
            println!(
                "   Progress: {}/{} iterations",
                (batch + 1) * max_concurrent,
                num_iterations
            );
        }

        // Small delay between batches
        sleep(Duration::from_millis(10)).await;
    }

    let successes = success_count.load(Ordering::SeqCst);
    let errors = error_count.load(Ordering::SeqCst);

    println!("\n=== Results ===");
    println!("Total iterations: {}", num_iterations);
    println!("Successes: {}", successes);
    println!("Errors: {}", errors);
    println!(
        "Success rate: {:.1}%",
        (successes as f64 / num_iterations as f64) * 100.0
    );

    backend_task.abort();

    // Allow some failures due to timing, but most should succeed
    assert!(
        successes >= (num_iterations as u64 * 80 / 100),
        "Too many failures: {}/{}",
        successes,
        num_iterations
    );

    println!("\n✅ Aggressive crash test passed - no crashes detected!");

    Ok(())
}
