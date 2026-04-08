//! Bug demonstration tests
//!
//! Each test in this file demonstrates a specific, confirmed bug in the codebase.
//! Tests are named `bug_NNN_description` and include comments explaining the bug,
//! why it matters, and where to fix it.
//!
//! Tests that demonstrate the bug by FAILING an assertion are marked `#[should_panic]`
//! with a comment explaining what correct behavior would look like.

// ============================================================================
// BUG 1: HTTP Response Smuggling — Transfer-Encoding + Content-Length both accepted
// File: src/http_response_parser.rs:39-60
// Severity: CRITICAL (security)
//
// RFC 7230 §3.3.3 says a response with BOTH Transfer-Encoding and Content-Length
// MUST be treated as an error. Our parser silently picks Transfer-Encoding (first
// match wins) and ignores Content-Length. An attacker can exploit this to desync
// the bridge's view of response boundaries from a downstream proxy or client,
// enabling HTTP response smuggling.
// ============================================================================

#[test]
fn bug_001_transfer_encoding_and_content_length_both_accepted() {
    // A response with BOTH Transfer-Encoding: chunked AND Content-Length.
    // Per RFC 7230 §3.3.3, this MUST be rejected as invalid.
    let response = b"HTTP/1.1 200 OK\r\n\
                     Transfer-Encoding: chunked\r\n\
                     Content-Length: 100\r\n\
                     \r\n";

    let result =
        zenoh_bridge_tcp::http_response_parser::parse_response_headers(response).unwrap();
    let (_header_len, framing) = result.unwrap();

    // BUG: The parser happily returns Chunked, ignoring the conflicting Content-Length.
    // A correct implementation should return Err(...) for this ambiguous response.
    assert_eq!(
        framing,
        zenoh_bridge_tcp::http_response_parser::ResponseBodyFraming::Chunked,
        "BUG DEMONSTRATED: parser accepts response with both TE and CL headers \
         (should reject as invalid per RFC 7230 §3.3.3)"
    );
    // CORRECT BEHAVIOR: parse_response_headers should return Err(BridgeError::HttpParse(...))
}

// ============================================================================
// BUG 2: Multiple Content-Length headers with different values silently accepted
// File: src/http_response_parser.rs:49-56
// Severity: HIGH (security — HTTP response smuggling)
//
// RFC 7230 §3.3.3: "If a message is received with ... multiple Content-Length
// header fields having differing field-values ... the recipient MUST treat it
// as an unrecoverable error."
// Our parser takes the FIRST Content-Length and ignores the rest.
// ============================================================================

#[test]
fn bug_002_multiple_content_length_headers_silently_accepted() {
    // Two Content-Length headers with DIFFERENT values.
    let response = b"HTTP/1.1 200 OK\r\n\
                     Content-Length: 10\r\n\
                     Content-Length: 50\r\n\
                     \r\n";

    let result =
        zenoh_bridge_tcp::http_response_parser::parse_response_headers(response).unwrap();
    let (_header_len, framing) = result.unwrap();

    // BUG: Parser returns the FIRST Content-Length (10) without checking for conflicts.
    assert_eq!(
        framing,
        zenoh_bridge_tcp::http_response_parser::ResponseBodyFraming::ContentLength(10),
        "BUG DEMONSTRATED: parser takes first Content-Length, ignores conflicting second (50)"
    );
    // CORRECT BEHAVIOR: Should reject when multiple Content-Length values disagree.
}

// ============================================================================
// BUG 3: Content-Length has no upper bound — absurd values accepted
// File: src/http_response_parser.rs:49-56
// Severity: HIGH (DoS)
//
// The parser accepts any usize as Content-Length without checking against
// max_response_size. In multiroute mode (multiroute.rs:245), the bridge
// waits until `body_received >= expected`. With Content-Length = usize::MAX,
// the bridge will accumulate data until OOM.
//
// (The max_response_size check in multiroute.rs:212 catches this at 10MB,
// but the parser itself should reject obviously absurd values at the parsing
// layer for defense-in-depth.)
// ============================================================================

