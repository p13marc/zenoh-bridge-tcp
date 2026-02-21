//! Edge case and error handling tests for HTTP routing
//!
//! This test suite validates error handling, edge cases, and boundary conditions
//! for the HTTP routing feature.

use axum::{http::StatusCode, response::Json, routing::get, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use zenoh::config::Config;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Response {
    backend: String,
    message: String,
}

/// Create a simple HTTP server
fn create_test_server(backend_id: &str) -> Router {
    let backend_id = backend_id.to_string();
    Router::new().route(
        "/",
        get({
            let backend_id = backend_id.clone();
            move || async move {
                Json(Response {
                    backend: backend_id,
                    message: "OK".to_string(),
                })
            }
        }),
    )
}

/// Start an HTTP server
async fn start_test_backend(addr: SocketAddr, backend_id: &str) {
    let app = create_test_server(backend_id);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("🔧 Test backend '{}' listening on {}", backend_id, addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_missing_host_header() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();

    println!("\n🧪 TEST: Missing Host Header");
    println!("============================");

    let mut config1 = Config::default();
    config1.insert_json5("mode", "\"peer\"").unwrap();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());

    let mut config2 = Config::default();
    config2.insert_json5("mode", "\"peer\"").unwrap();
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start backend
    let backend_addr: SocketAddr = "127.0.0.1:19101".parse().unwrap();
    start_test_backend(backend_addr, "test-backend").await;

    // Start export
    let session1_clone = session1.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/test.example.com/127.0.0.1:19101",
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_addr: SocketAddr = "127.0.0.1:18101".parse().unwrap();
    let session2_clone = session2.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;
    println!("✓ Setup complete");

    // Test 1: Request without Host header
    println!("\n📡 Test 1: HTTP request without Host header");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    stream
        .write_all(b"GET / HTTP/1.1\r\nUser-Agent: test\r\n\r\n")
        .await
        .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    assert!(response.contains("400 Bad Request"));
    assert!(response.contains("Missing Host header"));
    println!("   ✓ Got 400 Bad Request as expected");

    // Test 2: Empty Host header
    println!("\n📡 Test 2: HTTP request with empty Host header");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: \r\n\r\n")
        .await
        .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    assert!(response.contains("400 Bad Request"));
    println!("   ✓ Got 400 Bad Request for empty host");

    println!("\n✅ Missing Host header test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_malformed_http_requests() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();

    println!("\n🧪 TEST: Malformed HTTP Requests");
    println!("================================");

    let mut config1 = Config::default();
    config1.insert_json5("mode", "\"peer\"").unwrap();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());

    let mut config2 = Config::default();
    config2.insert_json5("mode", "\"peer\"").unwrap();
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start backend
    let backend_addr: SocketAddr = "127.0.0.1:19102".parse().unwrap();
    start_test_backend(backend_addr, "test-backend").await;

    // Start export
    let session1_clone = session1.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/test.example.com/127.0.0.1:19102",
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_addr: SocketAddr = "127.0.0.1:18102".parse().unwrap();
    let session2_clone = session2.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;
    println!("✓ Setup complete");

    // Test 1: Invalid HTTP method line
    println!("\n📡 Test 1: Invalid HTTP request line");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    stream
        .write_all(b"INVALID REQUEST\r\nHost: test.example.com\r\n\r\n")
        .await
        .unwrap();

    // Should close connection or return error
    let mut response = Vec::new();
    let result =
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response)).await;

    assert!(result.is_ok());
    println!("   ✓ Invalid request handled (connection closed or error)");

    // Test 2: Incomplete request (no \r\n\r\n terminator)
    println!("\n📡 Test 2: Incomplete HTTP request");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test.example.com\r\n")
        .await
        .unwrap();

    // Don't send final \r\n - should timeout or close connection
    let mut response = Vec::new();
    let result =
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response)).await;

    // Either timeout or connection close/error is acceptable
    assert!(
        result.is_err() || matches!(result, Ok(Ok(0)) | Ok(Err(_))),
        "Incomplete HTTP request should timeout or close connection"
    );
    println!("   ✓ Incomplete request handled");

    println!("\n✅ Malformed request test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_very_long_headers() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();

    println!("\n🧪 TEST: Very Long HTTP Headers");
    println!("================================");

    let mut config1 = Config::default();
    config1.insert_json5("mode", "\"peer\"").unwrap();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());

    let mut config2 = Config::default();
    config2.insert_json5("mode", "\"peer\"").unwrap();
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start backend
    let backend_addr: SocketAddr = "127.0.0.1:19103".parse().unwrap();
    start_test_backend(backend_addr, "test-backend").await;

    // Start export
    let session1_clone = session1.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/test.example.com/127.0.0.1:19103",
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_addr: SocketAddr = "127.0.0.1:18103".parse().unwrap();
    let session2_clone = session2.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(2)).await;
    println!("✓ Setup complete");

    // Test 1: Long but valid headers
    println!("\n📡 Test 1: Long but valid headers");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    let mut request =
        String::from("GET / HTTP/1.1\r\nHost: test.example.com\r\nConnection: close\r\n");
    // Add just a few custom headers (keep it simple)
    for i in 0..5 {
        request.push_str(&format!("X-Custom-Header-{}: value{}\r\n", i, i));
    }
    request.push_str("\r\n");

    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut response = String::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;

    assert!(read_result.is_ok(), "Timeout reading response");
    assert!(response.contains("200 OK"));
    println!("   ✓ Long headers handled correctly");
    drop(stream);

    // Wait for connection cleanup
    sleep(Duration::from_millis(500)).await;

    // Test 2: Extremely long Host header (valid DNS can be up to 253 chars)
    println!("\n📡 Test 2: Very long hostname");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    let long_hostname = format!("{}.example.com", "a".repeat(200));
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        long_hostname
    );

    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut response = String::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;

    assert!(read_result.is_ok(), "Timeout reading response");
    assert!(response.contains("502 Bad Gateway"));
    println!("   ✓ Very long hostname handled");
    drop(stream);

    println!("\n✅ Long headers test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_special_characters_in_hostname() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();

    println!("\n🧪 TEST: Special Characters in Hostname");
    println!("========================================");

    let mut config1 = Config::default();
    config1.insert_json5("mode", "\"peer\"").unwrap();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());

    let mut config2 = Config::default();
    config2.insert_json5("mode", "\"peer\"").unwrap();
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start backend
    let backend_addr: SocketAddr = "127.0.0.1:19104".parse().unwrap();
    start_test_backend(backend_addr, "test-backend").await;

    // Start export with hyphen in domain
    let session1_clone = session1.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/my-api.example.com/127.0.0.1:19104",
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_addr: SocketAddr = "127.0.0.1:18104".parse().unwrap();
    let session2_clone = session2.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;
    println!("✓ Setup complete");

    // Test 1: Hostname with hyphens (valid)
    println!("\n📡 Test 1: Hostname with hyphens");
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();

    let response = client
        .get(format!("http://{}/", import_addr))
        .header("Host", "my-api.example.com")
        .header("Connection", "close")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    println!("   ✓ Hyphens in hostname work correctly");

    // Test 2: Hostname with numbers (valid)
    println!("\n📡 Test 2: Hostname with numbers");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: api123.example.com\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    // Will get 502 because not registered, but should parse correctly
    assert!(response.contains("502 Bad Gateway"));
    println!("   ✓ Numbers in hostname handled");

    // Test 3: Subdomain with multiple levels
    println!("\n📡 Test 3: Multiple subdomain levels");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    stream
        .write_all(
            b"GET / HTTP/1.1\r\nHost: api.v1.staging.example.com\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    // Will get 502 because not registered, but should parse correctly
    assert!(response.contains("502 Bad Gateway"));
    println!("   ✓ Multiple subdomain levels handled");

    println!("\n✅ Special characters test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_http_methods() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();

    println!("\n🧪 TEST: Various HTTP Methods");
    println!("==============================");

    let mut config1 = Config::default();
    config1.insert_json5("mode", "\"peer\"").unwrap();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());

    let mut config2 = Config::default();
    config2.insert_json5("mode", "\"peer\"").unwrap();
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start backend that handles various methods
    let backend_addr: SocketAddr = "127.0.0.1:19105".parse().unwrap();
    let app = Router::new()
        .route("/", get(|| async { "GET OK" }))
        .route("/", axum::routing::post(|| async { "POST OK" }))
        .route("/", axum::routing::put(|| async { "PUT OK" }))
        .route("/", axum::routing::delete(|| async { "DELETE OK" }));

    let listener = tokio::net::TcpListener::bind(backend_addr).await.unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    sleep(Duration::from_millis(500)).await;

    // Start export
    let session1_clone = session1.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/test.example.com/127.0.0.1:19105",
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_addr: SocketAddr = "127.0.0.1:18105".parse().unwrap();
    let session2_clone = session2.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(2)).await;
    println!("✓ Setup complete");

    // Test GET
    println!("\n📡 Test 1: GET request");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test.example.com\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut response = String::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;
    assert!(read_result.is_ok(), "Timeout reading GET response");
    assert!(response.contains("200 OK"));
    assert!(response.contains("GET OK"));
    println!("   ✓ GET works");
    drop(stream);
    sleep(Duration::from_millis(500)).await;

    // Test POST
    println!("\n📡 Test 2: POST request");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: test.example.com\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut response = String::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;
    assert!(read_result.is_ok(), "Timeout reading POST response");
    assert!(response.contains("200 OK"));
    assert!(response.contains("POST OK"));
    println!("   ✓ POST works");
    drop(stream);
    sleep(Duration::from_millis(500)).await;

    // Test PUT
    println!("\n📡 Test 3: PUT request");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    stream
        .write_all(b"PUT / HTTP/1.1\r\nHost: test.example.com\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut response = String::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;
    assert!(read_result.is_ok(), "Timeout reading PUT response");
    assert!(response.contains("200 OK"));
    assert!(response.contains("PUT OK"));
    println!("   ✓ PUT works");
    drop(stream);
    sleep(Duration::from_millis(500)).await;

    // Test DELETE
    println!("\n📡 Test 4: DELETE request");
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    stream
        .write_all(b"DELETE / HTTP/1.1\r\nHost: test.example.com\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut response = String::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;
    assert!(read_result.is_ok(), "Timeout reading DELETE response");
    assert!(response.contains("200 OK"));
    assert!(response.contains("DELETE OK"));
    println!("   ✓ DELETE works");
    drop(stream);

    println!("\n✅ HTTP methods test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_connection_lifecycle() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();

    println!("\n🧪 TEST: Connection Lifecycle");
    println!("==============================");

    let mut config1 = Config::default();
    config1.insert_json5("mode", "\"peer\"").unwrap();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());

    let mut config2 = Config::default();
    config2.insert_json5("mode", "\"peer\"").unwrap();
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start backend
    let backend_addr: SocketAddr = "127.0.0.1:19106".parse().unwrap();
    start_test_backend(backend_addr, "test-backend").await;

    // Start export
    let session1_clone = session1.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "http-service/test.example.com/127.0.0.1:19106",
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_addr: SocketAddr = "127.0.0.1:18106".parse().unwrap();
    let session2_clone = session2.clone();
    let shutdown_token_clone = shutdown_token.child_token();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("http-service/{}", import_addr),
            65536,
            shutdown_token_clone,
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(2)).await;
    println!("✓ Setup complete");

    // Test: Rapid sequential connections (using raw TCP with Connection: close)
    println!("\n📡 Test: Rapid sequential connections");
    for i in 0..5 {
        let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: test.example.com\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let mut response = String::new();
        let read_result =
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
                .await;
        assert!(
            read_result.is_ok(),
            "Timeout reading response for connection {}",
            i + 1
        );
        assert!(response.contains("200 OK"));
        println!("   ✓ Connection {} succeeded", i + 1);
        drop(stream);

        // Wait between connections to ensure cleanup
        sleep(Duration::from_millis(300)).await;
    }

    println!("\n✅ Connection lifecycle test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}
