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

/// Retry a client handshake or request until it succeeds, or the deadline passes.
///
/// Import doors in HTTP mode resolve the backend AT CONNECT TIME
/// (`resolve_backend` in `src/import/connection.rs`), so the first client can
/// arrive before the export side's `{service}/available` liveliness token has
/// propagated and be refused — a 502 for HTTP and WebSocket upgrades, a bare
/// close for TLS/SNI. `wait_for_port` does not cover this: it only proves the
/// *listener* bound its socket.
///
/// Retrying is both the fix and what a real client does. Prefer this over a
/// fixed sleep, which is what made the WS and routing tests flaky.
pub async fn retry_client<T, E, F, Fut>(
    mut attempt: F,
    timeout: Duration,
    description: &str,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let start = std::time::Instant::now();
    let mut delay = Duration::from_millis(50);
    let mut last_err = String::from("never attempted");

    while start.elapsed() < timeout {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(500));
    }

    Err(anyhow::anyhow!(
        "'{}' did not succeed within {:?}; last error: {}",
        description,
        timeout,
        last_err
    ))
}

/// GET a URL through an import bridge, retrying while the door refuses it.
///
/// A refusal is either a `502` (the HTTP door could not resolve a backend) or a
/// transport error (the TLS/SNI door closes instead, since it cannot speak
/// HTTP). Both mean the export side has not announced `{service}/available`
/// yet — see [`retry_client`]. Any other status is handed back for the caller to
/// assert on, so this hides only the startup race, never a real failure.
pub async fn get_through_bridge(
    client: &reqwest::Client,
    url: &str,
    host: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<reqwest::Response> {
    retry_client(
        || async {
            let mut req = client.get(url).header("Connection", "close");
            if let Some(host) = host {
                req = req.header("Host", host);
            }
            let response = req.send().await?;
            if response.status() == reqwest::StatusCode::BAD_GATEWAY {
                anyhow::bail!("502: no backend announced yet");
            }
            Ok::<_, anyhow::Error>(response)
        },
        timeout,
        &format!("GET {url}"),
    )
    .await
}

/// Send `payload` through a raw import listener and return the echo.
///
/// Retries the WHOLE exchange — fresh connection, write, read — rather than
/// just the read. On the raw path the client's connection is what triggers the
/// chain (import declares liveliness -> export detects it -> export dials the
/// backend), so bytes written before the export side has dialled through can be
/// lost outright; re-reading the same socket would never recover them, but a new
/// exchange will. This replaces the `sleep(2s)`-and-hope convention.
pub async fn echo_roundtrip(
    import_addr: SocketAddr,
    payload: &[u8],
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    retry_client(
        || async {
            let mut stream = TcpStream::connect(import_addr).await?;
            stream.write_all(payload).await?;

            let mut buf = vec![0u8; payload.len().max(1024)];
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no echo yet"))??;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "backend not wired up yet (clean EOF)",
                ));
            }
            buf.truncate(n);
            Ok(buf)
        },
        timeout,
        &format!("echo round-trip through {import_addr}"),
    )
    .await
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

/// Build a bridge subprocess command that cannot outlive this test process.
///
/// Two layers, because one is not enough:
/// - `kill_on_drop` reaps the child when its handle drops on a normal path,
///   including a panic that unwinds past the test's own cleanup.
/// - `PR_SET_PDEATHSIG` has the kernel SIGKILL the child if the test binary dies
///   without running any Rust cleanup at all — exactly what happens when nextest
///   terminates a test that blew its slow-timeout.
///
/// A leaked bridge is not harmless: it keeps publishing and scouting on the
/// shared Zenoh domain, and then interferes with every later run on the machine.
/// Always spawn bridges through this, never `Command::new` directly.
pub fn bridge_command() -> tokio::process::Command {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"));

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // SAFETY: runs between fork and exec, so only async-signal-safe
            // calls are permitted. `prctl` is one; it touches no allocator or
            // lock inherited from the parent.
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    // Confine scouting to this test process's private multicast domain, so a
    // bridge only ever discovers this process's own peers (see scouting_port).
    cmd.arg("--zenoh-config").arg(zenoh_config_path());
    cmd
}

/// [`bridge_command`] WITHOUT the scouting-isolation config — for the handful
/// of tests that drive Zenoh endpoints explicitly (`--zenoh-listen`,
/// `--zenoh-connect`) or deliberately supply their own `--zenoh-config`.
pub fn bridge_command_raw() -> tokio::process::Command {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("zenoh-bridge-tcp"));
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    cmd
}

/// A bridge subprocess with automatic cleanup via `kill_on_drop`.
pub struct BridgeProcess {
    child: tokio::process::Child,
}

