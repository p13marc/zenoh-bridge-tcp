# Integration Tests for Zenoh TCP Bridge

This directory contains integration tests for the Zenoh TCP Bridge project.

## 🎯 Important: What We're Testing

**These tests validate the BRIDGE functionality, NOT the underlying protocols.**

- ✅ We test: The bridge's ability to tunnel TCP traffic through Zenoh
- ✅ We test: Bidirectional communication through the bridge
- ✅ We test: Bridge handles various protocol patterns
- ❌ We DON'T test: HTTP/HTTPS implementation (that's axum/hyper's job)
- ❌ We DON'T test: Zenoh's internal routing (that's Zenoh's job)

## Test Files

### `export_import_integration.rs` 🆕
**End-to-end integration tests for export/import bridge modes.**

This test suite spawns actual bridge processes and validates the complete data flow through the system.

**Tests:**
- `test_export_import_basic_communication` - Full round-trip communication through export/import bridges
- `test_multiple_clients_separate_connections` - Verifies each client gets its own backend connection
- `test_connection_basic` - Basic connectivity without close propagation checks
- `test_bidirectional_data_flow` - Echo server validation with multiple messages
- `test_connection_close_propagation` - Verifies TCP close propagates quickly through bridges
- `test_backend_unavailable_closes_client` - Verifies client is closed immediately when backend is unreachable
- `test_rapid_connect_disconnect` - Stress test with rapid connection/disconnection cycles
- `test_concurrent_connections` - Tests multiple concurrent client connections
- `test_large_message_transfer` - Validates large message (1MB) transfer through bridges
- `test_rapid_data_send` - Tests rapid sequential message sending
- `test_backend_restart_recovery` - Verifies bridge recovers when backend restarts

**Architecture tested:**
```
[Client] ──TCP──> [Import Bridge] ──Zenoh──> [Export Bridge] ──TCP──> [Backend]
                  (testservice/    liveliness    (testservice/         (actual
                   listen_addr)    tracking       backend_addr)         server)
```

**Key validations:**
- ✓ Import bridge accepts TCP connections
- ✓ Export bridge creates separate backend connections per client
- ✓ Data flows bidirectionally through Zenoh
- ✓ Multiple clients are handled independently
- ✓ Liveliness tokens track client lifecycle

**Recent fixes:**
- ✅ TCP close propagation now works correctly (fixed by aborting spawned tasks)
- ✅ Backend connections close within 2 seconds when client disconnects
- ✅ Clients are immediately closed when backend is unreachable (fixed subscription race condition)

**Test categories:**
- **Core functionality:** Basic communication, bidirectional data, per-client isolation
- **Reliability:** Connection close, backend unavailable, error handling
- **Stress tests:** Rapid connections, concurrent clients, large messages
- **Recovery:** Backend restart scenarios

**Why these tests:**
- Tests actual bridge binaries (not just library code)
- Validates real-world deployment scenario
- Ensures per-client connection isolation
- Stress tests ensure robustness under load
- No external protocol dependencies

### `bridge_integration.rs`
Basic TCP bridge functionality tests without external protocol dependencies.

**Tests:**
- TCP echo server functionality
- Connection handling and lifecycle
- Data transfer and buffering
- Concurrent connections
- Error handling

**Why these tests:**
- No external dependencies (no protoc required)
- Fast execution
- Core bridge functionality validation

### `http_integration.rs`
Tests using HTTP/HTTPS as **test protocols** to validate bridge with real-world protocols.

**Tests:**
- `test_http_through_bridge` - HTTP bridging test
- `test_https_through_bridge` - HTTPS bridging with TLS
- `test_multiple_http_requests` - Multiple sequential requests
- `test_http_https_documentation` - Explains test philosophy

**What we're actually testing:**
- Bridge accepts TCP connections ✓
- Bridge forwards data through Zenoh ✓
- Two bridge instances can communicate ✓
- Bridge maintains bidirectional streams ✓
- HTTP/HTTPS protocols work through the bridge ✓

