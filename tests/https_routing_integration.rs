//! Integration tests for HTTPS/TLS routing with SNI-based routing
//!
//! This test suite validates DNS-based routing with HTTPS backends using SNI extraction.
//! Tests the complete flow: HTTPS client -> Import bridge -> Zenoh -> Export bridge -> HTTPS Backend

use axum::{extract::Path as AxumPath, http::StatusCode, response::Json, routing::get, Router};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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

/// Generate self-signed certificate for testing
fn generate_test_cert(domain: &str) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let subject_alt_names = vec![domain.to_string(), "localhost".to_string()];
    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(subject_alt_names).unwrap();

    let cert_der = cert.der().clone();
    let key_der = PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

    (vec![cert_der], key_der)
}

/// Create an HTTPS server that identifies itself
fn create_https_server(backend_id: &str) -> Router {
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

/// Start an HTTPS server on the given address with the specified domain
async fn start_https_backend(addr: SocketAddr, domain: &str, backend_id: &str) {
    // Initialize crypto provider for rustls
    let _ = rustls::crypto::ring::default_provider().install_default();

    let app = create_https_server(backend_id);
    let (certs, key) = generate_test_cert(domain);

    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();

    println!(
        "🔧 HTTPS Backend '{}' (domain: {}) listening on {}",
        backend_id, domain, addr
    );

    let tls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls_config));

    tokio::spawn(async move {
        axum_server::bind_rustls(addr, tls_config)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    sleep(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_https_routing_multiple_backends() {
    // Initialize tracing for debugging
    let _ = tracing_subscriber::fmt::try_init();

    println!("\n🧪 TEST: HTTPS Routing with SNI and Multiple Backends");
    println!("====================================================");

    // Create Zenoh sessions
    let config1 = Config::default();
    let config2 = Config::default();

    let session1 = Arc::new(zenoh::open(config1).await.unwrap());
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    println!("✓ Zenoh sessions created");

    // Start HTTPS backend servers with different domains
    let api_backend_addr: SocketAddr = "127.0.0.1:29001".parse().unwrap();
    let web_backend_addr: SocketAddr = "127.0.0.1:29002".parse().unwrap();

    start_https_backend(api_backend_addr, "api.secure.test", "api-backend").await;
    start_https_backend(web_backend_addr, "web.secure.test", "web-backend").await;

    println!("✓ HTTPS backends started");

    // Start HTTP export bridges (one per DNS)
    let session1_clone = session1.clone();
    let export_api_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "https-service/api.secure.test/127.0.0.1:29001",
        )
        .await
        .unwrap();
    });

    let session1_clone = session1.clone();
    let export_web_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "https-service/web.secure.test/127.0.0.1:29002",
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;
    println!("✓ HTTPS export bridges started");

    // Start HTTP import bridge (single listener for all DNS via SNI)
    let import_addr: SocketAddr = "127.0.0.1:28443".parse().unwrap();
    let session2_clone = session2.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("https-service/{}", import_addr),
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;
    println!("✓ HTTPS import bridge started on {}", import_addr);

    // Give everything time to settle
    sleep(Duration::from_secs(1)).await;

    // Create an HTTPS client that accepts any certificate and uses custom DNS resolution
    // Map the hostname to the import bridge IP so SNI is sent correctly
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(0)
        .resolve("api.secure.test", import_addr)
        .build()
        .unwrap();

    // Test 1: Request to api.secure.test should reach api-backend
    println!("\n📡 Test 1: HTTPS Request to api.secure.test");
    let response = client
        .get("https://api.secure.test/")
        .send()
        .await
        .expect("Failed to send request to api.secure.test");

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    println!("   Response: backend={}, path={}", body.backend, body.path);
    assert_eq!(body.backend, "api-backend");
    assert_eq!(body.path, "/");
    println!("   ✓ Routed to correct backend via SNI (api-backend)");

    // Test 2: Request to web.secure.test should reach web-backend
    println!("\n📡 Test 2: HTTPS Request to web.secure.test");
    let client2 = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(0)
        .resolve("web.secure.test", import_addr)
        .build()
        .unwrap();
    let response = client2
        .get("https://web.secure.test/")
        .send()
        .await
        .expect("Failed to send request to web.secure.test");

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    println!("   Response: backend={}, path={}", body.backend, body.path);
    assert_eq!(body.backend, "web-backend");
    assert_eq!(body.path, "/");
    println!("   ✓ Routed to correct backend via SNI (web-backend)");

    // Test 3: Multiple HTTPS requests to different backends
    println!("\n📡 Test 3: Multiple HTTPS requests to different hosts");
    for _ in 0..3 {
        // Request to API
        let client_api = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .pool_max_idle_per_host(0)
            .resolve("api.secure.test", import_addr)
            .build()
            .unwrap();
        let response = client_api
            .get("https://api.secure.test/api/test")
            .send()
            .await
            .unwrap();
        let body: Response = response.json().await.unwrap();
        assert_eq!(body.backend, "api-backend");
        assert_eq!(body.path, "/api/test");

        // Request to Web
        let client_web = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .pool_max_idle_per_host(0)
            .resolve("web.secure.test", import_addr)
            .build()
            .unwrap();
        let response = client_web
            .get("https://web.secure.test/api/test")
            .send()
            .await
            .unwrap();
        let body: Response = response.json().await.unwrap();
        assert_eq!(body.backend, "web-backend");
        assert_eq!(body.path, "/api/test");
    }
    println!("   ✓ Multiple HTTPS requests routed correctly");

    // Test 4: DNS normalization with HTTPS
    println!("\n📡 Test 4: HTTPS DNS normalization (uppercase)");
    let client_norm = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(0)
        .resolve("API.SECURE.TEST", import_addr)
        .build()
        .unwrap();
    let response = client_norm
        .get("https://API.SECURE.TEST/")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    assert_eq!(body.backend, "api-backend");
    println!("   ✓ Uppercase SNI normalized correctly");

    // Test 5: DNS normalization - port 443 should be stripped
    println!("\n📡 Test 5: HTTPS DNS normalization (port 443)");
    let client_port = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(0)
        .resolve("api.secure.test", import_addr)
        .build()
        .unwrap();
    let response = client_port
        .get("https://api.secure.test:443/")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    assert_eq!(body.backend, "api-backend");
    println!("   ✓ Port 443 stripped correctly");

    println!("\n✅ All HTTPS routing tests passed!");

    // Cleanup
    export_api_task.abort();
    export_web_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_https_routing_concurrent_clients() {
    let _ = tracing_subscriber::fmt::try_init();

    println!("\n🧪 TEST: Concurrent HTTPS Clients with SNI");
    println!("==========================================");

    // Setup
    let config1 = Config::default();
    let config2 = Config::default();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start HTTPS backend
    let backend_addr: SocketAddr = "127.0.0.1:29003".parse().unwrap();
    start_https_backend(backend_addr, "concurrent.secure.test", "concurrent-backend").await;

    // Start export
    let session1_clone = session1.clone();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "https-service/concurrent.secure.test/127.0.0.1:29003",
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_addr: SocketAddr = "127.0.0.1:28444".parse().unwrap();
    let session2_clone = session2.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("https-service/{}", import_addr),
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;
    println!("✓ Setup complete");

    // Spawn 10 concurrent HTTPS clients
    println!("\n📡 Sending 10 concurrent HTTPS requests...");
    let mut tasks = vec![];
    for i in 0..10 {
        let task = tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .pool_max_idle_per_host(0)
                .resolve("concurrent.secure.test", import_addr)
                .build()
                .unwrap();
            let response = client
                .get(format!("https://concurrent.secure.test/api/request{}", i))
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
    println!("   ✓ All 10 concurrent HTTPS requests completed successfully");

    // Verify all went to the same backend
    for result in results {
        assert_eq!(result.backend, "concurrent-backend");
    }

    println!("\n✅ Concurrent HTTPS client test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_https_backend_becomes_available() {
    let _ = tracing_subscriber::fmt::try_init();

    println!("\n🧪 TEST: HTTPS Backend Becomes Available After Import");
    println!("=====================================================");

    let config1 = Config::default();
    let config2 = Config::default();
    let session1 = Arc::new(zenoh::open(config1).await.unwrap());
    let session2 = Arc::new(zenoh::open(config2).await.unwrap());

    // Start import FIRST (backend doesn't exist yet)
    let import_addr: SocketAddr = "127.0.0.1:28445".parse().unwrap();
    let session2_clone = session2.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_import_mode(
            session2_clone,
            &format!("https-service/{}", import_addr),
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_millis(500)).await;
    println!("✓ Import bridge started (no backend yet)");

    // Try to connect - should fail (connection refused or timeout)
    println!("\n📡 Test 1: HTTPS Request before backend exists");
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(2))
        .resolve("delayed.secure.test", import_addr)
        .build()
        .unwrap();

    let result = client.get("https://delayed.secure.test/").send().await;

    // Should fail because no backend is available
    assert!(result.is_err(), "Expected error when backend unavailable");
    println!("   ✓ Connection failed as expected (no backend)");

    // NOW start the backend and export
    let backend_addr: SocketAddr = "127.0.0.1:29004".parse().unwrap();
    start_https_backend(backend_addr, "delayed.secure.test", "delayed-backend").await;

    let session1_clone = session1.clone();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            "https-service/delayed.secure.test/127.0.0.1:29004",
        )
        .await
        .unwrap();
    });

    sleep(Duration::from_secs(1)).await;
    println!("✓ Backend and export started");

    // Now request should succeed
    println!("\n📡 Test 2: HTTPS Request after backend starts");
    let client2 = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(0)
        .resolve("delayed.secure.test", import_addr)
        .build()
        .unwrap();
    let response = client2
        .get("https://delayed.secure.test/")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Response = response.json().await.unwrap();
    assert_eq!(body.backend, "delayed-backend");
    println!("   ✓ Request succeeded after backend became available");

    println!("\n✅ HTTPS backend availability test passed!");

    // Cleanup
    export_task.abort();
    import_task.abort();
    drop(session1);
    drop(session2);
}
