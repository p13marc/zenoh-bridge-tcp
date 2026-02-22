//! Shared test utilities for integration tests.
//!
//! Provides helpers for:
//! - Dynamic port allocation (no more hardcoded ports)
//! - Bridge process management via assert_cmd
//! - Retry-based synchronization (no more sleep)
//! - Backend server helpers

#![allow(dead_code)]

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Allocate a free port by binding to port 0 and returning the assigned port.
/// The socket is kept alive until the returned guard is dropped.
pub struct PortGuard {
    addr: SocketAddr,
    _listener: Option<TcpListener>,
}

impl PortGuard {
    /// Allocate a port and keep it reserved.
    /// Drop the guard JUST before passing the port to the process that needs it.
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        Self {
            addr,
            _listener: Some(listener),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Release the port so another process can bind it.
    /// Returns the address for use.
    pub fn release(mut self) -> SocketAddr {
        self._listener = None;
        self.addr
    }
}

/// Wait for a TCP port to become connectable, with exponential backoff.
/// Returns Ok(()) when connection succeeds, Err after timeout.
pub async fn wait_for_port(addr: SocketAddr, timeout: Duration) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let mut delay = Duration::from_millis(50);

    while start.elapsed() < timeout {
        match TcpStream::connect(addr).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(500));
            }
        }
    }

    Err(anyhow::anyhow!(
        "Port {} did not become available within {:?}",
        addr,
        timeout
    ))
}

/// Wait for a condition to become true, with exponential backoff.
pub async fn wait_for<F, Fut>(
    condition: F,
    timeout: Duration,
    description: &str,
) -> anyhow::Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    let mut delay = Duration::from_millis(50);

    while start.elapsed() < timeout {
        if condition().await {
            return Ok(());
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(500));
    }

    Err(anyhow::anyhow!(
        "Condition '{}' not met within {:?}",
        description,
        timeout
    ))
}

/// Start a simple TCP echo server. Returns the listen address and a task handle.
pub async fn start_echo_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        }
    });

    (addr, handle)
}

/// Generate a unique service name for test isolation.
pub fn unique_service_name(prefix: &str) -> String {
    format!("{}_{}", prefix, uuid::Uuid::new_v4().as_simple())
}

/// A bridge subprocess with automatic cleanup via `kill_on_drop`.
pub struct BridgeProcess {
    child: tokio::process::Child,
}

impl BridgeProcess {
    pub async fn new(args: &[&str]) -> Self {
        use std::process::Stdio;
        let child = tokio::process::Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
            .args(args)
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to start bridge process");

        Self { child }
    }

    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }

    /// Kill and wait for the process to fully exit (up to 2s).
    pub async fn kill_and_wait(&mut self) {
        let _ = self.child.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        // kill_on_drop handles cleanup
    }
}

/// A pair of export + import bridge subprocesses.
/// Encapsulates the common boilerplate of starting both sides and waiting for readiness.
pub struct BridgePair {
    pub export: BridgeProcess,
    pub import: BridgeProcess,
    pub import_addr: SocketAddr,
}

impl BridgePair {
    /// Start a TCP export+import bridge pair.
    /// Waits for the import bridge to accept connections before returning.
    pub async fn tcp(service: &str, backend_addr: SocketAddr) -> Self {
        let export_spec = format!("{}/{}", service, backend_addr);
        let export = BridgeProcess::new(&["--export", &export_spec]).await;

        // Export bridge doesn't listen on TCP, so we must give it time
        // to connect to the Zenoh network before starting the import side.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{}", service, import_addr);
        let import_addr = import_port.release();
        let import = BridgeProcess::new(&["--import", &import_spec]).await;

        wait_for_port(import_addr, Duration::from_secs(10))
            .await
            .expect("Import bridge did not start in time");

        Self {
            export,
            import,
            import_addr,
        }
    }

    /// Start a TCP export+import pair with extra CLI args on each side.
    pub async fn tcp_with_args(
        service: &str,
        backend_addr: SocketAddr,
        extra_export_args: &[&str],
        extra_import_args: &[&str],
    ) -> Self {
        let export_spec = format!("{}/{}", service, backend_addr);
        let mut export_args: Vec<&str> = vec!["--export", &export_spec];
        export_args.extend_from_slice(extra_export_args);
        let export = BridgeProcess::new(&export_args).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{}", service, import_addr);
        let import_addr = import_port.release();
        let mut import_args: Vec<&str> = vec!["--import", &import_spec];
        import_args.extend_from_slice(extra_import_args);
        let import = BridgeProcess::new(&import_args).await;

        wait_for_port(import_addr, Duration::from_secs(10))
            .await
            .expect("Import bridge did not start in time");

        Self {
            export,
            import,
            import_addr,
        }
    }

    /// Start an HTTP export+import bridge pair with DNS-based routing.
    /// Waits for the import bridge to accept connections before returning.
    pub async fn http(service: &str, dns: &str, backend_addr: SocketAddr) -> Self {
        let export_spec = format!("{}/{}/{}", service, dns, backend_addr);
        let export = BridgeProcess::new(&["--http-export", &export_spec]).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{}", service, import_addr);
        let import_addr = import_port.release();
        let import = BridgeProcess::new(&["--http-import", &import_spec]).await;

        wait_for_port(import_addr, Duration::from_secs(10))
            .await
            .expect("HTTP import bridge did not start in time");

        Self {
            export,
            import,
            import_addr,
        }
    }

    /// Start a WebSocket export+import bridge pair.
    /// Waits for the import bridge to accept connections before returning.
    pub async fn ws(service: &str, backend_url: &str) -> Self {
        let export_spec = format!("{}/{}", service, backend_url);
        let export = BridgeProcess::new(&["--ws-export", &export_spec]).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{}", service, import_addr);
        let import_addr = import_port.release();
        let import = BridgeProcess::new(&["--ws-import", &import_spec]).await;

        wait_for_port(import_addr, Duration::from_secs(10))
            .await
            .expect("WS import bridge did not start in time");

        Self {
            export,
            import,
            import_addr,
        }
    }

    pub async fn kill_and_wait(&mut self) {
        self.export.kill_and_wait().await;
        self.import.kill_and_wait().await;
    }
}