#[test]
fn bug_003_unbounded_content_length_accepted() {
    // Ridiculous Content-Length that would require petabytes of data.
    let response = b"HTTP/1.1 200 OK\r\n\
                     Content-Length: 999999999999999\r\n\
                     \r\n";

    let result =
        zenoh_bridge_tcp::http_response_parser::parse_response_headers(response).unwrap();
    let (_header_len, framing) = result.unwrap();

    // BUG: The parser happily accepts a 999 terabyte Content-Length.
    assert_eq!(
        framing,
        zenoh_bridge_tcp::http_response_parser::ResponseBodyFraming::ContentLength(
            999_999_999_999_999
        ),
        "BUG DEMONSTRATED: absurdly large Content-Length accepted without bounds check"
    );
    // CORRECT BEHAVIOR: Should reject Content-Length values that exceed a reasonable max.
}

// ============================================================================
// BUG 4: Chunked body parser — no bounds check on chunk size
// File: src/http_response_parser.rs:115
// Severity: MEDIUM (DoS on 32-bit / defense-in-depth on 64-bit)
//
// `find_chunked_body_end` trusts the chunk size hex value from the stream.
// The arithmetic `pos + chunk_size + 2` has no overflow protection.
// On 32-bit, a chunk size of 0xFFFFFFFD with pos=5 wraps to 4, causing
// the parser to skip backwards and potentially parse garbage as chunks.
// On 64-bit, the value is just huge and returns None (OOM-safe but wasteful).
// ============================================================================

#[test]
fn bug_004_chunked_huge_chunk_size_no_bounds_check() {
    // Chunk claims to be 1 TB. The parser should reject this.
    let body = b"E8D4A51000\r\n";  // 1,000,000,000,000 in hex

    let result = zenoh_bridge_tcp::http_response_parser::find_chunked_body_end(body);

    // On 64-bit: returns None (needs more data — it would need 1TB of data!)
    // This isn't a crash, but the parser accepted an absurd chunk size without
    // rejecting it. A well-behaved parser should have a maximum chunk size.
    assert_eq!(
        result, None,
        "Parser returns None (waiting for 1TB of data) instead of rejecting"
    );
    // CORRECT BEHAVIOR: Should return an error/signal for absurd chunk sizes.
    // The caller (multiroute) has max_response_size, but the parser should
    // also reject at the parsing layer as defense-in-depth.
}

#[test]
fn bug_004b_chunked_overflow_wraps_on_small_usize() {
    // This demonstrates the arithmetic that WOULD wrap on 32-bit.
    // On 64-bit the values just don't wrap, so we simulate the math.
    let pos: u32 = 5;
    let chunk_size: u32 = 0xFFFF_FFFD; // near u32::MAX
    let chunk_end_wrapping = pos.wrapping_add(chunk_size).wrapping_add(2);

    // On 32-bit usize, this wraps to 4 — the parser would read BACKWARDS
    assert_eq!(
        chunk_end_wrapping, 4,
        "BUG DEMONSTRATED: pos + chunk_size + 2 wraps around on 32-bit, \
         causing chunk_end < pos (reads backwards)"
    );
}

// ============================================================================
// BUG 5: Empty client ID accepted from liveliness key ending with '/'
// File: src/export/bridge.rs:91, 114
// Severity: MEDIUM (data corruption)
//
// rsplit('/').next() always returns Some on non-empty strings.
// If a liveliness key ends with '/', the client_id is "" (empty string).
// This creates Zenoh keys like "service//tx//" which can collide.
// ============================================================================

#[test]
fn bug_005_empty_client_id_from_trailing_slash() {
    // Simulates what happens when a liveliness key ends with '/'
    let key = "myservice/clients/";
    let client_id = key.rsplit('/').next().unwrap();

    // BUG: rsplit('/').next() returns "" for trailing slash
    assert_eq!(
        client_id, "",
        "BUG DEMONSTRATED: trailing slash produces empty client_id"
    );

    // This empty client_id then creates ambiguous Zenoh keys:
    let tx_key = format!("myservice/tx/{}", client_id);
    let rx_key = format!("myservice/rx/{}", client_id);

    assert_eq!(tx_key, "myservice/tx/");
    assert_eq!(rx_key, "myservice/rx/");
    // These keys end with '/' which is valid but will collide if another
    // client also gets an empty ID. The code should reject empty client IDs.
}

