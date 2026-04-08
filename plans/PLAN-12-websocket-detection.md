# Plan 12: Proper WebSocket Detection in Auto-Import

**Priority**: Medium
**Report ref**: 3.5
**Effort**: Low
**Risk**: Low — replaces string matching with existing parser

## Problem

`src/import/auto.rs:152-158` detects WebSocket upgrade requests via fragile substring matching:

```rust
let peek_lower = String::from_utf8_lossy(&peek_buf[..peek_len]).to_lowercase();
let looks_like_ws =
    peek_lower.contains("upgrade: websocket") || peek_lower.contains("upgrade:websocket");
```

Issues:
- Misses case variations beyond what `to_lowercase()` handles (already OK for ASCII)
- Misses extra whitespace: `"Upgrade:  websocket"` or `"Upgrade : websocket"`
- Fails if the `Upgrade` header spans a peek buffer boundary (4096 bytes may not hold all headers)
- Doesn't validate the full HTTP request structure

The codebase already has `ParsedHttpRequest.is_websocket_upgrade` in `http_parser.rs` which uses proper `httparse` parsing.

## Plan

### Step 1: Refactor auto.rs WebSocket detection

The auto-detect flow already identifies HTTP vs TLS vs RawTcp. For HTTP connections, it currently:
1. Peeks to detect HTTP method
2. Peeks again (4096 bytes) to check for WebSocket upgrade
3. Dispatches to either WS handler or HTTP handler

Change step 2 to use the HTTP parser:

```rust
// Before:
let peek_lower = String::from_utf8_lossy(&peek_buf[..peek_len]).to_lowercase();
let looks_like_ws =
    peek_lower.contains("upgrade: websocket") || peek_lower.contains("upgrade:websocket");

// After:
// Use try_parse_request to check for WebSocket upgrade.
// This function is already available in http_parser and handles
// partial headers (returns None) and full headers (returns ParsedHttpRequest).
let looks_like_ws = match crate::http_parser::try_parse_request(&peek_buf[..peek_len]) {
    Ok(Some(parsed)) => parsed.is_websocket_upgrade,
    _ => false, // Partial headers or parse error: not WS
};
```

Note: `try_parse_request` is currently `fn(buffer: &[u8]) -> Result<Option<ParsedHttpRequest>>` and is not public. It needs to be made `pub(crate)`.

### Step 2: Make `try_parse_request` accessible

In `src/http_parser.rs`, change:
```rust
// Before:
fn try_parse_request(buffer: &[u8]) -> Result<Option<ParsedHttpRequest>> {

// After:
pub(crate) fn try_parse_request(buffer: &[u8]) -> Result<Option<ParsedHttpRequest>> {
```

### Step 3: Update tests

- Existing `auto_import_integration` tests should pass unchanged (they test HTTP and raw TCP detection)
- Add a unit test for WebSocket detection via the parser path

## Files Changed

- `src/import/auto.rs` — replace string matching with `try_parse_request`
- `src/http_parser.rs` — change `try_parse_request` visibility to `pub(crate)`
