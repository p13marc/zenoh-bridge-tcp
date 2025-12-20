# Zenoh TCP Bridge - Improvement Recommendations

This document outlines potential improvements for the zenoh-bridge-tcp project, organized by category and priority.

## Code Quality Issues

### 1. Clippy Errors (Build-Breaking)

The following clippy errors prevent the test suite from compiling:

**`tests/stress_test.rs:733`** - Using `.read()` without handling partial reads:
```rust
// Current (broken):
stream.read(&mut buf).await?;

// Fix: use read_exact or loop until complete
stream.read_exact(&mut buf).await?;
```

**`tests/export_import_integration.rs:648`** - Useless `vec![]`:
```rust
// Current:
let messages = vec!["Hello\n", "World\n", "Test\n"];

// Fix:
let messages = ["Hello\n", "World\n", "Test\n"];
```

**`tests/multi_service_integration.rs:418, 475`** - Loops that should be `while let`:
```rust
// Current:
loop {
    match subscriber.recv_async().await {
        Ok(sample) => { ... }
        Err(_) => break,
    }
}

// Fix:
while let Ok(sample) = subscriber.recv_async().await {
    ...
}
```

### 2. Error Handling

Currently uses `anyhow` everywhere. Consider a `thiserror`-based error enum for the library API to give callers structured errors:

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BridgeError {
    #[error("invalid export spec: {0}")]
    InvalidExportSpec(String),
    
    #[error("failed to connect to backend {addr}")]
    BackendConnectionFailed { 
        addr: std::net::SocketAddr, 
        #[source] 
        source: std::io::Error 
    },
    
    #[error("zenoh session failed: {0}")]
    ZenohSessionFailed(String),
    
    #[error("HTTP parsing failed: {0}")]
    HttpParseError(String),
    
    #[error("TLS/SNI parsing failed: {0}")]
    TlsParseError(String),
}
```

### 3. Magic Numbers

Buffer sizes and timeouts are scattered throughout the code. Consider a centralized config:

```rust
pub struct BridgeConfig {
    /// Buffer size for TCP read/write operations (default: 65536)
    pub buffer_size: usize,
    
    /// Maximum HTTP header size (default: 16384)
    pub max_header_size: usize,
    
    /// Timeout for reading HTTP/TLS headers (default: 10s)
    pub read_timeout: Duration,
    
    /// Timeout for backend connection attempts (default: 5s)
    pub backend_connect_timeout: Duration,
    
    /// Zenoh publisher heartbeat interval (default: 500ms)
    pub heartbeat_interval: Duration,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            buffer_size: 65536,
            max_header_size: 16 * 1024,
            read_timeout: Duration::from_secs(10),
            backend_connect_timeout: Duration::from_secs(5),
            heartbeat_interval: Duration::from_millis(500),
        }
    }
}
```

### 4. Code Duplication

`export.rs` and `import.rs` share similar patterns:
- Zenoh AdvancedPublisher/Subscriber setup
- Bridge loop logic (read from one side, write to other)
- Cancellation handling

Could extract a common abstraction:

```rust
struct BridgeChannel {
    publisher: AdvancedPublisher,
    subscriber: AdvancedSubscriber,
}

impl BridgeChannel {
    async fn bridge_to_tcp(&self, writer: &mut TcpWriteHalf) -> Result<()>;
    async fn bridge_from_tcp(&self, reader: &mut TcpReadHalf) -> Result<()>;
}
```

### 5. Missing Graceful Shutdown

`main.rs` spawns tasks but has no signal handling. The bridge doesn't clean up on Ctrl+C:

```rust
// Add to main.rs:
use tokio::signal;

// In main():
tokio::select! {
    _ = signal::ctrl_c() => {
        info!("Received Ctrl+C, shutting down...");
        // Cancel all tasks, close Zenoh session
    }
    _ = futures::future::join_all(tasks) => {}
}
```

---

## Feature Improvements

### Priority 1: Quick Wins

#### 1.1 Configurable Timeouts via CLI

```
--read-timeout <SECONDS>      Timeout for reading client data (default: 10)
--connect-timeout <SECONDS>   Timeout for backend connections (default: 5)
--buffer-size <BYTES>         Buffer size for TCP operations (default: 65536)
```

#### 1.2 Connection Logging Improvements

Add structured logging with connection IDs for easier debugging:

```rust
info!(
    client_id = %client_id,
    backend = %backend_addr,
    service = %service_name,
    "Client connected"
);
```

### Priority 2: Medium Effort

#### 2.1 Metrics & Observability

Add optional Prometheus metrics behind a `metrics` feature flag:

```rust
// Cargo.toml
[features]
metrics = ["prometheus", "hyper"]

