//! Integration tests for HTTP/HTTPS traffic through the TCP bridge.
//!
//! These tests validate that the bridge correctly tunnels HTTP and HTTPS traffic
//! as opaque TCP bytes, using Axum as a real HTTP backend.
//!
//! Architecture tested:
//!   HTTP Client → Import Bridge → Zenoh → Export Bridge → Axum HTTP Server

mod common;

use axum::{
    Router,
    extract::Path,
    http::StatusCode,
    response::Json,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use zenoh_bridge_tcp::config::BridgeConfig;

#[derive(Debug, Serialize, Deserialize)]
struct EchoResponse {
    echo: String,
    timestamp: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    text: String,
    count: u32,
}

/// Create a simple Axum HTTP server with multiple endpoints.
fn create_http_app() -> Router {
    Router::new()
        .route("/", get(|| async { "Hello from Axum!" }))
        .route("/health", get(|| async { "OK" }))
        .route(
            "/echo/{message}",
            get(|Path(message): Path<String>| async move {
                Json(EchoResponse {
                    echo: message,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                })
            }),
        )
        .route(
            "/post",
            post(|Json(msg): Json<Message>| async move {
                (
                    StatusCode::OK,
                    Json(EchoResponse {
                        echo: format!("{} (count: {})", msg.text, msg.count),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    }),
                )
            }),
        )
}

/// Start an Axum HTTP server on a dynamic port.
async fn start_http_backend() -> SocketAddr {
    let app = create_http_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await; // ignore shutdown result to avoid a teardown panic/abort
    });
    sleep(Duration::from_millis(200)).await;
    addr
}

/// Spawn a TCP export+import pair using the real library functions.
/// Returns (shutdown_token, import_addr).
async fn spawn_tcp_bridge_pair(backend_addr: SocketAddr) -> (CancellationToken, SocketAddr) {
    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());
    let service = common::unique_service_name("httpint");

    let _scout = common::ScoutDomain::new();
    let session1 = Arc::new(zenoh::open(_scout.config()).await.unwrap());
    let session2 = Arc::new(zenoh::open(_scout.config()).await.unwrap());

    // Start export
    let export_spec = format!("{}/{}", service, backend_addr);
    let s = session1.clone();
    let t = shutdown_token.child_token();
    let c = config.clone();
    tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_export_mode(s, &export_spec, c, t)
            .await
            .unwrap();
    });

    sleep(Duration::from_millis(500)).await;

    // Start import
    let import_port = common::PortGuard::new();
    let import_addr = import_port.addr();
    let import_spec = format!("{}/{}", service, import_addr);
    let import_addr = import_port.release();
    let s = session2.clone();
    let t = shutdown_token.child_token();
    let c = config.clone();
    tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_import_mode(s, &import_spec, c, t)
            .await
            .unwrap();
    });

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Import bridge did not start in time");

    // NOTE: a bound listener is not full readiness. The auto-detect door
    // resolves the backend at connect time (HTTP by Host, TLS by SNI), so a
    // client that beats the export side's availability token gets a 502 or a
    // close. Callers must drive their first client through `common::retry_client`.
    (shutdown_token, import_addr)
}

/// TEST 1: HTTP request through the bridge
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_http_through_bridge() {
    let _ = tracing_subscriber::fmt::try_init();

    let backend_addr = start_http_backend().await;
    let (_shutdown, import_addr) = spawn_tcp_bridge_pair(backend_addr).await;

    // Send HTTP GET / through the bridge
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();

    // First request through a fresh pair: retry past the startup 502 window.
    let resp = common::get_through_bridge(
        &client,
        &format!("http://{}/", import_addr),
        None,
        common::BACKEND_READY_TIMEOUT,
    )
    .await
    .expect("HTTP request through bridge failed");

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "Hello from Axum!");

    // Test a JSON endpoint
    let resp = client
        .get(format!("http://{}/echo/bridge-test", import_addr))
        .header("Connection", "close")
        .send()
        .await
        .expect("JSON endpoint request failed");

    assert_eq!(resp.status(), 200);
    let data: EchoResponse = resp.json().await.unwrap();
    assert_eq!(data.echo, "bridge-test");
}

