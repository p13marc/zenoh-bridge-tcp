//! Integration tests for the default (plain) backend serving host-routed traffic.
//!
//! A `--backend 'svc/addr'` with no `@host` is the service's catch-all: it
//! registers `{service}/available` and listeners route to it any connection
//! whose hostname no `@host` backend claims. These tests pin the token
//! registration, the fallback on every listener plane, `@host` precedence,
//! and the fast-502 when no backend of any kind is announced.

mod common;

use common::{BridgePair, BridgeProcess, PortGuard, unique_service_name, wait_for_port};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use zenoh::config::Config;

/// A minimal looping HTTP/1.1 backend answering every request 200 with `body`.
async fn start_http_backend(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                if let Ok(n) = stream.read(&mut buf).await
                    && n > 0
                {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                }
            });
        }
    });
    addr
}

/// Send one HTTP/1.1 GET with the given Host and read until close.
async fn http_get(addr: SocketAddr, host: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    sleep(Duration::from_millis(300)).await;
    let req = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        match timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&response).to_string()
}

/// Collect the key expressions of all live tokens matching `key`.
async fn live_tokens(session: &zenoh::Session, key: &str) -> Vec<String> {
    let replies = session.liveliness().get(key).await.unwrap();
    let mut keys = Vec::new();
    while let Ok(Ok(reply)) =
        tokio::time::timeout(Duration::from_secs(2), replies.recv_async()).await
    {
        if let Ok(sample) = reply.into_result() {
            keys.push(sample.key_expr().as_str().to_string());
        }
    }
    keys
}

/// A plain backend declares the 2-segment default token `{service}/available`;
/// an `@host` backend declares only the 3-segment `{service}/{host}/available`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_plain_backend_declares_default_token() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let config = Arc::new(zenoh_bridge_tcp::config::BridgeConfig::default());

    let service = unique_service_name("defback_token");
    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    // Plain export — the backend address does not need to be reachable for
    // token declaration (connections are lazy).
    let export_spec = format!("{}/127.0.0.1:1", service);
    let session1_clone = session1.clone();
    let token_clone = shutdown_token.child_token();
    let config_clone = config.clone();
    let export_task = tokio::spawn(async move {
        let _ = zenoh_bridge_tcp::export::run_export_mode(
            session1_clone,
            &export_spec,
            config_clone,
            token_clone,
        )
        .await;
    });

    sleep(Duration::from_secs(1)).await;

    let default_key = format!("{}/available", service);
    let tokens = live_tokens(&session2, &default_key).await;
    assert_eq!(
        tokens,
        vec![default_key.clone()],
        "plain backend must declare the service-level default token"
    );

    // No host-scoped token should exist for a plain backend.
    let host_tokens = live_tokens(&session2, &format!("{}/*/available", service)).await;
    assert!(
        host_tokens.is_empty(),
        "plain backend must not declare host-scoped availability, got {host_tokens:?}"
    );

    shutdown_token.cancel();
    let _ = export_task.await;

    // An @host export declares only the 3-segment token.
    let service = unique_service_name("defback_token_host");
    let shutdown_token = CancellationToken::new();
    let export_spec = format!("{}/api.example.com/127.0.0.1:1", service);
    let session1_clone = session1.clone();
    let token_clone = shutdown_token.child_token();
    let config_clone = config.clone();
    let export_task = tokio::spawn(async move {
        let _ = zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            &export_spec,
            config_clone,
            token_clone,
        )
        .await;
    });

    sleep(Duration::from_secs(1)).await;

    let host_key = format!("{}/api.example.com/available", service);
    let tokens = live_tokens(&session2, &host_key).await;
    assert_eq!(
        tokens,
        vec![host_key],
        "@host backend must declare the host-scoped token"
    );
    let default_tokens = live_tokens(&session2, &format!("{}/available", service)).await;
    assert!(
        default_tokens.is_empty(),
        "@host backend must not declare the default token, got {default_tokens:?}"
    );

    shutdown_token.cancel();
    let _ = export_task.await;
}

/// The exact scenario from the field report: a plain `--backend web/…`, an
/// auto `--listen web/…`, and `curl http://127.0.0.1:PORT` — the Host header
/// names the listener itself, no `@host` backend exists anywhere, and the
/// request must still reach the default backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_auto_listener_plain_backend_http_200() {
    let _ = tracing_subscriber::fmt::try_init();

    let backend_addr = start_http_backend("default-backend-ok").await;
    let service = unique_service_name("defback_http");
    let mut pair = BridgePair::http_default(&service, backend_addr).await;

    // Liveliness propagation (import token -> export detect -> backend dial).
    sleep(Duration::from_secs(2)).await;

    // curl sends `Host: 127.0.0.1:{port}` — the listener's own address.
    let host = pair.import_addr.to_string();
    let response = http_get(pair.import_addr, &host).await;

    assert!(
        response.contains("200 OK"),
        "expected 200 through the default backend, got: {response}"
    );
    assert!(
        response.contains("default-backend-ok"),
        "expected the backend body, got: {response}"
    );

    pair.kill_and_wait().await;
}

