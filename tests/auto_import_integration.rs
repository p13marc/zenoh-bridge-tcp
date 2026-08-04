mod common;

use anyhow::Result;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;

/// Test that --auto-import can handle raw TCP connections (auto-detected as RawTcp).
///
/// This is the most fundamental auto-import test: raw TCP bytes don't match
/// TLS or HTTP patterns, so they get dispatched to the raw TCP handler.
#[tokio::test]
async fn test_auto_import_raw_tcp() -> Result<()> {
    println!("\n=== Test: Auto-Import Raw TCP ===\n");

    // Start a backend echo server
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. Backend echo server listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        let (mut stream, _) = backend_listener.accept().await.unwrap();
        println!("2. Backend: Connection accepted");

        let mut buffer = vec![0u8; 1024];
        if let Ok(n) = stream.read(&mut buffer).await
            && n > 0
        {
            println!("3. Backend: Received {} bytes, echoing back", n);
            let _ = stream.write_all(&buffer[..n]).await;
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start export bridge
    let service = common::unique_service_name("autotest");
    let export_spec = format!("{}/{}", service, backend_addr);
    println!("4. Starting export bridge: --export '{}'", export_spec);

    let mut export_bridge = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--backend", &export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start auto-import bridge (instead of --import, use --auto-import)
    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("{}/{}", service, import_addr);
    println!(
        "5. Starting auto-import bridge: --auto-import '{}'",
        import_spec
    );

    let mut import_bridge = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--listen", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Auto-import bridge did not start in time");
    println!("6. Auto-import bridge started");

    // Connect a raw TCP client (binary data, not HTTP or TLS)
    println!("7. Client: Connecting with raw TCP data...");
    let mut client = TcpStream::connect(import_addr).await?;
    println!("8. Client: Connected");

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send raw binary data (not HTTP, not TLS)
    let message = b"\x00\x01\x02 Hello raw TCP!\n";
    client.write_all(message).await?;
    println!("9. Client: Sent raw TCP message");

    // Read echo response
    let mut buf = vec![0u8; 1024];
    match timeout(Duration::from_secs(3), client.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            println!("10. Client: Received {} bytes", n);
            assert_eq!(
                &buf[..n],
                message,
                "Echo response should match sent message"
            );
            println!("11. TEST PASSED: Auto-import handled raw TCP correctly");
        }
        Ok(Ok(_)) => {
            panic!("Connection closed before receiving response");
        }
        Ok(Err(e)) => {
            panic!("Read error: {:?}", e);
        }
        Err(_) => {
            panic!("Timeout waiting for echo response");
        }
    }

    // Cleanup
    drop(client);
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("12. TEST COMPLETED\n");

    Ok(())
}

