# Plan 15: Test Coverage Improvements

**Priority**: Medium (ongoing)
**Report ref**: 6.x
**Effort**: High (incremental)
**Risk**: None — purely additive

## Problem

~41% of implementation code has zero unit tests. Core bridging logic in `export/bridge.rs`, `import/bridge.rs`, and `import/multiroute.rs` is only covered by integration tests, which are slow (~50s) and mask individual component failures.

Integration test gaps: no tests for graceful shutdown with in-flight data, messages > buffer_size, responses > max_response_size, HTTP pipelining, slow clients, config file loading, or partial transfer close.

Flaky test patterns: hardcoded `sleep(2s)` for Zenoh liveliness propagation instead of event-driven synchronization.

## Plan

### Phase 1: Unit tests for response parser (http_response_parser.rs)

The parser now has security-critical logic (TE+CL rejection, Content-Length bounds, chunk size bounds). Add unit tests for:

- TE+CL conflict with various header orderings
- Multiple identical Content-Length (should pass)
- Multiple differing Content-Length (should reject)
- Content-Length at exactly MAX_CONTENT_LENGTH (boundary)
- Content-Length at MAX_CONTENT_LENGTH + 1 (reject)
- Chunk size at exactly MAX_CHUNK_SIZE (boundary)
- Chunk size at MAX_CHUNK_SIZE + 1 (reject)
- checked_add overflow paths (if reachable)

### Phase 2: Unit tests for transport.rs

- TcpReader with various buffer sizes (1024, 65536, boundary values)
- TcpReader with data larger than buffer (multi-read)
- TcpWriter flush behavior
- WsReader with Binary, Text, Close, Ping messages
- WsWriter message framing

### Phase 3: Integration tests for missing scenarios

Priority order:

1. **Graceful shutdown with in-flight data**: Start bridge, send data continuously, trigger shutdown, verify data drains without loss
2. **Messages larger than buffer_size**: Send a 128KB payload through a bridge with 64KB buffer_size, verify it arrives complete
3. **Responses larger than max_response_size**: Multiroute mode, backend sends > 10MB, verify 502 returned
4. **Partial transfer close**: Client closes mid-send, verify export side handles EOF correctly
5. **Config file loading**: Use `--config` flag with a JSON5 file, verify bridge starts correctly
6. **Slow client backpressure**: Client reads slowly, verify bridge doesn't OOM

### Phase 4: Replace hardcoded sleeps

Replace `tokio::time::sleep(Duration::from_secs(2))` patterns with event-driven synchronization where possible:

- For Zenoh liveliness propagation: poll with short retries instead of fixed sleep
- For backend readiness: use `wait_for_port()` consistently (already available in test helpers)
- For subscriber readiness: use a handshake message or retry loop

This is the highest-ROI flakiness fix but hardest to implement, as it requires understanding each test's specific timing dependency.

## Files Changed (incremental)

- `src/http_response_parser.rs` — add tests in `#[cfg(test)]` module
- `src/transport.rs` — add tests in `#[cfg(test)]` module
- `tests/` — new integration test files
- `tests/common/` — update test helpers for event-driven sync
