//! Integration tests for HTTPS termination (TLS offloading)
//!
//! Architecture tested:
//!   HTTPS Client -> Import Bridge (TLS terminated) -> Zenoh -> Export Bridge -> Plaintext HTTP Backend
//!
//! Unlike HTTPS passthrough (where backend handles TLS), here the import bridge
//! terminates TLS and forwards plaintext HTTP over Zenoh.

#![cfg(feature = "tls-termination")]

mod common;

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

/// Test that --https-terminate without --tls-cert/--tls-key fails validation
#[tokio::test]
async fn test_https_terminate_requires_cert_and_key() {
    let child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args(["--https-terminate", "svc/0.0.0.0:8443"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn bridge");

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("Timeout waiting for bridge exit")
        .expect("Failed to wait for bridge");

    assert!(
        !output.status.success(),
        "Bridge should fail without --tls-cert and --tls-key"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--tls-cert") || stderr.contains("--tls-key"),
        "Error should mention missing TLS cert/key, got: {}",
        stderr
    );
}

/// Test that --https-terminate with cert but missing key fails
#[tokio::test]
async fn test_https_terminate_requires_key() {
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_cert_only_integ.pem");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();

    let child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--https-terminate",
            "svc/0.0.0.0:8443",
            "--tls-cert",
            cert_path.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn bridge");

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("Timeout waiting for bridge exit")
        .expect("Failed to wait for bridge");

    assert!(
        !output.status.success(),
        "Bridge should fail without --tls-key"
    );

    std::fs::remove_file(&cert_path).unwrap();
}

/// Test that --https-terminate with valid cert and key starts successfully
/// (We can't do a full end-to-end test without a running Zenoh network,
/// but we verify the binary accepts the arguments and starts)
#[tokio::test]
async fn test_https_terminate_starts_with_valid_tls() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_integ_cert.pem");
    let key_path = dir.join("test_integ_key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    // Use port 0 style - find a free port first
    let port_guard = common::PortGuard::new();
    let addr = port_guard.release();

    let spec = format!("tls-test/{}", addr);

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--https-terminate",
            &spec,
            "--tls-cert",
            cert_path.to_str().unwrap(),
            "--tls-key",
            key_path.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn bridge");

    // Give it time to start (it should not crash immediately)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Check it's still running (hasn't crashed)
    let try_wait = child.try_wait().expect("Failed to check process status");
    assert!(
        try_wait.is_none(),
        "Bridge should still be running after 2s with valid TLS config, but exited: {:?}",
        try_wait
    );

    // Clean up
    let _ = child.kill().await;
    std::fs::remove_file(&cert_path).unwrap();
    std::fs::remove_file(&key_path).unwrap();
}

/// A client cert verifier that accepts anything — the bridge uses a self-signed
/// test cert, and these tests care only about ALPN negotiation, not trust.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_config_with_alpn(alpn: &[&[u8]]) -> rustls::ClientConfig {
    let mut cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    cfg
}

/// #46/#50: the terminated listener advertises ALPN `h2` and `http/1.1`, h2
/// preferred. A client offering both negotiates `h2`; one offering only
/// `http/1.1` negotiates `http/1.1`; one offering only `h2` also succeeds
/// (Phase C routes terminated h2 by `:authority`).
#[tokio::test]
async fn test_https_terminate_alpn_negotiation() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_alpn_integ_cert.pem");
    let key_path = dir.join("test_alpn_integ_key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let port_guard = common::PortGuard::new();
    let addr = port_guard.release();
    let spec = format!("alpn-test/{}", addr);

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
        .args([
            "--https-terminate",
            &spec,
            "--tls-cert",
            cert_path.to_str().unwrap(),
            "--tls-key",
            key_path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to spawn bridge");

    common::wait_for_port(addr, Duration::from_secs(10))
        .await
        .expect("terminate listener did not start");

    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();

    let negotiate = |offer: &'static [&'static [u8]]| {
        let server_name = server_name.clone();
        async move {
            let connector =
                tokio_rustls::TlsConnector::from(Arc::new(client_config_with_alpn(offer)));
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            connector
                .connect(server_name, tcp)
                .await
                .map(|tls| tls.get_ref().1.alpn_protocol().map(|p| p.to_vec()))
        }
    };

    // Offering both -> h2 (server prefers it).
    assert_eq!(
        negotiate(&[b"h2", b"http/1.1"]).await.unwrap(),
        Some(b"h2".to_vec()),
        "bridge must prefer h2 when offered"
    );

    // Offering only http/1.1 -> http/1.1.
    assert_eq!(
        negotiate(&[b"http/1.1"]).await.unwrap(),
        Some(b"http/1.1".to_vec()),
        "bridge must negotiate http/1.1 when that is all the client offers"
    );

    // Offering only h2 -> h2 (Phase C: terminated h2 is now supported).
    assert_eq!(
        negotiate(&[b"h2"]).await.unwrap(),
        Some(b"h2".to_vec()),
        "bridge must negotiate h2 for an h2-only client"
    );

    let _ = child.kill().await;
    std::fs::remove_file(&cert_path).unwrap();
    std::fs::remove_file(&key_path).unwrap();
}

/// A plaintext, prior-knowledge HTTP/2 backend that answers every request `200`
/// with a fixed body. The bridge relays the decrypted h2 bytes to it verbatim.
async fn start_h2_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(|_req| async {
                    Ok::<_, std::convert::Infallible>(hyper::Response::new(
                        http_body_util::Full::new(bytes::Bytes::from_static(b"h2-ok")),
                    ))
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, handle)
}

/// End-to-end (#50): a real h2 client over TLS -> `--https-terminate` (routes by
/// `:authority`) -> Zenoh -> `--http-export` -> plaintext h2 backend -> response.
#[tokio::test]
async fn test_https_terminate_h2_end_to_end() {
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let _ = rustls::crypto::ring::default_provider().install_default();

    let (backend_addr, _backend) = start_h2_backend().await;
    let authority = "grpc.test";
    let service = common::unique_service_name("h2term");

    // Export the h2 backend for this authority.
    let export_spec = format!("{service}/{authority}/{backend_addr}");
    let _export = common::BridgeProcess::new(&["--http-export", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Cert + terminating import (advertises h2 via ALPN).
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_h2e2e_cert.pem");
    let key_path = dir.join("test_h2e2e_key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let import_spec = format!("{service}/{import_addr}");
    let _import = common::BridgeProcess::new(&[
        "--https-terminate",
        &import_spec,
        "--tls-cert",
        cert_path.to_str().unwrap(),
        "--tls-key",
        key_path.to_str().unwrap(),
    ])
    .await;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("terminate listener did not start");
    // Liveliness propagation: import declares -> export connects to backend.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // h2 client over TLS to the terminating listener.
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config_with_alpn(&[b"h2"])));
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tcp = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    let tls = connector.connect(server_name, tcp).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));

    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    // The request's :authority (from the URI) is the routing key.
    let req = hyper::Request::builder()
        .uri(format!("https://{authority}/pkg.Svc/Method"))
        .body(Empty::<bytes::Bytes>::new())
        .unwrap();

    let resp = tokio::time::timeout(Duration::from_secs(10), sender.send_request(req))
        .await
        .expect("h2 request timed out")
        .expect("h2 request failed");
    assert_eq!(
        resp.status(),
        200,
        "terminated h2 request should route and 200"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"h2-ok");

    std::fs::remove_file(&cert_path).unwrap();
    std::fs::remove_file(&key_path).unwrap();
}

/// A plaintext WebSocket echo backend: accepts the upgrade and echoes every
/// text/binary message back.
async fn start_ws_echo_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use futures::{SinkExt, StreamExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    while let Some(Ok(msg)) = ws.next().await {
                        if msg.is_text() || msg.is_binary() {
                            if ws.send(msg).await.is_err() {
                                break;
                            }
                        } else if msg.is_close() {
                            break;
                        }
                    }
                }
            });
        }
    });
    (addr, handle)
}