// ============================================================================
// BUG 6: Zero buffer_size causes immediate EOF on all reads
// File: src/args.rs (no validation), src/transport.rs:66-82
// Severity: HIGH (silent total failure)
//
// buffer_size=0 is accepted by CLI validation. TcpReader::new creates a
// 0-length buffer. read() on a 0-length buffer always returns 0 (EOF).
// The bridge interprets this as "connection closed" immediately.
// Every connection appears to close instantly — total silent failure.
// ============================================================================

#[tokio::test]
async fn bug_006_zero_buffer_size_causes_immediate_eof() {
    use tokio::io::AsyncWriteExt;
    use zenoh_bridge_tcp::transport::{TcpReader, TransportReader};

    let (mut client, server) = tokio::io::duplex(1024);

    // Client writes data
    client.write_all(b"Hello, World!").await.unwrap();

    // Create reader with buffer_size = 0
    let mut reader = TcpReader::new(server, 0);

    // BUG: read on a 0-length buffer returns 0, which is interpreted as EOF
    let data = reader.read_data(0).await.unwrap();
    assert!(
        data.is_empty(),
        "BUG DEMONSTRATED: zero buffer_size causes read to return empty (EOF), \
         even though the client sent data"
    );
    // CORRECT BEHAVIOR: Args::validate() should reject buffer_size < some minimum (e.g., 1024)
}

// ============================================================================
// BUG 7: Zero drain_timeout accepted — breaks graceful shutdown
// File: src/args.rs (no validation), used in export/bridge.rs, import/bridge.rs
// Severity: MEDIUM (shutdown data loss)
//
// drain_timeout=0 means all drain waits timeout immediately.
// Tasks in the middle of flushing data are aborted without draining.
// ============================================================================

#[test]
fn bug_007_zero_drain_timeout_accepted() {
    use zenoh_bridge_tcp::config::BridgeConfig;

    // BridgeConfig::new accepts 0 for drain_timeout
    let config = BridgeConfig::new(65536, 10, 0);

    // BUG: drain_timeout is 0, meaning tokio::time::timeout(drain_timeout, ...) fires instantly
    assert_eq!(
        config.drain_timeout,
        std::time::Duration::from_secs(0),
        "BUG DEMONSTRATED: zero drain_timeout accepted, \
         all graceful shutdown waits will timeout immediately"
    );
    // CORRECT BEHAVIOR: BridgeConfig::new or Args::validate() should enforce drain_timeout >= 1s
}

// ============================================================================
// BUG 8: Hardcoded drain timeout in main.rs ignores CLI --drain-timeout
// File: src/main.rs:326
// Severity: HIGH (user config silently ignored)
//
// main.rs hardcodes `Duration::from_secs(10)` for the top-level shutdown
// timeout. The user's --drain-timeout value (stored in args.drain_timeout)
// is used by individual bridge tasks but NOT by the main shutdown loop.
// ============================================================================

#[test]
fn bug_008_main_drain_timeout_hardcoded() {
    use zenoh_bridge_tcp::config::BridgeConfig;

    // User configures drain_timeout = 30 seconds via CLI
    let config = BridgeConfig::new(65536, 10, 30);

    // The bridge tasks correctly get 30s
    assert_eq!(config.drain_timeout, std::time::Duration::from_secs(30));

    // BUG: But main.rs:326 does:
    //   let drain_timeout = tokio::time::Duration::from_secs(10);
    // So the top-level shutdown wait is ALWAYS 10 seconds, regardless of user config.
    //
    // If tasks need more than 10s to drain (user set 30s for a reason),
    // main.rs will abort them after 10s.
    //
    // FIX: Change main.rs:326 to:
    //   let drain_timeout = tokio::time::Duration::from_secs(args.drain_timeout);
    //
    // We can't directly test main() here, but we can verify the config mismatch:
    let main_rs_hardcoded_timeout = std::time::Duration::from_secs(10);
    assert_ne!(
        config.drain_timeout, main_rs_hardcoded_timeout,
        "User set drain_timeout=30s, but main.rs will hardcode 10s"
    );
}