## Running Tests

### Run all tests
```bash
cargo test
```

### Run with output
```bash
cargo test -- --nocapture
```

### Run specific test file
```bash
cargo test --test export_import_integration  # Export/import integration tests
cargo test --test bridge_integration          # Basic bridge tests
cargo test --test grpc_integration            # gRPC bridge tests
```

### Run specific test
```bash
cargo test test_export_import_basic_communication -- --nocapture
cargo test test_multiple_clients_separate_connections -- --nocapture
cargo test test_bridge_with_grpc_unary -- --nocapture
cargo test test_grpc_direct_connection_works -- --nocapture
```

### Run only fast tests (no gRPC)
```bash
cargo test --test bridge_integration
cargo test --test export_import_integration
```

### Run export/import tests with single thread (recommended)
```bash
cargo test --test export_import_integration -- --test-threads=1 --nocapture
```
**Note:** 
- Export/import tests spawn bridge processes and should run sequentially to avoid port conflicts.
- Tests use debug binaries (`target/debug/zenoh-bridge-tcp`) automatically built by cargo test.

## Requirements

### For `export_import_integration.rs`
- Tests spawn actual bridge processes (debug binary)
- Requires available TCP ports (tests use random ports)
- Zenoh peer discovery must work (uses shared memory transport in tests)
- No pre-build required - cargo test builds debug binary automatically

### For `bridge_integration.rs`
- No special requirements
- Pure Rust, no external tools

### For `grpc_integration.rs`
- **protoc** (Protocol Buffer Compiler) must be installed
- Automatically checked during build
- Tests are skipped if protoc is not available

**Installing protoc:**

```bash
# Debian/Ubuntu
sudo apt-get install protobuf-compiler

# Fedora/RHEL
sudo dnf install protobuf-compiler

# macOS
brew install protobuf

# Or download from:
# https://github.com/protocolbuffers/protobuf/releases
```

## Test Architecture Details

### Export/Import Test Pattern

The export/import tests validate the production bridge deployment:

1. **Setup Backend** - Start a TCP server (e.g., echo server, test backend)
2. **Build Bridge** - Compile the release binary
3. **Start Export Bridge** - `--export 'service/backend_addr'` connects to backend
4. **Start Import Bridge** - `--import 'service/listen_addr'` accepts clients
5. **Connect Client** - Client connects to import bridge
6. **Validate** - Verify: Client → Import → Zenoh → Export → Backend → (response back)

**Key differences from library tests:**
- Tests spawn real bridge processes (not in-process)
- Uses actual CLI arguments (`--export`, `--import`)
- Validates liveliness token lifecycle
- Tests per-client backend connection isolation

### Bridge Test Pattern

Each bridge test follows this pattern:

1. **Setup Backend** - Start a TCP server (e.g., gRPC server)
2. **Create Zenoh Sessions** - Set up peer sessions for bridge instances
3. **Start Bridge B** (Server-side) - Subscribes to client requests, connects to backend
4. **Start Bridge A** (Client-side) - Accepts client connections, publishes to Zenoh
5. **Connect Client** - Client connects to Bridge A (NOT directly to backend)
6. **Validate** - Verify data flows: Client → Bridge A → Zenoh → Bridge B → Backend

### Zenoh Key Design

**Export/Import Mode:**
The export/import bridges use per-client keys with liveliness tracking:

- `service/tx/<client_id>` - Client → Backend (import publishes, export subscribes)
- `service/rx/<client_id>` - Backend → Client (export publishes, import subscribes)
- `service/clients/<client_id>` - Liveliness token (import declares, export monitors)

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

**Library Mode:**
The bridge uses two Zenoh keys for bidirectional communication:

- `bridge/client_to_server` - Client requests flow through here
- `bridge/server_to_client` - Server responses flow back through here