/// End-to-end (#71): a wss client -> `--https-terminate` (TLS ends at the bridge,
/// routes the upgrade request by Host) -> Zenoh -> `--http-export` -> plaintext
/// WebSocket backend. The upgrade and both frame types must round-trip — the
/// terminated relay carries the 101 and subsequent frames opaquely.
#[tokio::test]
async fn test_https_terminate_wss_end_to_end() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let _ = rustls::crypto::ring::default_provider().install_default();

    let (backend_addr, _backend) = start_ws_echo_backend().await;
    let host = "ws.test";
    let service = common::unique_service_name("wssterm");

    // Export the plaintext WS backend for this host.
    let export_spec = format!("{service}/{host}/{backend_addr}");
    let _export = common::BridgeProcess::new(&["--http-export", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Cert + terminating import.
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_wsse2e_cert.pem");
    let key_path = dir.join("test_wsse2e_key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let import_spec = format!("{service}/{import_addr}");
    let _import = common::BridgeProcess::new(&[
        "--https-terminate",
        &import_spec,
        "--tls-cert",
        cert_path.to_str().unwrap(),
        "--tls-key",
        key_path.to_str().unwrap(),
    ])
    .await;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("terminate listener did not start");
    // Liveliness propagation: import declares -> export connects to backend.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // TLS to the terminating listener (http/1.1 — the upgrade is an h1 request),
    // then the WebSocket handshake over the established TLS stream. The request
    // URL's host is the routing key the bridge reads from the decrypted Host.
    let connector =
        tokio_rustls::TlsConnector::from(Arc::new(client_config_with_alpn(&[b"http/1.1"])));
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tcp = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    let tls = connector.connect(server_name, tcp).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));

    let (mut ws, resp) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::client_async(format!("wss://{host}/echo"), tls),
    )
    .await
    .expect("wss handshake timed out")
    .expect("wss handshake failed");
    assert_eq!(
        resp.status(),
        101,
        "terminated wss upgrade should complete with 101"
    );

    ws.send(Message::Text("hello-wss".into())).await.unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("text echo timed out")
        .expect("stream closed before text echo")
        .expect("text echo errored");
    assert_eq!(echoed, Message::Text("hello-wss".into()));

    ws.send(Message::Binary(vec![1u8, 2, 3, 250].into()))
        .await
        .unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("binary echo timed out")
        .expect("stream closed before binary echo")
        .expect("binary echo errored");
    assert_eq!(echoed, Message::Binary(vec![1u8, 2, 3, 250].into()));

    let _ = ws.close(None).await;

    std::fs::remove_file(&cert_path).unwrap();
    std::fs::remove_file(&key_path).unwrap();
}

