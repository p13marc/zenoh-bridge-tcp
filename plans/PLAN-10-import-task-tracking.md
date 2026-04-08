# Plan 10: Import Task Tracking with JoinSet

**Priority**: High
**Report ref**: 4.2
**Effort**: Medium
**Risk**: Medium — touches all 5 import listener files, must preserve shutdown semantics

## Problem

All import listener modes use fire-and-forget `tokio::spawn()` for per-connection tasks. When the shutdown signal arrives, the listener loop breaks but spawned tasks continue running without tracking. The process cannot cleanly drain in-flight connections.

Affected files:
- `src/import/listener.rs` — TCP import
- `src/import/ws.rs` — WebSocket import
- `src/import/auto.rs` — Auto-detect import
- `src/import/tls.rs` — HTTPS termination import
- `src/import/multiroute.rs` — HTTP multiroute import

## Plan

### Step 1: Replace `tokio::spawn` with `JoinSet` in each listener

Each listener file follows the same pattern:

```rust
// Before
loop {
    tokio::select! {
        result = listener.accept() => {
            // ...
            tokio::spawn(async move { /* handle connection */ });
        }
        _ = shutdown_token.cancelled() => { break; }
    }
}
```

Change to:

```rust
let mut tasks = tokio::task::JoinSet::new();

loop {
    tokio::select! {
        result = listener.accept() => {
            // ...
            tasks.spawn(async move { /* handle connection */ });
        }
        _ = shutdown_token.cancelled() => { break; }
    }

    // Reap completed tasks to prevent unbounded growth
    while let Some(result) = tasks.try_join_next() {
        if let Err(e) = result {
            warn!("Connection task panicked: {:?}", e);
        }
    }
}
```

### Step 2: Drain tasks after loop exits

After the `loop` breaks on shutdown, wait for all active tasks:

```rust
// After the loop
info!("Draining {} active connections...", tasks.len());
let deadline = tokio::time::Instant::now() + config.drain_timeout;
while !tasks.is_empty() {
    match tokio::time::timeout_at(deadline, tasks.join_next()).await {
        Ok(Some(Ok(()))) => {} // task completed
        Ok(Some(Err(e))) => warn!("Connection task error during drain: {:?}", e),
        Ok(None) => break, // all tasks done
        Err(_) => {
            warn!("{} connections still active after drain timeout, aborting", tasks.len());
            tasks.abort_all();
            break;
        }
    }
}
```

### Step 3: Apply to all 5 listener files

Apply the same pattern to:
1. `listener.rs` — `run_import_mode_internal()`
2. `ws.rs` — `run_ws_import_mode_internal()`
3. `auto.rs` — `run_auto_import_mode_internal()`
4. `tls.rs` — `run_https_terminate_import_mode_internal()`
5. `multiroute.rs` — `run_http_multiroute_import_mode()`

Each file has slightly different task spawning (some pass `config`, some pass `tls_config`), but the JoinSet pattern is identical.

### Step 4: Pass `drain_timeout` to listener functions

The `config: Arc<BridgeConfig>` is already available in all listeners. Use `config.drain_timeout` for the drain deadline.

### Step 5: Add integration test

Add a test that starts an import bridge, connects multiple clients, sends a shutdown signal, and verifies all connections are drained (not aborted) within `drain_timeout`.

## Files Changed

- `src/import/listener.rs`
- `src/import/ws.rs`
- `src/import/auto.rs`
- `src/import/tls.rs`
- `src/import/multiroute.rs`
- New integration test in `tests/`