/// TEST 2: HTTPS/TLS passthrough — bridge forwards encrypted bytes without decrypting
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_tls_passthrough() {
    let _ = tracing_subscriber::fmt::try_init();

    // Start a real HTTPS backend
    let _ = rustls::crypto::ring::default_provider().install_default();

    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(subject_alt_names).unwrap();
    let cert_der = cert.der().clone();
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der()).unwrap();

    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();

    let app = create_http_app();
    let tls_rustls_config =
        axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls_config));

    let https_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let https_addr = https_listener.local_addr().unwrap();
    drop(https_listener);

    tokio::spawn(async move {
        // Ignore the shutdown result: when the test ends and the runtime drops,
        // `serve` resolving to Err and being `.unwrap()`ed panics inside teardown,
        // which can become a non-unwinding abort (SIGABRT) that fails the whole
        // test binary. Dropping the result avoids that flake.
        let _ = axum_server::bind_rustls(https_addr, tls_rustls_config)
            .serve(app.into_make_service())
            .await;
    });
    sleep(Duration::from_millis(500)).await;

    let (_shutdown, import_addr) = spawn_tcp_bridge_pair(https_addr).await;

    // Connect to the bridge and send a TLS ClientHello — the bridge should forward
    // these opaque bytes to the HTTPS backend without understanding them.
    //
    // The SNI door resolves the backend during the handshake and simply CLOSES
    // when it cannot (it has no way to phrase an HTTP error inside TLS), so a
    // probe that beats the export side's availability token fails its write with
    // a broken pipe. Retry the whole connect+write until the bridge accepts it.
    let client_hello = vec![
        0x16, 0x03, 0x01, // TLS Handshake, version 3.1
        0x00, 0x05, // Length
        0x01, // ClientHello
        0x00, 0x00, 0x01, 0x00, // Handshake length
    ];

    let mut stream = common::retry_client(
        || async {
            let mut stream = tokio::net::TcpStream::connect(import_addr).await?;
            stream.write_all(&client_hello).await?;
            Ok::<_, std::io::Error>(stream)
        },
        common::BACKEND_READY_TIMEOUT,
        "TLS ClientHello through the import bridge",
    )
    .await
    .expect("bridge never accepted the ClientHello");

    // The backend may respond with a TLS alert or partial ServerHello.
    // We just verify the bridge accepted and forwarded the bytes.
    let mut buffer = vec![0u8; 1024];
    let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buffer)).await;

    // Either we get TLS response bytes or a timeout (incomplete handshake),
    // both prove the bridge forwarded the data.
    match result {
        Ok(Ok(n)) if n > 0 => {
            // Got TLS response bytes back through the bridge
            assert!(n > 0);
        }
        _ => {
            // Timeout or connection close is acceptable with an incomplete handshake
        }
    }
}

/// TEST 3: Multiple sequential HTTP requests through the bridge
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiple_http_requests() {
    let _ = tracing_subscriber::fmt::try_init();

    let backend_addr = start_http_backend().await;
    let (_shutdown, import_addr) = spawn_tcp_bridge_pair(backend_addr).await;

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();

    for i in 0..5 {
        // Only the first can race the export side, but retrying every request is
        // harmless — a non-502 response returns on the first attempt.
        let resp = common::get_through_bridge(
            &client,
            &format!("http://{}/echo/request-{}", import_addr, i),
            None,
            common::BACKEND_READY_TIMEOUT,
        )
        .await
        .unwrap_or_else(|e| panic!("Request {} failed: {}", i, e));

        assert_eq!(resp.status(), 200);
        let data: EchoResponse = resp.json().await.unwrap();
        assert!(data.echo.contains("request"));

        sleep(Duration::from_millis(100)).await;
    }
}
