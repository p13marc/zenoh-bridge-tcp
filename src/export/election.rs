//! Active/standby election between exporters of the same scope (A3).
//!
//! Two exporters announcing the same `{scope}` used to BOTH serve every
//! client: both dialed their backends (double execution of every request),
//! both published the same `rx/{client}` key (responses interleaved at sample
//! granularity — silent corruption), and whichever finished first half-closed
//! the client on the other. An "HA pair" was mutual sabotage.
//!
//! The election is deliberately minimal:
//!
//! - Each exporter declares a liveliness claim
//!   `{scope}/exporter-claim/{start_ts:020}-{zid}-{uuid}` and subscribes (with
//!   history) to `{scope}/exporter-claim/*`.
//! - The claimant with the **lexicographically smallest** segment is active.
//!   The zero-padded millisecond timestamp makes that "oldest wins" (sticky:
//!   a newcomer never preempts an established active), with the session id and
//!   a per-claim UUID as a total-order tiebreak — the UUID matters inside one
//!   process, where every claimant shares the session's zid.
//! - Everyone else stands by, ignoring client tokens. When the active's claim
//!   disappears (its process/session died), the next-smallest claimant takes
//!   over and replays the existing-clients query, so clients that were
//!   waiting get served.
//!
//! Known limits, accepted and documented: clocks are compared across machines,
//! so a large skew can elect a newcomer (deterministically — both sides agree
//! on the order, one drains); after a network partition heals, the younger of
//! two actives demotes and drains its connections.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use zenoh::Session;
use zenoh::sample::SampleKind;

/// Why a wait for the active role ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WaitOutcome {
    Active,
    Shutdown,
}

/// This exporter's claim plus the live view of every claim in the scope.
pub(super) struct Claim {
    /// Our claim token: dropping it (with the session) withdraws the claim.
    _token: zenoh::liveliness::LivelinessToken,
    subscriber:
        zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    own: String,
    claims: BTreeSet<String>,
}

impl Claim {
    /// Declare our claim and start observing the scope's claim set.
    ///
    /// The subscriber is declared BEFORE our own token so no concurrent
    /// claimant's Put can fall between the two; history replays claims that
    /// predate us.
    pub(super) async fn establish(session: &Arc<Session>, scope: &str) -> Result<Self> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        // A per-claim UUID after the zid: every backend in a process shares
        // one session (hence one zid), so two same-scope claims minted in the
        // same millisecond would otherwise be IDENTICAL — both inserted into
        // their own claim set, both electing themselves, reproducing exactly
        // the double-serving corruption this module exists to prevent.
        let own = format!(
            "{ts:020}-{}-{}",
            session.zid(),
            uuid::Uuid::new_v4().as_simple()
        );

        let subscriber = session
            .liveliness()
            .declare_subscriber(format!("{scope}/exporter-claim/*"))
            .history(true)
            .await
            .map_err(|e| anyhow::anyhow!("failed to subscribe to exporter claims: {e}"))?;
        let token = session
            .liveliness()
            .declare_token(format!("{scope}/exporter-claim/{own}"))
            .await
            .map_err(|e| anyhow::anyhow!("failed to declare exporter claim: {e}"))?;

