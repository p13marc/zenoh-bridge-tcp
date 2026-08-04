//! Observability surface (G7 #43).
//!
//! A process-global, dependency-free metrics registry plus a tiny HTTP server
//! that exposes:
//!
//! - `GET /healthz` — liveness: always `200 ok` while the process runs.
//! - `GET /readyz`  — readiness: `200 ready` once bridges are started, else `503`.
//! - `GET /metrics` — Prometheus text exposition of per-service counters.
//!
//! Counters are per **service** (the routing key segment), which is the natural
//! "per key" grouping. Global aggregates are the scraper's job (sum without the
//! `service` label), so there is a single source of truth and no double-counting.
//!
//! The registry is a `LazyLock` global so call sites can record without threading
//! a handle through every function; the hot path (byte counting) is a single
//! relaxed atomic add on an `Arc<Counters>` resolved once per connection.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

static REGISTRY: LazyLock<Metrics> = LazyLock::new(Metrics::new);

/// The process-global metrics registry.
pub fn metrics() -> &'static Metrics {
    &REGISTRY
}

/// Per-service counters. All fields are monotonic counters except `active`,
/// which is a gauge (incremented on connection start, decremented on end).
#[derive(Debug, Default)]
pub struct Counters {
    /// Currently-open connections (gauge).
    pub active: AtomicU64,
    /// Connections ever opened (counter).
    pub total: AtomicU64,
    /// Bytes relayed client -> backend (counter).
    pub bytes_up: AtomicU64,
    /// Bytes relayed backend -> client (counter).
    pub bytes_down: AtomicU64,
    /// Connections that ended cleanly (counter).
    pub completed: AtomicU64,
    /// Connections reset without an error (counter).
    pub reset: AtomicU64,
    /// Connections that ended in an error (counter).
    pub failed: AtomicU64,
}

impl Counters {
    /// Add to the client -> backend byte counter.
    pub fn add_up(&self, n: usize) {
        self.bytes_up.fetch_add(n as u64, Ordering::Relaxed);
    }
    /// Add to the backend -> client byte counter.
    pub fn add_down(&self, n: usize) {
        self.bytes_down.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// How a connection ended, mirrored from the export data plane's outcome.
#[derive(Debug, Clone, Copy)]
pub enum ConnOutcome {
    Completed,
    Reset,
    Failed,
}

/// The metrics registry: per-service counters plus a readiness flag.
#[derive(Debug)]
pub struct Metrics {
    services: Mutex<HashMap<String, Arc<Counters>>>,
    ready: AtomicBool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    fn new() -> Self {
        Self {
            services: Mutex::new(HashMap::new()),
            ready: AtomicBool::new(false),
        }
    }

    /// Get (or create) the counters for a service.
    pub fn service(&self, name: &str) -> Arc<Counters> {
        let mut map = self.services.lock().expect("metrics mutex poisoned");
        map.entry(name.to_string()).or_default().clone()
    }

    /// Record the start of a connection for `service`, returning a guard that
    /// decrements the active gauge when dropped (covering every exit path).
    pub fn conn_start(&self, service: &str) -> ConnGuard {
        let counters = self.service(service);
        counters.active.fetch_add(1, Ordering::Relaxed);
        counters.total.fetch_add(1, Ordering::Relaxed);
        ConnGuard { counters }
    }

    /// Mark bridges as started (readiness).
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Relaxed);
    }

