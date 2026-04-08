# Plan 11: Shutdown & Connection Lifecycle Correctness

**Priority**: Medium
**Report refs**: 3.2, 3.6, 3.7, 4.1
**Effort**: Low-Medium
**Risk**: Low — each fix is localized and independent

## Problem

Several related issues affect connection lifecycle and shutdown correctness:

1. **3.2 — Multiroute: No EOF on timeout**: When a multiroute response times out, the liveliness token is undeclared but no EOF is published. The export side may hang waiting on the subscriber.

2. **3.6 — Backend close doesn't signal writer**: In `export/bridge.rs`, when the backend closes (read returns EOF), the backend-to-zenoh task publishes an EOF but doesn't cancel `cancel_zenoh_to_backend`. The writer task keeps trying to read from Zenoh and write to a dead backend until it errors out naturally.

3. **3.7 — Drain timeout stacks**: In `import/bridge.rs`, the drain timeout is applied per-direction sequentially. If both directions are slow, total wait is `2 * drain_timeout` instead of `drain_timeout`.

4. **4.1 — Liveliness subscriber not undeclared**: In `export/bridge.rs`, the liveliness subscriber created at line 69 is never explicitly undeclared when the export loop exits.

## Plan

### Fix 3.2: Publish EOF before undeclaring in multiroute timeout

**File**: `src/import/multiroute.rs`, around line 270

```rust
// Before (current):
if let Err(e) = liveliness_token.undeclare().await { ... }
if let Err(e) = tx_publisher.undeclare().await { ... }

// After:
// Publish EOF so export side knows to clean up
let _ = tx_publisher.put(&Vec::<u8>::new()).await;
if let Err(e) = liveliness_token.undeclare().await { ... }
if let Err(e) = tx_publisher.undeclare().await { ... }
```

Also publish EOF in the error path (`Ok(Err(e))` at line 287-289) before breaking.

### Fix 3.6: Signal writer direction when backend closes

**File**: `src/export/bridge.rs`, backend-to-zenoh task (around line 261-298)

After the task breaks (backend EOF or error), before undeclaring the publisher, cancel the writer direction:

```rust
// After the loop breaks and before publisher.undeclare():
cancel_zenoh_to_backend_clone.cancel();
```

This requires passing a clone of `cancel_zenoh_to_backend` into the backend-to-zenoh task. Currently only `cancel_backend_to_zenoh` (the token for cancelling THIS task) is passed as `b2z_token`.

Change: pass both tokens into the task, use `b2z_token` for select cancellation, and cancel the other direction's token on exit.

Similarly, the zenoh-to-backend task should cancel `cancel_backend_to_zenoh` on exit.

### Fix 3.7: Global deadline instead of stacking drain timeouts

**File**: `src/import/bridge.rs`, around line 223-279

Replace per-direction timeouts with a single deadline:

```rust
// Before:
tokio::select! {
    _ = &mut zenoh_to_client => {
        cancel_client_to_zenoh.cancel();
        match tokio::time::timeout(drain_timeout, &mut client_to_zenoh).await { ... }
    }
    _ = &mut client_to_zenoh => {
        cancel_zenoh_to_client.cancel();
        match tokio::time::timeout(drain_timeout, &mut zenoh_to_client).await { ... }
    }
}

// After:
let deadline = tokio::time::Instant::now() + drain_timeout;
tokio::select! {
    _ = &mut zenoh_to_client => {
        cancel_client_to_zenoh.cancel();
        match tokio::time::timeout_at(deadline, &mut client_to_zenoh).await { ... }
    }
    _ = &mut client_to_zenoh => {
        cancel_zenoh_to_client.cancel();
        match tokio::time::timeout_at(deadline, &mut zenoh_to_client).await { ... }
    }
}
```

Apply the same pattern in `src/export/bridge.rs` for the cancellation branch (line 361-386) where both directions are drained sequentially.

### Fix 4.1: Explicitly undeclare liveliness subscriber

**File**: `src/export/bridge.rs`, after the main loop exits

```rust
// After the loop:
if let Err(e) = liveliness_subscriber.undeclare().await {
    debug!(service = %service_name, "Error undeclaring liveliness subscriber: {:?}", e);
}
```

## Files Changed

- `src/import/multiroute.rs` — EOF on timeout (3.2)
- `src/export/bridge.rs` — signal writer on backend close (3.6), undeclare subscriber (4.1), global deadline (3.7)
- `src/import/bridge.rs` — global deadline (3.7)

## Testing

- Existing integration tests should verify no regressions
- The drain_integration tests cover timeout behavior
- Backend-close propagation is tested in export_import_integration
