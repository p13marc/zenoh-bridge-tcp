# TODO: HTTP Protocol Support

## Quick Summary

### Selected Crates for Implementation

- **HTTP/1.x Parsing**: `httparse` - Zero-copy, battle-tested (6M downloads/month)
- **TLS/SNI Parsing**: `tls-parser` - Full TLS parser with nom combinators
- **HTTP/2 Support**: `h2` - Official Tokio HTTP/2 implementation (13M downloads/month)

### Key Design Decisions (Resolved)

1. ✅ Separate `--http-import` and `--http-export` CLI flags (explicit mode)
2. ✅ Parse SNI from TLS ClientHello for HTTPS support
3. ✅ Return HTTP 502 Bad Gateway when backend unavailable
4. ✅ Support HTTP/2 (phased approach due to complexity)
5. ✅ DNS normalization: case-insensitive + port stripping
6. ✅ DNS collision: first registration wins, log warning

### Delivery Timeline

- **Phase A (2-3 weeks)**: HTTP/1.1 with Host header routing - MVP
- **Phase B (+2 weeks)**: Add HTTPS via SNI parsing
- **Phase C (+2-3 weeks)**: Add HTTP/2 support (complex, may need proxy mode)

## Overview

Add support for HTTP protocol through DNS-based routing. This will allow a single TCP listener (import side) to connect to multiple HTTP servers (export side) by parsing the HTTP Host header to determine the destination.

## Current State

The bridge currently works at the TCP level:
- One import = One TCP listener → One Zenoh service key
- One export = One Zenoh service key → One backend TCP address
- 1:1 mapping between services and backends

## Proposed Feature

Enable HTTP-aware routing:
- **Import side**: Single TCP listener accepts HTTP connections
- **Parser**: Extract DNS/Host from first HTTP request
- **Routing**: Use DNS as Zenoh key suffix → `service/{dns}/tx/{client_id}`
- **Export side**: Multiple HTTP backends, each registered with a DNS name
- **Result**: N:M mapping - one listener can reach N different HTTP servers

### Example Architecture

```
HTTP Client → http://api.example.com
    ↓
Import Bridge (listening on :8080)
    ↓ (parses Host: api.example.com)
    ↓ (uses key: http-service/api.example.com/tx/{client_id})
    ↓
Zenoh Network
    ↓
Export Bridge A (registered: api.example.com → 192.168.1.10:8000)
Export Bridge B (registered: web.example.com → 192.168.1.20:8000)
    ↓
Backend HTTP Server (192.168.1.10:8000)
```

## Implementation Plan

### Phase 1: HTTP Request Parsing (Import Side)

**File**: `src/http_parser.rs` (new)