        let mut claims = BTreeSet::new();
        claims.insert(own.clone());
        Ok(Self {
            _token: token,
            subscriber,
            own,
            claims,
        })
    }

    fn apply(&mut self, sample: zenoh::sample::Sample) {
        let Some(segment) = sample.key_expr().as_str().rsplit('/').next() else {
            return;
        };
        match sample.kind() {
            SampleKind::Put => {
                debug!(claim = %segment, "Exporter claim appeared");
                self.claims.insert(segment.to_string());
            }
            SampleKind::Delete => {
                debug!(claim = %segment, "Exporter claim withdrawn");
                self.claims.remove(segment);
            }
        }
    }

    /// Ingest any already-delivered claim events without blocking.
    fn drain_pending(&mut self) {
        // try_recv yields Ok(None) when the channel is momentarily empty.
        while let Ok(Some(sample)) = self.subscriber.try_recv() {
            self.apply(sample);
        }
    }

    /// Whether this exporter currently holds the active role.
    pub(super) fn is_active(&mut self) -> bool {
        self.drain_pending();
        elect(&self.claims, &self.own)
    }

    /// The currently-elected claim segment, for logs.
    pub(super) fn current_active(&mut self) -> Option<String> {
        self.drain_pending();
        self.claims.first().cloned()
    }

    /// Block until this exporter becomes active (or shutdown is requested).
    pub(super) async fn wait_until_active(
        &mut self,
        shutdown_token: &CancellationToken,
    ) -> WaitOutcome {
        loop {
            if self.is_active() {
                return WaitOutcome::Active;
            }
            tokio::select! {
                result = self.subscriber.recv_async() => match result {
                    Ok(sample) => self.apply(sample),
                    Err(e) => {
                        // The claim view is gone; serving is the conservative
                        // choice — it restores the pre-election behaviour
                        // rather than deadlocking a standby forever.
                        warn!(error = %e, "Exporter-claim subscriber died; assuming the active role");
                        return WaitOutcome::Active;
                    }
                },
                _ = shutdown_token.cancelled() => return WaitOutcome::Shutdown,
            }
        }
    }

    /// Await the next claim change (used as a select arm while serving).
    /// Returns whether this exporter is STILL active afterwards.
    pub(super) async fn changed_still_active(&mut self) -> bool {
        match self.subscriber.recv_async().await {
            Ok(sample) => self.apply(sample),
            // See wait_until_active: a dead claim view falls back to serving.
            Err(_) => return true,
        }
        self.is_active()
    }
}

/// The election rule, separated for direct testing: smallest claim wins.
fn elect(claims: &BTreeSet<String>, own: &str) -> bool {
    claims.first().map(String::as_str) == Some(own)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn oldest_claim_wins() {
        // Zero-padded millisecond timestamps order lexicographically == numerically.
        let older = "00000001754800000000-zidB";
        let newer = "00000001754800000500-zidA";
        let claims = set(&[older, newer]);
        assert!(elect(&claims, older), "the earlier claimant must be active");
        assert!(!elect(&claims, newer), "a newcomer must stand by");
    }

    #[test]
    fn zid_breaks_simultaneous_ties_consistently() {
        let a = "00000001754800000000-aaaa";
        let b = "00000001754800000000-bbbb";
        let claims = set(&[a, b]);
        assert!(elect(&claims, a));
        assert!(!elect(&claims, b));
    }

    #[test]
    fn same_session_same_millisecond_claims_elect_exactly_one() {
        // Two backends in one process share a session (one zid) and can start
        // in the same millisecond; the per-claim UUID must still produce two
        // distinct claims with exactly one winner.
        let ts = 1754800000000u128;
        let zid = "samezid";
        let a = format!("{ts:020}-{zid}-{}", uuid::Uuid::new_v4().as_simple());
        let b = format!("{ts:020}-{zid}-{}", uuid::Uuid::new_v4().as_simple());
        assert_ne!(a, b, "claims from one session must never collide");
        let claims: BTreeSet<String> = [a.clone(), b.clone()].into_iter().collect();
        let winners = [&a, &b].iter().filter(|c| elect(&claims, c)).count();
        assert_eq!(
            winners, 1,
            "exactly one same-ms same-zid claimant may serve"
        );
    }

    #[test]
    fn sole_claimant_is_active() {
        let only = "00000001754800000000-zid";
        assert!(elect(&set(&[only]), only));
    }

    #[test]
    fn survivor_takes_over_after_delete() {
        let first = "00000001754800000000-a";
        let second = "00000001754800000100-b";
        let mut claims = set(&[first, second]);
        assert!(!elect(&claims, second));
        claims.remove(first); // active died
        assert!(elect(&claims, second), "the survivor must take over");
    }

    #[test]
    fn padding_keeps_ordering_numeric() {
        // Without zero padding, "9" > "10" lexicographically; the format must
        // never regress to unpadded timestamps.
        let t1 = format!("{:020}-z", 9u128);
        let t2 = format!("{:020}-z", 10u128);
        assert!(t1 < t2, "zero-padded timestamps must order numerically");
    }
}
