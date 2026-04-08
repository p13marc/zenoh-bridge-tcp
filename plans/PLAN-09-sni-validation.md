# Plan 09: SNI Hostname Validation

**Priority**: High
**Report ref**: 2.3
**Effort**: Low
**Risk**: Low — purely additive validation at a single callsite

## Problem

`src/tls_parser.rs:161-182` accepts any SNI hostname from TLS ClientHello without validation. `String::from_utf8_lossy()` silently replaces invalid bytes, and there are no length or character checks. This allows:

- Hostnames up to 65535 bytes (wire format max), far exceeding DNS limits
- Non-ASCII bytes silently converted to U+FFFD replacement characters
- Potential routing bypass via null bytes or invalid characters
- Memory waste from oversized hostname strings used as Zenoh key segments

Per RFC 6066 §3, SNI hostnames must be DNS hostnames in ASCII encoding, without trailing dot, no IP literals.

## Plan

### Step 1: Add `validate_sni_hostname()` in `tls_parser.rs`

Add a validation function after extracting the hostname from the SNI extension:

```rust
fn validate_sni_hostname(raw: &[u8]) -> Result<String> {
    // 1. Must be valid ASCII (reject non-ASCII bytes instead of lossy conversion)
    if !raw.is_ascii() {
        return Err(BridgeError::TlsParse("SNI hostname contains non-ASCII bytes".into()));
    }

    let hostname = std::str::from_utf8(raw)
        .map_err(|_| BridgeError::TlsParse("SNI hostname is not valid UTF-8".into()))?;

    // 2. Length check: DNS names max 253 bytes (RFC 1035)
    if hostname.len() > 253 {
        return Err(BridgeError::TlsParse(
            format!("SNI hostname too long: {} bytes (max 253)", hostname.len())
        ));
    }

    // 3. Must not be empty
    if hostname.is_empty() {
        return Err(BridgeError::TlsParse("SNI hostname is empty".into()));
    }

    // 4. Must not have trailing dot
    if hostname.ends_with('.') {
        return Err(BridgeError::TlsParse("SNI hostname must not have trailing dot".into()));
    }

    // 5. Each label: alphanumeric + hyphens, 1-63 bytes, no leading/trailing hyphen
    for label in hostname.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(BridgeError::TlsParse(
                format!("SNI hostname label invalid length: '{}'", label)
            ));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(BridgeError::TlsParse(
                format!("SNI hostname label contains invalid characters: '{}'", label)
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(BridgeError::TlsParse(
                format!("SNI hostname label must not start/end with hyphen: '{}'", label)
            ));
        }
    }

    Ok(hostname.to_string())
}
```

### Step 2: Replace `from_utf8_lossy` callsite

In `extract_sni_from_client_hello()`, change:
```rust
// Before
let hostname = String::from_utf8_lossy(name).to_string();

// After
let hostname = validate_sni_hostname(name)?;
```

### Step 3: Add unit tests

- Valid hostnames: `example.com`, `api.test.com`, `xn--nxasmq6b.com` (punycode)
- Reject: empty, > 253 bytes, trailing dot, label > 63 chars, non-ASCII bytes, null bytes, IP literals, hyphens at label boundaries

### Step 4: Verify integration tests still pass

HTTPS routing tests use real TLS ClientHellos with valid hostnames — they should pass unchanged.

## Files Changed

- `src/tls_parser.rs` — add `validate_sni_hostname()`, update callsite
