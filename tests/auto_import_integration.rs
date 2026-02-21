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
        if let Ok(n) = stream.read(&mut buffer).await {
            if n > 0 {
                println!("3. Backend: Received {} bytes, echoing back", n);
                let _ = stream.write_all(&buffer[..n]).await;
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start export bridge
    let service = common::unique_service_name("autotest");
    let export_spec = format!("{}/{}", service, backend_addr);
    println!("4. Starting export bridge: --export '{}'", export_spec);

    let mut export_bridge = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--export", &export_spec])
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
        .args(["--auto-import", &import_spec])
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
                &buf[..n], message,
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
        if let Ok(n) = stream.read(&mut buffer).await {
            if n > 0 {
                let request = String::from_utf8_lossy(&buffer[..n]);
                println!("3. Backend: Received HTTP request:\n{}", request);

                // Send HTTP response
                let response = "HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nHello, World!";
                let _ = stream.write_all(response.as_bytes()).await;
                println!("4. Backend: Sent HTTP response");
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start HTTP export bridge (backend registers for the DNS name)
    let service = common::unique_service_name("autohttp");
    let dns = "api.example.com";
    let http_export_spec = format!("{}/{}/{}", service, dns, backend_addr);
    println!(
        "5. Starting HTTP export bridge: --http-export '{}'",
        http_export_spec
    );

    let mut export_bridge = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--http-export", &http_export_spec])
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
        .args(["--auto-import", &import_spec])
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
        .args(["--auto-import", &import_spec])
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