    /// Whether the bridge is ready to serve.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Render the registry in Prometheus text exposition format (v0.0.4).
    pub fn render_prometheus(&self) -> String {
        // Snapshot under the lock, then render without holding it.
        let mut services: Vec<(String, Arc<Counters>)> = {
            let map = self.services.lock().expect("metrics mutex poisoned");
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        services.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = String::new();
        let ready = if self.is_ready() { 1 } else { 0 };
        out.push_str("# HELP zbridge_ready Whether the bridge is ready to serve (1) or not (0).\n");
        out.push_str("# TYPE zbridge_ready gauge\n");
        out.push_str(&format!("zbridge_ready {ready}\n"));

        out.push_str("# HELP zbridge_active_connections Currently open connections.\n");
        out.push_str("# TYPE zbridge_active_connections gauge\n");
        for (svc, c) in &services {
            out.push_str(&metric_line(
                "zbridge_active_connections",
                svc,
                None,
                c.active.load(Ordering::Relaxed),
            ));
        }

        out.push_str("# HELP zbridge_connections_total Connections ever opened.\n");
        out.push_str("# TYPE zbridge_connections_total counter\n");
        for (svc, c) in &services {
            out.push_str(&metric_line(
                "zbridge_connections_total",
                svc,
                None,
                c.total.load(Ordering::Relaxed),
            ));
        }

        out.push_str("# HELP zbridge_bytes_total Bytes relayed, by direction.\n");
        out.push_str("# TYPE zbridge_bytes_total counter\n");
        for (svc, c) in &services {
            out.push_str(&metric_line(
                "zbridge_bytes_total",
                svc,
                Some(("direction", "up")),
                c.bytes_up.load(Ordering::Relaxed),
            ));
            out.push_str(&metric_line(
                "zbridge_bytes_total",
                svc,
                Some(("direction", "down")),
                c.bytes_down.load(Ordering::Relaxed),
            ));
        }

        out.push_str("# HELP zbridge_connections_outcome_total Connections by how they ended.\n");
        out.push_str("# TYPE zbridge_connections_outcome_total counter\n");
        for (svc, c) in &services {
            out.push_str(&metric_line(
                "zbridge_connections_outcome_total",
                svc,
                Some(("outcome", "completed")),
                c.completed.load(Ordering::Relaxed),
            ));
            out.push_str(&metric_line(
                "zbridge_connections_outcome_total",
                svc,
                Some(("outcome", "reset")),
                c.reset.load(Ordering::Relaxed),
            ));
            out.push_str(&metric_line(
                "zbridge_connections_outcome_total",
                svc,
                Some(("outcome", "failed")),
                c.failed.load(Ordering::Relaxed),
            ));
        }

        out
    }
}

/// Escape a Prometheus label value (`\`, `"`, and newlines).
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Render one `name{service="…"[,extra]} value` line.
fn metric_line(name: &str, service: &str, extra: Option<(&str, &str)>, value: u64) -> String {
    match extra {
        Some((k, v)) => format!(
            "{name}{{service=\"{}\",{k}=\"{}\"}} {value}\n",
            escape_label(service),
            escape_label(v)
        ),
        None => format!("{name}{{service=\"{}\"}} {value}\n", escape_label(service)),
    }
}

/// RAII guard for one connection's lifetime. Decrements the active gauge on drop.
pub struct ConnGuard {
    counters: Arc<Counters>,
}

impl ConnGuard {
    /// The counters for this connection's service, for byte accounting on the
    /// relay hot path (a single relaxed atomic add per chunk).
    pub fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }

    /// Record how the connection ended.
    pub fn set_outcome(&self, outcome: ConnOutcome) {
        let counter = match outcome {
            ConnOutcome::Completed => &self.counters.completed,
            ConnOutcome::Reset => &self.counters.reset,
            ConnOutcome::Failed => &self.counters.failed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.counters.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Convenience: start a connection on the global registry.
pub fn conn_start(service: &str) -> ConnGuard {
    metrics().conn_start(service)
}

// ---------------------------------------------------------------------------
// Minimal HTTP server for /healthz, /readyz, /metrics.
// ---------------------------------------------------------------------------

/// Serve the health/metrics endpoints on `addr` until `shutdown` is cancelled.
///
/// Hand-rolled HTTP/1.1 (request-line + fixed responses, `Connection: close`) —
/// the endpoints are trivial GETs, so this stays dependency-free.
pub async fn serve(addr: SocketAddr, shutdown: CancellationToken) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "Metrics/health server listening (/healthz /readyz /metrics)");
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => { tokio::spawn(handle_conn(stream)); }
                    Err(e) => error!(error = %e, "metrics server accept failed"),
                }
            }
        }
    }
    info!("Metrics/health server stopped");
    Ok(())
}

