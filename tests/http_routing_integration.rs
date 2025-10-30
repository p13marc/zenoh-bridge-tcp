//! Integration tests for HTTP routing feature
//!
//! This test suite validates DNS-based routing with multiple HTTP backends.
//! Tests the complete flow: HTTP client -> Import bridge -> Zenoh -> Export bridge -> Backend

use axum::{extract::Path as AxumPath, http::StatusCode, response::Json, routing::get, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use zenoh::config::Config;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Response {
    backend: String,
    path: String,
    timestamp: u64,
}

/// Create an HTTP server that identifies itself
fn create_http_server(backend_id: &str) -> Router {
    let backend_id = backend_id.to_string();
    Router::new()
        .route(
            "/",
            get({
                let backend_id = backend_id.clone();
                move || async move {
                    Json(Response {
                        backend: backend_id,
                        path: "/".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    })
                }
            }),
        )
        .route(
            "/api/:action",
            get({
                let backend_id = backend_id.clone();
                move |AxumPath(action): AxumPath<String>| async move {
                    Json(Response {
                        backend: backend_id,
                        path: format!("/api/{}", action),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    })
                }
            }),
        )
}

/// Start an HTTP server on the given address
async fn start_http_backend(addr: SocketAddr, backend_id: &str) {
    let app = create_http_server(backend_id);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("🔧 Backend '{}' listening on {}", backend_id, addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_http_routing_multiple_backends() {
    // Initialize tracing for debugging
    let _ = tracing_subscriber::fmt::try_init();

    println!("\n🧪 TEST: HTTP Routing with Multiple Backends");
    println!("===========================================");

    // Create Zenoh sessions
    let config1 = Config::default();
    let config2 = Config::default();

    let session1 = Arc::new(zenoh::open(config1).await.unwrap());
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    println!("✓ Zenoh sessions created");

    // Start HTTP backend servers
    let api_backend_addr: SocketAddr = "127.0.0.1:19001".parse().unwrap();
    let web_backend_addr: SocketAddr = "127.0.0.1:19002".parse().unwrap();

    start_http_backend(api_backend_addr, "api-backend").await;
    start_http_backend(web_backend_addr, "web-backend").await;

    println!("✓ HTTP backends started");

    // Start HTTP export bridges (one per DNS)
    let session1_clone = session1.clone();
    let export_api_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/api.example.com/127.0.0.1:19001",
        )
        .await
        .unwrap();
    });

    let session1_clone = session1.clone();
    let export_web_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/web.example.com/127.0.0.1:19002",
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;
    println!("✓ HTTP export bridges started");

    // Start HTTP import bridge (single listener for all DNS)
    let import_addr: SocketAddr = "127.0.0.1:18080".parse().unwrap();
    let session2_clone = session2.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;
    println!("✓ HTTP import bridge started on {}", import_addr);

    // Give everything time to settle
    sleep(Duration::from_secs(1)).await;

    // Test 1: Request to api.example.com should reach api-backend
    println!("\n📡 Test 1: Request to api.example.com");
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let response = client
        .get(format!("http://{}/", import_addr))
        .header("Host", "api.example.com")
        .header("Connection", "close")
        .send()
        .await
        .expect("Failed to send request to api.example.com");

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    println!("   Response: backend={}, path={}", body.backend, body.path);
    assert_eq!(body.backend, "api-backend");
    assert_eq!(body.path, "/");
    println!("   ✓ Routed to correct backend (api-backend)");

    // Test 2: Request to web.example.com should reach web-backend
    println!("\n📡 Test 2: Request to web.example.com");
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let response = client
        .get(format!("http://{}/", import_addr))
        .header("Host", "web.example.com")
        .header("Connection", "close")
        .send()
        .await
        .expect("Failed to send request to web.example.com");

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    println!("   Response: backend={}, path={}", body.backend, body.path);
    assert_eq!(body.backend, "web-backend");
    assert_eq!(body.path, "/");
    println!("   ✓ Routed to correct backend (web-backend)");

    // Test 3: Multiple requests to different backends using reqwest with separate clients
    println!("\n📡 Test 3: Multiple requests to different hosts");
    for _ in 0..3 {
        // Request to API - create new client to force new connection
        let client_api = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap();
        let response = client_api
            .get(format!("http://{}/api/test", import_addr))
            .header("Host", "api.example.com")
            .header("Connection", "close")
            .send()
            .await
            .unwrap();
        let body: Response = response.json().await.unwrap();
        assert_eq!(body.backend, "api-backend");
        assert_eq!(body.path, "/api/test");

        // Request to Web - create new client to force new connection
        let client_web = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap();
        let response = client_web
            .get(format!("http://{}/api/test", import_addr))
            .header("Host", "web.example.com")
            .header("Connection", "close")
            .send()
            .await
            .unwrap();
        let body: Response = response.json().await.unwrap();
        assert_eq!(body.backend, "web-backend");
        assert_eq!(body.path, "/api/test");
    }
    println!("   ✓ Multiple requests routed correctly");

    // Test 4: DNS normalization - uppercase host should work
    println!("\n📡 Test 4: DNS normalization (uppercase)");
    let client_norm = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let response = client_norm
        .get(format!("http://{}/", import_addr))
        .header("Host", "API.EXAMPLE.COM")
        .header("Connection", "close")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    assert_eq!(body.backend, "api-backend");
    println!("   ✓ Uppercase host normalized correctly");

    // Test 5: DNS normalization - port 80 should be stripped
    println!("\n📡 Test 5: DNS normalization (port 80)");
    let client_port = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let response = client_port
        .get(format!("http://{}/", import_addr))
        .header("Host", "api.example.com:80")
        .header("Connection", "close")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    assert_eq!(body.backend, "api-backend");
    println!("   ✓ Port 80 stripped correctly");

    // Test 6: Unknown DNS should return 502
    println!("\n📡 Test 6: Unknown DNS (should return 502)");
    let client_unknown = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let response = client_unknown
        .get(format!("http://{}/", import_addr))
        .header("Host", "unknown.example.com")
        .header("Connection", "close")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body_text = response.text().await.unwrap();
    assert!(body_text.contains("502 Bad Gateway"));
    assert!(body_text.contains("unknown.example.com"));
    println!("   ✓ Unknown DNS returned 502 Bad Gateway");

    // Test 7: Missing Host header should return 400
    println!("\n📡 Test 7: Missing Host header (should return 400)");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tcp_stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    // Send HTTP request without Host header
    tcp_stream
        .write_all(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    let mut response = String::new();
    tcp_stream.read_to_string(&mut response).await.unwrap();

    assert!(response.contains("400 Bad Request"));
    assert!(response.contains("Missing Host header"));
    println!("   ✓ Missing Host header returned 400 Bad Request");

    println!("\n✅ All HTTP routing tests passed!");

    // Cleanup
    export_api_task.abort();
    export_web_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_http_routing_concurrent_clients() {
    let _ = tracing_subscriber::fmt::try_init();

    println!("\n🧪 TEST: Concurrent HTTP Clients");
    println!("================================");

    // Setup
    let config1 = Config::default();
    let config2 = Config::default();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start backend
    let backend_addr: SocketAddr = "127.0.0.1:19003".parse().unwrap();
    start_http_backend(backend_addr, "concurrent-backend").await;

    // Start export
    let session1_clone = session1.clone();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/concurrent.example.com/127.0.0.1:19003",
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_addr: SocketAddr = "127.0.0.1:18081".parse().unwrap();
    let session2_clone = session2.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;
    println!("✓ Setup complete");

    // Spawn 10 concurrent clients
    println!("\n📡 Sending 10 concurrent requests...");
    let mut tasks = vec![];
    for i in 0..10 {
        let client = reqwest::Client::new();
        let task = tokio::spawn(async move {
            let response = client
                .get(format!("http://{}/api/request{}", import_addr, i))
                .header("Host", "concurrent.example.com")
                .send()
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            let body: Response = response.json().await.unwrap();
            assert_eq!(body.backend, "concurrent-backend");
            body
        });
        tasks.push(task);
    }

    // Wait for all requests to complete
    let results: Vec<Response> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(results.len(), 10);
    println!("   ✓ All 10 concurrent requests completed successfully");

    // Verify all went to the same backend
    for result in results {
        assert_eq!(result.backend, "concurrent-backend");
    }

    println!("\n✅ Concurrent client test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_http_routing_backend_becomes_available() {
    let _ = tracing_subscriber::fmt::try_init();

    println!("\n🧪 TEST: Backend Becomes Available After Import");
    println!("===============================================");

    let config1 = Config::default();
    let config2 = Config::default();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start import FIRST (backend doesn't exist yet)
    let import_addr: SocketAddr = "127.0.0.1:18082".parse().unwrap();
    let session2_clone = session2.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;
    println!("✓ Import bridge started (no backend yet)");

    // Try to connect - should get 502
    println!("\n📡 Test 1: Request before backend exists");
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{}/", import_addr))
        .header("Host", "delayed.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    println!("   ✓ Got 502 as expected (no backend)");

    // NOW start the backend and export
    let backend_addr: SocketAddr = "127.0.0.1:19004".parse().unwrap();
    start_http_backend(backend_addr, "delayed-backend").await;

    let session1_clone = session1.clone();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/delayed.example.com/127.0.0.1:19004",
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;
    println!("✓ Backend and export started");

    // Now request should succeed
    println!("\n📡 Test 2: Request after backend starts");
    let response = client
        .get(format!("http://{}/", import_addr))
        .header("Host", "delayed.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    assert_eq!(body.backend, "delayed-backend");
    println!("   ✓ Request succeeded after backend became available");

    println!("\n✅ Backend availability test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}