/// An `@host` backend keeps precedence for its hostname; every other Host
/// falls back to the plain (default) backend of the same service.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_host_backend_wins_over_default() {
    let _ = tracing_subscriber::fmt::try_init();

    let host_backend_addr = start_http_backend("host-backend").await;
    let default_backend_addr = start_http_backend("default-backend").await;
    let service = unique_service_name("defback_prec");

    let host_spec = format!("{service}@host-a.test/{host_backend_addr}");
    let default_spec = format!("{service}/{default_backend_addr}");
    let _export = BridgeProcess::new(&[
        "--backend",
        &host_spec,
        "--backend",
        &default_spec,
    ])
    .await;
    sleep(Duration::from_millis(500)).await;

    let import_port = PortGuard::new();
    let import_addr = import_port.addr();
    let listen_spec = format!("{service}/{import_addr}");
    let import_addr = import_port.release();
    let _import = BridgeProcess::new(&["--listen", &listen_spec]).await;
    wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("import bridge did not start in time");
    sleep(Duration::from_secs(2)).await;

    let response = http_get(import_addr, "host-a.test").await;
    assert!(
        response.contains("host-backend"),
        "Host host-a.test must hit the @host backend, got: {response}"
    );

    let response = http_get(import_addr, "anything-else.test").await;
    assert!(
        response.contains("default-backend"),
        "an unclaimed Host must hit the default backend, got: {response}"
    );
}

/// With no backend of any kind announced, an HTTP request still fails fast
/// with a 502 — the default-backend probe must not turn the miss into a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_no_backend_still_fast_502() {
    let _ = tracing_subscriber::fmt::try_init();

    let service = unique_service_name("defback_none");
    let import_port = PortGuard::new();
    let import_addr = import_port.addr();
    let listen_spec = format!("{service}/{import_addr}");
    let import_addr = import_port.release();
    let _import = BridgeProcess::new(&["--listen", &listen_spec]).await;
    wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("import bridge did not start in time");
    sleep(Duration::from_secs(1)).await;

    let started = std::time::Instant::now();
    let response = http_get(import_addr, "nowhere.test").await;
    let elapsed = started.elapsed();

    assert!(
        response.contains("502 Bad Gateway"),
        "expected a 502 with no backends, got: {response}"
    );
    // http_get itself sleeps 300ms before sending; both availability probes
    // run concurrently and are bounded by one availability timeout (1s).
    assert!(
        elapsed < Duration::from_secs(5),
        "502 must be fast, took {elapsed:?}"
    );
}

/// TLS passthrough: an SNI no `@host` backend claims routes to the default
/// backend, which receives the ClientHello verbatim (zero-decrypt relay).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sni_passthrough_default_backend() {
    let _ = tracing_subscriber::fmt::try_init();

    // Byte-capture backend: records everything it receives.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let captured: Arc<tokio::sync::Mutex<Vec<u8>>> = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captured_clone = captured.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let captured = captured_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    captured.lock().await.extend_from_slice(&buf[..n]);
                }
            });
        }
    });

    let service = unique_service_name("defback_sni");
    let mut pair = BridgePair::http_default(&service, backend_addr).await;
    sleep(Duration::from_secs(2)).await;

    let hello = build_client_hello_with_sni("unclaimed.example.com");
    let mut stream = TcpStream::connect(pair.import_addr).await.unwrap();
    sleep(Duration::from_millis(300)).await;
    stream.write_all(&hello).await.unwrap();

    // The backend must receive the ClientHello bytes verbatim.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if *captured.lock().await == hello {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "backend did not receive the ClientHello; captured {} bytes",
            captured.lock().await.len()
        );
        sleep(Duration::from_millis(200)).await;
    }

    pair.kill_and_wait().await;
}

/// TLS passthrough negative: only an `@host` backend for a different hostname
/// exists — an unclaimed SNI closes without delivering anything anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sni_passthrough_unclaimed_closes() {
    let _ = tracing_subscriber::fmt::try_init();

    let service = unique_service_name("defback_sni_neg");
    let host_spec = format!("{service}@other.host.test/127.0.0.1:1");
    let _export = BridgeProcess::new(&["--backend", &host_spec]).await;
    sleep(Duration::from_millis(500)).await;

    let import_port = PortGuard::new();
    let import_addr = import_port.addr();
    let listen_spec = format!("{service}/{import_addr}");
    let import_addr = import_port.release();
    let _import = BridgeProcess::new(&["--listen", &listen_spec]).await;
    wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("import bridge did not start in time");
    sleep(Duration::from_secs(1)).await;

    let hello = build_client_hello_with_sni("unclaimed.example.com");
    let mut stream = TcpStream::connect(import_addr).await.unwrap();
    sleep(Duration::from_millis(300)).await;
    stream.write_all(&hello).await.unwrap();

    let mut buf = vec![0u8; 1024];
    match timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {} // closed, as intended
        Ok(Ok(n)) => panic!("expected close, got {n} bytes back"),
        Err(_) => panic!("expected a prompt close, connection hung"),
    }
}

