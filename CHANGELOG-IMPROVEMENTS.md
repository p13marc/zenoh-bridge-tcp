# Zenoh TCP Bridge - Improvements Report

This document summarizes the improvements made to the zenoh-bridge-tcp project.

## Overview

Four major improvements were implemented:

1. Fixed clippy errors (build-breaking)
2. Configurable timeouts and buffer sizes
3. Structured error types with thiserror
4. WebSocket support (--ws-import / --ws-export)

---

## 1. Fixed Clippy Errors

### Commits
- `d89b484` - fix: resolve clippy errors in tests
- `b9b3bf6` - Fix clippy collapsible_match warning in tls_parser.rs

### Changes

| File | Issue | Fix |
|------|-------|-----|
| `tests/stress_test.rs:733` | `unused_io_amount` | Changed `stream.read(&mut buf).await?` to `let _ = stream.read(&mut buf).await?` |
| `tests/export_import_integration.rs:648` | `useless_vec` | Changed `vec!["Hello\n", ...]` to `["Hello\n", ...]` |
| `tests/multi_service_integration.rs:418,475` | `while_let_loop` | Converted `loop { match x.recv_async().await { ... } }` to `while let Ok(sample) = x.recv_async().await { ... }` |
| `src/tls_parser.rs:156-157` | `collapsible_match` | Collapsed nested `if let` patterns into single match |

---

## 2. Configurable Timeouts and Buffer Sizes

### Commit
- `b1db92e` - feat: add BridgeError enum and BridgeConfig struct

### New CLI Arguments

```bash
# Buffer size for read operations (default: 65536)
--buffer-size <BYTES>

# Read timeout in seconds (default: 10)
--read-timeout <SECONDS>
```

### New Configuration Struct

Added `BridgeConfig` in `src/config.rs`:

```rust
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub buffer_size: usize,              // default: 65536
    pub max_header_size: usize,          // default: 16384
    pub read_timeout: Duration,          // default: 10s
    pub heartbeat_interval: Duration,    // default: 500ms
    pub availability_timeout: Duration,  // default: 1s
}
```

### Usage Example

```bash
zenoh-bridge-tcp --export 'myservice/127.0.0.1:8080' \
                 --buffer-size 131072 \
                 --read-timeout 30
```

---

## 3. Structured Error Types

### Commits
- `b1db92e` - feat: add BridgeError enum and BridgeConfig struct
- `ca80884` - refactor: update http_parser and tls_parser to use BridgeError

### New Error Type

Added `src/error.rs` with `BridgeError` enum:

```rust
#[derive(Error, Debug)]
pub enum BridgeError {
    #[error("invalid export spec '{spec}': {reason}")]
    InvalidExportSpec { spec: String, reason: String },

    #[error("invalid import spec '{spec}': {reason}")]
    InvalidImportSpec { spec: String, reason: String },

    #[error("invalid WebSocket export spec '{spec}': {reason}")]
    InvalidWsExportSpec { spec: String, reason: String },

    #[error("failed to connect to backend {addr}")]
    BackendConnection { addr: SocketAddr, #[source] source: std::io::Error },

    #[error("failed to bind to {addr}")]
    BindFailed { addr: SocketAddr, #[source] source: std::io::Error },

    #[error("zenoh error: {0}")]
    Zenoh(String),

    #[error("HTTP parse error: {0}")]
    HttpParse(String),

    #[error("TLS/SNI parse error: {0}")]
    TlsParse(String),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("no backend available for '{dns}'")]
    NoBackend { dns: String },

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
```

### Files Updated
- `src/lib.rs` - Added `pub mod error`
- `src/http_parser.rs` - Now returns `BridgeError` instead of `anyhow::Error`
- `src/tls_parser.rs` - Now returns `BridgeError` instead of `anyhow::Error`

---

## 4. WebSocket Support

### Commits
- `e409be4` - Add WebSocket support (--ws-export and --ws-import)
- `0cb4f80` - Add WebSocket integration tests

### New CLI Arguments

