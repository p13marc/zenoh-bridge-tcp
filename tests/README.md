# Integration Tests for Zenoh TCP Bridge

This directory contains integration tests for the Zenoh TCP Bridge project.

## What We're Testing

**These tests validate the BRIDGE functionality, NOT the underlying protocols.**

- We test: The bridge's ability to tunnel TCP traffic through Zenoh
- We test: Bidirectional communication through the bridge
- We test: Bridge handles various protocol patterns
- We DON'T test: HTTP/HTTPS implementation (that's axum/hyper's job)
- We DON'T test: Zenoh's internal routing (that's Zenoh's job)

## Test Suites

### `export_import_integration.rs`
End-to-end integration tests using bridge subprocesses.

**Tests:**
- `test_export_import_basic_communication` - Full round-trip through export/import bridges
- `test_multiple_clients_separate_connections` - Per-client backend connection isolation
- `test_connection_basic` - Basic connectivity without close propagation checks
- `test_bidirectional_data_flow` - Echo server validation with multiple messages
- `test_connection_close_propagation` - TCP close propagation through bridges
- `test_backend_unavailable_closes_client` - Client closed when backend unreachable
- `test_rapid_connect_disconnect` - Stress test with rapid connection cycles
- `test_concurrent_connections` - Multiple concurrent client connections
- `test_large_message_transfer` - Large message (1MB) transfer
- `test_rapid_data_send` - Rapid sequential message sending (ignored: flaky due to Zenoh session timing)
- `test_backend_restart_recovery` - Bridge recovery when backend restarts

**Architecture tested:**
```
[Client] --TCP--> [Import Bridge] --Zenoh--> [Export Bridge] --TCP--> [Backend]
```

### `bridge_integration.rs`
Basic TCP bridge library tests (in-process, no subprocesses).

- TCP echo server functionality
- Connection handling and lifecycle
- Data transfer and buffering
- Concurrent connections
- Error handling

### `http_routing_integration.rs`
HTTP routing with multiple backends (in-process).

- Multiple HTTP servers with different Host headers
- DNS normalization (case-insensitive, port stripping)
- Backend availability detection
- Concurrent clients (10 simultaneous)
- Dynamic backend registration
- Unknown DNS returns 502
- Missing Host header returns 400

### `https_routing_integration.rs`
HTTPS routing with SNI (in-process).

- SNI-based routing to multiple backends
- End-to-end TLS (no termination)
- Self-signed certificate testing
- DNS normalization for SNI
- Concurrent HTTPS clients

### `http_edge_cases.rs`
Edge cases and error handling (in-process).

- Missing/empty Host headers
- Malformed HTTP requests
- Very long headers
- Special characters in hostnames
- Various HTTP methods (GET, POST, PUT, DELETE)
- Connection lifecycle

### `http_multiroute_integration.rs`
Per-request HTTP routing with persistent connections (in-process).

- Single request routing
- Host switching on persistent connection
- 502 response for unavailable hosts (connection stays alive)

### `http_integration.rs`
HTTP/HTTPS as test protocols to validate bridge with real-world protocols.

### `ws_integration.rs`
WebSocket bridge tests using `--ws-export` and `--ws-import`.

### `drain_integration.rs`
Connection drain behavior during shutdown.

### `auto_import_integration.rs`
Protocol auto-detection (TLS, HTTP, WebSocket, raw TCP).

### `https_termination_integration.rs`
TLS termination at the bridge (requires `tls-termination` feature).

### `liveliness_integration.rs`
Zenoh liveliness token lifecycle tests.

### `multi_service_integration.rs`
Multiple concurrent services in a single bridge.

### `stress_test.rs`
Load and stress testing.

## Running Tests

```bash
# All tests (recommended)
cargo nextest run

# Unit tests only
cargo test --lib

# Specific test suite
cargo nextest run --test http_routing_integration

# With verbose output
cargo nextest run --nocapture
```

## Test Architecture

### Subprocess Tests (`export_import_integration.rs`)

These tests spawn actual bridge binaries and validate the production deployment model:

1. **Setup Backend** - Start a TCP server
2. **Start Export Bridge** - `--export 'service/backend_addr'`
3. **Start Import Bridge** - `--import 'service/listen_addr'`
4. **Connect Client** - Client connects to import bridge
5. **Validate** - Verify data flows end-to-end

### In-Process Tests (everything else)

These tests use the library API directly (`run_http_export_mode`, `run_http_import_mode`, etc.) with Zenoh sessions in the same process. They are faster and more reliable.

### Zenoh Key Design

```
Import Bridge (per client):
  - Declares:    "service/clients/client_1"  (liveliness)
  - Publishes:   "service/tx/client_1"       (client data)
  - Subscribes:  "service/rx/client_1"       (backend responses)

Export Bridge (per client liveliness):
  - Monitors:    "service/clients/*"         (detects new clients)
  - Subscribes:  "service/tx/client_1"       (receives client data)
  - Publishes:   "service/rx/client_1"       (sends backend responses)
  - Connects:    TCP to backend per client   (separate connection)
```

## Debugging Failed Tests

```bash
# Detailed output
RUST_LOG=debug cargo nextest run --nocapture --test http_routing_integration

# Focus on specific test
RUST_LOG=trace cargo nextest run test_http_routing_multiple_backends --nocapture
```

**Common issues:**
- **"Address already in use"** - Another test or process is using the port (tests use dynamic ports to minimize this)
- **"Connection refused"** - Bridge not started yet, Zenoh discovery still in progress
- **Timeout** - Zenoh session pollution from concurrent tests; try running the test in isolation

## Contributing New Tests

1. **Test the bridge, not the protocol** - Validate that bytes flow through Zenoh correctly
2. **Use dynamic ports** - Bind to `127.0.0.1:0` and use the assigned port
3. **Use unique service names** - Call `common::unique_service_name("prefix")` to avoid Zenoh key collisions
4. **Prefer in-process tests** - Use the library API instead of spawning subprocesses when possible
5. **Document your intent** - Add comments explaining what aspect of the bridge is tested