/// The PROCESS-WIDE fallback scouting domain, used by `bridge_command` and any
/// `BridgeProcess::new` that is not tied to a per-test [`ScoutDomain`]. It at
/// least keeps this test binary's bridges off the default multicast group so
/// they never see OTHER binaries' peers. Tests that spawn multiple
/// interoperating bridges AND want per-test isolation use `ScoutDomain::bridge`.
fn process_domain() -> ScoutDomain {
    use std::sync::OnceLock;
    static DOMAIN: OnceLock<ScoutDomain> = OnceLock::new();
    *DOMAIN.get_or_init(ScoutDomain::new)
}

fn zenoh_config_path() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    PATH.get_or_init(|| process_domain().config_file())
}

impl BridgeProcess {
    pub async fn new(args: &[&str]) -> Self {
        use std::process::Stdio;
        let mut cmd = bridge_command();
        cmd.args(args);
        // Debug aid: BRIDGE_LOG_DIR=<dir> captures every bridge subprocess's
        // stdout (at debug level) to a per-process file. Bridge logs are
        // otherwise discarded, which makes cross-process failures (liveliness
        // races, dial ordering) undiagnosable from nextest output alone. This
        // hook has paid for itself repeatedly; keep it.
        match std::env::var("BRIDGE_LOG_DIR") {
            Ok(dir) => {
                let name = format!("{}/bridge-{}.log", dir, uuid::Uuid::new_v4().as_simple());
                cmd.arg("--log-level").arg("debug");
                cmd.stdout(Stdio::from(std::fs::File::create(&name).unwrap()));
                cmd.stderr(Stdio::null());
            }
            Err(_) => {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
        }
        let child = cmd.spawn().expect("Failed to start bridge process");

        Self { child }
    }

    /// Like [`new`](Self::new) but WITHOUT scouting isolation — for tests that
    /// drive `--zenoh-listen`/`--zenoh-connect` or their own `--zenoh-config`.
    pub async fn new_raw(args: &[&str]) -> Self {
        use std::process::Stdio;
        let mut cmd = bridge_command_raw();
        cmd.args(args);
        match std::env::var("BRIDGE_LOG_DIR") {
            Ok(dir) => {
                let name = format!("{}/bridge-{}.log", dir, uuid::Uuid::new_v4().as_simple());
                cmd.arg("--log-level").arg("debug");
                cmd.stdout(Stdio::from(std::fs::File::create(&name).unwrap()));
                cmd.stderr(Stdio::null());
            }
            Err(_) => {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
        }
        Self {
            child: cmd.spawn().expect("Failed to start bridge process"),
        }
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

/// How long a client may keep retrying while the export side comes up. Generous:
/// two subprocesses have to boot and discover each other via Zenoh scouting,
/// which is slow on a loaded CI runner. It is a ceiling, not a delay — the first
/// successful attempt returns immediately.
pub const BACKEND_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// A pair of export + import bridge subprocesses.
///
/// Encapsulates the common boilerplate of starting both sides and waiting for
/// the import listener to bind. Note that a bound listener is NOT full
/// readiness for the HTTP-mode doors: see [`retry_client`].
pub struct BridgePair {
    pub export: BridgeProcess,
    pub import: BridgeProcess,
    pub import_addr: SocketAddr,
}

impl BridgePair {
    /// Start a TCP export+import bridge pair.
    /// Waits for the import bridge to accept connections before returning.
    ///
    /// No availability gate here, deliberately: `proto=raw` runs the import door
    /// with `http_mode` off, and only the HTTP-mode doors consult
    /// `resolve_backend`. A raw connection is never refused for a missing token,
    /// so the export side only needs a moment to reach the Zenoh network.
    pub async fn tcp(service: &str, backend_addr: SocketAddr) -> Self {
        let export_spec = format!("{}/{}", service, backend_addr);
        let export = BridgeProcess::new(&["--backend", &export_spec]).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{},proto=raw", service, import_addr);
        let import_addr = import_port.release();
        let import = BridgeProcess::new(&["--listen", &import_spec]).await;

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
        let mut export_args: Vec<&str> = vec!["--backend", &export_spec];
        export_args.extend_from_slice(extra_export_args);
        let export = BridgeProcess::new(&export_args).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{},proto=raw", service, import_addr);
        let import_addr = import_port.release();
        let mut import_args: Vec<&str> = vec!["--listen", &import_spec];
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
        let export_spec = format!("{}@{}/{}", service, dns, backend_addr);
        let export = BridgeProcess::new(&["--backend", &export_spec]).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{}", service, import_addr);
        let import_addr = import_port.release();
        let import = BridgeProcess::new(&["--listen", &import_spec]).await;

        wait_for_port(import_addr, Duration::from_secs(10))
            .await
            .expect("HTTP import bridge did not start in time");

        Self {
            export,
            import,
            import_addr,
        }
    }

    /// Start a plain (default) backend + auto listener pair — no `@host`
    /// anywhere. HTTP traffic reaches the backend via the default-backend
    /// fallback instead of a host-scoped registration.
    pub async fn http_default(service: &str, backend_addr: SocketAddr) -> Self {
        let export_spec = format!("{}/{}", service, backend_addr);
        let export = BridgeProcess::new(&["--backend", &export_spec]).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{}", service, import_addr);
        let import_addr = import_port.release();
        let import = BridgeProcess::new(&["--listen", &import_spec]).await;

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
        let export = BridgeProcess::new(&["--backend", &export_spec]).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let import_port = PortGuard::new();
        let import_addr = import_port.addr();
        let import_spec = format!("{}/{}", service, import_addr);
        let import_addr = import_port.release();
        let import = BridgeProcess::new(&["--listen", &import_spec]).await;

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

/// Sample `probe` until it stops changing, then return the settled value.
///
/// For counters that a still-finishing background task may still bump: reading
/// one straight away can capture a value that is about to move, which turns an
/// exact assertion into a flaky one.
pub async fn wait_for_stable<T, F>(mut probe: F, timeout: Duration) -> T
where
    F: FnMut() -> T,
    T: PartialEq + Copy,
{
    let start = std::time::Instant::now();
    let mut last = probe();
    while start.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let now = probe();
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

/// A backend listener immune to the harness's port probes.
///
/// `wait_for_port` (and any connect-then-close prober) creates a REAL bridged
/// connection on a `proto=raw` listener: raw doors relay unconditionally, so
/// the export dials the backend for the probe, and the backend sees a
/// connection that delivers zero bytes and closes. A single-accept backend is
/// consumed by that phantom; the real client's dial then lands in the kernel
/// queue of a listener nobody accepts on and dies with a reset when the
/// backend task exits — a confusing, timing-dependent failure.
///
/// This helper loop-accepts and invokes `handler` ONLY for connections that
/// deliver at least one byte; zero-byte connections are absorbed silently.
/// The handler receives the stream plus the already-read first chunk.
pub async fn start_probe_immune_backend<F, Fut>(
    handler: F,
) -> (SocketAddr, tokio::task::JoinHandle<()>)
where
    F: Fn(tokio::net::TcpStream, Vec<u8>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = spawn_probe_immune_accept_loop(listener, handler);
    (addr, handle)
}

/// [`start_probe_immune_backend`] on a caller-chosen address — for tests that
/// stop and later restart a backend on the same port.
pub async fn start_probe_immune_backend_on<F, Fut>(
    addr: SocketAddr,
    handler: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(tokio::net::TcpStream, Vec<u8>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    spawn_probe_immune_accept_loop(listener, handler)
}

fn spawn_probe_immune_accept_loop<F, Fut>(
    listener: tokio::net::TcpListener,
    handler: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(tokio::net::TcpStream, Vec<u8>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut first = vec![0u8; 65536];
                match stream.read(&mut first).await {
                    // Zero bytes then close: a probe phantom. Absorb it.
                    Ok(0) | Err(_) => {}
                    Ok(n) => {
                        first.truncate(n);
                        handler(stream, first).await;
                    }
                }
            });
        }
    })
}

/// Establish a raw-door connection that is PROVABLY served: retry the whole
/// connect + write + first-reply exchange until the backend answers, then hand
/// the live stream back for further traffic on the same connection.
///
/// The first reply is consumed here (it proves the export side is attached and
/// relaying); the caller continues the conversation from the second exchange.
pub async fn connected_raw_client(
    addr: SocketAddr,
    payload: &[u8],
    budget: Duration,
) -> anyhow::Result<TcpStream> {
    retry_client(
        || async {
            let mut stream = TcpStream::connect(addr).await?;
            stream.write_all(payload).await?;
            let mut buf = vec![0u8; 65536];
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no reply yet"))??;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "closed before a reply (backend not wired yet)",
                ));
            }
            Ok(stream)
        },
        budget,
        &format!("served connection to {addr}"),
    )
    .await
}

/// A private Zenoh scouting domain for ONE test.
///
/// nextest isolates test *binaries* into processes, but tests WITHIN a binary
/// run concurrently and, on the default multicast domain (224.0.0.224:7446),
/// contend: dozens of peers appear and die, and a running test's sessions burn
/// time connecting to corpses (the `OpenSyn -> close(GENERIC)` churn), which is
/// why discovery could exceed 20s under load. A `ScoutDomain` hands out a
/// unique multicast port so a test's own sessions and bridges find each other
/// and NO ONE else — the single biggest source of suite flakiness.
///
/// Create ONE per test and use it for every session and bridge in that test.
#[derive(Clone, Copy)]
pub struct ScoutDomain {
    port: u16,
}

/// The one per-process directory that holds every domain's `--zenoh-config`
/// file. Created exactly once (via `OnceLock`) so concurrent `config_file()`
/// calls never race `create_dir_all` against each other.
fn scout_config_dir() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("zb-scout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create zenoh config dir");
        dir
    })
}

