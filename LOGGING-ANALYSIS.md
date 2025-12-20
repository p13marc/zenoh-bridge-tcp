# Logging Analysis and Improvement Recommendations

## Current State Analysis

### Logging Framework
- Uses `tracing` crate with `tracing-subscriber`
- Basic initialization: `tracing_subscriber::fmt::init()`
- No structured fields, no spans, no log levels configuration

### Log Level Distribution

| Level | Count | Usage |
|-------|-------|-------|
| `info!` | ~60 | Startup, connections, mode announcements |
| `debug!` | ~25 | Data flow, byte counts |
| `error!` | ~35 | Failures, connection errors |
| `warn!` | ~8 | Missing headers, parse failures |
| `trace!` | 0 | Not used |

### Current Issues

#### 1. No Structured Logging
Current logs use string interpolation:
```rust
info!("Client {}: ← {} bytes from Zenoh", client_id, payload.len());
```

Should use structured fields:
```rust
info!(client_id = %client_id, bytes = payload.len(), "Received data from Zenoh");
```

#### 2. No Spans for Request Tracing
No way to correlate logs for a single connection across multiple log lines. Each log is independent.

#### 3. Inconsistent Prefixes
- Some use emojis: `"🚀 EXPORT MODE"`, `"✓ Client connected"`
- Some use plain text: `"Client {}: Detected TLS/HTTPS connection"`
- Some use `"✗"` for disconnections

#### 4. No Configurable Log Levels
- No CLI flag to set log level
- No per-module log level control
- Users must use `RUST_LOG` environment variable

#### 5. Verbose Startup Logs
Each mode prints 6-7 `info!` lines at startup:
```rust
info!("🚀 HTTP EXPORT MODE");
info!("   Service name: {}", service_name);
info!("   DNS: {}", dns);
info!("   Backend: {}", backend_addr);
// ... more lines
```

#### 6. Missing Context in Error Logs
```rust
error!("Liveliness subscriber error: {:?}", e);
```
No service name, no client context.

#### 7. No Performance Metrics Logging
- No throughput logging
- No latency measurements
- No connection duration tracking

#### 8. Debug Logs in Hot Paths
```rust
debug!("Client {}: ← {} bytes from Zenoh", client_id, payload.len());
```
This is called for every message - could impact performance even when disabled.

---

## Recommendations

### 1. Add Structured Logging with Spans

**Before:**
```rust
info!("✓ Client connected: {}", client_id);
// ... many operations ...
debug!("Client {}: ← {} bytes from Zenoh", client_id, payload.len());
// ... more operations ...
info!("✗ Client disconnected: {}", client_id);
```

**After:**
```rust
let span = info_span!("client_session", 
    client_id = %client_id,
    service = %service_name,
    remote_addr = %addr
);
let _guard = span.enter();

info!("Connected");
// ... many operations ...
debug!(bytes = payload.len(), direction = "rx", "Data received");
// ... more operations ...
info!(duration_ms = elapsed.as_millis(), "Disconnected");
```

### 2. Add Log Level CLI Flag

```rust
// In args.rs
#[arg(long, default_value = "info")]
pub log_level: String,

#[arg(long)]
pub log_format: Option<String>, // "json", "pretty", "compact"
```

```rust
// In main.rs
let filter = EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| EnvFilter::new(&args.log_level));

tracing_subscriber::fmt()
    .with_env_filter(filter)
    .init();
```

### 3. Use JSON Logging for Production

Add feature flag for JSON output:
```rust
if args.log_format == Some("json".to_string()) {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();
} else {
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();
}
```

### 4. Consolidate Startup Logs

**Before (7 lines):**
```rust
info!("🚀 HTTP EXPORT MODE");
info!("   Service name: {}", service_name);
info!("   DNS: {}", dns);
info!("   Backend: {}", backend_addr);
info!("   Zenoh TX key: {}/{}/tx/<client_id>", service_name, dns);
info!("   Zenoh RX key: {}/{}/rx/<client_id>", service_name, dns);
info!("   Liveliness: {}/{}/clients/*", service_name, dns);
```