async fn handle_conn(mut stream: TcpStream) {
    // Read until end-of-headers (or a sane cap). GET requests have no body, so
    // the request line is available in the first read.
    let mut got: Vec<u8> = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                if got.windows(4).any(|w| w == b"\r\n\r\n") || got.len() > 8192 {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    let request_line = String::from_utf8_lossy(&got);
    let path = request_line
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let (status, ctype, body) = route(path);
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Route a request path to (status line, content-type, body).
fn route(path: &str) -> (&'static str, &'static str, String) {
    let path = path.split('?').next().unwrap_or(path);
    match path {
        "/healthz" => ("200 OK", "text/plain", "ok\n".to_string()),
        "/readyz" => {
            if metrics().is_ready() {
                ("200 OK", "text/plain", "ready\n".to_string())
            } else {
                (
                    "503 Service Unavailable",
                    "text/plain",
                    "not ready\n".to_string(),
                )
            }
        }
        "/metrics" => (
            "200 OK",
            "text/plain; version=0.0.4",
            metrics().render_prometheus(),
        ),
        _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_guard_tracks_active_and_total() {
        let m = Metrics::new();
        let svc = m.service("svc");
        {
            let _g1 = m.conn_start("svc");
            let _g2 = m.conn_start("svc");
            assert_eq!(svc.active.load(Ordering::Relaxed), 2);
            assert_eq!(svc.total.load(Ordering::Relaxed), 2);
        }
        // Both guards dropped -> active back to zero, total unchanged.
        assert_eq!(svc.active.load(Ordering::Relaxed), 0);
        assert_eq!(svc.total.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn bytes_and_outcome_recorded() {
        let m = Metrics::new();
        let guard = m.conn_start("api");
        let c = guard.counters();
        c.add_up(100);
        c.add_down(250);
        guard.set_outcome(ConnOutcome::Failed);
        drop(guard);

        let svc = m.service("api");
        assert_eq!(svc.bytes_up.load(Ordering::Relaxed), 100);
        assert_eq!(svc.bytes_down.load(Ordering::Relaxed), 250);
        assert_eq!(svc.failed.load(Ordering::Relaxed), 1);
        assert_eq!(svc.completed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn prometheus_render_has_expected_series() {
        let m = Metrics::new();
        let g = m.conn_start("web");
        g.counters().add_up(10);
        g.counters().add_down(20);
        g.set_outcome(ConnOutcome::Completed);
        m.set_ready(true);
        let text = m.render_prometheus();

        assert!(text.contains("zbridge_ready 1"));
        assert!(text.contains("# TYPE zbridge_bytes_total counter"));
        assert!(text.contains("zbridge_bytes_total{service=\"web\",direction=\"up\"} 10"));
        assert!(text.contains("zbridge_bytes_total{service=\"web\",direction=\"down\"} 20"));
        assert!(text.contains(
            "zbridge_connections_outcome_total{service=\"web\",outcome=\"completed\"} 1"
        ));
        // active is 1 while the guard is alive.
        assert!(text.contains("zbridge_active_connections{service=\"web\"} 1"));
    }

    #[test]
    fn route_health_ready_metrics_and_404() {
        // Note: uses the global registry's readiness flag.
        assert_eq!(route("/healthz").0, "200 OK");
        assert_eq!(route("/nope").0, "404 Not Found");
        // /readyz reflects global readiness; /metrics always 200.
        assert_eq!(route("/metrics").0, "200 OK");
        assert!(matches!(
            route("/readyz").0,
            "200 OK" | "503 Service Unavailable"
        ));
        // Query strings are stripped.
        assert_eq!(route("/healthz?foo=bar").0, "200 OK");
    }

    #[test]
    fn label_escaping() {
        assert_eq!(escape_label("a\"b\\c"), "a\\\"b\\\\c");
    }
}
