# Changelog

## [0.5.0] - 2026-04-08

### Added

- **Deep audit and 16 bug fixes**: Comprehensive code audit identified and resolved 16 bugs across HTTP parsing, TLS handling, shutdown correctness, and CLI validation
- **Strict SNI validation**: TLS ClientHello SNI parsing now enforces RFC 6066 and RFC 1035 hostname rules (253-byte limit, 63-byte labels, no leading/trailing hyphens, ASCII-only, no trailing dots)
- **CLI argument validation**: Early validation of `--buffer-size` (minimum 1024), `--drain-timeout` (minimum 1s), `--log-format`, `--log-level`, and all spec formats before starting the bridge
- **Import task tracking**: Import listener uses `JoinSet` for per-connection task tracking with graceful drain on shutdown
- **29 new tests**: Coverage integration tests (large messages up to 200KB, partial transfers, concurrent clients, rapid connect/disconnect cycles) and 16 bug fix verification tests
- **Bug demonstration test suite**: `tests/bug_demonstrations.rs` with 16 tests verifying each audit fix

### Fixed

- HTTP response smuggling: reject requests with both Transfer-Encoding and Content-Length headers
- Duplicate Content-Length header validation
- Content-Length bounds checking (max 1GB)
- Chunked transfer encoding overflow safety
- Empty client ID rejection
- Export reconnect ordering: cancel old connection before spawning new one
- Explicit cancellation signal send instead of relying on drop
- Mutex release before await in export shutdown path
- Multiroute 504 response guard for unavailable backends
- TLS handshake size consistency validation (max 16KB)
- Main process drain timeout now uses configured value instead of hardcoded default

### Changed

- **Zenoh upgraded to 1.8.0** (from 1.6.2)
- Export shutdown drains task handle map, releases mutex, sends cancellation signals, then awaits with drain timeout
- Import shutdown stops accept loop on cancellation, then drains active connections up to drain timeout

## [0.4.0] - 2026-02-22

### Changed

- **Transport trait abstraction**: Introduced `TransportReader` and `TransportWriter` traits in `src/transport.rs`, replacing duplicated TCP/WebSocket bridging logic with generic `bridge_import_connection<R, W>()` and `handle_client_bridge<R, W>()`
- **Unified export liveliness loop**: Consolidated TCP and WebSocket export modes into a single `run_export_loop()` that dispatches based on `ExportBackend` enum
- **Module directory split**: Split monolithic `export.rs` (1005 lines) and `import.rs` (1387 lines) into focused submodule directories (`src/export/` and `src/import/`) with no public API changes

### Fixed

- WebSocket export now sends error signal to import side on backend connection failure (was silently dropping)
- Integration tests use dynamic ports and unique service names to prevent cross-test interference
- WebSocket integration tests resilient to stale processes and race conditions

## [0.3.0] - 2026-02-21

### Added

- **Graceful shutdown**: CancellationToken-based shutdown propagation across all tasks
- **Backend reconnection**: Exponential backoff retry when backend connections fail
- **TLS termination**: Optional TLS termination on import side (`--https-terminate`, `--tls-cert`, `--tls-key`), feature-gated behind `tls-termination`
- **Protocol auto-detection**: `--auto-import` mode that detects TLS, HTTP, WebSocket, or raw TCP and dispatches accordingly
- **Connection draining**: Configurable drain timeout (`--drain-timeout`) allows in-flight data to flush before connections close
- **Bidirectional HTTP mode**: `--http-multiroute-import` for per-request HTTP/1.1 routing with keep-alive support
- HTTP 504 response helper for backend timeout/unavailability
- WebSocket upgrade detection in HTTP parser
- HTTP response body framing parser for chunked/content-length/close-delimited responses
- Shared test utilities module with `PortGuard`, `wait_for_port`, `BridgeProcess`, and `unique_service_name`
- Explicit Zenoh resource undeclaration on task exit

### Fixed

- HTTP body truncation: parser now returns full buffer including body bytes (BUG-1)
- Connection ID collisions: use UUID v4 for globally unique client IDs (BUG-4)
- Buffer size config not threaded through export/import call chains (BUG-2)
- Hardcoded Content-Length in `http_400_response` (BUG-3)
- Race condition on fast publishers: increased AdvancedPublisher cache from 10 to 64 (BUG-5)
- Export mode missed pre-existing clients on startup (BUG-6)
- WebSocket export had no error signal on backend failure (BUG-7)
- `normalize_dns` port stripping used incorrect parsing (BUG-8)
- Removed emoji from log messages for cleaner structured logging

### Changed

- Test infrastructure overhauled: dynamic ports, unique service names, meaningful assertions, wait-based synchronization instead of sleeps

## [0.2.0]

Initial release with export/import modes, HTTP/HTTPS routing, and WebSocket support.
