//! Per-connection reception backpressure (D2 #22).
//!
//! Zenoh's default subscriber handler is a bounded FIFO whose delivery callback
//! *blocks* the shared session's reception thread when the channel is full — so
//! a single slow TCP writer, by not draining its response subscriber,
//! head-of-line-blocks **every** other client on the same session (zenoh's own
//! `FifoChannel` docs note this hazard and point at `RingChannel`).
//!
//! Instead, each client's response subscriber is drained through a bounded
//! [`tokio::sync::mpsc`] channel fed by a **non-blocking** callback (`try_send`),
//! so the reception thread is never blocked. On overflow, the policy is
//! reliability-mode aware:
//!
//! - [`ReliabilityMode::Stream`] (byte-exact): reset *this one* connection (trip
//!   its cancellation token). Bytes are never dropped or reordered — the slow
//!   client loses its own connection instead of stalling the session.
//! - [`ReliabilityMode::Telemetry`] (loss-tolerant): shed the overflowing sample
//!   and count it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::ReliabilityMode;

/// Build a non-blocking reception callback and its bounded drain channel.
///
/// Install the callback on the subscriber via `.callback(cb)`, and drain the
/// returned [`mpsc::Receiver`] in the connection's reception loop in place of
/// `subscriber.recv_async()`. `conn_cancel` is the connection's reset token,
/// tripped on overflow in [`ReliabilityMode::Stream`].
///
/// Generic over the item type so it can be unit-tested without constructing a
/// Zenoh `Sample`; in the bridge it is instantiated with `T = zenoh::sample::Sample`.
pub(crate) fn rx_channel<T: Send + 'static>(
    capacity: usize,
    mode: ReliabilityMode,
    conn_cancel: CancellationToken,
    client_id: String,
) -> (impl Fn(T) + Send + Sync + 'static, mpsc::Receiver<T>) {
    let (tx, rx) = mpsc::channel::<T>(capacity);
    // Total samples shed on overflow (Telemetry mode); kept inside the callback
    // for its logging cadence.
    let shed = Arc::new(AtomicU64::new(0));
    let shed_cb = shed;

    let callback = move |sample: T| {
        // `try_send` never blocks the Zenoh reception thread — this is the whole
        // point of D2. A full channel means this client's TCP writer is not
        // keeping up.
        match tx.try_send(sample) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => match mode {
                ReliabilityMode::Stream => {
                    // Byte-exact: we must not drop. Reset this one connection so
                    // it stops backing up; every other client keeps flowing.
                    warn!(
                        client_id = %client_id,
                        "reception buffer full — resetting slow connection (D2)"
                    );
                    conn_cancel.cancel();
                }
                ReliabilityMode::Telemetry => {
                    // Loss-tolerant: shed the sample. Log on power-of-two counts
                    // to surface the condition without flooding.
                    let n = shed_cb.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_power_of_two() {
                        warn!(
                            client_id = %client_id,
                            shed_total = n,
                            "reception buffer full — shedding sample (D2)"
                        );
                    }
                }
            },
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Receiver dropped: the connection is tearing down. Discard.
            }
        }
    };

    (callback, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivers_up_to_capacity_without_blocking() {
        let token = CancellationToken::new();
        let (cb, mut rx) = rx_channel::<u32>(4, ReliabilityMode::Stream, token.clone(), "c".into());
        // Fill to capacity. The callback must never block (this test would hang
        // if it did — that is the D2 hazard being guarded).
        for i in 0..4 {
            cb(i);
        }
        assert!(!token.is_cancelled(), "no overflow yet");
        for i in 0..4 {
            assert_eq!(rx.recv().await, Some(i));
        }
    }

    #[tokio::test]
    async fn stream_overflow_resets_connection() {
        let token = CancellationToken::new();
        // Do NOT drain rx, so the channel fills.
        let (cb, _rx) = rx_channel::<u32>(2, ReliabilityMode::Stream, token.clone(), "c".into());
        cb(1);
        cb(2);
        assert!(!token.is_cancelled(), "at capacity, not over");
        cb(3); // overflow
        assert!(
            token.is_cancelled(),
            "Stream mode must reset the connection on reception overflow"
        );
    }

    #[tokio::test]
    async fn telemetry_overflow_sheds_without_reset() {
        let token = CancellationToken::new();
        let (cb, _rx) = rx_channel::<u32>(2, ReliabilityMode::Telemetry, token.clone(), "c".into());
        cb(1);
        cb(2);
        cb(3); // overflow -> shed
        cb(4); // overflow -> shed
        assert!(
            !token.is_cancelled(),
            "Telemetry mode sheds on overflow, never resets the connection"
        );
    }

    #[tokio::test]
    async fn closed_receiver_is_a_noop() {
        let token = CancellationToken::new();
        let (cb, rx) = rx_channel::<u32>(2, ReliabilityMode::Stream, token.clone(), "c".into());
        drop(rx); // connection tearing down
        cb(1); // must not panic or reset
        assert!(!token.is_cancelled());
    }
}