**After (1 structured log):**
```rust
info!(
    mode = "http_export",
    service = %service_name,
    dns = %dns,
    backend = %backend_addr,
    "Started export bridge"
);
```

### 5. Add Connection Metrics Span

```rust
async fn handle_client_bridge(...) {
    let start = Instant::now();
    let span = info_span!("bridge_session",
        client_id = %client_id,
        service = %service_name,
    );
    
    async {
        // ... bridge logic ...
    }.instrument(span).await;
    
    info!(
        duration_secs = start.elapsed().as_secs_f64(),
        bytes_tx = tx_bytes.load(Ordering::Relaxed),
        bytes_rx = rx_bytes.load(Ordering::Relaxed),
        "Session ended"
    );
}
```

### 6. Standardize Log Message Format

Create logging conventions:

| Event | Level | Format |
|-------|-------|--------|
| Mode startup | INFO | `mode=X, service=Y, "Bridge started"` |
| Client connect | INFO | `client_id=X, addr=Y, "Client connected"` |
| Client disconnect | INFO | `client_id=X, duration=Y, "Client disconnected"` |
| Data transfer | DEBUG | `client_id=X, bytes=Y, dir=rx/tx, "Data transferred"` |
| Error | ERROR | `client_id=X, error=Y, "Operation failed"` |
| Backend unavailable | WARN | `client_id=X, dns=Y, "No backend available"` |

### 7. Add Trace Level for Verbose Debugging

```rust
trace!(
    client_id = %client_id,
    payload_preview = ?&payload[..payload.len().min(64)],
    "Raw payload"
);
```

### 8. Remove Emojis (Optional)

Emojis can cause issues in:
- Log aggregation systems
- Terminals without Unicode support
- Log parsing scripts

Replace with structured tags:
```rust
// Before
info!("🚀 EXPORT MODE");
info!("✓ Client connected");
info!("✗ Client disconnected");

// After
info!(event = "startup", mode = "export", "Bridge started");
info!(event = "connect", client_id = %id, "Client connected");
info!(event = "disconnect", client_id = %id, "Client disconnected");
```

---

## Implementation Priority

### High Priority
1. **Add `--log-level` CLI flag** - Easy win, immediate user benefit
2. **Add spans for client sessions** - Critical for debugging production issues
3. **Structured fields on all logs** - Enables log aggregation/searching

### Medium Priority
4. **JSON logging option** - For production deployments
5. **Consolidate startup logs** - Reduce noise
6. **Standardize error context** - Include service/client info in all errors

### Low Priority
7. **Remove emojis** - Cosmetic, but improves compatibility
8. **Add trace level logs** - For deep debugging
9. **Add metrics logging** - Throughput, latency tracking

---

## Example Refactored Code

```rust
use tracing::{info, debug, error, warn, info_span, Instrument};

pub async fn run_export_mode(
    session: Arc<Session>,
    export_spec: &str,
) -> Result<()> {
    let (service_name, backend_addr) = parse_export_spec(export_spec)?;
    
    let span = info_span!("export_bridge",
        service = %service_name,
        backend = %backend_addr,
    );
    
    async {
        info!("Starting export bridge");
        
        // ... setup code ...
        
        loop {
            match liveliness_subscriber.recv_async().await {
                Ok(sample) => {
                    let client_id = extract_client_id(&sample);
                    handle_client(client_id, &session, &backend_addr)
                        .instrument(info_span!("client", id = %client_id))
                        .await;
                }
                Err(e) => {
                    error!(error = %e, "Liveliness subscriber failed");
                    break;
                }
            }
        }
    }.instrument(span).await
}
```

---

## Dependencies to Add

```toml
[dependencies]
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

---

## Summary

The current logging is functional but lacks:
- **Structured data** for machine parsing
- **Spans** for request correlation  
- **Configurability** for production use
- **Consistency** in format and style

Implementing these recommendations would significantly improve debuggability and operational visibility.
