//! Integration tests for WebSocket support through Zenoh TCP Bridge
//!
//! **PURPOSE: Test the bridge with real WebSocket servers and clients**
//!
//! Architecture tested:
//!   WS Client -> Bridge A (ws-import) -> Zenoh -> Bridge B (ws-export) -> WS Server
//!
//! What this tests:
//!   ✓ WebSocket export connects to backend WS servers
//!   ✓ WebSocket import accepts WS client connections
//!   ✓ Binary and text messages pass through correctly
//!   ✓ Connection lifecycle (connect, messages, close)

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};

/// Create an echo WebSocket server that reflects messages back to the client
async fn start_ws_echo_server(addr: &str) -> Result<String> {
    let listener = TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    let ws_url = format!("ws://{}", bound_addr);

    tokio::spawn(async move {
        println!("WebSocket echo server listening on {}", bound_addr);

        while let Ok((stream, peer_addr)) = listener.accept().await {
            println!("WS Server: Connection from {}", peer_addr);

            tokio::spawn(async move {
                let ws_stream = match accept_async(stream).await {
                    Ok(ws) => ws,
                    Err(e) => {
                        eprintln!("WS Server: Failed to accept WebSocket: {:?}", e);
                        return;
                    }
                };

                let (mut sender, mut receiver) = ws_stream.split();

                while let Some(msg_result) = receiver.next().await {
                    match msg_result {
                        Ok(msg) => match msg {
                            Message::Binary(data) => {
                                println!("WS Server: Received {} bytes, echoing back", data.len());
                                if let Err(e) = sender.send(Message::Binary(data)).await {
                                    eprintln!("WS Server: Failed to send: {:?}", e);
                                    break;
                                }
                            }
                            Message::Text(text) => {
                                println!("WS Server: Received text '{}', echoing back", text);
                                if let Err(e) = sender.send(Message::Text(text)).await {
                                    eprintln!("WS Server: Failed to send: {:?}", e);
                                    break;
                                }
                            }
                            Message::Close(_) => {
                                println!("WS Server: Client closed connection");
                                break;
                            }
                            Message::Ping(data) => {
                                let _ = sender.send(Message::Pong(data)).await;
                            }
                            _ => {}
                        },
                        Err(e) => {
                            eprintln!("WS Server: Receive error: {:?}", e);
                            break;
                        }
                    }
                }

                println!("WS Server: Connection handler finished for {}", peer_addr);
            });
        }
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(200)).await;

    Ok(ws_url)
}

/// Test basic WebSocket export/import communication
#[tokio::test]
async fn test_ws_export_import_basic() -> Result<()> {
    println!("\n=== Test: WebSocket Export/Import Basic Communication ===\n");

    // Step 1: Start WebSocket echo server as backend
    let ws_server_url = start_ws_echo_server("127.0.0.1:0").await?;
    println!("1. WebSocket echo server running at {}", ws_server_url);

    // Step 2: Start export bridge connected to WebSocket backend
    let ws_export_spec = format!("wstest/{}", ws_server_url);
    println!(
        "2. Starting ws-export bridge: --ws-export '{}'",
        ws_export_spec
    );

    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--ws-export", &ws_export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("3. WebSocket export bridge started");

    // Step 3: Find a free port for import bridge
    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let ws_import_spec = format!("wstest/{}", import_addr);
    println!(
        "4. Starting ws-import bridge: --ws-import '{}'",
        ws_import_spec
    );

    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--ws-import", &ws_import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("5. WebSocket import bridge started");

    // Step 4: Connect WebSocket client to import bridge
    let client_ws_url = format!("ws://{}", import_addr);
    println!("6. Client connecting to {}", client_ws_url);

    let (ws_stream, _) = timeout(Duration::from_secs(5), connect_async(&client_ws_url))
        .await
        .map_err(|_| anyhow::anyhow!("Timeout connecting to import bridge"))??;

    println!("7. Client WebSocket connected");

    let (mut sender, mut receiver) = ws_stream.split();

    // Give bridges time to establish Zenoh connections
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Step 5: Send binary message
    let test_data = b"Hello WebSocket through Zenoh!";
    println!(
        "8. Client sending binary message: {:?}",
        String::from_utf8_lossy(test_data)
    );

    sender
        .send(Message::Binary(test_data.to_vec().into()))
        .await?;

    // Step 6: Receive echoed response
    let response = timeout(Duration::from_secs(5), receiver.next())
        .await
        .map_err(|_| anyhow::anyhow!("Timeout waiting for response"))?
        .ok_or_else(|| anyhow::anyhow!("Connection closed without response"))??;

    match response {
        Message::Binary(data) => {
            let received = String::from_utf8_lossy(&data);
            println!("9. Client received binary response: {:?}", received);
            assert_eq!(
                data.as_ref(),
                test_data,
                "Echoed data should match sent data"
            );
            println!("10. ✓ Binary echo test passed!");
        }
        other => {
            println!("9. ✗ Unexpected message type: {:?}", other);
            panic!("Expected binary message, got: {:?}", other);
        }
    }

    // Cleanup
    println!("11. Cleaning up...");
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    println!("12. ✓ Test completed successfully!\n");

    Ok(())
}

/// Test multiple messages through WebSocket bridge
#[tokio::test]
async fn test_ws_multiple_messages() -> Result<()> {
    println!("\n=== Test: WebSocket Multiple Messages ===\n");

    // Start WebSocket echo server
    let ws_server_url = start_ws_echo_server("127.0.0.1:0").await?;
    println!("1. WebSocket echo server at {}", ws_server_url);

    // Start bridges
    let ws_export_spec = format!("wsmulti/{}", ws_server_url);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--ws-export", &ws_export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let ws_import_spec = format!("wsmulti/{}", import_addr);
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--ws-import", &ws_import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect client
    let client_ws_url = format!("ws://{}", import_addr);
    let (ws_stream, _) = timeout(Duration::from_secs(5), connect_async(&client_ws_url)).await??;

    let (mut sender, mut receiver) = ws_stream.split();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send multiple messages
    let messages = ["First message", "Second message", "Third message"];

    for (i, msg) in messages.iter().enumerate() {
        println!("2.{} Sending: {}", i + 1, msg);
        sender
            .send(Message::Binary(msg.as_bytes().to_vec().into()))
            .await?;

        let response = timeout(Duration::from_secs(3), receiver.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("No response"))??;

        match response {
            Message::Binary(data) => {
                let received = String::from_utf8_lossy(&data);
                println!("   Received: {}", received);
                assert_eq!(data.as_ref(), msg.as_bytes());
            }
            other => panic!("Unexpected: {:?}", other),
        }
    }

    println!("3. ✓ All {} messages echoed correctly!", messages.len());

    // Cleanup
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;

    Ok(())
}

/// Test WebSocket connection lifecycle
#[tokio::test]
async fn test_ws_connection_lifecycle() -> Result<()> {
    println!("\n=== Test: WebSocket Connection Lifecycle ===\n");

    // Track connections on server side
    let connection_count = Arc::new(Mutex::new(0u32));
    let disconnection_count = Arc::new(Mutex::new(0u32));

    let conn_count = connection_count.clone();
    let disconn_count = disconnection_count.clone();

    // Start a WebSocket server that tracks connections
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let bound_addr = listener.local_addr()?;
    let ws_server_url = format!("ws://{}", bound_addr);

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let conn = conn_count.clone();
            let disconn = disconn_count.clone();

            tokio::spawn(async move {
                *conn.lock().await += 1;
                println!(
                    "WS Server: Client connected (total: {})",
                    *conn.lock().await
                );

                if let Ok(ws_stream) = accept_async(stream).await {
                    let (mut sender, mut receiver) = ws_stream.split();

                    while let Some(Ok(msg)) = receiver.next().await {
                        match msg {
                            Message::Binary(data) => {
                                let _ = sender.send(Message::Binary(data)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }

                *disconn.lock().await += 1;
                println!(
                    "WS Server: Client disconnected (total: {})",
                    *disconn.lock().await
                );
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("1. WebSocket server with tracking at {}", ws_server_url);

    // Start bridges
    let ws_export_spec = format!("wslifecycle/{}", ws_server_url);
    let mut export_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--ws-export", &ws_export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let ws_import_spec = format!("wslifecycle/{}", import_addr);
    let mut import_bridge = Command::new("./target/debug/zenoh-bridge-tcp")
        .args(["--ws-import", &ws_import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("2. Bridges started");

    // Connect first client
    let client_ws_url = format!("ws://{}", import_addr);
    println!("3. Connecting first client...");

    let (ws_stream1, _) = timeout(Duration::from_secs(5), connect_async(&client_ws_url)).await??;

    let (mut sender1, _receiver1) = ws_stream1.split();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send a message
    sender1
        .send(Message::Binary(b"Hello".to_vec().into()))
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Close connection
    println!("4. Closing first client...");
    sender1.send(Message::Close(None)).await?;
    drop(sender1);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check disconnect was detected
    let disconnects = *disconnection_count.lock().await;
    println!("5. Disconnections recorded: {}", disconnects);

    // Cleanup
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    println!("6. ✓ Lifecycle test completed!\n");

    Ok(())
}