impl ScoutDomain {
    /// Allocate a fresh, currently-free multicast scouting port.
    ///
    /// Zenoh binds the multicast group on this UDP port; two domains sharing it
    /// collide with `Address already in use`. A bare counter can repeat (the
    /// range wraps, and separate binaries seed independently), so probe each
    /// candidate by binding it first and only keep one the OS accepts. The bind
    /// is released immediately; the small TOCTOU window is unproblematic in
    /// practice because ports are handed out sparsely.
    pub fn new() -> Self {
        use std::net::UdpSocket;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let seed = std::process::id().wrapping_mul(2654435761);
        for _ in 0..10_000 {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let port = 20000 + ((seed.wrapping_add(n)) % 45000) as u16;
            // Binding the multicast group address on the port is the same
            // operation Zenoh performs; if it succeeds the port is free.
            if UdpSocket::bind(("0.0.0.0", port)).is_ok() {
                return Self { port };
            }
        }
        panic!("could not find a free scouting port");
    }

    fn address(&self) -> String {
        format!("224.0.0.224:{}", self.port)
    }

    /// A Zenoh config confined to this domain, for an in-process session.
    pub fn config(&self) -> zenoh::Config {
        let mut config = zenoh::Config::default();
        config
            .insert_json5(
                "scouting/multicast/address",
                &format!("\"{}\"", self.address()),
            )
            .expect("set multicast address");
        config
    }