/// Test that --auto-import can handle HTTP requests (auto-detected as Http).
///
/// When the first bytes match an HTTP method, the auto-import handler
/// should route the connection using the HTTP Host header for DNS-based routing.
#[tokio::test]
async fn test_auto_import_http_detection() -> Result<()> {
    println!("\n=== Test: Auto-Import HTTP Detection ===\n");

    // Start an HTTP backend
    let backend_listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = backend_listener.local_addr()?;
    println!("1. HTTP backend listening on {}", backend_addr);

    let backend_task = tokio::spawn(async move {
        let (mut stream, _) = backend_listener.accept().await.unwrap();
        println!("2. Backend: Connection accepted");

        let mut buffer = vec![0u8; 4096];
        if let Ok(n) = stream.read(&mut buffer).await
            && n > 0
        {
            let request = String::from_utf8_lossy(&buffer[..n]);
            println!("3. Backend: Received HTTP request:\n{}", request);

            // Send HTTP response
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nHello, World!";
            let _ = stream.write_all(response.as_bytes()).await;
            println!("4. Backend: Sent HTTP response");
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start HTTP export bridge (backend registers for the DNS name)
    let service = common::unique_service_name("autohttp");
    let dns = "api.example.com";
    let http_export_spec = format!("{}@{}/{}", service, dns, backend_addr);
    println!(
        "5. Starting HTTP export bridge: --http-export '{}'",
        http_export_spec
    );

    let mut export_bridge = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--backend", &http_export_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start auto-import bridge
    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("{}/{}", service, import_addr);
    println!(
        "6. Starting auto-import bridge: --auto-import '{}'",
        import_spec
    );

    let mut import_bridge = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--listen", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("Auto-import bridge did not start in time");
    println!("7. Auto-import bridge started");

    // Send an HTTP request (auto-detected as HTTP)
    println!("8. Client: Connecting with HTTP request...");
    let mut client = TcpStream::connect(import_addr).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let http_request = format!(
        "GET /test HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        dns
    );
    client.write_all(http_request.as_bytes()).await?;
    println!("9. Client: Sent HTTP request");

    // Read HTTP response
    let mut buf = vec![0u8; 4096];
    match timeout(Duration::from_secs(5), client.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&buf[..n]);
            println!("10. Client: Received response:\n{}", response);
            assert!(
                response.contains("200 OK"),
                "Should receive 200 OK response"
            );
            assert!(
                response.contains("Hello, World!"),
                "Should receive body from backend"
            );
            println!("11. TEST PASSED: Auto-import handled HTTP correctly");
        }
        Ok(Ok(_)) => {
            panic!("Connection closed before receiving response");
        }
        Ok(Err(e)) => {
            panic!("Read error: {:?}", e);
        }
        Err(_) => {
            panic!("Timeout waiting for HTTP response");
        }
    }

    // Cleanup
    drop(client);
    let _ = export_bridge.kill().await;
    let _ = import_bridge.kill().await;
    let _ = timeout(Duration::from_millis(500), backend_task).await;
    println!("12. TEST COMPLETED\n");

    Ok(())
}

/// Test that --auto-import accepts the CLI flag correctly and starts listening.
#[tokio::test]
async fn test_auto_import_cli_starts() -> Result<()> {
    println!("\n=== Test: Auto-Import CLI Starts ===\n");

    let import_listener = TcpListener::bind("127.0.0.1:0").await?;
    let import_addr = import_listener.local_addr()?;
    drop(import_listener);

    let import_spec = format!("cliptest/{}", import_addr);
    println!(
        "1. Starting auto-import bridge: --auto-import '{}'",
        import_spec
    );

    let mut bridge = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--listen", &import_spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Wait for the bridge to start listening
    match common::wait_for_port(import_addr, Duration::from_secs(10)).await {
        Ok(()) => {
            println!("2. TEST PASSED: --auto-import bridge started and is listening");
        }
        Err(e) => {
            let _ = bridge.kill().await;
            panic!("Bridge did not start: {}", e);
        }
    }

    // Cleanup
    let _ = bridge.kill().await;
    println!("3. TEST COMPLETED\n");

    Ok(())
}

/// A plaintext prior-knowledge HTTP/2 backend answering every request 200
/// with a fixed body.
async fn start_h2c_backend() -> Result<std::net::SocketAddr> {
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(|_req| async {
                    Ok::<_, std::convert::Infallible>(hyper::Response::new(
                        http_body_util::Full::new(bytes::Bytes::from_static(b"h2c-ok")),
                    ))
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(addr)
}

/// #74: a plaintext prior-knowledge HTTP/2 client (h2c — the wire form of
/// plaintext gRPC) through the auto-detect door, routed by the first stream's
/// `:authority`, relayed to an h2c backend. No TLS anywhere; runs in the
/// default build.
#[tokio::test]
async fn test_auto_import_h2c_routes_by_authority() -> Result<()> {
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let backend_addr = start_h2c_backend().await?;
    let authority = "h2c.test";
    let service = common::unique_service_name("h2cauto");

    let export_spec = format!("{service}@{authority}/{backend_addr}");
    let _export = common::BridgeProcess::new(&["--backend", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let listen_spec = format!("{service}/{import_addr}");
    let _import = common::BridgeProcess::new(&["--listen", &listen_spec]).await;
    common::wait_for_port(import_addr, Duration::from_secs(10)).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Prior-knowledge h2 straight over TCP.
    let tcp = TcpStream::connect(import_addr).await?;
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = hyper::Request::builder()
        .uri(format!("http://{authority}/pkg.Svc/Method"))
        .body(Empty::<bytes::Bytes>::new())?;
    let resp = timeout(Duration::from_secs(10), sender.send_request(req)).await??;
    assert_eq!(resp.status(), 200, "h2c request should route and 200");
    let body = resp.into_body().collect().await?.to_bytes();
    assert_eq!(&body[..], b"h2c-ok");

    Ok(())
}

/// #74 + #62: a plaintext gRPC call's `grpc-status` trailer is surfaced on the
/// importer's /metrics — the response tap now runs in the default build.
#[tokio::test]
async fn test_auto_import_h2c_grpc_status_surfaced() -> Result<()> {
    use http_body_util::{Empty, StreamBody};
    use hyper::body::Frame;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    // Backend returns gRPC status 7 (PERMISSION_DENIED) in trailers.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(move |_req| async move {
                    let mut trailers = hyper::HeaderMap::new();
                    trailers.insert("grpc-status", hyper::header::HeaderValue::from_static("7"));
                    let frames: Vec<Result<Frame<bytes::Bytes>, std::convert::Infallible>> = vec![
                        Ok(Frame::data(bytes::Bytes::from_static(
                            b"\x00\x00\x00\x00\x00",
                        ))),
                        Ok(Frame::trailers(trailers)),
                    ];
                    let body = StreamBody::new(futures::stream::iter(frames));
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .header("content-type", "application/grpc")
                            .body(body)
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    let authority = "grpc-plain.test";
    let service = common::unique_service_name("h2cgrpc");

    let export_spec = format!("{service}@{authority}/{backend_addr}");
    let _export = common::BridgeProcess::new(&["--backend", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let metrics_port = common::PortGuard::new();
    let metrics_addr = metrics_port.release();
    let listen_spec = format!("{service}/{import_addr}");
    let _import = common::BridgeProcess::new(&[
        "--listen",
        &listen_spec,
        "--metrics-addr",
        &metrics_addr.to_string(),
    ])
    .await;
    common::wait_for_port(import_addr, Duration::from_secs(10)).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let tcp = TcpStream::connect(import_addr).await?;
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = hyper::Request::builder()
        .uri(format!("http://{authority}/pkg.Svc/Method"))
        .body(Empty::<bytes::Bytes>::new())?;
    let resp = timeout(Duration::from_secs(10), sender.send_request(req)).await??;
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body();

    let needle = format!("zbridge_grpc_status_total{{service=\"{service}\",code=\"7\"}} ");
    common::wait_for(
        || {
            let url = format!("http://{metrics_addr}/metrics");
            let needle = needle.clone();
            async move {
                match reqwest::get(url).await {
                    Ok(r) => r.text().await.map(|b| b.contains(&needle)).unwrap_or(false),
                    Err(_) => false,
                }
            }
        },
        Duration::from_secs(10),
        "plaintext grpc-status metric surfaced",
    )
    .await?;

    Ok(())
}
