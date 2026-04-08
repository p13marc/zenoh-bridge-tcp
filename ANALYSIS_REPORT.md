# Zenoh TCP Bridge - Deep Analysis Report

**Date**: 2026-04-08
**Version Analyzed**: 0.4.0 (commit fe9e082)

---

## Table of Contents

1. [Critical Bugs](#1-critical-bugs)
2. [Security Vulnerabilities](#2-security-vulnerabilities)
3. [Logic & Behavioral Issues](#3-logic--behavioral-issues)
4. [Resource Management Issues](#4-resource-management-issues)
5. [Configuration & CLI Issues](#5-configuration--cli-issues)
6. [Test Coverage Analysis](#6-test-coverage-analysis)
7. [Improvement Opportunities](#7-improvement-opportunities)
8. [Feature Ideas](#8-feature-ideas)
9. [Summary & Priority Matrix](#9-summary--priority-matrix)

---

## 1. Critical Bugs

### 1.1 Client Reconnection Race Condition (Export)

**Files**: `src/export/tcp.rs:94-102`, `src/export/ws.rs:95-104`

When a client reconnects (sends a new liveliness Put for an existing client_id), the code drops the old cancellation sender but doesn't explicitly signal shutdown:

```rust
if let Some((old_cancel_tx, old_handle)) = senders.remove(&client_id_for_map) {
    drop(old_cancel_tx);  // Drops without sending signal
    let _ = tokio::time::timeout(config.drain_timeout, old_handle).await;
}
senders.insert(client_id_for_map, (cancel_tx, main_handle));
```

The old task continues running until the channel closes. Meanwhile, the new task starts publishers/subscribers on the same Zenoh keys, causing **concurrent writes and potential data corruption**.

**Fix**: Send an explicit cancellation signal before dropping: `let _ = old_cancel_tx.send(()).await;`

---

### 1.2 Client Bridges Not Awaited on Shutdown (Export)

**File**: `src/export/bridge.rs:141-154`

During shutdown, the code sends cancellation signals to client bridges but **never waits for them to complete**:

```rust
_ = shutdown_token.cancelled() => {
    let senders = cancellation_senders.lock().await;
    for (client_id, (tx, _)) in senders.iter() {
        let _ = tx.send(()).await;  // Sends signal...
    }
    break;  // ...but immediately exits without waiting for JoinHandles
}
```

Task handles (`JoinHandle`) are stored but ignored. In-flight Zenoh publishes and backend writes may be lost.

**Fix**: Collect handles, release the lock, then await all handles with a drain timeout.

---

### 1.3 Hardcoded Drain Timeout Ignores CLI Argument

**File**: `src/main.rs:326`

The shutdown drain timeout is hardcoded to 10 seconds:

```rust
let drain_timeout = tokio::time::Duration::from_secs(10);
```

But the user can set `--drain-timeout` (default 5s) via CLI. The user's configuration is silently ignored during the top-level shutdown sequence.

**Fix**: Use `Duration::from_secs(args.drain_timeout)` instead of the hardcoded value.

---

### 1.4 HTTP Response Smuggling: Transfer-Encoding + Content-Length Conflict

**File**: `src/http_response_parser.rs:39-60`

The parser checks Transfer-Encoding first, then Content-Length, but **never validates that both aren't present simultaneously** (RFC 7230 Section 3.3.3 forbids this):

```rust
// Checks Transfer-Encoding first
for header in response.headers.iter() {
    if header.name.eq_ignore_ascii_case("transfer-encoding") && ... {
        return Ok(Some((header_len, ResponseBodyFraming::Chunked)));  // Early return
    }
}
// Then Content-Length
for header in response.headers.iter() {
    if header.name.eq_ignore_ascii_case("content-length") && ... {
        return Ok(Some((header_len, ResponseBodyFraming::ContentLength(len))));
    }
}
```

A response with **both** headers could be interpreted differently by different parsers, enabling HTTP response smuggling. Similarly, **multiple Content-Length headers with differing values** are not rejected (first one wins silently).

**Fix**: Scan all headers first, reject if both Transfer-Encoding and Content-Length are present, reject if multiple Content-Length headers disagree.

---

## 2. Security Vulnerabilities

### 2.1 Chunked Encoding Integer Overflow

**File**: `src/http_response_parser.rs:92-121`

The `find_chunked_body_end()` function trusts chunk size headers without bounds checking:

```rust
let chunk_end = pos + chunk_size + 2;
if body.len() < chunk_end {
    return None;
}
```

On 32-bit architectures, `pos + chunk_size + 2` can overflow. On 64-bit, a malicious chunk size of `0x7FFFFFFFFFFFFFFF` causes memory exhaustion.

**Fix**: Use `checked_add()` and enforce a maximum chunk size.

---

### 2.2 Unbounded Content-Length

**File**: `src/http_response_parser.rs:49-56`

Content-Length is parsed as `usize` without any maximum. In multiroute mode (`src/import/multiroute.rs:245`), the code waits until `body_received >= expected`, where `expected` could be astronomically large.

**Fix**: Reject Content-Length values exceeding `max_response_size`.

---

### 2.3 SNI Hostname Not Validated

**File**: `src/tls_parser.rs:161-182`

SNI hostnames are accepted without length or character validation:

```rust
let hostname = String::from_utf8_lossy(name).to_string();
```

- No maximum length check (SNI allows up to 65535 bytes)
- `from_utf8_lossy` silently replaces invalid bytes with U+FFFD
- A hostname like `"example.com\x00admin.com"` becomes `"example.com<FFFD>admin.com"`, potentially bypassing DNS routing

**Fix**: Validate hostname is ASCII, <= 255 bytes, and contains only valid DNS characters.

---

### 2.4 TLS ClientHello Size Validation Off-by-One

**File**: `src/tls_parser.rs:80-93`

```rust
let length = u16::from_be_bytes([buffer[3], buffer[4]]) as usize;
if length > max_handshake_size { return Err(...); }
let total_size = 5 + length;
while buffer.len() < total_size {
    if buffer.len() >= max_handshake_size {  // Wrong: should check total_size
        return Err(...);
    }
}
```

If `length == max_handshake_size`, the first check passes but `total_size = 5 + max_handshake_size` exceeds the limit. The inner loop check is also wrong (checks `buffer.len()` against `max_handshake_size` instead of `total_size`).

---

### 2.5 TLS Handshake Detection: 5-Byte Buffer Bypass

**File**: `src/tls_parser.rs:200-222`

With exactly 5 bytes `[0x16, 0x03, ?, ?, ?]`, the function returns `true` without validating the handshake type byte (byte 5 should be 0x01 for ClientHello). The check `if buffer.len() > 5 && buffer[5] != TLS_CLIENT_HELLO` is skipped when `len == 5`.

---

## 3. Logic & Behavioral Issues

### 3.1 Multiroute: Error Response Sent While Already Streaming

**File**: `src/import/multiroute.rs:291-297`

When a response timeout fires, the code sends an HTTP 504 response. But if data has already been streamed to the client (`bytes_written > 0`), this produces invalid HTTP (two response status lines on one connection).

**Fix**: Only send error responses when `bytes_written == 0` (already done for 502 at line 214, but missing for 504).

---

### 3.2 Multiroute: No EOF Published on Timeout

**File**: `src/import/multiroute.rs:270-281`

When a response timeout occurs, the liveliness token is undeclared but no EOF is published to the export side. The export bridge may hang waiting for data on the subscriber.

---

### 3.3 Mutex Held Across Async Await (Export Shutdown)

**File**: `src/export/bridge.rs:141-148`

The `cancellation_senders` mutex is held while iterating and sending cancellation signals:

```rust
let senders = cancellation_senders.lock().await;
for (client_id, (tx, _)) in senders.iter() {
    let _ = tx.send(()).await;  // Await with lock held
}
```

If any channel is slow, all subsequent cancellations are delayed, and any concurrent disconnect operations are blocked.

---

### 3.4 Empty Client ID Accepted

**File**: `src/export/bridge.rs:88-104`

`key.rsplit('/').next()` always returns `Some()` on non-empty strings. If the key ends with `/`, the client ID is an empty string, creating Zenoh keys like `service//tx//`.

---

### 3.5 Auto-Detect: WebSocket Detection via String Search

**File**: `src/import/auto.rs:152-158`

WebSocket upgrade detection uses substring matching:

```rust
let peek_lower = String::from_utf8_lossy(&peek_buf[..peek_len]).to_lowercase();
let looks_like_ws = peek_lower.contains("upgrade: websocket") || ...;
```

This is fragile (case variations, extra whitespace, partial headers in peek buffer). Should use proper HTTP parsing via `ParsedHttpRequest.is_websocket_upgrade`.

---

### 3.6 Backend Close Doesn't Signal Other Direction

**File**: `src/export/bridge.rs:266-271`

When the backend closes and EOF is published, the cancellation token for the other direction (`zenoh_to_backend`) is not triggered. The writer side may keep trying to write to a closed connection.

---

### 3.7 Drain Timeout Stacks Instead of Being Global

**File**: `src/import/bridge.rs:223-279`

The drain timeout applies per-direction sequentially. If both directions are slow:
- Direction 1 takes `drain_timeout` seconds
- Direction 2 takes another `drain_timeout` seconds
- Total: `2 * drain_timeout` (expected: `drain_timeout`)

A global deadline for the entire shutdown sequence would be more predictable.

---

## 4. Resource Management Issues

### 4.1 Liveliness Subscriber Never Undeclared (Export)

**File**: `src/export/bridge.rs:69-73`

The liveliness subscriber is created but never explicitly undeclared when the export loop exits. Depending on Zenoh's Drop implementation, this may leave dangling subscribers.

---

### 4.2 Spawned Tasks Not Tracked on Shutdown (Import)

**Files**: `src/import/listener.rs`, `ws.rs`, `auto.rs`, `tls.rs`, `multiroute.rs`

All listener modes use fire-and-forget `tokio::spawn()`. When the shutdown signal arrives, the listener stops accepting new connections, but existing connection tasks continue running without tracking or draining.

**Fix**: Use `tokio::task::JoinSet` to track and await all spawned tasks during shutdown.

---

### 4.3 No Backpressure on Zenoh Publishing

**Files**: All bridge functions (`src/export/bridge.rs`, `src/import/bridge.rs`)

`publisher.put()` is called without checking for Zenoh backpressure. If the network is congested or subscribers are slow, the publisher's cache fills up and samples may be silently dropped.

---

## 5. Configuration & CLI Issues

### 5.1 Rust Edition "2024" Does Not Exist

**File**: `Cargo.toml:4`

```toml
edition = "2024"
```

Rust editions are: 2015, 2018, 2021, and 2024. **Update**: Rust edition 2024 was stabilized in Rust 1.85 (Feb 2025). This is valid if using a recent enough toolchain. Verify minimum supported Rust version is >= 1.85.

---

### 5.2 No Validation on log_level and log_format

**File**: `src/args.rs:114-120`

Invalid values like `--log-level foobar` silently fall back to defaults. The `validate()` method doesn't check these fields.

---

### 5.3 No Bounds Checking on buffer_size, read_timeout, drain_timeout

**File**: `src/args.rs:103-112`

- `buffer_size = 0` causes zero-length reads
- `drain_timeout = 0` breaks graceful shutdown
- `read_timeout = 0` causes immediate timeouts
- No upper bounds (extreme values cause resource exhaustion)

---

### 5.4 Spec Format Not Validated Before Task Spawning

**File**: `src/args.rs:157-173`

The `validate()` method only checks that at least one spec list is non-empty. It does **not** parse spec strings, so format errors surface at runtime with cryptic messages.

**Fix**: Call `parse_export_spec()`, `parse_import_spec()`, etc. during validation.

---

### 5.5 Config File Precedence Not Logged

**File**: `src/main.rs:101-108`

When `--config` is provided, `--mode`/`--connect`/`--listen` arguments are silently ignored. No warning is logged about which configuration takes precedence.

---

### 5.6 Zenoh Version Drift

**File**: `Cargo.toml:18-19`

Specified `zenoh = "1.6.2"` but semver resolves to 1.7.2. If API changes occurred between versions, this could cause subtle issues.

**Fix**: Either pin exactly (`=1.6.2`) or update the spec to match reality.

---

## 6. Test Coverage Analysis

### 6.1 Overall Statistics

| Metric | Value |
|--------|-------|
| Unit tests | 128 functions |
| Integration tests | 50 functions |
| Test LoC | ~6,451 |
| Source LoC | ~5,444 |
| Test-to-code ratio | 1.2:1 |
| **Unit test coverage** | **~33% of implementation** |

### 6.2 Modules with Zero Unit Tests

| Module | Lines | Risk |
|--------|-------|------|
| `src/export/bridge.rs` | 429 | Core export logic |
| `src/export/tcp.rs` | 145 | TCP retry/backoff |
| `src/export/ws.rs` | 151 | WebSocket export |
| `src/import/bridge.rs` | 290 | Core import bridging |
| `src/import/connection.rs` | 146 | Connection handling |
| `src/import/listener.rs` | 97 | TCP listener |
| `src/import/tls.rs` | 168 | TLS termination |
| `src/import/ws.rs` | 173 | WebSocket import |
| `src/import/auto.rs` | 196 | Auto-detection |
| `src/import/multiroute.rs` | 325 | Per-request routing |
| `src/transport.rs` | 229 | Reader/Writer traits |
| `src/http_response_parser.rs` | 246 | Response parsing |
| `src/tls_config.rs` | 149 | TLS config loading |
| **Total untested** | **~2,744** | **~50% of impl** |

### 6.3 Missing Integration Test Scenarios

- TLS termination + auto-import combined
- WebSocket + multiroute combined
- Graceful shutdown with in-flight data
- Zenoh session failures during operation
- Large concurrent load (stress test is `#[ignore]`)
- Messages larger than `buffer_size`
- Responses larger than `max_response_size` (10 MB)
- HTTP pipelined requests
- Slow client/backend scenarios
- Cascading bridge chains (A -> Zenoh -> B -> Zenoh -> C)
- Config file loading
- Certificate/key loading errors
- Backend connection timeout with retry/backoff
- Partial data transfer (mid-transfer close from either side)

### 6.4 Flaky Test Patterns

Many integration tests use hardcoded `sleep()` for Zenoh liveliness propagation timing:

```rust
tokio::time::sleep(Duration::from_secs(2)).await;  // Race condition masked by sleep
```

On slow CI systems, these can fail. The `wait_for_port()` utility with exponential backoff is better but inconsistently applied.

---

## 7. Improvement Opportunities

### 7.1 Architecture

| Area | Current | Suggested |
|------|---------|-----------|
| Task tracking | Fire-and-forget `tokio::spawn` | `JoinSet` for tracking + graceful drain |
| Shutdown | Per-direction timeouts stack | Global deadline for entire shutdown |
| HTTP parsing | Dual check (TE then CL) | Single-pass validation per RFC 7230 |
| WebSocket detection | String substring matching | Proper HTTP parser-based detection |
| Error propagation | Logged and swallowed | Structured error channels to caller |
| Backpressure | None | Flow control between TCP and Zenoh |

### 7.2 Code Quality

- **Explicit cleanup**: Replace implicit Drop-based cleanup with explicit `undeclare()` calls for Zenoh resources
- **Validated newtypes**: Wrap `client_id`, `service_name`, `dns_name` in validated types instead of raw `String`
- **Input validation**: Validate all inputs (SNI hostnames, Content-Length, chunk sizes) at system boundaries
- **Consistent error context**: Use `anyhow::Context` consistently instead of ad-hoc `format!()` error messages
- **Tokio features**: Replace `features = ["full"]` with only needed features to reduce binary size

### 7.3 Observability

- Add structured metrics: connection count, bytes transferred, error rates, latency histograms
- Add connection-level tracing spans for request lifecycle visibility
- Log when configuration falls back to defaults (log_level, log_format)

---

## 8. Feature Ideas

### 8.1 High Value

| Feature | Description | Complexity |
|---------|-------------|------------|
| **Health check endpoint** | HTTP `/healthz` endpoint for liveness/readiness probes | Low |
| **Dry-run / validate mode** | `--validate` flag to check config without starting | Low |
| **Prometheus metrics** | Export connection counts, bytes, errors, latencies | Medium |
| **Rate limiting** | Per-client or per-service rate limiting | Medium |
| **Connection pooling** | Reuse backend connections across client requests | Medium |
| **SIGHUP config reload** | Hot-reload service specs without restart | Medium |
| **mTLS support** | Mutual TLS authentication for import clients | Medium |

### 8.2 Medium Value

| Feature | Description | Complexity |
|---------|-------------|------------|
| **Per-service config** | Override buffer_size, timeouts per service spec | Low |
| **Access control** | Allow/deny lists for client IPs or DNS names | Medium |
| **Circuit breaker** | Auto-disable backends after N consecutive failures | Medium |
| **gRPC support** | Bridge gRPC services (HTTP/2 framing) | High |
| **UDP support** | Bridge UDP datagrams over Zenoh | High |
| **Admin API** | REST API to inspect active connections and service state | Medium |
| **Streaming responses** | Avoid buffering entire response in multiroute mode | Medium |

### 8.3 Nice to Have

| Feature | Description | Complexity |
|---------|-------------|------------|
| **Config file support** | YAML/TOML config file as alternative to CLI flags | Low |
| **Docker healthcheck** | Built-in healthcheck for container orchestration | Low |
| **Graceful restart** | Socket passing for zero-downtime restarts | High |
| **Request/response logging** | Optional detailed HTTP request/response logging | Low |
| **SNI-based routing for raw TCP** | Route raw TCP based on TLS SNI without termination | Low |
| **WebSocket compression** | permessage-deflate for WS bridges | Medium |
| **Load balancing** | Round-robin or least-connections across multiple backends | Medium |

---

## 9. Summary & Priority Matrix

### Critical (Fix ASAP)

| # | Issue | Type | Impact |
|---|-------|------|--------|
| 1.1 | Client reconnection race condition | Bug | Data corruption |
| 1.2 | Client bridges not awaited on shutdown | Bug | Data loss |
| 1.4 | HTTP response smuggling (TE+CL conflict) | Security | Request smuggling |
| 2.1 | Chunked encoding integer overflow | Security | DoS / memory exhaustion |
| 2.2 | Unbounded Content-Length | Security | DoS / memory exhaustion |

### High Priority

| # | Issue | Type | Impact |
|---|-------|------|--------|
| 1.3 | Hardcoded drain timeout ignores CLI | Bug | Config ignored |
| 2.3 | SNI hostname not validated | Security | Routing bypass |
| 2.4 | TLS size validation off-by-one | Security | Parser bypass |
| 3.1 | Error response while streaming (multiroute) | Bug | Invalid HTTP |
| 3.3 | Mutex held across async await | Bug | Deadlock risk |
| 4.2 | Spawned tasks not tracked | Resource | Unclean shutdown |

### Medium Priority

| # | Issue | Type | Impact |
|---|-------|------|--------|
| 3.2 | No EOF on multiroute timeout | Bug | Export hangs |
| 3.4 | Empty client ID accepted | Bug | Key collision |
| 3.5 | Fragile WebSocket detection | Bug | Misdetection |
| 3.6 | Backend close doesn't signal writer | Bug | Stuck connection |
| 3.7 | Drain timeout stacks | Bug | Slow shutdown |
| 4.1 | Liveliness subscriber not undeclared | Resource | Leak |
| 4.3 | No backpressure on publishing | Performance | Data loss |
| 5.2-5.5 | CLI validation gaps | UX | Confusing errors |

### Low Priority

| # | Issue | Type | Impact |
|---|-------|------|--------|
| 2.5 | TLS detection 5-byte bypass | Security | Minor |
| 5.6 | Zenoh version drift | Maintenance | Subtle bugs |
| 6.x | Test coverage gaps | Quality | Regression risk |

---

*Report generated by deep static analysis of the codebase. Findings should be verified with runtime testing before applying fixes.*
