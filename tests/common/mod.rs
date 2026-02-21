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

/// Start a bridge process using assert_cmd's cargo_bin.
pub struct BridgeProcess {
    child: tokio::process::Child,
}

impl BridgeProcess {
    pub async fn new(args: &[&str]) -> Self {
        let child = tokio::process::Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"))
            .args(args)
            .kill_on_drop(true)
            .spawn()
            .expect("Failed to start bridge process");

        Self { child }
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        // kill_on_drop handles cleanup
    }
}
