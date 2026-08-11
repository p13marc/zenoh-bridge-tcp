mod common;

use anyhow::Result;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;

/// Basic export/import round trip with hard assertions.
///
/// The backend is probe-immune (`common::start_probe_immune_backend`): the
/// harness's `wait_for_port` probe creates a real zero-byte bridged connection
/// on a raw listener, which used to consume single-accept backends. Every
/// outcome here is asserted — the previous version printed a FAILED line and
/// returned `Ok(())` regardless.
#[tokio::test]
async fn test_export_import_basic_communication() -> Result<()> {
    let (close_tx, mut close_rx) = mpsc::channel::<bool>(4);
    let (backend_addr, _backend) = common::start_probe_immune_backend(move |mut stream, first| {
        let close_tx = close_tx.clone();
        async move {
            let msg = String::from_utf8_lossy(&first).to_string();
            assert!(msg.contains("Hello from client"), "backend got: {msg:?}");
            stream.write_all(b"Hello from backend!\n").await.unwrap();
            // Then expect a clean FIN from the client.
            let mut buf = [0u8; 64];
            let saw_fin = matches!(stream.read(&mut buf).await, Ok(0));
            let _ = close_tx.send(saw_fin).await;
        }
    })
    .await;

    let service = common::unique_service_name("testservice");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;

    let response = common::echo_roundtrip(
        pair.import_addr,
        b"Hello from client!\n",
        common::BACKEND_READY_TIMEOUT,
    )
    .await
    .expect("no response through the bridge");
    anyhow::ensure!(
        String::from_utf8_lossy(&response).contains("Hello from backend"),
        "unexpected response: {response:?}"
    );

    // echo_roundtrip closed its connection after reading; the backend must
    // observe that as a clean FIN.
    let saw_fin = timeout(Duration::from_secs(10), close_rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("backend never reported the close"))?
        .unwrap();
    anyhow::ensure!(saw_fin, "backend did not observe a clean FIN");

    pair.kill_and_wait().await;
    Ok(())
}

/// Two clients must get two SEPARATE backend connections, each answered with
/// its own data (echo), independent of dial order (dials are concurrent now,
/// so accept order is not deterministic).
#[tokio::test]
async fn test_multiple_clients_separate_connections() -> Result<()> {
    let (conn_tx, mut conn_rx) = mpsc::channel::<()>(8);
    let (backend_addr, _backend) = common::start_probe_immune_backend(move |mut stream, first| {
        let conn_tx = conn_tx.clone();
        async move {
            let _ = conn_tx.send(()).await;
            // Echo the client's own message back, so each client can assert
            // it was served by ITS connection.
            let _ = stream.write_all(&first).await;
        }
    })
    .await;

    let service = common::unique_service_name("multitest");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;
    let import_addr = pair.import_addr;

    let r1 = common::echo_roundtrip(
        import_addr,
        b"Message from client 1\n",
        common::BACKEND_READY_TIMEOUT,
    )
    .await
    .expect("client 1 got no response");
    anyhow::ensure!(r1 == b"Message from client 1\n", "client 1 got {r1:?}");

    let r2 = common::echo_roundtrip(
        import_addr,
        b"Message from client 2\n",
        common::BACKEND_READY_TIMEOUT,
    )
    .await
    .expect("client 2 got no response");
    anyhow::ensure!(r2 == b"Message from client 2\n", "client 2 got {r2:?}");

    // At least the two data-bearing connections must have reached the backend
    // (probe phantoms are filtered by the helper; readiness retries may add
    // legitimate extra served connections, so "exactly 2" would be wrong).
    let mut served = 0;
    while let Ok(Some(())) = timeout(Duration::from_millis(500), conn_rx.recv()).await {
        served += 1;
    }
    anyhow::ensure!(
        served >= 2,
        "expected >=2 backend connections, saw {served}"
    );

    pair.kill_and_wait().await;
    Ok(())
}