```bash
# Export: Connect to a WebSocket backend server
--ws-export 'service_name/ws://host:port'
--ws-export 'service_name/wss://host:port'  # TLS

# Import: Accept WebSocket client connections
--ws-import 'service_name/listen_addr'
```

### Architecture

```
WebSocket Export Mode:
  Zenoh Client Token → Bridge → WebSocket Backend Server
  
WebSocket Import Mode:
  WebSocket Client → Bridge Listener → Zenoh → Export Bridge → Backend
```

### Usage Examples

```bash
# Export a WebSocket backend
zenoh-bridge-tcp --ws-export 'chat/ws://127.0.0.1:9000'

# Import to expose as WebSocket listener
zenoh-bridge-tcp --ws-import 'chat/0.0.0.0:8080'

# Combined with regular TCP
zenoh-bridge-tcp \
    --export 'tcp-service/127.0.0.1:3000' \
    --ws-export 'ws-service/ws://127.0.0.1:9000' \
    --import 'tcp-service/0.0.0.0:4000' \
    --ws-import 'ws-service/0.0.0.0:8080'
```

### New Functions

**src/export.rs:**
- `parse_ws_export_spec(spec: &str) -> Result<(String, String)>`
- `run_ws_export_mode(session: Arc<Session>, export_spec: &str) -> Result<()>`

**src/import.rs:**
- `run_ws_import_mode(session: Arc<Session>, import_spec: &str) -> Result<()>`

### Dependencies Added

```toml
thiserror = "1"
tokio-tungstenite = "0.26"
futures-util = "0.3"
```

### Integration Tests

New file `tests/ws_integration.rs` with:
- `test_ws_export_import_basic` - Basic echo roundtrip
- `test_ws_multiple_messages` - Multiple sequential messages
- `test_ws_connection_lifecycle` - Connection tracking

---

## Files Modified

| File | Changes |
|------|---------|
| `Cargo.toml` | Added thiserror, tokio-tungstenite, futures-util |
| `src/lib.rs` | Added `pub mod error` |
| `src/error.rs` | **New file** - BridgeError enum |
| `src/config.rs` | Added BridgeConfig struct |
| `src/args.rs` | Added --buffer-size, --read-timeout, --ws-export, --ws-import |
| `src/export.rs` | Added WebSocket export functions, uses BridgeError |
| `src/import.rs` | Added WebSocket import functions, uses BridgeError |
| `src/http_parser.rs` | Refactored to use BridgeError |
| `src/tls_parser.rs` | Refactored to use BridgeError, fixed collapsible_match |
| `src/main.rs` | Added WebSocket task spawning |
| `tests/stress_test.rs` | Fixed clippy warning |
| `tests/export_import_integration.rs` | Fixed clippy warning |
| `tests/multi_service_integration.rs` | Fixed clippy warnings |
| `tests/ws_integration.rs` | **New file** - WebSocket integration tests |

---

## Test Results

```
Running 56 unit tests ... ok
Running 56 integration tests ... ok (with --test-threads=1)
Running clippy ... no warnings
```

**Note:** One pre-existing flaky test (`test_http_methods`) may fail when tests run in parallel due to hardcoded port conflicts. This is unrelated to these changes and passes when run in isolation or sequentially.

---

## Git Commits

```
b9b3bf6 Fix clippy collapsible_match warning in tls_parser.rs
0cb4f80 Add WebSocket integration tests
e409be4 Add WebSocket support (--ws-export and --ws-import)
ca80884 refactor: update http_parser and tls_parser to use BridgeError
b1db92e feat: add BridgeError enum and BridgeConfig struct
d89b484 fix: resolve clippy errors in tests
```

---

## Remaining Work

All planned improvements have been completed. Potential future enhancements:

1. **Fix flaky test** - Update `test_http_methods` to use dynamic ports instead of hardcoded ones
2. **Secure WebSocket (wss://)** - Currently parses wss:// URLs but full TLS client support could be enhanced
3. **Config file support** - Allow BridgeConfig to be loaded from a configuration file
4. **Metrics/observability** - Add Prometheus metrics for connection counts, throughput, etc.