/// A plaintext h2 backend that answers with a gRPC-shaped response: HEADERS(200,
/// content-type application/grpc) + DATA + trailers carrying `grpc-status`.
async fn start_grpc_backend(
    grpc_status: &'static str,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use http_body_util::StreamBody;
    use hyper::body::Frame;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(move |_req| async move {
                    let mut trailers = hyper::HeaderMap::new();
                    trailers.insert(
                        "grpc-status",
                        hyper::header::HeaderValue::from_static(grpc_status),
                    );
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
    (addr, handle)
}

/// End-to-end (#62): a terminated gRPC call's `grpc-status` is surfaced on the
/// import bridge's `/metrics` (`zbridge_grpc_status_total{service,code}`).
#[tokio::test]
async fn test_https_terminate_grpc_status_surfaced() {
    use http_body_util::Empty;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let _ = rustls::crypto::ring::default_provider().install_default();

    // Backend returns gRPC status 5 (NOT_FOUND) in trailers.
    let (backend_addr, _backend) = start_grpc_backend("5").await;
    let authority = "grpc.test";
    let service = common::unique_service_name("grpcstatus");

    let export_spec = format!("{service}/{authority}/{backend_addr}");
    let _export = common::BridgeProcess::new(&["--http-export", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = std::env::temp_dir();
    let cert_path = dir.join("test_grpcstatus_cert.pem");
    let key_path = dir.join("test_grpcstatus_key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let metrics_port = common::PortGuard::new();
    let metrics_addr = metrics_port.release();
    let import_spec = format!("{service}/{import_addr}");
    let _import = common::BridgeProcess::new(&[
        "--https-terminate",
        &import_spec,
        "--tls-cert",
        cert_path.to_str().unwrap(),
        "--tls-key",
        key_path.to_str().unwrap(),
        "--metrics-addr",
        &metrics_addr.to_string(),
    ])
    .await;

    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("terminate listener did not start");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Make the terminated gRPC call.
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config_with_alpn(&[b"h2"])));
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tcp = tokio::net::TcpStream::connect(import_addr).await.unwrap();
    let tls = connector.connect(server_name, tcp).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = hyper::Request::builder()
        .uri(format!("https://{authority}/pkg.Svc/Method"))
        .body(Empty::<bytes::Bytes>::new())
        .unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(10), sender.send_request(req))
        .await
        .expect("gRPC request timed out")
        .expect("gRPC request failed");
    // gRPC keeps HTTP 200 even on error — the meaningful code is in trailers.
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body();

    // The tap records the status as the trailers pass through; poll /metrics.
    let needle = format!("zbridge_grpc_status_total{{service=\"{service}\",code=\"5\"}} ");
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
        "grpc-status metric surfaced",
    )
    .await
    .expect("terminated gRPC status 5 was not surfaced in /metrics");

    std::fs::remove_file(&cert_path).unwrap();
    std::fs::remove_file(&key_path).unwrap();
}