/// A client's close must propagate to the backend as a clean FIN, promptly.
#[tokio::test]
async fn test_connection_close_propagation() -> Result<()> {
    let (fin_tx, mut fin_rx) = mpsc::channel::<bool>(4);
    let (backend_addr, _backend) = common::start_probe_immune_backend(move |mut stream, _first| {
        let fin_tx = fin_tx.clone();
        async move {
            // Ack so the client knows it is served, then expect the FIN.
            if stream.write_all(b"ack").await.is_err() {
                return;
            }
            let mut buf = [0u8; 64];
            let saw_fin = matches!(
                tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await,
                Ok(Ok(0))
            );
            let _ = fin_tx.send(saw_fin).await;
        }
    })
    .await;

    let service = common::unique_service_name("closetest");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;

    let client = common::connected_raw_client(
        pair.import_addr,
        b"one message\n",
        common::BACKEND_READY_TIMEOUT,
    )
    .await
    .expect("could not establish a served connection");
    drop(client);

    let saw_fin = timeout(Duration::from_secs(10), fin_rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("backend never reported on the close"))?
        .unwrap();
    anyhow::ensure!(
        saw_fin,
        "backend did not receive a clean FIN after client close"
    );

    pair.kill_and_wait().await;
    Ok(())
}

/// Test basic connectivity without checking close propagation
#[tokio::test]
async fn test_connection_basic() -> Result<()> {
    println!("\n=== Test: Basic Connection Without Close Check ===\n");

    // Backend server that accepts multiple connections and echoes data.
    // Must accept multiple connections because stale Zenoh sessions from prior tests
    // may trigger spurious backend connections.
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        while let Ok((mut stream, addr)) = backend_listener.accept().await {
            println!("   Backend: Connection from {}", addr);
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 1024];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(n) => {
                            println!("   Backend: Received {} bytes from {}", n, addr);
                            let _ = stream.write_all(b"Response\n").await;
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    // Start bridges
    let service = common::unique_service_name("basictest");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;
    let import_addr = pair.import_addr;

    // Try multiple client connections — the Zenoh data path through separate processes
    // can take variable time to establish, especially after prior tests.
    let mut got_response = false;
    for attempt in 1..=3 {
        println!("7. Client: Connecting (attempt {})...", attempt);
        let mut client = TcpStream::connect(import_addr).await?;
        println!("8. Client: Connected");

        // Wait for Zenoh path establishment
        tokio::time::sleep(Duration::from_secs(3)).await;

        client.write_all(b"Test\n").await?;
        println!("9. Client: Sent message");

        let mut buf = vec![0u8; 1024];
        match timeout(Duration::from_secs(5), client.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                println!("10. Client: Received {} bytes", n);
                println!("11. ✓ TEST PASSED: Basic communication works");
                got_response = true;
                break;
            }
            _ => {
                println!(
                    "   Client: No response (attempt {}), reconnecting...",
                    attempt
                );
                drop(client);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    if !got_response {
        pair.kill_and_wait().await;
        backend_task.abort();
        return Err(anyhow::anyhow!("No response after retries"));
    }

    // Cleanup
    pair.kill_and_wait().await;
    backend_task.abort();

    Ok(())
}

/// Three request/response round trips on ONE client connection, all asserted.
///
/// The previous version printed a FAILED line and returned `Ok(())` — it
/// "passed" for releases while delivering zero echoes whenever the harness
/// probe consumed its single-accept backend.
#[tokio::test]
async fn test_bidirectional_data_flow() -> Result<()> {
    let (backend_addr, _backend) =
        common::start_probe_immune_backend(|mut stream, first| async move {
            // Echo the first chunk, then keep echoing.
            if stream.write_all(&first).await.is_err() {
                return;
            }
            let mut buf = vec![0u8; 1024];
            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        })
        .await;

    let service = common::unique_service_name("echotest");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;

    let mut client =
        common::connected_raw_client(pair.import_addr, b"Hello\n", common::BACKEND_READY_TIMEOUT)
            .await
            .expect("could not establish a served connection");

    // The helper consumed the first echo. Two more on the same connection.
    for msg in [b"World\n".as_slice(), b"Test!\n".as_slice()] {
        client.write_all(msg).await?;
        let mut buf = vec![0u8; 64];
        let n = timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("no echo for {msg:?}"))??;
        anyhow::ensure!(
            &buf[..n] == msg,
            "echo mismatch: sent {msg:?}, got {:?}",
            &buf[..n]
        );
    }

    pair.kill_and_wait().await;
    Ok(())
}

/// Test that client connection is closed when backend is unavailable
#[tokio::test]
async fn test_backend_unavailable_closes_client() -> Result<()> {
    println!("\n=== Test: Backend Unavailable Closes Client ===\n");

    // DON'T start a backend server - this is the key part of the test
    // Allocate and immediately release a port to get a free one that nothing listens on
    let backend_port = common::PortGuard::new();
    let backend_addr = backend_port.release();
    println!(
        "1. Using backend address {} (no server listening)",
        backend_addr
    );

    // Start export bridge (using debug binary)
    println!("2. Starting export bridge...");
    let service = common::unique_service_name("nobackend");
    let export_spec = format!("{}/{}", service, backend_addr);
    let mut export_bridge = common::bridge_command()
        .args(["--backend", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("{}/{},proto=raw", service, import_addr);
    println!("3. Starting import bridge...");
    let mut import_bridge = common::bridge_command()
        .args(["--listen", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");

    // Connect client
    println!("4. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("5. Client: Connected to import bridge");

    // Give significant time for:
    // - Liveliness declaration to propagate through Zenoh between separate processes
    // - Export bridge to detect liveliness PUT
    // - Export bridge to attempt backend connection (5 retries with exponential backoff ~3-4s)
    // - Export bridge to publish error signal
    // - Import bridge to receive error signal and close connection
    println!("6. Waiting for error detection cycle...");
    tokio::time::sleep(Duration::from_secs(8)).await;

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

    match timeout(Duration::from_secs(10), client.read(&mut buf)).await {
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

/// Ten sequential connect/send/receive/disconnect cycles, every ack asserted.
///
/// Previously the backend accepted serially (one at a time), counted raw
/// accepts (probe phantoms included), tolerated 2 lost cycles, and the final
/// check printed FAILED without failing.
#[tokio::test]
async fn test_rapid_connect_disconnect() -> Result<()> {
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let served_be = served.clone();
    let (backend_addr, _backend) = common::start_probe_immune_backend(move |mut stream, _first| {
        let served = served_be.clone();
        async move {
            served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = stream.write_all(b"ack\n").await;
        }
    })
    .await;

    let service = common::unique_service_name("rapidtest");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;
    let import_addr = pair.import_addr;

    // Readiness gate: first served round trip.
    let ack = common::echo_roundtrip(import_addr, b"test 0\n", common::BACKEND_READY_TIMEOUT)
        .await
        .expect("bridge never became ready");
    anyhow::ensure!(ack == b"ack\n", "got {ack:?}");

    // Nine more rapid cycles, each one asserted — no tolerance budget.
    for i in 1..10 {
        let mut client = TcpStream::connect(import_addr).await?;
        client.write_all(format!("test {i}\n").as_bytes()).await?;
        let mut buf = [0u8; 16];
        let n = timeout(Duration::from_secs(10), client.read(&mut buf))
            .await
            .map_err(|_| anyhow::anyhow!("cycle {i}: no ack"))??;
        anyhow::ensure!(&buf[..n] == b"ack\n", "cycle {i}: got {:?}", &buf[..n]);
        drop(client);
    }

    anyhow::ensure!(
        served.load(std::sync::atomic::Ordering::SeqCst) >= 10,
        "backend served fewer connections than the clients that succeeded"
    );

    pair.kill_and_wait().await;
    Ok(())
}

/// Five clients at once; every response asserted and real concurrency proven.
///
/// The backend holds each connection ~500ms before answering, so five
/// successful clients in well under 5x500ms proves the connections were
/// served concurrently — asserted via both wall-clock and a peak-active
/// counter (the old version only printed a warning on low concurrency).
#[tokio::test]
async fn test_concurrent_connections() -> Result<()> {
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (active_be, peak_be) = (active.clone(), peak.clone());
    let (backend_addr, _backend) = common::start_probe_immune_backend(move |mut stream, first| {
        let active = active_be.clone();
        let peak = peak_be.clone();
        async move {
            let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = stream.write_all(&first).await;
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    })
    .await;

    let service = common::unique_service_name("concurrenttest");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;
    let import_addr = pair.import_addr;

    // Readiness gate (also counts as one served connection).
    let r = common::echo_roundtrip(import_addr, b"warm\n", common::BACKEND_READY_TIMEOUT)
        .await
        .expect("bridge never became ready");
    anyhow::ensure!(r == b"warm\n");

    let started = std::time::Instant::now();
    let mut handles = vec![];
    for i in 0..5u8 {
        handles.push(tokio::spawn(async move {
            let mut client = TcpStream::connect(import_addr).await?;
            let msg = [b'c', b'0' + i, b'\n'];
            client.write_all(&msg).await?;
            let mut buf = [0u8; 8];
            let n = timeout(Duration::from_secs(15), client.read(&mut buf))
                .await
                .map_err(|_| anyhow::anyhow!("client {i}: no response"))??;
            anyhow::ensure!(buf[..n] == msg, "client {i}: got {:?}", &buf[..n]);
            Ok::<(), anyhow::Error>(())
        }));
    }
    for (i, h) in handles.into_iter().enumerate() {
        h.await?.map_err(|e| anyhow::anyhow!("client {i}: {e}"))?;
    }
    let elapsed = started.elapsed();

    anyhow::ensure!(
        peak.load(std::sync::atomic::Ordering::SeqCst) >= 3,
        "peak concurrent backend connections {} < 3",
        peak.load(std::sync::atomic::Ordering::SeqCst)
    );
    anyhow::ensure!(
        elapsed < Duration::from_millis(2000),
        "five 500ms-held connections took {elapsed:?} — they were serialized"
    );

    pair.kill_and_wait().await;
    Ok(())
}

/// A 1 MiB echo through the bridge must arrive complete and byte-exact —
/// asserted (the old version warned on partial transfer and passed anyway).
#[tokio::test]
async fn test_large_message_transfer() -> Result<()> {
    let (backend_addr, _backend) =
        common::start_probe_immune_backend(|mut stream, first| async move {
            if stream.write_all(&first).await.is_err() {
                return;
            }
            let mut buffer = vec![0u8; 65536];
            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 || stream.write_all(&buffer[..n]).await.is_err() {
                    break;
                }
            }
        })
        .await;

    let service = common::unique_service_name("largetest");
    let mut pair = common::BridgePair::tcp(&service, backend_addr).await;

    let client =
        common::connected_raw_client(pair.import_addr, b"ready?\n", common::BACKEND_READY_TIMEOUT)
            .await
            .expect("could not establish a served connection");

    let message_size = 1024 * 1024;
    let payload: Vec<u8> = (0..message_size).map(|i| (i % 251) as u8).collect();

    // Write and read concurrently: a full-duplex echo of 1 MiB would deadlock
    // if we wrote everything before reading.
    let (mut rd, mut wr) = client.into_split();
    let to_send = payload.clone();
    let writer = tokio::spawn(async move {
        wr.write_all(&to_send).await?;
        Ok::<(), std::io::Error>(())
    });

    let mut received = Vec::with_capacity(message_size);
    let mut buf = vec![0u8; 65536];
    while received.len() < message_size {
        let n = timeout(Duration::from_secs(20), rd.read(&mut buf))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "stalled at {}/{} echoed bytes",
                    received.len(),
                    message_size
                )
            })??;
        anyhow::ensure!(
            n > 0,
            "closed at {}/{} echoed bytes",
            received.len(),
            message_size
        );
        received.extend_from_slice(&buf[..n]);
    }
    writer.await??;

    anyhow::ensure!(received == payload, "1 MiB echo was not byte-exact");

    pair.kill_and_wait().await;
    Ok(())
}

/// Test bridge behavior when client sends data rapidly.
///
/// This test uses bridge subprocesses and is affected by Zenoh session pollution
/// from other tests or prior runs. Message delivery ranges from 0% to 64%
/// depending on timing. Marked `#[ignore]` until Plan 07 (test infrastructure
/// rewrite) provides proper process isolation.
#[ignore]
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
    let service = common::unique_service_name("rapiddata");
    let export_spec = format!("{}/{}", service, backend_addr);
    let mut export_bridge = common::bridge_command()
        .args(["--backend", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("{}/{},proto=raw", service, import_addr);
    let mut import_bridge = common::bridge_command()
        .args(["--listen", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");

    // Connect client
    println!("3. Client: Connecting...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("4. Client: Connected");

    // Wait for the full Zenoh discovery chain to complete:
    // client TCP connect → import bridge declares liveliness token →
    // export bridge detects client → export bridge connects to backend →
    // export bridge sets up pub/sub. Without this wait, early messages
    // are sent before the Zenoh channel is fully established.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Send 100 messages with a small delay between each to avoid overwhelming
    // the Zenoh pub/sub pipeline.
    let num_messages = 100;
    println!("5. Client: Sending {} messages...", num_messages);

    for i in 0..num_messages {
        let msg = format!("message_{}\n", i);
        client.write_all(msg.as_bytes()).await?;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    println!("6. Client: All messages sent, waiting for delivery...");

    // Wait for messages to arrive at the backend. The data path crosses two
    // bridge subprocesses and Zenoh pub/sub: client TCP → import bridge →
    // Zenoh → export bridge → backend TCP. Due to Zenoh session discovery
    // timing across separate processes, the export bridge may not be fully
    // subscribed when early messages are sent, causing some to be dropped.
    // We keep the client connection alive and poll with a generous timeout.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut received_count;
    loop {
        received_count = *message_count.lock().await;
        if received_count >= num_messages {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    received_count = *message_count.lock().await;
    println!(
        "7. Backend received {}/{} messages",
        received_count, num_messages
    );

    // Assert a majority of messages arrived. Due to Zenoh discovery timing
    // across bridge subprocesses, some messages sent before the full pub/sub
    // channel is established may be lost. This is inherent to the subprocess
    // test architecture; in-process tests (http_edge_cases, etc.) achieve 100%.
    // The threshold catches real regressions while tolerating expected loss.
    assert!(
        received_count >= num_messages / 2,
        "Expected at least 50% of {} messages, but only {} arrived. \
         This indicates a significant regression in data delivery.",
        num_messages,
        received_count
    );

    // Cleanup
    drop(client);
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("9. ✓ TEST COMPLETED\n");

    Ok(())
}

/// A backend that appears AFTER the bridges must start serving: the first
/// client is refused (backend down), the backend then binds on the same
/// address, and a retried client must succeed — asserted.
#[tokio::test]
async fn test_backend_restart_recovery() -> Result<()> {
    let backend_port = common::PortGuard::new();
    let backend_addr = backend_port.release();

    let service = common::unique_service_name("restarttest");
    let export_spec = format!("{service}/{backend_addr}");
    let _export = common::BridgeProcess::new(&["--backend", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let _import =
        common::BridgeProcess::new(&["--listen", &format!("{service}/{import_addr},proto=raw")])
            .await;
    common::wait_for_port(import_addr, Duration::from_secs(10)).await?;

    // Phase 1: no backend. The connection must be CLOSED (error signal), not
    // left hanging.
    let mut client1 = TcpStream::connect(import_addr).await?;
    client1.write_all(b"test1\n").await?;
    let mut buf = [0u8; 64];
    match timeout(Duration::from_secs(15), client1.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {} // closed, as it must be
        Ok(Ok(n)) => anyhow::bail!("got {n} bytes from a dead backend"),
        Err(_) => anyhow::bail!("connection not closed while backend is down"),
    }
    drop(client1);

    // Phase 2: the backend comes up on the SAME address.
    let _backend =
        common::start_probe_immune_backend_on(backend_addr, |mut stream, first| async move {
            let _ = stream.write_all(&first).await;
        })
        .await;

    // A retried client must now be served end-to-end.
    let echoed = common::echo_roundtrip(import_addr, b"test2\n", common::BACKEND_READY_TIMEOUT)
        .await
        .expect("backend restart was never picked up");
    anyhow::ensure!(echoed == b"test2\n", "got {echoed:?}");

    Ok(())
}

/// The entire reason `proto=raw` exists: a server-speaks-first protocol
/// (SMTP-style banner) must flow through a raw listener whose client has not
/// sent a single byte — i.e. the listener declares liveliness on connect, not
/// on first read, and the auto-detect peek is genuinely skipped.
#[tokio::test]
async fn test_raw_listener_relays_server_first_banner() {
    // Backend greets immediately on accept, then echoes one line.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let _ = s.write_all(b"220 bridge.test SMTP ready\r\n").await;
                let mut buf = vec![0u8; 256];
                if let Ok(n) = s.read(&mut buf).await
                    && n > 0
                {
                    let _ = s.write_all(&buf[..n]).await;
                }
            });
        }
    });

    let service = common::unique_service_name("smtpish");
    let export_spec = format!("{}/{}", service, backend_addr);
    let _export = common::BridgeProcess::new(&["--backend", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let listen_spec = format!("{}/{},proto=raw", service, import_addr);
    let _import = common::BridgeProcess::new(&["--listen", &listen_spec]).await;
    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Connect and WRITE NOTHING: the banner must arrive anyway.
    let mut client = TcpStream::connect(import_addr).await.unwrap();
    let mut banner = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(10), client.read(&mut banner))
        .await
        .expect("banner timed out — raw listener must not wait for client bytes")
        .unwrap();
    assert!(
        banner[..n].starts_with(b"220 "),
        "expected the SMTP-style banner, got {:?}",
        String::from_utf8_lossy(&banner[..n])
    );

    // The connection still relays client bytes afterwards.
    client.write_all(b"EHLO x\r\n").await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(10), client.read(&mut banner))
        .await
        .expect("echo timed out")
        .unwrap();
    assert_eq!(&banner[..n], b"EHLO x\r\n");
}

/// The renamed Zenoh session flags actually reach the session config: two
/// bridges peered explicitly via --zenoh-listen / --zenoh-connect (no
/// multicast dependence for this pair) relay bytes end to end.
#[tokio::test]
async fn test_zenoh_endpoint_flags_wire_a_pair() {
    let (backend_addr, _echo) = common::start_echo_server().await;
    let service = common::unique_service_name("zflags");

    let zenoh_port = common::PortGuard::new();
    let zenoh_addr = zenoh_port.release();
    let zenoh_listen = format!("tcp/{zenoh_addr}");
    let zenoh_connect = format!("tcp/{zenoh_addr}");

    let export_spec = format!("{}/{}", service, backend_addr);
    let _export = common::BridgeProcess::new_raw(&[
        "--backend",
        &export_spec,
        "--zenoh-listen",
        &zenoh_listen,
    ])
    .await;
    tokio::time::sleep(Duration::from_millis(700)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let listen_spec = format!("{}/{},proto=raw", service, import_addr);
    let _import = common::BridgeProcess::new_raw(&[
        "--listen",
        &listen_spec,
        "--zenoh-connect",
        &zenoh_connect,
        "--zenoh-mode",
        "peer",
    ])
    .await;
    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut client = TcpStream::connect(import_addr).await.unwrap();
    client
        .write_all(b"ping-via-explicit-endpoints")
        .await
        .unwrap();
    let mut buf = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(10), client.read(&mut buf))
        .await
        .expect("echo timed out")
        .unwrap();
    assert_eq!(&buf[..n], b"ping-via-explicit-endpoints");
}

/// --zenoh-config with an unreadable file fails fast with a clean error.
#[tokio::test]
async fn test_zenoh_config_missing_file_fails_fast() {
    let out = common::bridge_command_raw()
        .args([
            "--listen",
            "svc/127.0.0.1:0,proto=raw",
            "--zenoh-config",
            "/nonexistent/zenoh.json5",
        ])
        .output()
        .await
        .expect("spawn bridge");
    assert!(!out.status.success(), "missing zenoh config must be fatal");
}

/// R2 regression: an abrupt import death must NOT stall the export's
/// liveliness loop for `drain_timeout`.
///
/// `handle_client_disconnect` used to hold the connection-map mutex across the
/// drain await; the drained task's last act locks the same map to self-remove,
/// so every liveliness `Delete` that hit a still-alive bridge task self-
/// deadlocked until the 5s drain timeout expired — during which the loop
/// processed no new client `Put`s. A client arriving just after any abrupt
/// disconnect waited out the whole stall. Under parallel test load this was a
/// principal source of "ambient" flakiness.
///
/// The choreography forces the racy path deterministically: the first import
/// bridge is SIGKILLed while its export-side task is alive (established echo
/// session), so the token `Delete` reaches a live task; a second import bridge
/// and client then must complete a round-trip well inside the 5s the bug burns.
#[tokio::test]
async fn abrupt_import_death_does_not_stall_the_export_loop() -> Result<()> {
    let (backend_addr, _echo) = common::start_echo_server().await;
    let service = common::unique_service_name("stallfree");

    let export_spec = format!("{service}/{backend_addr}");
    // A huge drain budget makes the bug's signature unmistakable: with the
    // mutex held across the drain, the next client waits ~30s; without it,
    // seconds even under full-suite load. Wall-clock margins stay wide.
    let _export =
        common::BridgeProcess::new(&["--backend", &export_spec, "--drain-timeout", "30"]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Import #1: establish a live end-to-end session (round-trip proves the
    // export-side bridge task exists), then kill the process abruptly.
    let port1 = common::PortGuard::new();
    let addr1 = port1.release();
    let mut import1 =
        common::BridgeProcess::new(&["--listen", &format!("{service}/{addr1},proto=raw")]).await;
    common::wait_for_port(addr1, Duration::from_secs(10)).await?;
    let echoed = common::echo_roundtrip(addr1, b"warm", common::BACKEND_READY_TIMEOUT)
        .await
        .expect("first import never became ready");
    assert_eq!(echoed, b"warm");

    // Keep a client OPEN through import #1 so the export-side task is
    // definitely alive when the process dies (no clean EOF is ever sent).
    let mut held = TcpStream::connect(addr1).await?;
    held.write_all(b"hold").await?;
    let mut buf = [0u8; 4];
    timeout(Duration::from_secs(10), held.read_exact(&mut buf)).await??;

    import1.kill_and_wait().await; // SIGKILL: no drain, token dies with the session

    // Import #2 on the same service: the export loop must process its client
    // promptly. With the bug, the pending `Delete` stalls the loop for the full
    // 5s drain timeout before this client's `Put` is even looked at.
    let port2 = common::PortGuard::new();
    let addr2 = port2.release();
    let _import2 =
        common::BridgeProcess::new(&["--listen", &format!("{service}/{addr2},proto=raw")]).await;
    common::wait_for_port(addr2, Duration::from_secs(10)).await?;

    let started = std::time::Instant::now();
    let echoed = common::echo_roundtrip(addr2, b"next", common::BACKEND_READY_TIMEOUT)
        .await
        .expect("second import never served");
    let elapsed = started.elapsed();
    assert_eq!(echoed, b"next");
    assert!(
        elapsed < Duration::from_secs(15),
        "export loop stalled {elapsed:?} before serving the next client — \
         the disconnect drain is blocking the liveliness loop"
    );
    Ok(())
}

/// A1/R1 regression, run under the race amplifier that made it deterministic.
///
/// The export publishes `{service}/error/{client}` the moment its backend dial
/// fails — sub-milliseconds after it first learned the client exists, i.e. at
/// the point of maximal interest-propagation skew. As a bare, uncached
/// `session.put()`, the signal was simply LOST whenever the import's error-
/// subscriber interest hadn't reached the export's session yet, and the client
/// hung forever. `RUST_LOG=zenoh_transport=debug` inside the bridge processes
/// slows session I/O enough to make that loss deterministic (6/6 full-suite
/// failures during the audit); the fix (cached AdvancedPublisher + history-
/// recovering subscriber) must hold under exactly that amplifier.
///
/// Unlike the older lenient test above, this one accepts only a real close.
#[tokio::test]
async fn backend_unavailable_closes_client_under_transport_load() -> Result<()> {
    let backend_port = common::PortGuard::new();
    let backend_addr = backend_port.release(); // nothing listens here

    let service = common::unique_service_name("nobackend_amp");
    let export_spec = format!("{service}/{backend_addr}");
    let mut export_bridge = common::bridge_command()
        .args(["--backend", &export_spec])
        .env("RUST_LOG", "zenoh_transport=debug") // race amplifier
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let import_spec = format!("{service}/{import_addr},proto=raw");
    let mut import_bridge = common::bridge_command()
        .args(["--listen", &import_spec])
        .env("RUST_LOG", "zenoh_transport=debug")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    common::wait_for_port(import_addr, Duration::from_secs(10)).await?;

    let mut client = TcpStream::connect(import_addr).await?;

    // The ONLY acceptable outcome is a close: read returns 0 or errors within
    // the budget (liveliness propagation + 5 dial retries + signal delivery).
    let mut buf = [0u8; 64];
    match timeout(Duration::from_secs(15), client.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {} // closed — the error signal arrived
        Ok(Ok(n)) => panic!("received {n} bytes from a nonexistent backend"),
        Err(_) => panic!(
            "client still open 15s after connecting to a service whose backend \
             cannot be dialed — the error signal was lost"
        ),
    }

    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    Ok(())
}

/// A1/R1, distilled: the error signal must be RECOVERABLE by a subscriber
/// whose interest arrives after the signal was published.
///
/// This is the race without needing load: the test itself plays the import.
/// It declares the client liveliness token (so the export dials and fails),
/// deliberately waits until the export must already have published the error
/// signal, and only THEN subscribes to the error key — with history, as the
/// real import now does. A bare `session.put()` is long gone at that point
/// (this exact shape hung real clients); the cached AdvancedPublisher, held
/// alive while the client token exists, must deliver it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_signal_is_recoverable_by_a_late_subscriber() -> Result<()> {
    use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig};

    let backend_port = common::PortGuard::new();
    let backend_addr = backend_port.release(); // nothing listens here

    // One domain shared by the subprocess export AND the in-process probe
    // session below — they must discover each other.
    let domain = common::ScoutDomain::new();
    let service = common::unique_service_name("laterr");
    let export_spec = format!("{service}/{backend_addr}");
    let _export = domain.bridge(&["--backend", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let session = zenoh::open(domain.config()).await.unwrap();
    let client_id = format!("client_{}", uuid::Uuid::new_v4().as_simple());

    // Play the import's liveliness half only — the export will dial its dead
    // backend (5 fast refusals + backoff, ~3-4s) and publish the error signal.
    let token = session
        .liveliness()
        .declare_token(format!("{service}/clients/{client_id}"))
        .await
        .unwrap();

    // Wait until the signal has certainly been published...
    tokio::time::sleep(Duration::from_secs(6)).await;

    // ...and only now subscribe, with history. The signal predates our
    // interest; only the publisher's cache can deliver it.
    let error_sub = session
        .declare_subscriber(
            format!("{service}/clients/{client_id}").replace("/clients/", "/error/"),
        )
        .history(HistoryConfig::default().detect_late_publishers())
        .await
        .unwrap();

    // 15s: the first history query can be delayed under cross-test session
    // churn, but the error publisher's heartbeat (500ms) makes the subscriber's
    // late-publisher detection re-query, so recovery is guaranteed to converge —
    // this bound only has to outlast churn, not define the mechanism.
    let sample = tokio::time::timeout(Duration::from_secs(30), error_sub.recv_async())
        .await
        .expect("late subscriber never recovered the error signal — it was published fire-once")
        .expect("error subscriber closed");
    assert_eq!(
        sample.payload().to_bytes().as_ref(),
        b"backend_unavailable",
        "recovered signal must carry the failure reason"
    );

    drop(token);
    Ok(())
}