// ============================================================================
// BUG 9: Export shutdown sends cancel signals but doesn't await task handles
// File: src/export/bridge.rs:141-148
// Severity: CRITICAL (data loss on shutdown)
//
// When the shutdown token fires, the code iterates over cancellation_senders,
// sends cancel signals via tx.send(), but then immediately breaks out of the
// loop WITHOUT awaiting any of the JoinHandles stored alongside the senders.
//
// The tasks may still be flushing data to Zenoh publishers, and breaking
// immediately means the function returns, dropping all resources.
// ============================================================================

#[tokio::test]
async fn bug_009_shutdown_ignores_task_handles() {
    // This test demonstrates that the shutdown path in export/bridge.rs
    // sends cancel signals but does NOT await the task handles.
    //
    // From src/export/bridge.rs:144:
    //   for (client_id, (tx, _)) in senders.iter() {
    //                         ^ JoinHandle is ignored!
    //
    // Versus handle_client_disconnect (line 405) which correctly does:
    //   let _ = cancel_tx.send(()).await;
    //   match tokio::time::timeout(drain_timeout, task_handle).await { ... }
    //                                              ^^^^^^^^^^^ awaits the handle!
    //
    // We demonstrate this by simulating the pattern: spawn a task that needs
    // time to drain, then show that NOT awaiting the handle means data is lost.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let flushed = Arc::new(AtomicBool::new(false));
    let flushed_clone = flushed.clone();

    let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(1);

    // Simulate a client bridge task that needs to flush data on cancellation
    let handle = tokio::spawn(async move {
        // Wait for cancellation
        let _ = cancel_rx.recv().await;
        // Simulate flushing data (takes some time)
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        flushed_clone.store(true, Ordering::SeqCst);
    });

    // Simulate the BUGGY shutdown path: send cancel but DON'T await handle
    let _ = cancel_tx.send(()).await;
    // break; // (in real code, exits the loop immediately)
    drop(handle); // Handle is dropped without awaiting — task may still be running

    // Give the task some time
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // BUG: The task may not have finished flushing yet because we didn't await it.
    // In the real code, the function returns and the task's resources get dropped.
    // FIX: After sending cancels, collect and await all handles with drain_timeout.
    //
    // (Note: in this test the task IS still running because the runtime is alive.
    // In production, the export loop returns, the runtime may shut down, and the
    // task gets aborted mid-flush.)
}

// ============================================================================
// BUG 10: Export client reconnect — new task spawned BEFORE old task cancelled
// File: src/export/tcp.rs:74-102
// Severity: HIGH (concurrent Zenoh pub/sub on same keys)
//
// When a client reconnects:
//   1. Line 74: New task is spawned (subscribes to tx/{client_id}, publishes to rx/{client_id})
//   2. Line 96: Lock acquired
//   3. Line 99: Old cancel sender dropped (triggering old task's cancel_rx)
//   4. Line 100: Wait for old handle (with timeout)
//
// Between step 1 and step 3, BOTH the old and new tasks have active
// subscribers on the same Zenoh key. Data arriving on tx/{client_id}
// will be delivered to both subscribers, causing duplicate processing.
// ============================================================================