```
Bridge A publishes to:   "bridge/client_to_server"
Bridge A subscribes to:  "bridge/server_to_client"

Bridge B subscribes to:  "bridge/client_to_server"
Bridge B publishes to:   "bridge/server_to_client"
```

### Why Two Bridge Instances?

Real deployment scenario:
```
[Public Network]          [Zenoh Network]          [Private Network]
                                                    
 gRPC Client  ──┐                            ┌──  gRPC Server
 Web Client   ──┼──> Bridge A ─┐   ┌─> Bridge B ─┼─── Database
 Mobile App   ──┘              │   │              └─── Internal API
                             Zenoh Mesh
                                │   │
                            (distributed,
                             peer-to-peer)
```

Bridge A is client-facing, Bridge B is server-facing. They communicate through Zenoh.

## Test Philosophy

### The Right Level of Testing

```
┌─────────────────────────────────────────────────┐
│  NOT OUR JOB (tested by upstream projects)      │
│  - gRPC correctness (Tonic's tests)             │
│  - Protobuf encoding (protobuf's tests)         │
│  - Zenoh routing (Zenoh's tests)                │
└─────────────────────────────────────────────────┘
                         ▼
┌─────────────────────────────────────────────────┐
│  OUR JOB (what we test)                         │
│  - Bridge accepts TCP connections               │
│  - Bridge forwards bytes through Zenoh          │
│  - Bridge maintains connection semantics        │
│  - Bridge works with real protocols             │
└─────────────────────────────────────────────────┘
                         ▼
┌─────────────────────────────────────────────────┐
│  USER'S JOB (what users verify)                 │
│  - Their application works through bridge       │
│  - Performance meets requirements               │
│  - Security and reliability in production       │
└─────────────────────────────────────────────────┘
```

### Why This Matters

**Anti-pattern:** Testing that Tonic works
```rust
// ❌ BAD - This tests Tonic, not the bridge
#[test]
fn test_grpc_serialization() {
    let msg = EchoRequest { ... };
    let bytes = msg.encode_to_vec();
    assert!(bytes.len() > 0);  // Testing protobuf, not bridge!
}
```

**Good pattern:** Testing that bridge tunnels gRPC
```rust
// ✅ GOOD - This tests the bridge with gRPC as payload
#[test]
async fn test_bridge_with_grpc() {
    let server = start_grpc_server();        // Backend
    let bridge_a = start_bridge(...);        // Client-facing
    let bridge_b = start_bridge(...);        // Server-facing
    
    let client = connect_to_bridge_a();      // Connect to bridge, not server!
    let response = client.call();            // gRPC call through bridge
    
    assert!(response.is_ok());               // Bridge worked!
}
```

## Expected Test Behavior

### `export_import_integration.rs`
- `test_export_import_basic_communication` - Should PASS ✓
- `test_multiple_clients_separate_connections` - Should PASS ✓
- `test_connection_basic` - Should PASS ✓
- `test_bidirectional_data_flow` - Should PASS ✓
- `test_connection_close_propagation` - Should PASS ✓ (recently fixed!)
- `test_backend_unavailable_closes_client` - Should PASS ✓ (race condition fixed!)
- `test_rapid_connect_disconnect` - Should PASS ✓ (stress test)
- `test_concurrent_connections` - Should PASS ✓ (concurrency test)
- `test_large_message_transfer` - Should PASS ✓ (1MB message test)
- `test_rapid_data_send` - Should PASS ✓ (100 messages rapidly)
- `test_backend_restart_recovery` - Should PASS ✓ (recovery test)
- Execution time: ~24 seconds (spawns processes, waits for Zenoh, includes stress tests)
- No pre-build required (uses debug binary)

**Recent Fixes:**
- ✅ TCP close propagation now works correctly (abort spawned tasks)
- ✅ Backend connections abort spawned tasks when client disconnects
- ✅ Close detection happens within 2 seconds (previously 30+ seconds)
- ✅ Clients are closed immediately when backend is unreachable (fixed subscription race condition)
- ✅ Import bridge now subscribes to error channel BEFORE declaring liveliness

