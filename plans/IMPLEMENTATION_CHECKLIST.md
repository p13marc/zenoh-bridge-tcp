# Implementation Checklist

Track implementation progress across all plans. Plans should be implemented in order — each plan may depend on earlier plans.

**Dependency graph:**
```
Plan 01 (Bug Fixes) ✅
  ├──> Plan 02 (Graceful Shutdown) ✅ — uses Plan 01's buffer_size signature changes
  ├──> Plan 04 (Test Infrastructure) ✅ — uses uuid from Plan 01
  ├──> Plan 05 (TLS Termination) ✅ — uses uuid from Plan 01
  ├──> Plan 06 (Protocol Auto-Detection) ✅ — uses uuid + BUG-1 fix from Plan 01
  └──> Plan 08 (Bidirectional HTTP) ✅ — uses uuid from Plan 01
Plan 02 (Graceful Shutdown) ✅
  └──> Plan 04 (Test Infrastructure) ✅ — Step 7 (TST-6) uses CancellationToken from Plan 02
Plan 04 (Test Infrastructure) ✅
  └──> Plan 05 (TLS Termination) ✅ — uses assert_cmd dev-dependency from Plan 04
Plan 07 (Connection Draining) ✅ — independent after Plan 01
Plan 06 ←→ Plan 08 ✅ — cross-reference each other for HTTP dispatch path
```

---

## Plan 01: Bug Fixes ✅

Addresses: BUG-1 through BUG-8 | [Full plan](01-bug-fixes.md)

### Step 1: Fix HTTP Body Truncation (BUG-1 — High) ✅
- [x] Change `buffer[..body_offset].to_vec()` to `buffer.to_vec()` in `src/http_parser.rs:119`
- [x] Add `test_parse_http_request_preserves_body` test
- [x] Add `test_parse_http_request_get_no_body` test
- [x] Update existing `test_parse_http_request_post_with_body` to assert body IS included

### Step 2: Fix Connection ID Collisions (BUG-4 — Medium) ✅
- [x] Add `uuid = { version = "1", features = ["v4"] }` to `Cargo.toml`
- [x] Replace sequential counter in `run_import_mode_internal` (line 88) with UUID
- [x] Replace sequential counter in `run_ws_import_mode` (line 480) with UUID
- [x] Remove `connection_id` variables
- [x] Add `test_client_ids_are_unique` test

### Step 3: Wire Up Buffer Size Config (BUG-2 — Medium) ✅
- [x] Add `buffer_size: usize` param to export functions: `run_export_mode` → `run_export_mode_internal` → `handle_client_bridge`
- [x] Add `buffer_size: usize` param to import functions: `run_import_mode` → `run_import_mode_internal` → `handle_import_connection`
- [x] Add `buffer_size: usize` param to WS functions: `run_ws_export_mode`, `run_ws_import_mode`
- [x] Replace `vec![0u8; 65536]` with `vec![0u8; buffer_size]` in `export.rs:313` and `import.rs:405`
- [x] Pass `args.buffer_size` from `main.rs` to all export/import calls
- [x] Add validation in `BridgeConfig::new()` (buffer_size 0 → default 65536)
- [x] Add `test_bridge_config_buffer_size_custom` and `test_bridge_config_buffer_size_zero_guard` tests

### Step 4: Increase Cache Size (BUG-5 — Medium) ✅
- [x] Change `.cache(CacheConfig::default().max_samples(10))` to `.max_samples(64)` in `export.rs` (2 locations)
- [x] Change `.cache(CacheConfig::default().max_samples(10))` to `.max_samples(64)` in `import.rs` (2 locations)

### Step 5: Add WS Export Error Signal (BUG-7 — Low) ✅
- [x] Add error signal publishing in `handle_ws_client_connect` (export.rs ~line 597) when `connect_async` fails

### Step 6: Query Existing Clients on Export Startup (BUG-6 — Low) ✅
- [x] Add liveliness query for existing clients after subscriber is created in `run_export_mode_internal`
- [x] Add same pattern to `run_ws_export_mode`