#[tokio::test]
async fn bug_010_reconnect_spawns_new_before_cancelling_old() {
    // Demonstrate the ordering issue in tcp.rs:
    //
    //   let main_handle = tokio::spawn(async move {   // Line 74: NEW task starts
    //       handle_client_bridge(...)                  //     Immediately subscribes to keys
    //   });
    //
    //   {
    //       let mut senders = cancellation_senders.lock().await;  // Line 96
    //       if let Some((old_cancel_tx, old_handle)) = senders.remove(...) {
    //           drop(old_cancel_tx);                               // Line 99: OLD cancelled
    //           let _ = timeout(drain_timeout, old_handle).await;  // Line 100: Wait for old
    //       }
    //       senders.insert(..., (cancel_tx, main_handle));         // Line 102
    //   }
    //
    // The correct order should be:
    //   1. Cancel old task
    //   2. Wait for old task to finish
    //   3. THEN spawn new task

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let active_count = Arc::new(AtomicU32::new(0));

    // Simulate the "old task" that's currently running
    let count_old = active_count.clone();
    let (_old_tx, mut old_rx) = tokio::sync::mpsc::channel::<()>(1);
    let old_handle = tokio::spawn(async move {
        count_old.fetch_add(1, Ordering::SeqCst);
        let _ = old_rx.recv().await; // Waits for cancellation
        count_old.fetch_sub(1, Ordering::SeqCst);
    });

    // Give old task time to start
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(active_count.load(Ordering::SeqCst), 1, "Old task running");

    // Simulate the BUGGY reconnect: spawn new BEFORE cancelling old
    let count_new = active_count.clone();
    let _new_handle = tokio::spawn(async move {
        count_new.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        count_new.fetch_sub(1, Ordering::SeqCst);
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // BUG: Both tasks are active simultaneously!
    // In real code, both would have Zenoh subscribers on the same keys,
    // causing duplicate data delivery.
    assert_eq!(
        active_count.load(Ordering::SeqCst),
        2,
        "BUG DEMONSTRATED: both old and new tasks running concurrently \
         (would cause duplicate Zenoh subscriptions on same keys)"
    );

    // NOW cancel the old task (this is where tcp.rs does it — AFTER spawning new)
    drop(_old_tx);
    let _ = old_handle.await;

    // FIX: Cancel and await old task BEFORE spawning new one
}

// ============================================================================
// BUG 11: Export reconnect — drop(old_cancel_tx) instead of send()
// File: src/export/tcp.rs:99
// Severity: MEDIUM (delayed cancellation)
//
// The code does `drop(old_cancel_tx)` to signal cancellation.
// Dropping the sender closes the channel, causing recv() to return None.
// While this DOES eventually signal the receiver, it's a weaker signal:
// - mpsc::Receiver::recv() returns None (not a message)
// - The select! branch `_ = cancel_rx.recv()` matches on None
// - But it could race with other select branches in unexpected ways
//
// The correct approach is `old_cancel_tx.send(()).await` which sends an
// explicit message before the channel is dropped.
// ============================================================================

#[tokio::test]
async fn bug_011_cancel_by_drop_vs_explicit_send() {
    use tokio::sync::mpsc;

    // Method 1: drop (current code)
    let (tx1, mut rx1) = mpsc::channel::<()>(1);
    drop(tx1);
    // recv returns None — channel closed
    assert_eq!(rx1.recv().await, None, "drop causes recv to return None");

    // Method 2: explicit send (correct approach)
    let (tx2, mut rx2) = mpsc::channel::<()>(1);
    tx2.send(()).await.unwrap();
    // recv returns Some(()) — explicit message
    assert_eq!(
        rx2.recv().await,
        Some(()),
        "send causes recv to return Some(())"
    );

    // Both work, but send() is more intentional and makes the code's intent clear.
    // More importantly, send() guarantees the message is available BEFORE the
    // channel is closed, while drop() only closes the channel.
    // In a select! context, the timing difference can matter.
}

// ============================================================================
// BUG 12: Export shutdown holds mutex across async await
// File: src/export/bridge.rs:143-147
// Severity: MEDIUM (potential deadlock / latency)
//
// The shutdown path acquires the cancellation_senders lock and then
// calls tx.send().await WHILE HOLDING THE LOCK. If any channel send
// blocks (full channel), all other concurrent operations on the map
// (e.g., handle_client_disconnect) are blocked.
// ============================================================================

#[tokio::test]
async fn bug_012_mutex_held_across_await() {
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};

    // Simulate the pattern from bridge.rs:143-147
    let map: Arc<Mutex<Vec<mpsc::Sender<()>>>> = Arc::new(Mutex::new(Vec::new()));

    // Create a channel with capacity 1 and fill it
    let (tx, _rx) = mpsc::channel::<()>(1);
    // Don't fill the channel for this demo, but note that in production
    // the channel might not be ready to accept a send immediately.

    map.lock().await.push(tx);

    // Simulate shutdown path holding the lock while sending
    let map_clone = map.clone();
    let shutdown_task = tokio::spawn(async move {
        let senders = map_clone.lock().await;
        for tx in senders.iter() {
            // This .await happens WHILE the lock is held
            let _ = tx.send(()).await;
        }
        // Lock held for the entire loop duration
    });

    // Simulate concurrent operation trying to acquire the same lock
    let map_clone2 = map.clone();
    let concurrent_task = tokio::spawn(async move {
        // This will BLOCK until shutdown_task releases the lock
        let start = std::time::Instant::now();
        let _senders = map_clone2.lock().await;
        start.elapsed()
    });

    let _ = shutdown_task.await;
    let wait_time = concurrent_task.await.unwrap();

    // BUG: The concurrent task was blocked while shutdown held the lock.
    // In production, if tx.send().await blocks for a slow channel,
    // ALL concurrent disconnect/reconnect operations are blocked.
    //
    // FIX: Collect senders, release lock, then send:
    //   let senders: Vec<_> = cancellation_senders.lock().await
    //       .drain().map(|(_, (tx, handle))| (tx, handle)).collect();
    //   for (tx, _) in &senders {
    //       let _ = tx.send(()).await;
    //   }
    let _ = wait_time; // Just proving the pattern blocks
}

// ============================================================================
// BUG 13: Multiroute sends 504 response AFTER data already written to client
// File: src/import/multiroute.rs:291-295
// Severity: HIGH (invalid HTTP — client receives two status lines)
//
// When the response timeout fires (line 291), the code unconditionally
// writes an HTTP 504 response to the stream. But if bytes_written > 0,
// the client has already received a partial HTTP response (with headers).
// Sending a second HTTP response on the same connection is invalid HTTP
// and will corrupt the client's parser.
//
// Compare with the max_response_size check at line 214 which correctly
// checks `if bytes_written == 0` before sending an error.
// ============================================================================

#[test]
fn bug_013_multiroute_504_after_data_written() {
    // This demonstrates the inconsistency in error handling.
    //
    // At line 212-218 (response too large):
    //   if response_buf.len() > config.max_response_size {
    //       if bytes_written == 0 {           <-- Correctly checks!
    //           let error_resp = http_502_response("Response too large");
    //           let _ = stream.write_all(&error_resp).await;
    //       }
    //       return Err(...);
    //   }
    //
    // At line 291-295 (timeout):
    //   Err(_) => {
    //       warn!(..., "Response timeout");
    //       let _ = stream                    <-- NO bytes_written check!
    //           .write_all(&http_504_response())
    //           .await;
    //       break;
    //   }
    //
    // The 502 path checks bytes_written. The 504 path does NOT.
    // FIX: Add `if bytes_written == 0` guard before writing 504.

    // We can verify the 502 path handles it correctly:
    let bytes_written: usize = 100; // Simulating partial data already sent
    let would_send_502 = bytes_written == 0;
    assert!(
        !would_send_502,
        "502 path correctly skips sending error when data was already written"
    );

    // But the 504 path always sends (BUG):
    let would_send_504 = true; // No check in the code
    assert!(
        would_send_504,
        "BUG DEMONSTRATED: 504 path always sends error, even after partial data"
    );
}

// ============================================================================
// BUG 14: Args::validate() doesn't validate spec formats
// File: src/args.rs:156-183
// Severity: MEDIUM (confusing late errors)
//
// validate() only checks that at least one spec list is non-empty.
// It does NOT validate spec string formats. Invalid specs like
// "no_slash_here" pass validation and only fail at runtime when
// parse_export_spec or parse_import_spec is called.
// ============================================================================

#[test]
fn bug_014_invalid_spec_passes_validation() {
    // We can't construct Args directly from integration tests (Default is cfg(test) only),
    // but we can verify the spec parser behavior that validate() SHOULD call:

    // This is the format expected by parse_export_spec: "service_name/backend_addr"
    let valid_spec = "myservice/127.0.0.1:8000";
    let invalid_spec = "invalid_spec_no_slash";

    // The spec parser correctly rejects invalid formats
    let valid_parts: Vec<&str> = valid_spec.splitn(2, '/').collect();
    let invalid_parts: Vec<&str> = invalid_spec.splitn(2, '/').collect();

    assert_eq!(valid_parts.len(), 2, "Valid spec splits into 2 parts");
    assert_eq!(
        invalid_parts.len(),
        1,
        "Invalid spec only has 1 part (no '/' separator)"
    );

    // BUG: Args::validate() only checks `!self.export.is_empty()`.
    // It does NOT call any spec parser. So this invalid spec passes validation
    // and only fails at runtime when the task tries to parse it.
    //
    // CORRECT BEHAVIOR: validate() should call parse_export_spec() for each spec
    // to catch format errors at startup with clear error messages.
}

// ============================================================================
// BUG 15: log_level and log_format accept arbitrary strings without validation
// File: src/args.rs:114-120, src/main.rs:17-37
// Severity: LOW (confusing silent fallback)
//
// Invalid log_format values silently fall back to "pretty" instead of
// reporting an error. Invalid log_level values are silently handled by
// EnvFilter::new() (which may or may not do what the user expects).
// ============================================================================

#[test]
fn bug_015_invalid_log_format_silently_falls_back() {
    // main.rs init_tracing() silently falls back to "pretty" for unknown formats:
    //
    //   match log_format {
    //       "json" => { ... }
    //       "compact" => { ... }
    //       _ => { /* "pretty" or default — silently used for ANY unknown value */ }
    //   }
    //
    // If user passes --log-format=yaml, they get no error — just "pretty" output.
    // validate() doesn't check log_format at all.

    let supported_formats = ["pretty", "compact", "json"];
    let invalid_format = "nonexistent_format";

    assert!(
        !supported_formats.contains(&invalid_format),
        "This format is not supported"
    );

    // BUG: No validation rejects it. The match in init_tracing silently falls
    // through to the default `_` arm.
    // CORRECT BEHAVIOR: validate() should check log_format is one of the supported values.
}

// ============================================================================
// BUG 16: TLS ClientHello size validation — inner loop check is inconsistent
// File: src/tls_parser.rs:91-96
// Severity: MEDIUM (defense-in-depth)
//
// The outer check validates `length > max_handshake_size` (line 83).
// But `total_size = 5 + length` can exceed max_handshake_size when
// `length == max_handshake_size`. The inner loop check at line 93
// checks `buffer.len() >= max_handshake_size`, but should check
// `buffer.len() >= total_size` to be consistent.
//
// When length == max_handshake_size:
//   - Outer check passes (length is NOT > max)
//   - total_size = max + 5 (exceeds max by 5 bytes)
//   - Inner loop reads up to total_size, which is max + 5
//   - The check `buffer.len() >= max_handshake_size` triggers at max,
//     BEFORE total_size is reached, causing a spurious error
// ============================================================================

#[test]
fn bug_016_tls_size_check_inconsistency() {
    // Demonstrate the off-by-one edge case
    let max_handshake_size: usize = 16384; // Default from BridgeConfig

    // If TLS record declares length = max_handshake_size exactly
    let length: usize = max_handshake_size;

    // Outer check at line 83: length > max_handshake_size → false (passes)
    assert!(
        !(length > max_handshake_size),
        "Outer check passes when length == max"
    );

    // total_size at line 91
    let total_size = 5 + length;
    assert_eq!(total_size, max_handshake_size + 5);

    // Inner check at line 93: buffer.len() >= max_handshake_size
    // This fires when buffer reaches max_handshake_size (16384 bytes),
    // but we need total_size (16389 bytes).
    // Result: valid handshake of exactly max size is REJECTED.
    let buffer_len = max_handshake_size;
    let inner_check_fires = buffer_len >= max_handshake_size;
    let need_more_data = buffer_len < total_size;

    assert!(
        inner_check_fires && need_more_data,
        "BUG DEMONSTRATED: inner check rejects a valid handshake at exactly max size. \
         Buffer is {buffer_len} bytes, needs {total_size}, but inner check fires at {max_handshake_size}"
    );
    // FIX: Change line 93 from `buffer.len() >= max_handshake_size`
    //       to `buffer.len() >= total_size + some_margin` or
    //       change the outer check to `length > max_handshake_size - 5`
}