### `bridge_integration.rs`
- All tests should PASS ✓
- Fast execution (< 5 seconds total)
- No external dependencies

### `grpc_integration.rs`

**Baseline Test** (`test_grpc_direct_connection_works`):
- Should always PASS ✓
- Proves gRPC/Tonic is working
- No bridge involved

**Documentation Test** (`test_documentation_what_we_test`):
- Always PASS ✓
- Prints explanation of test philosophy
- No actual functionality tested

**Bridge Test** (`test_bridge_with_grpc_unary`):
- May PASS or FAIL depending on bridge implementation ⚠️
- Success means: Bridge successfully tunneled gRPC traffic
- Failure means: Bridge needs work (NOT that gRPC is broken)
- Connection success alone validates basic bridge functionality

### Interpreting Results

```
✅ bridge_integration.rs: All pass
✅ test_grpc_direct_connection_works: PASS
✅ test_documentation_what_we_test: PASS
⚠️  test_bridge_with_grpc_unary: May pass or fail

If bridge test fails:
  → Bridge implementation needs improvement
  → NOT a problem with Tonic, protobuf, or Zenoh
  → Check: Connection handling, bidirectional flow, HTTP/2 compatibility
```

## Debugging Failed Tests

### Export/import test fails

**Check:**
1. Is the bridge binary built? (`cargo build --release`)
2. Are TCP ports available? (tests use random ports but check for conflicts)
3. Is Zenoh peer discovery working? (tests use shared memory by default)
4. Are bridge processes starting? (check stdout/stderr from spawned processes)
5. Check timing - increase sleep durations if needed

**Common issues:**
- **"Address already in use"** - Another test or bridge process is running
- **"Connection refused"** - Bridge not started yet, increase startup delay
- **Test hangs** - Likely waiting for TCP close that won't come (see known issues)
- **No response received** - Check Zenoh session connection and key names

### Bridge test fails but direct gRPC works

This is **expected** and means:
- ✓ gRPC is working (baseline passed)
- ✗ Bridge needs improvement

**Check:**
1. Are both bridge instances running?
2. Are Zenoh sessions connected?
3. Are the pub/sub keys correct?
4. Is data flowing bidirectionally?
5. Are connections maintained properly?

### Enable detailed logging

```bash
# See all bridge activity
RUST_LOG=debug cargo test -- --nocapture

# Focus on specific test
RUST_LOG=trace cargo test test_bridge_with_grpc_unary -- --nocapture

# See Zenoh activity
RUST_LOG=zenoh=debug cargo test -- --nocapture
```

### Common Issues

**"Address already in use"**
- Another test or process is using the port
- Kill the process or wait a moment
- Tests use different ports to minimize conflicts

**"Connection timeout"**
- Bridge may not be starting fast enough
- Increase sleep durations in test
- Check Zenoh session is established

**"Protocol error"**
- HTTP/2 connection handling issue
- Expected with current simple bridge implementation
- Doesn't invalidate basic bridge functionality

**"Backend didn't detect close"**
- ✅ This issue has been fixed!
- Backend connections now abort spawned tasks on cancellation
- If you see this error, check that you're running the latest code
- Close should complete within 2 seconds

**"Client not closed when backend unavailable"**
- ✅ This issue has been fixed!
- Race condition resolved by subscribing to error channel before declaring liveliness
- Client connections now close within 1 second when backend is unreachable
- If you see this error, verify bridges are starting correctly

## Contributing New Tests

### Guidelines

1. **Test the bridge, not the protocol**
   - Bad: Testing protobuf serialization
   - Good: Testing bridge forwards protobuf-encoded data

2. **Use realistic scenarios**
   - Bad: Sending "hello" string
   - Good: Using real protocol (gRPC, HTTP, etc.)

3. **Isolate what you're testing**
   - Always have a baseline test (without bridge)
   - Shows that protocol works before involving bridge