**Crate to use**: `httparse` (https://crates.io/crates/httparse)
- Zero-copy, zero-allocation HTTP/1.x parser
- ~6M downloads/month, battle-tested
- Used by hyper and many other projects
- Fast, safe, supports partial parsing

**Implementation tasks:**
- [ ] Add `httparse` dependency to `Cargo.toml`
- [ ] Create HTTP request parser module using `httparse::Request`
- [ ] Implement function to extract Host header from HTTP/1.1 request
  - Use `httparse::Request::parse()` for request line and headers
  - Extract Host header value from parsed headers
  - Handle both `Host: domain.com` and `Host: domain.com:port`
- [ ] Support HTTP/1.0 requests with absolute URIs: `GET http://domain.com/path HTTP/1.0`
- [ ] Add timeout for reading initial request (prevent hanging on slow clients)
- [ ] Buffer and preserve the entire initial request for forwarding
- [ ] Handle edge cases:
  - Missing Host header (return 400 Bad Request)
  - Invalid/malformed requests (httparse returns Error)
  - Very large headers (buffer size limit)
  - Partial reads (httparse returns Partial status)
- [ ] Implement DNS normalization:
  - Case-insensitive DNS (convert to lowercase)
  - Normalize ports (strip default ports 80 for HTTP, 443 for HTTPS)
  - Example: `Example.COM:80` → `example.com`

### Phase 2: HTTP-Aware Import Mode

**File**: `src/http_import.rs` (new) or modify `src/import.rs`

- [ ] Add new import mode type: `--http-import <SPEC>`
  - Format: `service_name/listen_addr` (similar to regular import)
  - Example: `--http-import 'http-service/0.0.0.0:8080'`
- [ ] Modify connection handler to:
  1. Accept TCP connection
  2. Read and parse first HTTP request to extract Host header
  3. Apply DNS normalization (lowercase, port stripping)
  4. Construct Zenoh key: `{service}/{dns}/tx/{client_id}`
  5. Forward buffered request + continue streaming
- [ ] Update liveliness key: `{service}/{dns}/clients/{client_id}`
- [ ] Update subscriber keys: `{service}/{dns}/rx/{client_id}`
- [ ] Ensure first request is not lost during parsing
- [ ] Handle backend unavailable: Send HTTP 502 Bad Gateway response
- [ ] Query Zenoh for available backends before connecting client

### Phase 3: DNS-Based Export Registration

**File**: `src/http_export.rs` (new) or modify `src/export.rs`

- [ ] Add new export mode type: `--http-export <SPEC>`
  - Format: `service_name/dns/backend_addr`
  - Example: `--http-export 'http-service/api.example.com/192.168.1.10:8000'`
- [ ] Apply DNS normalization to registration (lowercase, port handling)
- [ ] Modify liveliness subscription to include DNS:
  - Monitor: `{service}/{dns}/clients/*`
  - Instead of: `{service}/clients/*`
- [ ] Update subscriber/publisher keys with DNS component:
  - Subscribe: `{service}/{dns}/tx/{client_id}`
  - Publish: `{service}/{dns}/rx/{client_id}`
- [ ] Handle DNS collision detection:
  - Track registered DNS names per service
  - Log warning if duplicate detected
  - First registration wins
- [ ] Add DNS query/discovery mechanism via Zenoh liveliness

### Phase 4: Command Line Interface Updates

**File**: `src/args.rs`

- [ ] Add `--http-import` option
- [ ] Add `--http-export` option
- [ ] Update help text with examples
- [ ] Add validation for HTTP-specific specs
- [ ] Ensure backward compatibility with existing `--import` and `--export`

**File**: `src/main.rs`

- [ ] Handle new HTTP import/export arguments
- [ ] Spawn appropriate tasks for HTTP mode vs TCP mode
- [ ] Update logging to show HTTP routing info

### Phase 5: TLS/SNI Support

**File**: `src/tls_parser.rs` (new)

**Crate to use**: `tls-parser` (https://crates.io/crates/tls-parser)
- Full TLS parser with nom combinators
- Parses TLS 1.2 and TLS 1.3 handshakes
- Zero-copy, designed for security analysis
- Part of the Rusticata project (used in network IDS)

**Implementation tasks:**
- [ ] Add `tls-parser` dependency to `Cargo.toml`
- [ ] Create TLS ClientHello parser using `tls_parser::parse_tls_plaintext`
- [ ] Extract SNI (Server Name Indication) from TLS handshake extensions
  - Parse ClientHello message
  - Find SNI extension (type 0x0000)
  - Extract hostname from ServerNameList
- [ ] Handle TLS 1.2 and TLS 1.3 variations
- [ ] Buffer TLS handshake for forwarding (preserve raw bytes)
- [ ] Apply DNS normalization to SNI hostname
- [ ] Integrate SNI parser with HTTP import mode (detect TLS vs HTTP)
- [ ] Test HTTPS routing with real certificates
- [ ] Handle edge cases:
  - Missing SNI extension (return error or close)
  - Invalid TLS handshake
  - Mixed HTTP/HTTPS on same listener (protocol detection)
  - TLS version negotiation

### Phase 6: HTTP/2 Support

**File**: `src/http2_parser.rs` (new)

**Crate to use**: `h2` (https://crates.io/crates/h2)
- Official Tokio-based HTTP/2 implementation
- ~13M downloads/month
- Used by hyper and many production systems
- Passes h2spec compliance tests
- Includes HPACK encoder/decoder

**Alternative for parsing only**: `fluke-h2-parse` (lightweight parser-only)

**Implementation tasks:**
- [ ] Add `h2` dependency to `Cargo.toml`
- [ ] Create HTTP/2 frame parser or use h2's codec
- [ ] Detect HTTP/2 connection preface: `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n`
- [ ] Parse HEADERS frame to extract `:authority` pseudo-header
  - Use h2's HPACK decoder for decompressing headers
  - Extract `:authority` (equivalent to Host header)
- [ ] Handle HPACK compression/decompression
- [ ] Buffer initial frames for forwarding
- [ ] Apply DNS normalization to authority
- [ ] Test with HTTP/2 clients and servers
- [ ] Handle HTTP/2 upgrade from HTTP/1.1 (via `Upgrade: h2c` header)
- [ ] Consider: May need to act as HTTP/2 proxy rather than raw TCP pass-through
  - This is more complex than HTTP/1.1 due to multiplexing
  - HTTP/2 streams are multiplexed, can't just extract authority and forward raw bytes
  - May require full HTTP/2 proxy mode (decode → route → re-encode)
  - **Recommendation**: Start with HTTP/1.1 only, add HTTP/2 in later phase

### Phase 7: Testing

**File**: `tests/http_routing_integration.rs` (new)

- [ ] Test single HTTP server through HTTP-aware bridge
- [ ] Test multiple HTTP servers with different Host headers
- [ ] Test mixed scenario: client requests to different domains
- [ ] Test DNS normalization:
  - Case insensitivity (Example.COM vs example.com)
  - Port normalization (example.com:80 vs example.com)
- [ ] Test edge cases:
  - Missing Host header (expect 400 response)
  - Malformed HTTP requests
  - Backend unavailable (expect 502 response)
  - DNS collision (multiple exports, first wins)
- [ ] Test with real HTTP frameworks (Axum, like existing tests)
- [ ] Load testing with multiple concurrent clients

**HTTPS/TLS test scenarios:**
- [ ] Test HTTPS routing via SNI parsing
- [ ] Test TLS 1.2 and TLS 1.3
- [ ] Test missing SNI (fallback behavior)
- [ ] Verify end-to-end TLS (no termination)

**HTTP/2 test scenarios:**
- [ ] Test HTTP/2 direct connection
- [ ] Test HTTP/2 upgrade from HTTP/1.1
- [ ] Test HPACK-compressed headers
- [ ] Test multiple concurrent streams

**Additional test scenarios:**
- [ ] Test HTTP/1.0 and HTTP/1.1
- [ ] Test chunked transfer encoding
- [ ] Test keep-alive connections
- [ ] Test large requests (streaming)
- [ ] Test WebSocket upgrade requests

### Phase 8: Documentation

**File**: `README.md`

- [ ] Add HTTP routing section
- [ ] Add architecture diagram for HTTP mode
- [ ] Add usage examples
- [ ] Document limitations (HTTP/1.x only initially)
- [ ] Add troubleshooting section

**File**: `examples/http_routing_example.md` (new)

- [ ] Complete working example with multiple backends
- [ ] Show configuration for typical use cases
- [ ] Docker deployment example

## Technical Considerations

### Protocol Parsing

- **HTTP/1.1 is text-based**: Easy to parse headers
- **HTTP/2 is binary**: Requires parsing binary frames and HPACK compression
  - Will be supported but may come in a later phase
- **HTTPS**: Use SNI parsing from TLS ClientHello
  - SNI (Server Name Indication) contains hostname before encryption starts
  - Allows routing HTTPS without terminating TLS

### Performance

- **Parsing overhead**: Reading/buffering first request adds latency
- **Memory**: Each connection needs to buffer initial request
- **Optimization**: Use `bytes` crate for zero-copy parsing where possible

### Error Handling

- **Invalid HTTP**: How to respond? TCP close? HTTP 400?
- **Unknown Host**: Should we send HTTP 404 or close connection?
- **Backend unavailable**: Already handled, but need DNS-specific error messages

### State Management

- **DNS to backend mapping**: How to discover available backends?
  - Use Zenoh liveliness queries to find available `{service}/{dns}` backends
  - Cache mapping? Or query on each connection?

### Key Expression Design

Current:
```
{service}/tx/{client_id}
{service}/rx/{client_id}
{service}/clients/{client_id}
```

Proposed HTTP:
```
{service}/{dns}/tx/{client_id}
{service}/{dns}/rx/{client_id}
{service}/{dns}/clients/{client_id}
```

Alternative (flatter):
```
{service}/http/{dns}/tx/{client_id}
{service}/http/{dns}/rx/{client_id}
{service}/http/{dns}/clients/{client_id}
```

**Decision needed**: Which key structure is better?

## Future Enhancements (Out of Scope for v1)

- [ ] HTTP/3 / QUIC support
- [ ] Load balancing between multiple backends with same DNS
- [ ] Health checking of backends
- [ ] Metrics: requests per DNS, latency, errors
- [ ] WebSocket support (upgrade from HTTP)
- [ ] Path-based routing in addition to Host-based
- [ ] Header-based routing (custom headers)
- [ ] Content-based routing
- [ ] HTTP response caching in Zenoh
- [ ] Rate limiting per DNS
- [ ] Authentication/authorization layer

## Design Decisions (RESOLVED)

1. **HTTP mode separate from TCP mode**
   - ✅ **DECISION**: Separate `--http-import` and `--http-export` flags
   - Rationale: Explicit control, no surprises, maintains backward compatibility

2. **HTTPS support via SNI parsing**
   - ✅ **DECISION**: Parse SNI from TLS ClientHello
   - Implementation: Extract hostname from TLS handshake before encryption starts
   - Benefit: Works with encrypted HTTPS traffic without terminating TLS

3. **DNS resolution failure handling**
   - ✅ **DECISION**: Return HTTP 502 Bad Gateway
   - Implementation: Send proper HTTP error response when backend not available
   - Benefit: Most HTTP-compliant, clear error for clients

4. **HTTP/2 support**
   - ✅ **DECISION**: Yes, support HTTP/2
   - Implementation: Parse binary frames and HPACK-compressed headers
   - Note: More complex than HTTP/1.1, may be phased approach

5. **DNS normalization**
   - ✅ **DECISION**: Yes to both
   - Normalize ports: `example.com` and `example.com:80` treated as same
   - Case-insensitive: `Example.COM` and `example.com` treated as same
   - Implementation: Normalize DNS during parsing and key construction

6. **Virtual host collision**
   - ✅ **DECISION**: First one wins, log warning
   - Implementation: Track registered DNS names, warn on duplicates
   - Future: Could add load balancing as enhancement

## Success Criteria

- [ ] Can route HTTP requests to different backends based on Host header
- [ ] Maintains backward compatibility with existing TCP mode
- [ ] No performance degradation for non-HTTP TCP traffic
- [ ] Comprehensive test coverage
- [ ] Clear documentation and examples
- [ ] No memory leaks or resource exhaustion under load

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| HTTP parsing bugs cause crashes | High | Extensive fuzzing, graceful error handling |
| Performance degradation | Medium | Benchmark and optimize parser |
| Complex key expression patterns | Medium | Keep design simple, document clearly |
| HTTPS limitation confuses users | Low | Clear documentation, consider SNI in future |
| Backward compatibility breaks | High | Thorough testing, separate modes |

## Development Order

1. Start with HTTP parser (Phase 1) - can be tested in isolation
2. Implement HTTP import mode (Phase 2) - test with existing TCP export
3. Implement HTTP export with DNS (Phase 3)
4. Add CLI support (Phase 4)
5. Write comprehensive tests (Phase 5)
6. Document everything (Phase 6)

## Estimated Effort

- Phase 1: 2-3 days (HTTP parser + tests)
- Phase 2: 3-4 days (HTTP import mode + integration)
- Phase 3: 2-3 days (HTTP export mode + DNS handling)
- Phase 4: 1 day (CLI + main.rs)
- Phase 5: 3-4 days (TLS/SNI support)
- Phase 6: 4-5 days (HTTP/2 support)
- Phase 7: 4-5 days (comprehensive testing all protocols)
- Phase 8: 2-3 days (documentation)

**Total: ~4-5 weeks** for complete implementation with HTTP/1.1, HTTPS (SNI), HTTP/2, testing and documentation.

**Phased Delivery:**
- **Phase A - MVP (2-3 weeks)**: HTTP/1.1 only
  - Phases 1-4: HTTP parsing, import/export, CLI
  - Partial Phase 7: HTTP/1.1 testing only
  - Phase 8: Basic documentation
  - Deliverable: Working HTTP routing with Host header
  
- **Phase B - TLS Support (+2 weeks)**: Add HTTPS via SNI
  - Phase 5: TLS/SNI parsing
  - Extended Phase 7: HTTPS testing
  - Updated Phase 8: Document HTTPS support
  - Deliverable: HTTPS routing without terminating TLS
  
- **Phase C - HTTP/2 Support (+2-3 weeks)**: Add HTTP/2 (complex)
  - Phase 6: HTTP/2 frame parsing or proxying
  - Extended Phase 7: HTTP/2 testing
  - Final Phase 8: Complete documentation
  - Deliverable: Full HTTP/1.1 + HTTPS + HTTP/2 support
  - Note: May require proxy mode rather than pass-through due to multiplexing

## References

### RFCs
- HTTP/1.1 RFC: https://www.rfc-editor.org/rfc/rfc7230
- Host Header: https://www.rfc-editor.org/rfc/rfc7230#section-5.4
- HTTP/2 RFC: https://www.rfc-editor.org/rfc/rfc7540
- HTTP/2 HPACK: https://www.rfc-editor.org/rfc/rfc7541
- TLS 1.3 RFC: https://www.rfc-editor.org/rfc/rfc8446
- TLS 1.2 RFC: https://www.rfc-editor.org/rfc/rfc5246
- SNI Extension: https://www.rfc-editor.org/rfc/rfc6066#section-3

### Crates
- `httparse`: https://crates.io/crates/httparse (HTTP/1.x parser)
- `tls-parser`: https://crates.io/crates/tls-parser (TLS/SNI parser)
- `h2`: https://crates.io/crates/h2 (HTTP/2 implementation)

### Project Files
- Current bridge architecture: See `src/import.rs` and `src/export.rs`
- Existing HTTP tests: See `tests/http_integration.rs`