// Metrics to expose:
- bridge_connections_total{service, direction} (counter)
- bridge_active_connections{service} (gauge)
- bridge_bytes_transferred{service, direction} (counter)
- bridge_connection_duration_seconds{service} (histogram)
- bridge_backend_connect_errors_total{service} (counter)
```

CLI flag: `--metrics-port 9090`

#### 2.2 Health Check Endpoint

Add health/readiness endpoints for container orchestration:

```
--health-port <PORT>    Enable health check endpoint on this port
```

Endpoints:
- `GET /health` - Returns 200 if process is running
- `GET /ready` - Returns 200 if Zenoh session is connected
- `GET /metrics` - Prometheus metrics (if enabled)

#### 2.3 Structured Error Types

Replace `anyhow` in the library with custom error types (see section 2 above).

#### 2.4 Reconnection Logic

Add exponential backoff reconnection for backend connections:

```rust
struct ReconnectConfig {
    initial_delay: Duration,    // 100ms
    max_delay: Duration,        // 30s
    multiplier: f64,            // 2.0
    max_attempts: Option<u32>,  // None = infinite
}
```

### Priority 3: Larger Features

#### 3.1 Configuration File Support

Support TOML configuration for complex deployments:

```toml
# bridge.toml
[zenoh]
mode = "peer"
connect = ["tcp/router:7447"]

[[export]]
service = "api"
backend = "127.0.0.1:8000"

[[export]]
service = "database"
backend = "127.0.0.1:5432"

[[http_export]]
service = "web"
dns = "api.example.com"
backend = "127.0.0.1:8080"

[[http_export]]
service = "web"
dns = "admin.example.com"
backend = "127.0.0.1:8081"

[[import]]
service = "api"
listen = "0.0.0.0:3000"

[[http_import]]
service = "web"
listen = "0.0.0.0:80"
```

CLI: `zenoh-bridge-tcp --config bridge.toml`

#### 3.2 Load Balancing for HTTP Mode

Support multiple backends per DNS with load balancing:

```toml
[[http_export]]
service = "web"
dns = "api.example.com"
backends = [
    "127.0.0.1:8080",
    "127.0.0.1:8081",
    "127.0.0.1:8082",
]
strategy = "round-robin"  # or "least-connections", "random"
```

#### 3.3 Connection Limits

Prevent resource exhaustion:

```
--max-connections <N>           Global connection limit
--max-connections-per-service <N>   Per-service limit
```

#### 3.4 Access Control

Allow/deny rules:

```toml
[access]
allow_cidrs = ["10.0.0.0/8", "192.168.0.0/16"]
deny_cidrs = ["10.0.0.5/32"]
rate_limit = { requests_per_second = 100, burst = 20 }
```

#### 3.5 WebSocket Support

Add WebSocket bridging for browser clients:

```
--ws-import <SPEC>    Import Zenoh service as WebSocket listener
--ws-export <SPEC>    Export WebSocket backend as Zenoh service
```

#### 3.6 TLS Termination Option

Currently HTTPS is pass-through only. Add option to terminate TLS:

```
--tls-cert <PATH>     TLS certificate for termination
--tls-key <PATH>      TLS private key
--tls-terminate       Terminate TLS instead of pass-through
```

#### 3.7 Hot Reload

Watch config file and reload without restart:

```bash
# Reload on SIGHUP
kill -HUP $(pidof zenoh-bridge-tcp)
```

---

## Testing Improvements

### 1. Fix Clippy Errors
The test suite doesn't compile due to clippy errors listed above.

### 2. Add Benchmarks
Use `criterion` for performance testing:

```rust
// benches/throughput.rs
fn benchmark_tcp_bridge(c: &mut Criterion) {
    c.bench_function("1KB message throughput", |b| {
        b.iter(|| { /* send 1KB through bridge */ })
    });
}
```

### 3. Property-Based Testing
Use `proptest` for parser edge cases:

```rust
proptest! {
    #[test]
    fn http_parser_doesnt_crash(data: Vec<u8>) {
        let _ = try_parse_request(&data);
    }
}
```

### 4. Fuzzing
Add `cargo-fuzz` targets for HTTP and TLS parsers:

```
fuzz/
  fuzz_targets/
    http_parser.rs
    tls_parser.rs
```

---

## Documentation Improvements

### 1. API Documentation
Add `#![deny(missing_docs)]` to `lib.rs` and document all public items.

### 2. Architecture Diagram
Add a visual diagram showing data flow:

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│ TCP Backend │◄───►│ Export Bridge │◄───►│   Zenoh     │
└─────────────┘     └──────────────┘     │   Network   │
                                         │             │
┌─────────────┐     ┌──────────────┐     │             │
│ TCP Client  │◄───►│ Import Bridge │◄───►│             │
└─────────────┘     └──────────────┘     └─────────────┘
```

### 3. Troubleshooting Guide
Document common issues:
- Connection timeouts
- Zenoh discovery problems
- TLS/SNI routing failures

---

## Summary: Recommended Priority Order

| Priority | Item | Effort | Impact |
|----------|------|--------|--------|
| 1 | Fix clippy errors | Low | High (build broken) |
| 2 | Add graceful shutdown | Low | Medium |
| 3 | Configurable timeouts | Low | Medium |
| 4 | Structured error types | Medium | Medium |
| 5 | Metrics/Prometheus | Medium | High |
| 6 | Health check endpoint | Low | Medium |
| 7 | Config file support | Medium | High |
| 8 | Reconnection logic | Medium | Medium |
| 9 | Load balancing | High | Medium |
| 10 | WebSocket support | High | Medium |