    /// The JSON5 body for a subprocess `--zenoh-config` file.
    pub fn json5(&self) -> String {
        format!(
            "{{ mode: \"peer\", scouting: {{ multicast: {{ address: \"{}\" }} }} }}",
            self.address()
        )
    }

    /// Write this domain's config to a temp file and return the path, for
    /// `--zenoh-config`. The file lives for the test process's lifetime.
    pub fn config_file(&self) -> std::path::PathBuf {
        // The per-process config dir is shared by every domain (its name is keyed
        // only on the pid), so many tests build configs into it concurrently.
        // Create it exactly ONCE via a OnceLock — a bare `create_dir_all` racing
        // itself across threads can surface a non-EEXIST error and panic
        // (observed as a ~1-in-4 flake). Each domain's file is uniquely named by
        // its (unique, probe-bound) port, so the writes never collide.
        let dir = scout_config_dir();
        let path = dir.join(format!("zenoh-{}.json5", self.port));
        std::fs::write(&path, self.json5()).expect("write zenoh config");
        path
    }

    /// A bridge subprocess confined to this domain (isolation + PDEATHSIG).
    pub async fn bridge(&self, args: &[&str]) -> BridgeProcess {
        let mut full: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        full.push("--zenoh-config".into());
        full.push(self.config_file().to_string_lossy().into_owned());
        let refs: Vec<&str> = full.iter().map(String::as_str).collect();
        BridgeProcess::new_raw(&refs).await
    }
}

impl Default for ScoutDomain {
    fn default() -> Self {
        Self::new()
    }
}

/// Send one raw HTTP/1.1 request to a host-routed import door and return the
/// response text, retrying past the startup 502/`no-response` window.
///
/// The many `sleep(2s)` + raw-request edge-case tests raced readiness under
/// CPU load; this gates the first request the way `get_through_bridge` does for
/// reqwest, without pulling in a client.
pub async fn raw_http_until_served(
    addr: SocketAddr,
    request: &[u8],
    budget: Duration,
) -> anyhow::Result<String> {
    retry_client(
        || async {
            let mut stream = TcpStream::connect(addr).await?;
            stream.write_all(request).await?;
            stream.flush().await?;
            let mut response = String::new();
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no response"))??;
            if response.contains("502") || response.is_empty() {
                return Err(std::io::Error::other("no backend yet"));
            }
            Ok(response)
        },
        budget,
        "raw HTTP through the bridge",
    )
    .await
}

/// Close in-process Zenoh sessions cleanly BEFORE a `#[tokio::test]` returns.
///
/// A `#[tokio::test]` owns its runtime and drops it on return. A Zenoh session
/// dropped during that teardown races Zenoh's own background runtime and panics
/// the worker ("closure claimed permanent executor" -> SIGABRT, which fails the
/// whole test binary). Closing explicitly, while the runtime is still live,
/// shuts the session's tasks down in order. A short settle follows so in-flight
/// undeclarations complete.
pub async fn shutdown_sessions<const N: usize>(sessions: [std::sync::Arc<zenoh::Session>; N]) {
    for s in &sessions {
        let _ = s.close().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}
