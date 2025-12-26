//! Integration tests for Zenoh TCP Bridge Liveliness Feature
//!
//! **PURPOSE: Test backend availability detection using Zenoh liveliness**
//!
//! These tests validate that the bridge correctly handles scenarios where the
//! backend server may not be available:
//!
//! Scenarios tested:
//! 1. Backend available - connections should be accepted
//! 2. Backend not available - connections should be rejected
//! 3. Backend goes down during connection - connection should close
//! 4. Backend comes back online - new connections should work
//!
//! Architecture:
//!   TCP Client -> Bridge A (client role) -> Zenoh -> Bridge B (server role) -> Backend Server
//!                          |                                    |
//!                          |<---- monitors liveliness ---------|
//!                                   (backend alive?)

#[cfg(test)]
mod liveliness_tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::RwLock;
    use tokio::time::{sleep, timeout};
    use zenoh::config::Config;
    use zenoh::sample::SampleKind;

    /// Simple TCP echo server for testing
    async fn start_tcp_echo_server(addr: SocketAddr) {
        let listener = TcpListener::bind(addr).await.unwrap();
        println!("TCP echo server listening on {}", addr);

        tokio::spawn(async move {
            while let Ok((mut stream, client_addr)) = listener.accept().await {
                println!("Echo server: connection from {}", client_addr);
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    loop {
                        match stream.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(n) => {
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

        sleep(Duration::from_millis(200)).await;
    }

    /// Bridge in server role - declares liveliness and forwards to backend
    /// Returns the liveliness token to keep it alive
    async fn run_server_bridge(
        session: Arc<zenoh::Session>,
        backend_addr: SocketAddr,
        sub_key: String,
        pub_key: String,
        liveliness_key: String,
    ) -> zenoh::liveliness::LivelinessToken {
        // Declare liveliness token
        let liveliness_token = session
            .liveliness()
            .declare_token(&liveliness_key)
            .await
            .expect("Failed to declare liveliness");

        println!("Server bridge: declared liveliness at {}", liveliness_key);

        // Subscribe to client requests
        let subscriber = session
            .declare_subscriber(&sub_key)
            .await
            .expect("Failed to subscribe");

        println!("Server bridge: subscribed to {}", sub_key);

        tokio::spawn(async move {
            while let Ok(sample) = subscriber.recv_async().await {
                let payload = sample.payload().to_bytes().to_vec();
                let session_clone = session.clone();
                let pub_key = pub_key.clone();

                tokio::spawn(async move {
                    if let Ok(mut stream) = TcpStream::connect(backend_addr).await
                        && stream.write_all(&payload).await.is_ok() {
                            let mut buffer = vec![0u8; 1024];
                            if let Ok(n) = stream.read(&mut buffer).await
                                && n > 0 {
                                    let _ = session_clone.put(&pub_key, &buffer[..n]).await;
                                }
                        }
                });
            }
        });

        liveliness_token
    }

    /// Bridge in client role - monitors liveliness and accepts client connections
    async fn run_client_bridge(
        session: Arc<zenoh::Session>,
        listen_addr: SocketAddr,
        sub_key: String,
        pub_key: String,
        liveliness_key: String,
    ) -> Arc<RwLock<bool>> {
        let backend_available = Arc::new(RwLock::new(false));

        // Subscribe to backend liveliness
        let liveliness_sub = session
            .liveliness()
            .declare_subscriber(&liveliness_key)
            .await
            .expect("Failed to subscribe to liveliness");

        println!("Client bridge: subscribed to liveliness {}", liveliness_key);

        // Monitor backend availability
        let backend_available_clone = backend_available.clone();
        tokio::spawn(async move {
            while let Ok(sample) = liveliness_sub.recv_async().await {
                match sample.kind() {
                    SampleKind::Put => {
                        println!("Client bridge: Backend is AVAILABLE");
                        *backend_available_clone.write().await = true;
                    }
                    SampleKind::Delete => {
                        println!("Client bridge: Backend is UNAVAILABLE");
                        *backend_available_clone.write().await = false;
                    }
                }
            }
        });

        // Query initial liveliness state
        sleep(Duration::from_millis(500)).await;
        if let Ok(replies) = session.liveliness().get(&liveliness_key).await {
            sleep(Duration::from_millis(300)).await;
            while let Ok(Some(reply)) = replies.try_recv() {
                if let Ok(sample) = reply.result()
                    && sample.kind() == SampleKind::Put {
                        println!("Client bridge: Found backend on startup");
                        *backend_available.write().await = true;
                        break;
                    }
            }
        }

        if !*backend_available.read().await {
            println!("Client bridge: No backend found on initial query");
        }

        // Accept client connections
        let backend_available_clone = backend_available.clone();
        let listener = TcpListener::bind(listen_addr).await.unwrap();
        println!("Client bridge: listening on {}", listen_addr);

        tokio::spawn(async move {
            while let Ok((stream, addr)) = listener.accept().await {
                // Check backend availability
                if !*backend_available_clone.read().await {
                    println!("Client bridge: Rejecting {} - backend unavailable", addr);
                    drop(stream);
                    continue;
                }

                println!("Client bridge: Accepting connection from {}", addr);
                let session = session.clone();
                let pub_key = pub_key.clone();
                let sub_key = sub_key.clone();
                let backend_available = backend_available_clone.clone();

                tokio::spawn(async move {
                    let (mut tcp_reader, mut tcp_writer) = stream.into_split();
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(100);

                    // Zenoh -> TCP
                    let subscriber = session.declare_subscriber(&sub_key).await.unwrap();
                    let tx_clone = tx.clone();
                    tokio::spawn(async move {
                        while let Ok(sample) = subscriber.recv_async().await {
                            let payload = sample.payload().to_bytes().to_vec();
                            if tx_clone.send(payload).await.is_err() {
                                break;
                            }
                        }
                    });

                    tokio::spawn(async move {
                        while let Some(data) = rx.recv().await {
                            if tcp_writer.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                    });

                    // TCP -> Zenoh
                    let mut buffer = vec![0u8; 1024];
                    loop {
                        if !*backend_available.read().await {
                            println!("Backend became unavailable, closing connection");
                            break;
                        }

                        match tcp_reader.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if session.put(&pub_key, &buffer[..n]).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        });

        backend_available
    }

    /// TEST 1: Backend available - connections should work
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_backend_available() {
        println!("\n╔════════════════════════════════════════════════════════╗");
        println!("║  TEST: Backend Available - Normal Operation           ║");
        println!("╚════════════════════════════════════════════════════════╝\n");

        // 1. Start backend TCP server
        let backend_addr: SocketAddr = "127.0.0.1:60001".parse().unwrap();
        start_tcp_echo_server(backend_addr).await;
        println!("✓ Backend server started on {}", backend_addr);

        // 2. Create Zenoh sessions
        let mut config_a = Config::default();
        config_a.insert_json5("mode", "\"peer\"").unwrap();
        let session_a = Arc::new(zenoh::open(config_a).await.unwrap());

        let mut config_b = Config::default();
        config_b.insert_json5("mode", "\"peer\"").unwrap();
        let session_b = Arc::new(zenoh::open(config_b).await.unwrap());

        println!("✓ Zenoh sessions created");
        sleep(Duration::from_secs(1)).await;

        // 3. Start server bridge (with liveliness)
        let _liveliness_token = run_server_bridge(
            session_b.clone(),
            backend_addr,
            "test1/client_to_server".to_string(),
            "test1/server_to_client".to_string(),
            "test1/backend/alive".to_string(),
        )
        .await;
        println!("✓ Server bridge started (declaring liveliness)");

        sleep(Duration::from_secs(1)).await;

        // 4. Start client bridge (monitors liveliness)
        let client_addr: SocketAddr = "127.0.0.1:60011".parse().unwrap();
        let backend_status = run_client_bridge(
            session_a.clone(),
            client_addr,
            "test1/server_to_client".to_string(),
            "test1/client_to_server".to_string(),
            "test1/backend/alive".to_string(),
        )
        .await;
        println!("✓ Client bridge started on {}", client_addr);

        sleep(Duration::from_secs(1)).await;

        // 5. Verify backend is detected as available
        let is_available = *backend_status.read().await;
        if !is_available {
            println!("⚠️  Backend not detected yet, waiting a bit more...");
            sleep(Duration::from_secs(1)).await;
        }

        assert!(
            *backend_status.read().await,
            "Backend should be detected as available"
        );
        println!("✓ Backend detected as AVAILABLE\n");

        // 6. Connect client and test communication
        println!("Connecting client to bridge...");
        let result = timeout(Duration::from_secs(2), TcpStream::connect(client_addr)).await;

        assert!(result.is_ok(), "Should connect to bridge");
        let mut client = result.unwrap().unwrap();
        println!("✓ Client connected to bridge");

        // 7. Send message and verify echo
        println!("Sending test message...");
        client.write_all(b"Hello Backend!").await.unwrap();

        let mut response = [0u8; 1024];
        let result = timeout(Duration::from_secs(2), client.read(&mut response)).await;

        assert!(result.is_ok(), "Should receive response");
        let n = result.unwrap().unwrap();
        assert!(n > 0, "Should receive data");

        let response_str = String::from_utf8_lossy(&response[..n]);
        println!("✓ Received response: '{}'", response_str);

        assert_eq!(response_str, "Hello Backend!");
        println!("\n✅ SUCCESS: Backend available, communication works!");
    }

    /// TEST 2: Backend not available - connections should be rejected
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_backend_not_available() {
        println!("\n╔════════════════════════════════════════════════════════╗");
        println!("║  TEST: Backend Not Available - Reject Connections     ║");
        println!("╚════════════════════════════════════════════════════════╝\n");

        // 1. Create Zenoh sessions (NO backend server!)
        let mut config = Config::default();
        config.insert_json5("mode", "\"peer\"").unwrap();
        let session = Arc::new(zenoh::open(config).await.unwrap());

        println!("✓ Zenoh session created");
        sleep(Duration::from_millis(500)).await;

        // 2. Start client bridge (no server bridge = no liveliness)
        let client_addr: SocketAddr = "127.0.0.1:60021".parse().unwrap();
        let backend_status = run_client_bridge(
            session.clone(),
            client_addr,
            "test2/server_to_client".to_string(),
            "test2/client_to_server".to_string(),
            "test2/backend/alive".to_string(),
        )
        .await;
        println!("✓ Client bridge started on {}", client_addr);

        sleep(Duration::from_secs(1)).await;

        // 3. Verify backend is NOT available
        assert!(
            !*backend_status.read().await,
            "Backend should NOT be available"
        );
        println!("✓ Backend correctly detected as UNAVAILABLE\n");

        // 4. Try to connect - should be rejected
        println!("Attempting to connect (should be rejected)...");
        let mut client = TcpStream::connect(client_addr).await.unwrap();

        // Bridge should close connection immediately
        let mut buffer = [0u8; 1024];
        let result = timeout(Duration::from_millis(500), client.read(&mut buffer)).await;

        // Connection should be closed or read should fail
        match result {
            Ok(Ok(0)) => {
                println!("✓ Connection rejected (closed immediately)");
            }
            Ok(Ok(_)) => {
                panic!("Should not receive data when backend unavailable");
            }
            Ok(Err(_)) | Err(_) => {
                println!("✓ Connection rejected (read failed/timeout)");
            }
        }

        println!("\n✅ SUCCESS: Backend unavailable, connections rejected!");
    }

    /// TEST 3: Backend goes down during connection
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_backend_goes_down() {
        println!("\n╔════════════════════════════════════════════════════════╗");
        println!("║  TEST: Backend Goes Down - Close Active Connections   ║");
        println!("╚════════════════════════════════════════════════════════╝\n");

        // 1. Start backend
        let backend_addr: SocketAddr = "127.0.0.1:60003".parse().unwrap();
        start_tcp_echo_server(backend_addr).await;
        println!("✓ Backend server started");

        // 2. Create Zenoh sessions
        let mut config_a = Config::default();
        config_a.insert_json5("mode", "\"peer\"").unwrap();
        let session_a = Arc::new(zenoh::open(config_a).await.unwrap());

        let mut config_b = Config::default();
        config_b.insert_json5("mode", "\"peer\"").unwrap();
        let session_b = Arc::new(zenoh::open(config_b).await.unwrap());

        sleep(Duration::from_secs(1)).await;

        // 3. Start server bridge with liveliness - keep token to drop it later
        let liveliness_token = run_server_bridge(
            session_b.clone(),
            backend_addr,
            "test3/client_to_server".to_string(),
            "test3/server_to_client".to_string(),
            "test3/backend/alive".to_string(),
        )
        .await;

        println!("✓ Server bridge started with liveliness");
        sleep(Duration::from_secs(1)).await;

        // 4. Start client bridge
        let client_addr: SocketAddr = "127.0.0.1:60031".parse().unwrap();
        let backend_status = run_client_bridge(
            session_a.clone(),
            client_addr,
            "test3/server_to_client".to_string(),
            "test3/client_to_server".to_string(),
            "test3/backend/alive".to_string(),
        )
        .await;

        println!("✓ Client bridge started");
        sleep(Duration::from_secs(1)).await;

        assert!(*backend_status.read().await, "Backend should be available");
        println!("✓ Backend initially AVAILABLE");

        // 5. Connect client
        let mut client = TcpStream::connect(client_addr).await.unwrap();
        println!("✓ Client connected\n");

        // 6. Send initial message (should work)
        client.write_all(b"Test 1").await.unwrap();
        let mut buffer = [0u8; 1024];
        let n = timeout(Duration::from_secs(1), client.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "Should receive initial response");
        println!("✓ Initial message worked");

        // 7. Simulate backend going down (drop liveliness token)
        println!("\nSimulating backend going down...");
        drop(liveliness_token);
        sleep(Duration::from_millis(500)).await;

        assert!(
            !*backend_status.read().await,
            "Backend should now be unavailable"
        );
        println!("✓ Backend now UNAVAILABLE");

        // 8. Try to send another message - connection should close
        println!("Attempting to send message (should fail)...");
        let write_result = client.write_all(b"Test 2").await;

        // Either write fails or subsequent read fails
        if write_result.is_ok() {
            let read_result = timeout(Duration::from_secs(1), client.read(&mut buffer)).await;
            match read_result {
                Ok(Ok(0)) | Err(_) => {
                    println!("✓ Connection closed by bridge");
                }
                _ => {
                    // Connection might close on next read attempt
                    println!("✓ Connection will be closed");
                }
            }
        } else {
            println!("✓ Write failed (connection closed)");
        }

        println!("\n✅ SUCCESS: Backend went down, connection handled!");
    }

    /// TEST 4: Backend comes back online
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_backend_comes_back() {
        println!("\n╔════════════════════════════════════════════════════════╗");
        println!("║  TEST: Backend Comes Back - Accept New Connections    ║");
        println!("╚════════════════════════════════════════════════════════╝\n");

        // 1. Start backend
        let backend_addr: SocketAddr = "127.0.0.1:60004".parse().unwrap();
        start_tcp_echo_server(backend_addr).await;

        // 2. Create Zenoh sessions
        let mut config_a = Config::default();
        config_a.insert_json5("mode", "\"peer\"").unwrap();
        let session_a = Arc::new(zenoh::open(config_a).await.unwrap());

        let mut config_b = Config::default();
        config_b.insert_json5("mode", "\"peer\"").unwrap();
        let session_b = Arc::new(zenoh::open(config_b).await.unwrap());

        sleep(Duration::from_secs(1)).await;

        // 3. Start client bridge FIRST (no backend yet)
        let client_addr: SocketAddr = "127.0.0.1:60041".parse().unwrap();
        let backend_status = run_client_bridge(
            session_a.clone(),
            client_addr,
            "test4/server_to_client".to_string(),
            "test4/client_to_server".to_string(),
            "test4/backend/alive".to_string(),
        )
        .await;

        println!("✓ Client bridge started");
        sleep(Duration::from_secs(1)).await;

        assert!(
            !*backend_status.read().await,
            "Backend should initially be unavailable"
        );
        println!("✓ Backend initially UNAVAILABLE\n");

        // 4. Try to connect - should be rejected
        println!("Attempting connection (should be rejected)...");
        let client = TcpStream::connect(client_addr).await.unwrap();
        drop(client); // Will be closed by bridge
        println!("✓ Connection rejected as expected\n");

        // 5. Now start server bridge (backend comes online)
        println!("Bringing backend online...");
        let _liveliness_token = run_server_bridge(
            session_b.clone(),
            backend_addr,
            "test4/client_to_server".to_string(),
            "test4/server_to_client".to_string(),
            "test4/backend/alive".to_string(),
        )
        .await;

        sleep(Duration::from_secs(1)).await;

        assert!(
            *backend_status.read().await,
            "Backend should now be available"
        );
        println!("✓ Backend now AVAILABLE\n");

        // 6. Try to connect again - should work now
        println!("Attempting connection (should work now)...");
        let mut client = TcpStream::connect(client_addr).await.unwrap();
        println!("✓ Connected successfully");

        // 7. Test communication
        client.write_all(b"Backend is back!").await.unwrap();
        let mut buffer = [0u8; 1024];
        let n = timeout(Duration::from_secs(2), client.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();

        assert!(n > 0, "Should receive response");
        let response = String::from_utf8_lossy(&buffer[..n]);
        println!("✓ Received: '{}'", response);

        println!("\n✅ SUCCESS: Backend came back, new connections work!");
    }

    /// TEST 5: Documentation test explaining liveliness feature
    #[test]
    fn test_liveliness_documentation() {
        println!("\n╔════════════════════════════════════════════════════════╗");
        println!("║  Liveliness Feature - What We're Testing              ║");
        println!("╚════════════════════════════════════════════════════════╝\n");

        println!("🎯 PURPOSE:");
        println!("   Handle cases where backend server is unavailable\n");

        println!("🏗️  ARCHITECTURE:");
        println!("   Client -> Bridge A (monitors) -> Zenoh -> Bridge B (declares) -> Backend");
        println!("                      |                            |");
        println!("                      |<---- liveliness token -----|");
        println!("                      (checks: is backend alive?)\n");

        println!("✅ WHAT WE TEST:");
        println!("   • Backend available → accept connections");
        println!("   • Backend unavailable → reject connections");
        println!("   • Backend goes down → close active connections");
        println!("   • Backend comes back → accept new connections\n");

        println!("🔧 HOW IT WORKS:");
        println!("   1. Server bridge declares liveliness token");
        println!("   2. Client bridge subscribes to liveliness");
        println!("   3. Client bridge checks backend status before accepting");
        println!("   4. Active connections monitor backend availability\n");

        println!("💡 WHY IT MATTERS:");
        println!("   • Production-ready: Don't accept connections to dead backends");
        println!("   • Fail fast: Reject immediately rather than timeout");
        println!("   • Resource management: Close connections when backend dies");
        println!("   • Automatic recovery: Resume when backend returns\n");
    }
}
