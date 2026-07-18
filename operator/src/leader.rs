use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::api::PostParams;
use kube::{Api, Client};
use tracing::info;

/// Single-leader election over a `coordination.k8s.io/v1` Lease so `replicas: 2+` can run safely -
/// only the holder performs mutating passes. Renewed once per reconcile tick; a stale `renewTime`
/// (older than `lease_duration`) lets another replica take over.
pub struct LeaderElector {
    leases: Api<Lease>,
    name: String,
    identity: String,
    lease_duration: Duration,
    /// Extra time a peer must observe the lease expired before taking over, to absorb clock skew
    /// between pods and Lease GET/PUT latency (`renewTime` is wall-clock from the holder, compared
    /// on the peer's clock).
    acquire_skew: Duration,
}

/// Clock-skew / API-latency margin added on top of `lease_duration` before a peer takes over.
const ACQUIRE_SKEW_SECONDS: i64 = 5;

impl LeaderElector {
    pub fn new(
        client: Client,
        namespace: &str,
        name: String,
        identity: String,
        lease_duration: Duration,
    ) -> Self {
        Self {
            leases: Api::namespaced(client, namespace),
            name,
            identity,
            lease_duration,
            acquire_skew: Duration::seconds(ACQUIRE_SKEW_SECONDS),
        }
    }

    /// Acquire or renew leadership, returning `true` if this replica holds the lease afterward.
    /// Renew/takeover replaces the lease guarded by the observed `resourceVersion`, so when two
    /// replicas race for the same stale lease exactly one wins and the loser gets a 409 (no
    /// split-brain). `create` is likewise safe: a concurrent create returns 409.
    pub async fn try_acquire(&self, now: DateTime<Utc>) -> Result<bool> {
        match self.leases.get_opt(&self.name).await? {
            None => {
                self.leases.create(&PostParams::default(), &self.lease(now, true, None)).await?;
                info!(lease = %self.name, identity = %self.identity, "acquired leader lease");
                Ok(true)
            }
            Some(lease)
                if lease_is_acquirable(
                    &lease,
                    &self.identity,
                    now,
                    self.lease_duration,
                    self.acquire_skew,
                ) =>
            {
                let held_by_me = lease.spec.as_ref().and_then(|s| s.holder_identity.as_deref())
                    == Some(self.identity.as_str());
                let next = self.lease(now, !held_by_me, lease.metadata.resource_version);
                self.leases.replace(&self.name, &PostParams::default(), &next).await?;
                Ok(true)
            }
            Some(_) => Ok(false),
        }
    }

    fn lease(
        &self,
        now: DateTime<Utc>,
        acquiring: bool,
        resource_version: Option<String>,
    ) -> Lease {
        Lease {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                resource_version,
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(self.identity.clone()),
                lease_duration_seconds: Some(self.lease_duration.num_seconds() as i32),
                renew_time: Some(MicroTime(now)),
                acquire_time: acquiring.then_some(MicroTime(now)),
                ..Default::default()
            }),
        }
    }
}

/// Lease duration for a given reconcile interval. The leader renews once per tick, so the lease
/// spans two ticks (tolerating one missed renewal); the clock-skew/latency margin is applied
/// separately at takeover rather than by inflating the lease, which would only slow failover. Never
/// drops below one minute so a tiny interval can't make failover hair-trigger.
pub fn lease_duration_for_interval(interval_seconds: u64) -> Duration {
    Duration::seconds((interval_seconds * 2).max(60) as i64)
}

/// Pure leadership decision: the lease is free to take if it has no holder, we already hold it, or
/// the current holder's `renewTime` is stale by more than `lease_duration + skew` (or absent). The
/// skew term keeps a peer from grabbing a lease that is only marginally past expiry on a skewed clock.
fn lease_is_acquirable(
    lease: &Lease,
    me: &str,
    now: DateTime<Utc>,
    lease_duration: Duration,
    skew: Duration,
) -> bool {
    let Some(spec) = lease.spec.as_ref() else {
        return true;
    };
    match spec.holder_identity.as_deref() {
        None => true,
        Some(holder) if holder == me => true,
        Some(_) => {
            spec.renew_time.as_ref().is_none_or(|renew| now - renew.0 > lease_duration + skew)
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn lease_held_by(holder: Option<&str>, renew: Option<DateTime<Utc>>) -> Lease {
        Lease {
            metadata: ObjectMeta::default(),
            spec: Some(LeaseSpec {
                holder_identity: holder.map(str::to_owned),
                renew_time: renew.map(MicroTime),
                ..Default::default()
            }),
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    }

    fn skew() -> Duration {
        Duration::seconds(ACQUIRE_SKEW_SECONDS)
    }

    #[test]
    fn empty_lease_is_acquirable() {
        let lease = Lease { metadata: ObjectMeta::default(), spec: None };
        assert!(lease_is_acquirable(&lease, "me", now(), Duration::seconds(30), skew()));
    }

    #[test]
    fn unheld_lease_is_acquirable() {
        let lease = lease_held_by(None, None);
        assert!(lease_is_acquirable(&lease, "me", now(), Duration::seconds(30), skew()));
    }

    #[test]
    fn self_held_lease_is_acquirable() {
        let lease = lease_held_by(Some("me"), Some(now()));
        assert!(lease_is_acquirable(&lease, "me", now(), Duration::seconds(30), skew()));
    }

    #[test]
    fn fresh_lease_held_by_other_is_not_acquirable() {
        let lease = lease_held_by(Some("peer"), Some(now() - Duration::seconds(10)));
        assert!(!lease_is_acquirable(&lease, "me", now(), Duration::seconds(30), skew()));
    }

    #[test]
    fn stale_lease_held_by_other_is_acquirable() {
        let lease = lease_held_by(Some("peer"), Some(now() - Duration::seconds(120)));
        assert!(lease_is_acquirable(&lease, "me", now(), Duration::seconds(30), skew()));
    }

    #[test]
    fn lease_held_by_other_without_renew_time_is_acquirable() {
        let lease = lease_held_by(Some("peer"), None);
        assert!(lease_is_acquirable(&lease, "me", now(), Duration::seconds(30), skew()));
    }

    #[test]
    fn marginally_expired_lease_is_not_yet_acquirable() {
        // Expired by 32s with a 30s lease: within the 5s skew margin, so a peer must NOT take over
        // yet (a clock-skewed peer could otherwise steal a lease that is barely past expiry).
        let lease = lease_held_by(Some("peer"), Some(now() - Duration::seconds(32)));
        assert!(!lease_is_acquirable(&lease, "me", now(), Duration::seconds(30), skew()));
        // Well past expiry (beyond lease + skew) -> acquirable.
        let stale = lease_held_by(Some("peer"), Some(now() - Duration::seconds(40)));
        assert!(lease_is_acquirable(&stale, "me", now(), Duration::seconds(30), skew()));
    }

    #[test]
    fn lease_duration_spans_two_renew_intervals_not_more() {
        // Renew runs once per interval; the lease must tolerate one missed renewal (2x), and the
        // round-1 widening to 3x (slower failover) is reverted in favour of the skew margin.
        assert_eq!(lease_duration_for_interval(300), Duration::seconds(600));
        assert_eq!(lease_duration_for_interval(1), Duration::seconds(60));
    }
}
