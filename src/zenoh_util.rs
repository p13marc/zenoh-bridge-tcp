//! Shared Zenoh reliability helpers used by both data planes.
//!
//! Two hazards keep recurring in this codebase, and both live here so the fix
//! is written once:
//!
//! 1. **Publishing before the peer's interest has propagated.** A sample put
//!    while no matching subscriber is known to the session lands nowhere; only
//!    the publisher's bounded cache can save it, and only for subscribers that
//!    query history. [`await_matching_subscriber`] gates the first publish.
//! 2. **Fire-once control signals.** The error channel used to be a bare
//!    `session.put()` — uncached, unrecoverable, and raced by its own trigger
//!    (a backend dial refusal fails within microseconds of the export first
//!    learning the client exists, i.e. at maximal interest-propagation skew).
//!    [`publish_error_signal`] makes the signal recoverable.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
use zenoh::sample::SampleKind;
use zenoh_ext::{AdvancedPublisher, AdvancedPublisherBuilderExt, CacheConfig, MissDetectionConfig};

use crate::config::ReliabilityMode;

/// Wait until `publisher` has a matching subscriber, bounded by `timeout`.
///
/// Connections publish on **fresh** per-client keys, and the peer subscribes
/// only after it observes a liveliness token — milliseconds later. Relaying
/// into that window publishes bytes nowhere recoverable except the publisher's
/// cache, which holds `cache_size` *samples*: a burst larger than the cache
/// reached the peer truncated and was still reported as a clean completion.
///
/// Best-effort by design: on timeout we relay anyway, which is exactly the
/// old behaviour. This can only ever reduce loss — it never converts a working
/// connection into a stalled one.
///
/// Only `Stream` gates: `Telemetry` is explicitly loss-tolerant, so making it
/// pay setup latency to avoid a loss it accepts by definition would be a
/// straight regression.
pub(crate) async fn await_matching_subscriber<T>(
    publisher: &AdvancedPublisher<'_>,
    reliability: ReliabilityMode,
    timeout: Duration,
    what: T,
) where
    T: std::fmt::Display,
{
    if reliability != ReliabilityMode::Stream {
        return;
    }

    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    loop {
        // Bound the query itself, not just the loop. `matching_status()` is an
        // async round-trip through the session: if it stays pending, a loop that
        // only checks its deadline *between* attempts never gets to check it
        // again, and a bounded wait silently becomes a hung connection.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, publisher.matching_status()).await {
            Ok(Ok(status)) if status.matching() => {
                debug!(
                    waited_ms = started.elapsed().as_millis() as u64,
                    "Peer subscriber attached"
                );
                return;
            }
            // Not yet, or the query failed or timed out — a failed status says
            // nothing about the peer, so let the deadline below decide.
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                what = %what,
                timeout_ms = timeout.as_millis() as u64,
                "No subscriber matched in time; relaying anyway (bytes sent now may be lost)"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// How long the error publisher outlives its put when the client token cannot
/// be observed at all — a backstop so the holder task can never leak.
const ERROR_HOLD_CAP: Duration = Duration::from_secs(60);

/// Publish a **recoverable** teardown signal on `error_key`.
///
/// The signal races its own cause: it fires within microseconds of the export
/// first learning that `clients_key`'s owner exists, so the import's error
/// subscriber — declared first on its side, but whose *interest* travels the
/// same links — may not be known to this session yet. A bare put is then lost
/// forever and the client hangs. Three layers close that:
///
/// 1. The publisher is Advanced with a 1-sample cache, so a subscriber whose
///    history query arrives late still recovers the signal.
/// 2. A bounded [`await_matching_subscriber`] gate catches the common case
///    up front.
/// 3. The publisher (and its cache) is **held alive until the client's
///    liveliness token disappears** — the natural lifetime of the signal: it
///    matters exactly as long as the failed client still exists. A capped
///    watcher task owns it, so nothing leaks if the token is never seen.
///
/// Returns `Err` only when the signal could not even be published locally;
/// callers must not log success in that case.
pub(crate) async fn publish_error_signal(
    session: &Arc<Session>,
    error_key: String,
    clients_key: String,
    availability_timeout: Duration,
    heartbeat_interval: Duration,
    message: &'static str,
) -> Result<()> {
    // Bound the WHOLE publish: declaring and putting are async round-trips
    // through the session, and this path runs precisely when a peer just died
    // abruptly — observed to leave a declare pending for tens of seconds while
    // the dead link tears down. An unbounded await here would pin the caller
    // (and, transitively, whoever drains it) on a signal that has nobody left
    // to help.
    let budget = availability_timeout.max(Duration::from_secs(1)) * 3;
    match tokio::time::timeout(
        budget,
        publish_error_signal_inner(
            session,
            error_key,
            clients_key,
            availability_timeout,
            heartbeat_interval,
            message,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "error-signal publish did not complete within {budget:?} (peer link likely dying)"
        )),
    }
}

async fn publish_error_signal_inner(
    session: &Arc<Session>,
    error_key: String,
    clients_key: String,
    availability_timeout: Duration,
    heartbeat_interval: Duration,
    message: &'static str,
) -> Result<()> {
    let key: KeyExpr<'static> = error_key
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("invalid error key '{error_key}': {e}"))?;
    // `sample_miss_detection` is not for miss detection here: it switches the
    // Advanced cache to sequence-number sequencing. The default (timestamp
    // sequencing) requires session-level timestamping, which the bridge does
    // not enable — the same combination every data-plane publisher uses.
    let publisher = session
        .declare_publisher(key)
        .cache(CacheConfig::default().max_samples(1))
        .sample_miss_detection(MissDetectionConfig::default().heartbeat(heartbeat_interval))
        .publisher_detection()
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare error publisher: {e}"))?;

    // The gate is always Stream-mode here: this is a control signal, not
    // loss-tolerant data, regardless of the data plane's reliability posture.
    await_matching_subscriber(
        &publisher,
        ReliabilityMode::Stream,
        availability_timeout,
        &error_key,
    )
    .await;

    publisher
        .put(message)
        .await
        .map_err(|e| anyhow::anyhow!("failed to publish error signal: {e}"))?;

    // Hold the publisher until the client token is gone (capped).
    let holder_session = session.clone();
    tokio::spawn(async move {
        let watch = async {
            let sub = match holder_session
                .liveliness()
                .declare_subscriber(&clients_key)
                .history(true)
                .await
            {
                Ok(sub) => {
                    debug!(key = %clients_key, "error-signal holder watching client token");
                    sub
                }
                // Cannot watch: fall through to a short grace instead.
                Err(e) => {
                    warn!(key = %clients_key, error = %e, "error-signal holder cannot watch; dropping early");
                    return;
                }
            };
            // Fast path: a token that is ALREADY gone will never produce a
            // Delete for the subscriber above, so the hold would ride out the
            // full cap — and this is the dominant case at normal teardown,
            // where the import undeclares its token back-to-back with the EOF
            // marker (once per REQUEST in route=request mode: a 60s publisher
            //+ task leak each). A completed liveliness query with no reply
            // says the token is gone; the subscriber (declared first) covers
            // the alive->Delete race from here on.
            let alive_probe = async {
                match holder_session.liveliness().get(&clients_key).await {
                    Ok(replies) => replies.recv_async().await.is_ok(),
                    Err(_) => false,
                }
            };
            // On Ok(true) — alive — or Err — the probe itself stalled
            // (congestion) — hold on and wait for the Delete as before:
            // releasing early on an unanswered probe would re-open the race
            // this publisher exists to close.
            if let Ok(false) = tokio::time::timeout(availability_timeout, alive_probe).await {
                debug!(key = %clients_key, "client token already gone; releasing early");
                return;
            }
            // Hold until a Delete is observed (history replays a live token as
            // a Put first). Deliberately no short aliveness heuristic beyond
            // the completed probe above: under load the history replay can
            // arrive late, and dropping the cache early re-opens the very race
            // this publisher exists to close.
            loop {
                match sub.recv_async().await {
                    Ok(s) if s.kind() == SampleKind::Delete => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        };
        let _ = tokio::time::timeout(ERROR_HOLD_CAP, watch).await;
        debug!("error-signal holder releasing the cached publisher");
        let _ = publisher.undeclare().await;
    });

    Ok(())
}
