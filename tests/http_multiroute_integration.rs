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

/// #63: a multiroute request moves the per-service byte + connection metrics.
///
/// The import runs in-process, so the global metrics registry is readable here;
/// nextest isolates each test in its own process, so the counters for this
/// (unique) service name are clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiroute_byte_metrics() {
    use std::sync::atomic::Ordering;

    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());
    let service = "mr-metrics";

    let backend_addr = start_backend("backend-m").await;
    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let s1 = session1.clone();
    let t1 = shutdown_token.child_token();
    let bc = config.clone();
    let export_task = tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(
            s1,
            &format!("{service}/host-m.test/{backend_addr}"),
            bc,
            t1,
        )
        .await
        .unwrap();
    });
    sleep(Duration::from_millis(500)).await;

    let import_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let import_addr = import_listener.local_addr().unwrap();
    drop(import_listener);
    let s2 = session2.clone();
    let t2 = shutdown_token.child_token();
    let bc = config.clone();
    let import_task = tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_multiroute_import_mode(
            s2,
            &format!("{service}/{import_addr}"),
            bc,
            t2,
        )
        .await
        .unwrap();
    });
    sleep(Duration::from_secs(1)).await;

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://{}/", import_addr))
        .header("Host", "host-m.test")
        .header("Connection", "close")
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();

    let c = zenoh_bridge_tcp::metrics::metrics().service(service);
    assert!(
        c.total.load(Ordering::Relaxed) >= 1,
        "connections_total should be >= 1"
    );
    assert!(
        c.bytes_up.load(Ordering::Relaxed) > 0,
        "bytes_up should count the forwarded request head"
    );
    assert!(
        c.bytes_down.load(Ordering::Relaxed) > 0,
        "bytes_down should count the relayed response"
    );

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

/// Backend with a GET `/` (so HEAD yields Content-Length + no body) and a POST
/// `/echo` that returns the exact request body.
async fn start_echo_backend() -> SocketAddr {
    use axum::body::Bytes;
    use axum::routing::post;
    let app = Router::new()
        .route("/", get(|| async { "hello-body-of-known-length" }))
        .route("/echo", post(|body: Bytes| async move { body }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    sleep(Duration::from_millis(200)).await;
    addr
}

async fn spawn_multiroute(
    dns: &str,
    backend_addr: SocketAddr,
    shutdown_token: &CancellationToken,
) -> SocketAddr {
    let config = Arc::new(BridgeConfig::default());
    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let s1 = session1.clone();
    let t1 = shutdown_token.child_token();
    let spec = format!("mr-echo/{}/{}", dns, backend_addr);
    let bc = config.clone();
    tokio::spawn(async move {
        zenoh_bridge_tcp::export::run_http_export_mode(s1, &spec, bc, t1)
            .await
            .unwrap();
    });
    sleep(Duration::from_millis(500)).await;

    let import_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let import_addr = import_listener.local_addr().unwrap();
    drop(import_listener);
    let s2 = session2.clone();
    let t2 = shutdown_token.child_token();
    let spec_import = format!("mr-echo/{}", import_addr);
    tokio::spawn(async move {
        zenoh_bridge_tcp::import::run_http_multiroute_import_mode(s2, &spec_import, config, t2)
            .await
            .unwrap();
    });
    sleep(Duration::from_secs(1)).await;
    import_addr
}

/// E1: a POST whose body arrives in a segment after the headers must be
/// forwarded in full (not truncated), and the echoed body must match.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiroute_post_with_delayed_body() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let backend_addr = start_echo_backend().await;
    let import_addr = spawn_multiroute("echo.test", backend_addr, &shutdown_token).await;

    let body = "A".repeat(20_000);
    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    let headers = format!(
        "POST /echo HTTP/1.1\r\nHost: echo.test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    // Delay so the bridge parses the headers before the body arrives — the
    // segmentation that used to truncate the body.
    sleep(Duration::from_millis(300)).await;
    stream.write_all(body.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut response = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(15), stream.read(&mut buf))
            .await
            .expect("timeout reading POST response")
            .unwrap();
        if n == 0 {
            break;
        }
        response.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.contains("200"),
        "expected 200, got: {}",
        &text[..text.len().min(120)]
    );
    let echoed = text.rsplit("\r\n\r\n").next().unwrap_or("");
    assert!(
        echoed.matches('A').count() >= body.len(),
        "echoed body truncated: got {} 'A's, expected {}",
        echoed.matches('A').count(),
        body.len()
    );

    shutdown_token.cancel();
}

/// E3: a HEAD response has Content-Length but no body; the bridge must complete
/// it promptly instead of blocking ~30s waiting for a body that never comes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiroute_head_completes_promptly() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let backend_addr = start_echo_backend().await;
    let import_addr = spawn_multiroute("head.test", backend_addr, &shutdown_token).await;

    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    stream
        .write_all(b"HEAD / HTTP/1.1\r\nHost: head.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let mut response = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match tokio::time::timeout(Duration::from_secs(12), stream.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
            Ok(Err(_)) => break,
        }
    }
    let elapsed = start.elapsed();
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.contains("200"),
        "expected 200 for HEAD, got: {}",
        &text[..text.len().min(120)]
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "HEAD response took {:?} — likely hung waiting for a body (E3)",
        elapsed
    );

    shutdown_token.cancel();
}

/// E2 (#27): two pipelined requests sent in a single write must both be routed
/// and answered, in order, on one connection. The previous framing loop dropped
/// bytes past the first request's body and could not locate the second request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiroute_pipelined_requests() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let config = Arc::new(BridgeConfig::default());

    let backend_a_addr = start_backend("backend-a").await;
    let backend_b_addr = start_backend("backend-b").await;

    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let service = "mr-pipeline";

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

    let mut stream = tokio::net::TcpStream::connect(import_addr).await.unwrap();

    // Both requests in ONE write — classic pipelining.
    let pipelined = "GET / HTTP/1.1\r\nHost: host-a.test\r\n\r\n\
                     GET / HTTP/1.1\r\nHost: host-b.test\r\n\r\n";
    stream.write_all(pipelined.as_bytes()).await.unwrap();

    // Read until both backends have answered (or time out).
    let mut response = Vec::new();
    let mut buf = vec![0u8; 8192];
    let collected = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let n = stream.read(&mut buf).await.expect("read error");
            if n == 0 {
                break;
            }
            response.extend_from_slice(&buf[..n]);
            let s = String::from_utf8_lossy(&response);
            if s.contains("backend-a") && s.contains("backend-b") {
                break;
            }
        }
    })
    .await;
    assert!(
        collected.is_ok(),
        "timed out waiting for pipelined responses"
    );

    let s = String::from_utf8_lossy(&response);
    // Two responses, both 200, and in request order (a before b).
    assert_eq!(
        s.matches("200 OK").count(),
        2,
        "expected two 200 responses, got: {}",
        s
    );
    let a = s.find("backend-a").expect("missing backend-a response");
    let b = s.find("backend-b").expect("missing backend-b response");
    assert!(
        a < b,
        "responses out of order (a at {}, b at {}): {}",
        a,
        b,
        s
    );

    drop(stream);
    shutdown_token.cancel();
    export_a.abort();
    export_b.abort();
    import_task.abort();
}
