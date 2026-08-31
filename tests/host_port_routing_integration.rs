//! Host-header port handling (0.10 routing-key overhaul).
//!
//! A browser always puts a non-default port into `Host` (`api.local:8080` when
//! the listener is on 8080), while the backend registers the bare
//! `--backend 'svc@api.local/…'`. Until 0.10 only `:80`/`:443` were collapsed,
//! so exactly the browser case 502'd while curl (port-less Host) and HTTPS
//! (SNI carries no port) worked. Routing keys are host-only now.
//!
//! Also pins the injection hardening that shipped with it: a client-supplied
//! Host is validated before it can reach Zenoh keyexpr machinery, so `Host: *`
//! (a live keyexpr wildcard that would match every backend's liveliness token)
//! and `Host: a/b` (key-segment injection) answer 400 instead of routing.

mod common;

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Minimal HTTP backend: answers any request with a recognizable 200 and closes.
async fn start_http_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    common::start_probe_immune_backend(|mut stream: TcpStream, _first: Vec<u8>| async move {
        let body = "hello-from-backend";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    })
    .await
}

/// One-shot raw HTTP exchange (no retry): returns whatever the bridge answers.
async fn raw_http_once(addr: SocketAddr, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request).await.expect("write");
    let mut response = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
        .await
        .expect("bridge answered nothing within 5s");
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_with_port_routes_to_bare_host_backend() {
    let _ = tracing_subscriber::fmt::try_init();
    let service = common::unique_service_name("hostport");
    let (backend_addr, _backend) = start_http_backend().await;

    // Backend registers the bare name; the client's Host carries the
    // listener's (arbitrary, non-default) port — the browser case.
    let mut pair = common::BridgePair::http(&service, "api.local", backend_addr).await;
    let port = pair.import_addr.port();

    let request = format!("GET / HTTP/1.1\r\nHost: api.local:{port}\r\nConnection: close\r\n\r\n");
    let response = common::raw_http_until_served(
        pair.import_addr,
        request.as_bytes(),
        Duration::from_secs(30),
    )
    .await
    .expect("port-carrying Host must route to the bare-host backend");
    assert!(
        response.contains("200 OK") && response.contains("hello-from-backend"),
        "expected the @api.local backend to serve Host: api.local:{port}, got:\n{response}"
    );

    // The port-less spelling keeps working, of course.
    let request = "GET / HTTP/1.1\r\nHost: api.local\r\nConnection: close\r\n\r\n";
    let response = raw_http_once(pair.import_addr, request.as_bytes()).await;
    assert!(
        response.contains("200 OK"),
        "port-less Host broke:\n{response}"
    );

    pair.kill_and_wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_with_port_routes_on_request_routed_listener() {
    let _ = tracing_subscriber::fmt::try_init();
    let service = common::unique_service_name("hostportrr");
    let (backend_addr, _backend) = start_http_backend().await;

    // Same as above but through the per-request routing door.
    let export_spec = format!("{}@api.local/{}", service, backend_addr);
    let mut export = common::BridgeProcess::new(&["--backend", &export_spec]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.addr();
    let import_spec = format!("{}/{},route=request", service, import_addr);
    let import_addr = import_port.release();
    let mut import = common::BridgeProcess::new(&["--listen", &import_spec]).await;
    common::wait_for_port(import_addr, Duration::from_secs(10))
        .await
        .expect("import bridge did not start");

    let port = import_addr.port();
    let request = format!("GET / HTTP/1.1\r\nHost: api.local:{port}\r\nConnection: close\r\n\r\n");
    let response =
        common::raw_http_until_served(import_addr, request.as_bytes(), Duration::from_secs(30))
            .await
            .expect("route=request must strip the Host port too");
    assert!(
        response.contains("200 OK"),
        "expected 200 via route=request with a port-carrying Host, got:\n{response}"
    );

    import.kill_and_wait().await;
    export.kill_and_wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metacharacter_hosts_answer_400_not_route() {
    let _ = tracing_subscriber::fmt::try_init();
    let service = common::unique_service_name("hostmeta");
    let (backend_addr, _backend) = start_http_backend().await;

    let mut pair = common::BridgePair::http(&service, "api.local", backend_addr).await;

    // Prove the pair is fully wired first, so a 400 below can only mean
    // rejection, not "backend not announced yet".
    let request = "GET / HTTP/1.1\r\nHost: api.local\r\nConnection: close\r\n\r\n";
    common::raw_http_until_served(
        pair.import_addr,
        request.as_bytes(),
        Duration::from_secs(30),
    )
    .await
    .expect("pair must serve a legitimate Host before the hostile cases");

    // `*` would probe `svc/*/available` — a liveliness wildcard matching ANY
    // backend token, including the live @api.local one; `a/b` would inject a
    // key segment. Both must die at validation with a 400.
    for hostile in ["*", "a/b", "api.local/../other", "a b"] {
        let request = format!("GET / HTTP/1.1\r\nHost: {hostile}\r\nConnection: close\r\n\r\n");
        let response = raw_http_once(pair.import_addr, request.as_bytes()).await;
        assert!(
            response.contains("400"),
            "Host: {hostile:?} must answer 400, got:\n{response}"
        );
        assert!(
            !response.contains("hello-from-backend"),
            "Host: {hostile:?} reached a backend!"
        );
    }

    pair.kill_and_wait().await;
}
