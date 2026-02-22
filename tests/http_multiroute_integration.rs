//! Integration tests for HTTP multi-route import mode.
//!
//! Tests per-request Host-header routing where a single persistent TCP
//! connection can reach different backends based on Host header per request.

use axum::{Router, response::Json, routing::get};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use zenoh::config::Config;
use zenoh_bridge_tcp::config::BridgeConfig;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Response {
    backend: String,
}

/// Create a simple HTTP server that identifies itself.
fn create_backend(backend_id: &str) -> Router {
    let backend_id = backend_id.to_string();
    Router::new().route(
        "/",
        get(move || {
            let id = backend_id.clone();
            async move { Json(Response { backend: id }) }
        }),
    )
}

/// Start an HTTP backend on a random port, returning its address.
async fn start_backend(backend_id: &str) -> SocketAddr {
    let app = create_backend(backend_id);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    sleep(Duration::from_millis(200)).await;
    addr
}

/// Send a raw HTTP/1.1 request on an existing TCP stream and read the response.
/// Returns the full response as a string.
async fn http_request(stream: &mut tokio::net::TcpStream, host: &str) -> String {
    let request = format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", host);
    stream.write_all(request.as_bytes()).await.unwrap();

    // Read response -- we expect small JSON responses, read in a loop until we
    // have a complete HTTP response (headers + body per Content-Length).
    let mut buf = vec![0u8; 8192];
    let mut response = Vec::new();

    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
            .await
            .expect("Timeout reading response")
            .expect("Read error");

        if n == 0 {
            break;
        }
        response.extend_from_slice(&buf[..n]);

        // Check if we have a complete response (headers + full body)
        let resp_str = String::from_utf8_lossy(&response);
        if let Some(header_end) = resp_str.find("\r\n\r\n") {
            let headers = &resp_str[..header_end];
            if let Some(cl_line) = headers
                .lines()
                .find(|l| l.to_lowercase().starts_with("content-length:"))
            {
                let cl: usize = cl_line.split(':').nth(1).unwrap().trim().parse().unwrap();
                let body_start = header_end + 4;
                if response.len() >= body_start + cl {
                    break;
                }
            }
        }
    }

    String::from_utf8_lossy(&response).to_string()
}

/// Test that a single HTTP request is correctly routed to the right backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiroute_single_request() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());

    // Start backend
    let backend_addr = start_backend("backend-a").await;

    // Open Zenoh sessions
    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    // Start HTTP export for host-a.test
    let s1 = session1.clone();
    let t1 = shutdown_token.child_token();
    let bridge_config = config.clone();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            s1,
            &format!("mr-test/host-a.test/{}", backend_addr),
            bridge_config,
            t1,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start multiroute import
    let import_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let import_addr = import_listener.local_addr().unwrap();
    drop(import_listener);

    let s2 = session2.clone();
    let t2 = shutdown_token.child_token();
    let bridge_config = config.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_multiroute_import_mode(
            s2,
            &format!("mr-test/{}", import_addr),
            bridge_config,
            t2,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;

    // Send a request
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://{}/", import_addr))
        .header("Host", "host-a.test")
        .header("Connection", "close")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(resp.status(), 200);
    let body: Response = resp.json().await.unwrap();
    assert_eq!(body.backend, "backend-a");

    // Cleanup
    shutdown_token.cancel();
    export_task.abort();
    import_task.abort();
}

/// Test that multiple requests on a persistent connection can route to different backends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiroute_persistent_connection_switches_hosts() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());

    // Start two backends
    let backend_a_addr = start_backend("backend-a").await;
    let backend_b_addr = start_backend("backend-b").await;

    // Open Zenoh sessions
    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let service = "mr-switch";

    // Start HTTP exports for two hosts
    let s1 = session1.clone();
    let t1 = shutdown_token.child_token();
    let spec_a = format!("{}/host-a.test/{}", service, backend_a_addr);
    let bridge_config = config.clone();
    let export_a = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(s1, &spec_a, bridge_config, t1)
            .await
            .unwrap();
    });

    let s1 = session1.clone();
    let t1 = shutdown_token.child_token();
    let spec_b = format!("{}/host-b.test/{}", service, backend_b_addr);
    let bridge_config = config.clone();
    let export_b = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(s1, &spec_b, bridge_config, t1)
            .await
            .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start multiroute import
    let import_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let import_addr = import_listener.local_addr().unwrap();
    drop(import_listener);

    let s2 = session2.clone();
    let t2 = shutdown_token.child_token();
    let spec_import = format!("{}/{}", service, import_addr);
    let bridge_config = config.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_multiroute_import_mode(
            s2,
            &spec_import,
            bridge_config,
            t2,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;

    // Open a raw TCP connection (persistent HTTP/1.1)
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    // Request 1: host-a.test -> backend-a
    let resp1 = http_request(&mut stream, "host-a.test").await;
    assert!(resp1.contains("200 OK"), "Expected 200, got: {}", resp1);
    assert!(
        resp1.contains("backend-a"),
        "Expected backend-a, got: {}",
        resp1
    );

    // Request 2: host-b.test -> backend-b (same TCP connection!)
    let resp2 = http_request(&mut stream, "host-b.test").await;
    assert!(resp2.contains("200 OK"), "Expected 200, got: {}", resp2);
    assert!(
        resp2.contains("backend-b"),
        "Expected backend-b, got: {}",
        resp2
    );

    // Request 3: back to host-a.test
    let resp3 = http_request(&mut stream, "host-a.test").await;
    assert!(resp3.contains("200 OK"), "Expected 200, got: {}", resp3);
    assert!(
        resp3.contains("backend-a"),
        "Expected backend-a, got: {}",
        resp3
    );

    drop(stream);

    // Cleanup
    shutdown_token.cancel();
    export_a.abort();
    export_b.abort();
    import_task.abort();
}

/// Test that a request to an unavailable host returns 502 but doesn't kill the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiroute_unavailable_host_returns_502() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());

    // Start one backend only (for host-a)
    let backend_addr = start_backend("backend-a").await;

    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let service = "mr-502";

    // Export only host-a
    let s1 = session1.clone();
    let t1 = shutdown_token.child_token();
    let spec = format!("{}/host-a.test/{}", service, backend_addr);
    let bridge_config = config.clone();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(s1, &spec, bridge_config, t1)
            .await
            .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Multiroute import
    let import_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let import_addr = import_listener.local_addr().unwrap();
    drop(import_listener);

    let s2 = session2.clone();
    let t2 = shutdown_token.child_token();
    let spec_import = format!("{}/{}", service, import_addr);
    let bridge_config = config.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_multiroute_import_mode(
            s2,
            &spec_import,
            bridge_config,
            t2,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;

    // Open persistent connection
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    // Request to nonexistent host -> should get 502
    let resp1 = http_request(&mut stream, "nonexistent.test").await;
    assert!(
        resp1.starts_with("HTTP/1.1 502"),
        "Expected HTTP/1.1 502 status line, got: {}",
        resp1
    );

    // Same connection: request to available host -> should work
    let resp2 = http_request(&mut stream, "host-a.test").await;
    assert!(resp2.contains("200 OK"), "Expected 200, got: {}", resp2);
    assert!(
        resp2.contains("backend-a"),
        "Expected backend-a, got: {}",
        resp2
    );

    drop(stream);

    // Cleanup
    shutdown_token.cancel();
    export_task.abort();
    import_task.abort();
}
