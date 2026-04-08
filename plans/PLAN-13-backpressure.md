# Plan 13: Zenoh Publishing Backpressure

**Priority**: Medium
**Report ref**: 4.3
**Effort**: High
**Risk**: Medium — changes core data flow in both export and import bridges

## Problem

All bridge functions call `publisher.put()` without any form of backpressure. If the Zenoh network is congested or subscribers are slow, the AdvancedPublisher's cache fills up (max 64 samples) and excess samples may be silently dropped or cause memory growth.

The TCP reader keeps pulling data as fast as the socket provides it, publishing each chunk immediately. There is no feedback loop between Zenoh's ability to deliver samples and the rate at which the bridge reads from TCP.

Affected paths:
- `src/export/bridge.rs` — backend-to-zenoh task reads from backend, calls `publisher.put()`
- `src/import/bridge.rs` — client-to-zenoh task reads from client, calls `publisher.put()`
- `src/import/multiroute.rs` — publishes request data via `tx_publisher.put()`

## Analysis

Zenoh's `publisher.put()` is currently fire-and-forget (returns `Result` but doesn't indicate backpressure). The AdvancedPublisher with `.cache(max_samples(64))` stores recent samples for late subscribers, but this cache is not a backpressure mechanism.

Options considered:

1. **Bounded channel between reader and publisher** (recommended)
2. **Check put() errors and retry** (insufficient — put() doesn't indicate backpressure)
3. **Rate limiting** (too blunt — penalizes all traffic, not just congested paths)
4. **Circuit breaker** (complementary, not a replacement)

## Plan

### Step 1: Add a bounded async channel between TCP reader and Zenoh publisher

In both `export/bridge.rs` and `import/bridge.rs`, introduce a `tokio::sync::mpsc::channel` with a bounded capacity between the read task and the publish task:

```
TCP Read → [bounded channel (capacity N)] → Zenoh Publish
```

When the channel is full, the TCP reader blocks on `channel.send()`, which naturally slows down TCP reads. This propagates backpressure from Zenoh all the way back to the TCP socket, causing TCP flow control to kick in.

**Capacity**: Use `config.buffer_size / 1024` or a dedicated config field (e.g., `publish_queue_depth`, default 64). This gives ~64 chunks of buffering before backpressure kicks in.

### Step 2: Refactor backend-to-zenoh task in export/bridge.rs

```rust
// Current (single task):
let backend_to_zenoh = tokio::spawn(async move {
    loop {
        let data = backend_reader.read_data(buffer_size).await?;
        publisher.put(&data[..]).await?;
    }
});

// New (two tasks with channel):
let (pub_tx, mut pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(publish_queue_depth);

let reader_task = tokio::spawn(async move {
    loop {
        let data = backend_reader.read_data(buffer_size).await?;
        if data.is_empty() { break; } // EOF
        if pub_tx.send(data).await.is_err() { break; } // publisher gone
    }
});

let publisher_task = tokio::spawn(async move {
    while let Some(data) = pub_rx.recv().await {
        if let Err(e) = publisher.put(&data[..]).await {
            error!("Publish failed: {:?}", e);
            break;
        }
    }
});
```

### Step 3: Apply same pattern to import/bridge.rs

The client-to-zenoh direction has the same structure. Apply the bounded channel pattern.

### Step 4: Add `publish_queue_depth` config field

Add to `BridgeConfig`:
```rust
pub publish_queue_depth: usize, // default: 64
```

Thread it through `BridgeConfig::new()` and `Args`.

### Step 5: Monitor and log backpressure events

When the channel is full and the reader blocks, log at `debug` level:
```rust
if pub_tx.try_send(data).is_err() {
    debug!("Backpressure: publish queue full, slowing TCP read");
    pub_tx.send(data).await?; // blocks until space available
}
```

## Files Changed

- `src/export/bridge.rs` — split backend-to-zenoh into reader + publisher tasks
- `src/import/bridge.rs` — split client-to-zenoh into reader + publisher tasks
- `src/config.rs` — add `publish_queue_depth` field
- `src/args.rs` — add `--publish-queue-depth` CLI arg

## Testing

- Existing integration tests should pass (backpressure only activates under load)
- Add a stress test that sends data faster than Zenoh can deliver, verify no data loss
- Verify the bounded channel doesn't deadlock during shutdown

## Trade-offs

- **Pro**: Natural TCP flow control, prevents unbounded memory growth
- **Pro**: Graceful degradation under load instead of silent data loss
- **Con**: Adds one extra copy per chunk (through the channel)
- **Con**: Slightly more complex shutdown (two tasks per direction instead of one)
- **Con**: New config parameter to document and tune

## Alternative: Defer

If the complexity is too high for now, a simpler interim fix is to just log when `publisher.put()` returns an error, so operators at least know data is being lost. This doesn't fix the problem but makes it observable.
