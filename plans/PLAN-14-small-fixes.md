# Plan 14: Small Fixes & Maintenance

**Priority**: Low
**Report refs**: 5.5, 5.6
**Effort**: Trivial
**Risk**: None

## Items

### 5.5 — Config file precedence not logged

**File**: `src/main.rs:101-108`

When `--config` is provided alongside `--mode`/`--connect`/`--listen`, the CLI args are silently ignored.

**Fix**:

```rust
let config = if let Some(config_file) = &args.config {
    if args.mode != "peer" || args.connect.is_some() || args.listen.is_some() {
        warn!("Config file provided; --mode, --connect, and --listen CLI arguments will be ignored");
    }
    info!(config_file = %config_file, "Loading Zenoh configuration from file");
    config::create_zenoh_config_from_file(config_file)?
} else {
    config::create_zenoh_config(&args.mode, args.connect.as_ref(), args.listen.as_ref())?
};
```

### 5.6 — Zenoh version drift

**File**: `Cargo.toml:18-19`

Current: `zenoh = "1.6.2"` (resolves to 1.7.2 via lock file; latest is 1.8.0).

This project uses `zenoh-ext` with `features = ["unstable"]`, and unstable APIs are not semver-protected. A minor version bump could break unstable API usage.

**Options** (pick one):

1. **Pin to current resolved version**: Change to `zenoh = "~1.7.2"` and `zenoh-ext = { version = "~1.7.2", ... }` to lock to 1.7.x patch releases only
2. **Update to latest**: Change to `zenoh = "1.8.0"`, run full test suite, fix any breakage
3. **Exact pin**: Change to `zenoh = "=1.7.2"` for maximum reproducibility (most conservative)

**Recommended**: Option 1 (`~1.7.2`) — allows patch fixes but prevents surprise minor bumps. Then periodically update to new minors explicitly.

## Files Changed

- `src/main.rs` — config precedence warning
- `Cargo.toml` — zenoh version pin
