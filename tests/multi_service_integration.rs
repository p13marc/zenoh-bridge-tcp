//! Integration tests for multiple simultaneous export and import operations.
//!
//! Verifies that the bridge can handle multiple services concurrently
//! using real library functions (not custom simulations).

mod common;

use common::{PortGuard, unique_service_name, wait_for_port};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use zenoh::config::Config;
use zenoh_bridge_tcp::config::BridgeConfig;

/// Start a simple echo server, returning its address.
async fn start_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        }
    });
    addr
}

/// Start a counter server that responds with "count:N\n" for each message.
async fn start_counter_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1024];
                    let mut counter = 0u32;
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_n) => {
                                counter += 1;
                                let response = format!("count:{}\n", counter);
                                if stream.write_all(response.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        }
    });
    addr
}

/// Start a reverse server that reverses the input bytes.
async fn start_reverse_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1024];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let mut reversed = buf[..n].to_vec();
                                reversed.reverse();
                                if stream.write_all(&reversed).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        }
    });
    addr
}

/// Spawn an export+import pair using real library functions.
/// Returns (shutdown_token, import_addr).
async fn spawn_bridge_pair(
    service: &str,
    backend_addr: SocketAddr,
    session_export: Arc<zenoh::Session>,
    session_import: Arc<zenoh::Session>,
    shutdown_token: &CancellationToken,
    config: &Arc<BridgeConfig>,
) -> SocketAddr {
    // Export
    let export_spec = format!("{}/{}", service, backend_addr);
    let s = session_export.clone();
    let t = shutdown_token.child_token();
    let c = config.clone();
    tokio::spawn(async move {
        let _ = zenoh_bridge_tcp::export::run_export_mode(s, &export_spec, c, t).await;
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Import
    let import_port = PortGuard::new();
    let import_addr = import_port.addr();
    let import_spec = format!("{}/{}", service, import_addr);
    let import_addr = import_port.release();
    let s = session_import.clone();
    let t = shutdown_token.child_token();
    let c = config.clone();
    tokio::spawn(async move {
        let _ = zenoh_bridge_tcp::import::run_import_mode(s, &import_spec, c, t).await;
    });

    wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");

    import_addr
}

/// Send a message through the bridge and read the response.
/// Includes a post-connect delay for Zenoh liveliness propagation between processes.
async fn send_and_receive(import_addr: SocketAddr, data: &[u8]) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    // Wait for liveliness propagation: import declares token → export detects → connects to backend
    tokio::time::sleep(Duration::from_secs(2)).await;
    stream.write_all(data).await.unwrap();

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
        .await
        .expect("Timeout waiting for response")
        .expect("Read error");
    buf[..n].to_vec()
}

/// Test multiple exports working simultaneously (echo + counter).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiple_exports() {
    let _ = tracing_subscriber::fmt::try_init();

    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());

    let echo_addr = start_echo_server().await;
    let counter_addr = start_counter_server().await;

    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let svc_echo = unique_service_name("multiexp_echo");
    let svc_counter = unique_service_name("multiexp_count");

    let import_echo = spawn_bridge_pair(
        &svc_echo,
        echo_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    let import_counter = spawn_bridge_pair(
        &svc_counter,
        counter_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    // Test echo service
    let response = send_and_receive(import_echo, b"hello").await;
    assert_eq!(response, b"hello");

    // Test counter service
    let response = send_and_receive(import_counter, b"test").await;
    assert!(response.starts_with(b"count:"));

    shutdown_token.cancel();
}

/// Test multiple imports from different services.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiple_imports() {
    let _ = tracing_subscriber::fmt::try_init();

    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());

    let echo_addr = start_echo_server().await;
    let counter_addr = start_counter_server().await;

    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let svc1 = unique_service_name("multiimp_svc1");
    let svc2 = unique_service_name("multiimp_svc2");

    let import1 = spawn_bridge_pair(
        &svc1,
        echo_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    let import2 = spawn_bridge_pair(
        &svc2,
        counter_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    // Both should work independently
    let resp1 = send_and_receive(import1, b"hello").await;
    assert_eq!(resp1, b"hello");

    let resp2 = send_and_receive(import2, b"test").await;
    assert!(resp2.starts_with(b"count:"));

    shutdown_token.cancel();
}

/// Test three exports simultaneously (echo, counter, reverse).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mixed_export_import() {
    let _ = tracing_subscriber::fmt::try_init();

    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());

    let echo_addr = start_echo_server().await;
    let counter_addr = start_counter_server().await;
    let reverse_addr = start_reverse_server().await;

    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let svc_echo = unique_service_name("mixed_echo");
    let svc_counter = unique_service_name("mixed_count");
    let svc_reverse = unique_service_name("mixed_rev");

    let import_echo = spawn_bridge_pair(
        &svc_echo,
        echo_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    let import_counter = spawn_bridge_pair(
        &svc_counter,
        counter_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    let import_reverse = spawn_bridge_pair(
        &svc_reverse,
        reverse_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    let resp = send_and_receive(import_echo, b"hello").await;
    assert_eq!(resp, b"hello");

    let resp = send_and_receive(import_counter, b"test").await;
    assert!(resp.starts_with(b"count:"));

    let resp = send_and_receive(import_reverse, b"ABCD").await;
    assert_eq!(resp, b"DCBA");

    shutdown_token.cancel();
}

/// Test that different services are isolated — messages don't cross.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_service_isolation() {
    let _ = tracing_subscriber::fmt::try_init();

    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());

    let echo_addr = start_echo_server().await;
    let reverse_addr = start_reverse_server().await;

    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let svc_echo = unique_service_name("iso_echo");
    let svc_reverse = unique_service_name("iso_rev");

    let import_echo = spawn_bridge_pair(
        &svc_echo,
        echo_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    let import_reverse = spawn_bridge_pair(
        &svc_reverse,
        reverse_addr,
        session1.clone(),
        session2.clone(),
        &shutdown_token,
        &config,
    )
    .await;

    // Open persistent connections and send multiple messages on each.
    // This tests isolation without the per-connection liveliness delay.
    let echo_task = tokio::spawn(async move {
        let mut stream = tokio::net::TcpStream::connect(import_echo).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut buf = [0u8; 1024];
        for i in 0..5 {
            let msg = format!("echo{}", i);
            stream.write_all(msg.as_bytes()).await.unwrap();
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("Echo read timeout")
                .expect("Echo read error");
            assert_eq!(&buf[..n], msg.as_bytes(), "Echo service should echo back");
        }
    });

    let reverse_task = tokio::spawn(async move {
        let mut stream = tokio::net::TcpStream::connect(import_reverse).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut buf = [0u8; 1024];
        for i in 0..5 {
            let msg = format!("rev{}", i);
            stream.write_all(msg.as_bytes()).await.unwrap();
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("Reverse read timeout")
                .expect("Reverse read error");
            let expected: Vec<u8> = msg.bytes().rev().collect();
            assert_eq!(&buf[..n], expected, "Reverse service should reverse bytes");
        }
    });

    tokio::time::timeout(Duration::from_secs(30), echo_task)
        .await
        .expect("Echo task timeout")
        .expect("Echo task panicked");
    tokio::time::timeout(Duration::from_secs(30), reverse_task)
        .await
        .expect("Reverse task timeout")
        .expect("Reverse task panicked");

    shutdown_token.cancel();
}