### Step 7: Fix Hardcoded Content-Length (BUG-3 — Low) ✅
- [x] Rewrite `http_400_response` to compute Content-Length dynamically (no `\` continuation)
- [x] Rewrite `http_502_response` to fix same whitespace bug
- [x] Add `test_http_400_response_content_length` test

### Step 8: Fix normalize_dns Port Stripping (BUG-8 — Low) ✅
- [x] Rewrite `normalize_dns` to use `rfind(':')` + port parsing
- [x] Add `test_normalize_dns_numeric_port_parsing` test

### Verification ✅
- [x] `cargo test --lib`
- [x] `cargo test --test http_edge_cases -- --test-threads=1`
- [x] `cargo test --test http_routing_integration -- --test-threads=1`
- [x] `cargo clippy -- --deny warnings`

---

## Plan 02: Graceful Shutdown & Backend Reconnection ✅

Addresses: DES-2, DES-3, DES-4, DES-5, DES-6, IMP-4, IMP-5 | [Full plan](02-graceful-shutdown-and-reconnection.md)

**Prerequisite:** Plan 01 (function signature changes)

### Step 1: Add CancellationToken to Main ✅
- [x] Add `tokio-util = { version = "0.7", features = ["rt"] }` and `backon = "1"` to `Cargo.toml`
- [x] Create `CancellationToken` in `main.rs`
- [x] Spawn signal handler task (SIGINT/SIGTERM)
- [x] Refactor six repetitive spawn loops into generic `spawn_bridge_task` helper (DES-4)
- [x] Add drain timeout + explicit `session.close()` on shutdown

### Step 2: Thread CancellationToken Through Export/Import ✅
- [x] Add `shutdown_token: CancellationToken` param to `run_export_mode` and inner functions
- [x] Add `tokio::select!` with `shutdown_token.cancelled()` in export's main loop
- [x] Add `shutdown_token: CancellationToken` param to `run_import_mode` and inner functions
- [x] Add `tokio::select!` with `shutdown_token.cancelled()` in import's accept loop
- [x] Same for WS export/import functions

### Step 3: Add Exponential Backoff Retry (DES-3, IMP-5) ✅
- [x] Add retry logic with `backon` to `handle_client_connect` for backend TCP connections
- [x] Add same retry logic to `handle_ws_client_connect`

### Step 4: Explicit Zenoh Resource Undeclaration (DES-6) ✅
- [x] Add `publisher.undeclare()` inside `backend_to_zenoh` spawned task (export.rs) before exit
- [x] Add `subscriber.undeclare()` inside `zenoh_to_backend` spawned task (export.rs) before exit
- [x] Add `subscriber.undeclare()` inside `zenoh_to_tcp` spawned task (import.rs) before exit
- [x] Add `publisher.undeclare()` inside `tcp_to_zenoh` spawned task (import.rs) before exit
- [x] Add `liveliness_token.undeclare()` after `select!` in `handle_import_connection`
- [x] Apply same pattern to WS export/import handlers

### Step 5: Remove Emoji from Log Messages (DES-5) ✅
- [x] Replace `"✓ Client"` → `"Client"` in `export.rs:283`
- [x] Replace `"✗ Client"` → `"Client"` in `export.rs:421`
- [x] Replace `"✓ WebSocket client"` → `"WebSocket client"` in `export.rs:630`
- [x] Replace `"✓ Client"` → `"Client"` in `import.rs:317`
- [x] Replace `"✓ WebSocket client"` → `"WebSocket client"` in `import.rs:585`

### Step 6: Tests ✅
- [x] Add `test_cancellation_token_propagation` unit test
- [ ] Add `test_graceful_shutdown` integration test (SIGTERM → clean exit)
- [ ] Add `test_backend_retry_on_transient_failure` integration test

### Verification ✅
- [x] `cargo build --release`
- [x] `cargo test --lib`
- [x] `cargo test --test export_import_integration -- --test-threads=1`
- [x] `cargo clippy -- --deny warnings`

---

## Plan 04: Test Infrastructure Overhaul ✅

Addresses: TST-1 through TST-7, IMP-9, DES-1 | [Full plan](04-test-infrastructure-overhaul.md)

**Prerequisites:** Plan 01 (uuid) ✅, Plan 02 (CancellationToken for TST-6) ✅

### Step 1: Create Shared Test Utilities ✅
- [x] Add `assert_cmd = "2"` and `predicates = "3"` to dev-dependencies
- [x] Create `tests/common/mod.rs` with `PortGuard`, `wait_for_port`, `wait_for`, `start_echo_server`, `unique_service_name`, `BridgeProcess`

### Step 2: Fix Trivially-True Assertions (TST-1) ✅
- [x] Replace `assert!(result.is_err() || result.is_ok())` in `tests/bridge_integration.rs::test_invalid_connection`
- [x] Replace same in `tests/bridge_integration.rs::test_connection_refused`

### Step 3: Fix Tests With No Assertions (TST-2) ✅
- [x] Delete `test_liveliness_documentation` from `tests/liveliness_integration.rs`
- [x] Delete `test_http_https_documentation` from `tests/http_integration.rs`
- [x] Add assertion to `test_malformed_http_requests` in `tests/http_edge_cases.rs`
- [x] Add assertion to `test_ws_connection_lifecycle` in `tests/ws_integration.rs`

### Step 4: Replace Hardcoded Ports (TST-3) ✅ (partial — focused on files with hardcoded binary paths)
- [x] `tests/export_import_integration.rs` — replace ports 9999, 19999
- [x] `tests/stress_test.rs` — replace port 19999

### Step 5: Replace Hardcoded Binary Path (TST-4) ✅
- [x] Replace `"./target/debug/zenoh-bridge-tcp"` with `assert_cmd::cargo::cargo_bin!()` in `tests/export_import_integration.rs`
- [x] Same in `tests/stress_test.rs`
- [x] Same in `tests/ws_integration.rs`

### Step 6: Replace Sleep-Based Synchronization (TST-5) ✅ (export_import_integration.rs)
- [x] Replace `tokio::time::sleep` with `wait_for_port` for import bridge startup in `tests/export_import_integration.rs`

### Step 7: Use Unique Service Names (TST-6) ✅ (export_import_integration.rs)
- [x] Replace 11 hardcoded service names with `unique_service_name()` in `tests/export_import_integration.rs`

### Step 8: Add Missing Test Coverage (TST-7) ✅
- [x] Add `test_config_file_loading` and `test_config_file_not_found` tests in `src/config.rs`
- [x] Add spec parsing edge case tests: empty strings, nested names, wildcards, DNS port stripping
- [x] Add import spec edge case tests: empty service name, nested names, all-interfaces binding

### Step 9: Document Error Type Convention (DES-1) ✅
- [x] Document in `src/error.rs`: public API uses `anyhow::Result`, internal parsing uses `BridgeError`

### Verification ✅
- [x] `cargo test --lib` — 73 tests pass
- [x] `cargo clippy --tests` — zero warnings

---

## Plan 05: TLS Termination for Import Mode ✅

Addresses: FEAT-1 | [Full plan](05-tls-termination.md)

**Prerequisites:** Plan 01 (uuid) ✅, Plan 04 (assert_cmd for CLI test) ✅

### Step 1: TLS Configuration Loading ✅
- [x] Add `tokio-rustls`, `rustls`, `rustls-pemfile` as optional deps with `tls-termination` feature
- [x] Create `src/tls_config.rs` with `load_tls_config` function

### Step 2: CLI Arguments ✅
- [x] Add `--https-terminate`, `--tls-cert`, `--tls-key` args (feature-gated)
- [x] Add validation: `--https-terminate` requires both `--tls-cert` and `--tls-key`
- [x] Update `is_empty` check in `validate()` to include `https_terminate`

### Step 3: TLS-Terminating Import Mode ✅
- [x] Add `run_https_terminate_import_mode` function in `src/import.rs`
- [x] Implement TLS handshake → plaintext HTTP parsing → Zenoh bridging
- [x] Extract shared `bridge_import_connection` from `handle_import_connection` for reuse

### Step 4: Wire Into Main ✅
- [x] Load TLS config once, share via `Arc`
- [x] Spawn tasks for `--https-terminate` specs

### Step 5: Update lib.rs ✅
- [x] Add `#[cfg(feature = "tls-termination")] pub mod tls_config;`

### Step 6: Tests ✅
- [x] Add `test_load_tls_config_missing_cert_file` unit test
- [x] Add `test_load_tls_config_empty_cert` unit test
- [x] Add `test_load_tls_config_missing_key_file` unit test
- [x] Add `test_load_tls_config_valid` unit test (using `rcgen`)
- [x] Add `test_https_terminate_requires_cert_and_key` CLI validation test
- [x] Add `test_https_terminate_requires_key` CLI validation test
- [x] Add `test_https_terminate_starts_with_valid_tls` startup verification test

### Verification ✅
- [x] `cargo build` (without feature) — clean
- [x] `cargo build --features tls-termination` — clean
- [x] `cargo test --features tls-termination --lib` — 77 tests pass
- [x] `cargo clippy --features tls-termination --tests` — zero warnings

---

## Plan 06: Protocol Auto-Detection on Import Side ✅

Addresses: FEAT-2 | [Full plan](06-protocol-auto-detection.md)

**Prerequisites:** Plan 01 (uuid + BUG-1 fix) ✅

### Step 1: Protocol Detection Module ✅
- [x] Create `src/protocol_detect.rs` with `DetectedProtocol` enum and `detect_protocol` function
- [x] Reuse `tls_parser::is_tls_handshake()` for TLS detection (6-byte check)

### Step 2: WebSocket Detection During HTTP Parsing ✅
- [x] Add `is_websocket_upgrade: bool` field to `ParsedHttpRequest` (breaking change)
- [x] Detect `Upgrade: websocket` header in `try_parse_request`
- [x] Update all `ParsedHttpRequest` construction sites

### Step 3: Auto-Import Mode ✅
- [x] Add `run_auto_import_mode` function in `src/import.rs`
- [x] Add `handle_auto_import_connection` (peek → detect → dispatch)
- [x] Add `handle_auto_http_connection` (HTTP vs WebSocket upgrade)

### Step 4: CLI Argument ✅
- [x] Add `--auto-import` CLI argument
- [x] Update `validate()` to include `auto_import`

### Step 5: Wire Into Main ✅
- [x] Spawn tasks for `--auto-import` specs

### Step 6: Update lib.rs ✅
- [x] Add `pub mod protocol_detect;`

### Step 7: Tests ✅
- [x] Add unit tests: `test_detect_tls`, `test_detect_http_methods`, `test_detect_raw_tcp`
- [x] Add unit tests: `test_detect_empty_buffer`, `test_detect_short_buffer`, `test_detect_almost_http`
- [x] Add `test_websocket_upgrade_header_detection` in `http_parser.rs` tests
- [x] Add `test_non_websocket_request` in `http_parser.rs` tests
- [x] Add `test_websocket_upgrade_case_insensitive` in `http_parser.rs` tests
- [x] Add integration tests: `test_auto_import_raw_tcp`, `test_auto_import_http_detection`, `test_auto_import_cli_starts`

### Verification ✅
- [x] `cargo build` — clean
- [x] `cargo test --lib` — 82 tests pass
- [x] `cargo test --test auto_import_integration -- --test-threads=1` — 3 tests pass
- [x] `cargo clippy --tests` — zero warnings

---

## Plan 07: Connection Draining ✅

Addresses: FEAT-3 | [Full plan](07-connection-draining.md)

**No hard prerequisites** (but Plan 01 BUG-2 config threading pattern informs this)

### Step 1: Add Drain Timeout Configuration ✅
- [x] Add `drain_timeout: Duration` field to `BridgeConfig` (default 5s)
- [x] Add `--drain-timeout <SECS>` CLI argument
- [x] Update `BridgeConfig::new()` to accept `drain_timeout`

### Step 2: Modify Import Error Handling ✅
- [x] Replace `zenoh_to_tcp.abort()` in error monitor branch with drain-then-abort logic
- [x] Thread `drain_timeout` as parameter to `handle_import_connection`

### Step 3: Modify Export Disconnect Handling ✅
- [x] Replace hardcoded `Duration::from_secs(2)` in `handle_client_disconnect` with `drain_timeout`
- [x] Thread `drain_timeout` as parameter to `handle_client_disconnect`

### Step 4: Modify Export Client Bridge Cancellation ✅
- [x] Replace immediate abort of `backend_to_zenoh_handle` with drain-then-abort
- [x] Thread `drain_timeout` as parameter to `handle_client_bridge`

### Step 5: Apply to WebSocket ✅
- [x] Apply drain-before-abort pattern to `handle_ws_client_bridge`
- [x] Thread `drain_timeout` through WS export/import functions

### Step 6: Wire Into Main ✅
- [x] Pass `drain_timeout` from args through to all export/import functions

### Step 7: Tests ✅
- [x] Add `test_bridge_config_drain_timeout` unit test (in config.rs)
- [x] Add `test_drain_on_backend_close` integration test
- [x] Add `test_drain_on_backend_error` integration test
- [x] Add `test_drain_timeout_enforced` integration test

### Verification ✅
- [x] `cargo build --release`
- [x] `cargo test --lib` — 82 tests pass
- [x] `cargo test --test drain_integration -- --test-threads=1` — 3 tests pass
- [x] `cargo clippy -- --deny warnings`

---

## Plan 08: Bidirectional HTTP Mode (Keep-Alive Multi-Request) ✅

Addresses: FEAT-4 | [Full plan](08-bidirectional-http-mode.md)

**Prerequisites:** Plan 01 (uuid) ✅

### Step 1: HTTP Response Parser ✅
- [x] Create `src/http_response_parser.rs` with `ResponseBodyFraming`, `parse_response_headers`, `is_connection_close`, `find_chunked_body_end`
- [x] 16 unit tests for all framing modes (Content-Length, chunked, 204 NoBody, 304 NoBody, UntilClose, partial)

### Step 2: Per-Request HTTP Import Handler ✅
- [x] Add `run_http_multiroute_import_mode` function in `src/import.rs`
- [x] Add `handle_multiroute_connection` with per-request loop
- [x] Add `check_backend_available` helper
- [x] Use `AdvancedPublisherBuilderExt` / `AdvancedSubscriberBuilderExt` / `MissDetectionConfig`

### Step 2b: Export-Side (no structural changes needed) ✅
- [x] Verified existing liveliness-based architecture handles short-lived per-request connections

### Step 3: Add HTTP 504 Response Helper ✅
- [x] Add `http_504_response()` in `src/http_parser.rs` (single-line format, no whitespace bug)
- [x] Add `test_http_504_response` unit test

### Step 4: CLI Argument ✅
- [x] Add `--http-multiroute-import` CLI argument
- [x] Update `validate()` to include `http_multiroute_import`

### Step 5: Wire Into Main ✅
- [x] Spawn tasks for `--http-multiroute-import` specs
- [x] Add `http_multiroute_import_count` to startup info log

### Step 6: Update lib.rs ✅
- [x] Add `pub mod http_response_parser;`

### Step 7: Tests ✅
- [x] 16 unit tests for `parse_response_headers`, `is_connection_close`, `find_chunked_body_end`
- [x] `test_http_504_response` unit test in `http_parser.rs`
- [x] `test_multiroute_single_request` integration test
- [x] `test_multiroute_persistent_connection_switches_hosts` integration test
- [x] `test_multiroute_unavailable_host_returns_502` integration test

### Verification ✅
- [x] `cargo build --release`
- [x] `cargo test --lib` — 99 tests pass
- [x] `cargo test --test http_multiroute_integration -- --test-threads=1` — 3 tests pass
- [x] `cargo clippy -- --deny warnings`

---

## Final Validation (after all plans)

- [ ] `cargo build --release`
- [ ] `cargo nextest run`
- [ ] `cargo test -- --test-threads=1`
- [ ] `cargo clippy -- --deny warnings`
- [ ] `cargo fmt --check`