/// h2c: a prior-knowledge HTTP/2 connection whose `:authority` no `@host`
/// backend claims relays to the default backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_h2c_default_backend() {
    use http_body_util::{BodyExt, Empty};
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let _ = tracing_subscriber::fmt::try_init();

    // Plaintext prior-knowledge h2 backend.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(|_req| async {
                    Ok::<_, std::convert::Infallible>(hyper::Response::new(
                        http_body_util::Full::new(bytes::Bytes::from_static(b"h2c-default-ok")),
                    ))
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    let service = unique_service_name("defback_h2c");
    let mut pair = BridgePair::http_default(&service, backend_addr).await;
    sleep(Duration::from_secs(2)).await;

    let tcp = TcpStream::connect(pair.import_addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = hyper::Request::builder()
        .uri("http://unclaimed.h2c.test/pkg.Svc/Method")
        .body(Empty::<bytes::Bytes>::new())
        .unwrap();
    let resp = timeout(Duration::from_secs(10), sender.send_request(req))
        .await
        .expect("h2c request timed out")
        .expect("h2c request failed");
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"h2c-default-ok");

    pair.kill_and_wait().await;
}

/// A WebSocket upgrade with no backend of any kind now fails fast with a 502
/// instead of proceeding blindly onto the bus and hanging.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_ws_no_backend_fast_502() {
    let _ = tracing_subscriber::fmt::try_init();

    let service = unique_service_name("defback_ws_none");
    let import_port = PortGuard::new();
    let import_addr = import_port.addr();
    let listen_spec = format!("{service}/{import_addr}");
    let import_addr = import_port.release();
    let _import = BridgeProcess::new(&["--listen", &listen_spec]).await;
    wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("import bridge did not start in time");
    sleep(Duration::from_secs(1)).await;

    let started = std::time::Instant::now();
    let result = timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(format!("ws://{import_addr}/")),
    )
    .await;
    let elapsed = started.elapsed();

    match result {
        Ok(Err(_)) => {} // handshake rejected, as intended
        Ok(Ok(_)) => panic!("WS handshake must not succeed with no backend"),
        Err(_) => panic!("WS handshake must fail fast, hung for {elapsed:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "WS rejection must be fast, took {elapsed:?}"
    );
}

/// Copy of the src-internal test builder: a minimal TLS 1.2 ClientHello
/// carrying one SNI extension.
fn build_client_hello_with_sni(hostname: &str) -> Vec<u8> {
    let name_bytes = hostname.as_bytes();

    let sni_ext_len = 2 + 1 + 2 + name_bytes.len();
    let ext_len = sni_ext_len;
    let sni_ext: Vec<u8> = [
        &[0x00, 0x00],                                  // Extension type: SNI
        &((ext_len as u16).to_be_bytes())[..],          // Extension data length
        &(((ext_len - 2) as u16).to_be_bytes())[..],    // SNI list length
        &[0x00],                                        // Name type: host_name
        &((name_bytes.len() as u16).to_be_bytes())[..], // Name length
        name_bytes,
    ]
    .concat();

    let extensions_total_len = sni_ext.len();

    let client_hello_body: Vec<u8> = [
        &[0x03, 0x03],     // TLS 1.2
        &[0x00u8; 32][..], // Random
        &[0x00],           // Session ID length: 0
        &[0x00, 0x02],     // Cipher suites length: 2
        &[0x00, 0xFF],     // Cipher suite: TLS_EMPTY_RENEGOTIATION_INFO_SCSV
        &[0x01, 0x00],     // Compression methods: 1, null
        &((extensions_total_len as u16).to_be_bytes())[..],
        &sni_ext,
    ]
    .concat();

    let hs_len = client_hello_body.len();
    let mut handshake = Vec::with_capacity(4 + hs_len);
    handshake.push(0x01); // ClientHello
    handshake.extend_from_slice(&[0x00, ((hs_len >> 8) & 0xFF) as u8, (hs_len & 0xFF) as u8]);
    handshake.extend_from_slice(&client_hello_body);

    let record_len = handshake.len();
    [
        &[0x16, 0x03, 0x01], // Handshake, TLS 1.0 (legacy record version)
        &((record_len as u16).to_be_bytes())[..],
        &handshake,
    ]
    .concat()
}