4. **Make failures informative**
   - Explain WHAT failed (connection? data transfer? ordering?)
   - Not just "test failed"

5. **Document your intent**
   - Add comments explaining what aspect of bridge is tested
   - Explain why this test matters

### Example Template

```rust
/// Test: Bridge preserves message ordering
/// 
/// What we test: Bridge maintains order of TCP packets
/// What we don't test: TCP's ordering mechanism itself
#[tokio::test]
async fn test_bridge_preserves_ordering() {
    // 1. Setup: Start backend server
    let server = start_tcp_server();
    
    // 2. Setup: Start bridge instances
    let bridge_a = start_bridge(...);
    let bridge_b = start_bridge(...);
    
    // 3. Test: Send ordered messages through bridge
    let client = connect_to_bridge_a();
    for i in 0..100 {
        client.send(i).await;
    }
    
    // 4. Verify: Order preserved at server
    let received = server.get_messages();
    assert_eq!(received, (0..100).collect());
    
    // This proves the BRIDGE maintained order,
    // not testing TCP's ordering (that's TCP's job)
}
```

## CI/CD Integration

These tests are designed for automated testing:

```yaml
# .github/workflows/test.yml
- name: Install protoc
  run: |
    sudo apt-get update
    sudo apt-get install -y protobuf-compiler

- name: Run export/import integration tests
  run: cargo test --test export_import_integration -- --test-threads=1 --nocapture

- name: Run basic tests
  run: cargo test --test bridge_integration

- name: Run gRPC bridge tests
  run: cargo test --test grpc_integration -- --nocapture
```

## Performance Notes

- **export_import_integration.rs**: 24 seconds (spawns processes, Zenoh discovery, stress tests)
- **bridge_integration.rs**: < 5 seconds
- **grpc_integration.rs**: 5-15 seconds (includes server startup, Zenoh discovery)
- Total suite: ~40 seconds

Tests are optimized for CI environments and should run reliably on any platform.

**Note:** Export/import tests spawn separate bridge processes, which adds overhead:
- Spawn separate bridge processes (debug builds)
- Wait for Zenoh peer discovery between processes
- Use real TCP sockets and connections
- Include stress tests (rapid connections, large messages, concurrent clients)
- Comprehensive coverage takes ~24 seconds for 11 tests

## Further Reading

- [Bridge Architecture](../README.md) - Overall project documentation
- [Zenoh Documentation](https://zenoh.io/docs/) - Understanding Zenoh
- [Tonic Guide](https://github.com/hyperium/tonic) - gRPC in Rust
- [Testing Async Rust](https://tokio.rs/tokio/topics/testing) - Tokio testing patterns

## Summary

> **Remember: We're testing the BRIDGE, not the protocols!**
> 
> - **Export/Import tests** validate the production deployment model with actual bridge processes
> - **gRPC tests** use gRPC as a test protocol to validate complex protocol handling
> - **Basic tests** validate core TCP bridging functionality
>
> The bridge is protocol-agnostic—it tunnels TCP, regardless of what runs on top.
>
> If tests fail, it means the bridge needs work, not the test protocols.
> If tests pass, it means the bridge successfully tunnels real-world traffic through Zenoh!

## Test Coverage Summary

| Test Suite | What It Tests | Dependencies | Speed | Status |
|------------|---------------|--------------|-------|--------|
| `export_import_integration.rs` | Production export/import mode, process spawning, liveliness, error handling, stress tests | None (uses debug binary) | Medium (~24s) | ✓ 11 pass (100% reliable) |
| `bridge_integration.rs` | Core TCP bridge library | None | Fast (<5s) | ✓ All pass |
| `grpc_integration.rs` | Complex protocol handling | protoc | Medium (~10s) | ⚠️ Varies |

**Total: ~40 seconds for complete test suite**

---

**Last Updated**: 2024
**Test Framework**: Tokio + Tonic  
**Purpose**: Validate TCP bridge functionality with real-world protocols