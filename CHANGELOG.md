# Changelog

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
