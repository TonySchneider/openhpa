use openhpa_core::{MetricPoint, WorkloadConfig};

/// Minimum post-apply samples before a verdict can be `Degraded` - never roll back on thin data.
const MIN_AFTER_POINTS: usize = 3;
/// Fraction of the post-apply window pinned at `max_replicas` that counts as saturation.
const MAX_PINNED_FRACTION: f64 = 0.8;
/// Increase in scaling oscillation (replica changes per interval) that counts as new thrashing.
const THRASHING_DELTA: f64 = 0.2;
/// Tail-vs-head queue-depth ratio over the post-apply window that counts as a growing backlog.
const QUEUE_GROWTH_RATIO: f64 = 1.5;

#[derive(Debug, PartialEq, Eq)]
pub enum HealthVerdict {
    Healthy,
    Degraded(String),
    /// Not enough post-apply data to judge yet - caller should keep the change on probation rather
    /// than mark it verified (so a degraded-monitoring gap can't silently rubber-stamp a change).
    Inconclusive,
}

/// Deterministic post-apply health check: compare the `after` window (and, for thrashing, the
/// pre-apply `before` window) against the now-live `config`. Returns `Inconclusive` on insufficient
/// after-data (never `Degraded`, so the safety net never reverts a change it cannot yet judge).
pub fn evaluate_health(
    before: &[MetricPoint],
    after: &[MetricPoint],
    config: &WorkloadConfig,
    cpu_margin: f64,
) -> HealthVerdict {
    if after.len() < MIN_AFTER_POINTS {
        return HealthVerdict::Inconclusive;
    }
    let cpu = mean(after.iter().map(|point| point.cpu_util));
    let ceiling = f64::from(config.target_cpu_pct) / 100.0 + cpu_margin;
    if cpu > ceiling {
        return HealthVerdict::Degraded(format!(
            "cpu {:.0}% sustained above target+margin {:.0}%",
            cpu * 100.0,
            ceiling * 100.0
        ));
    }
    let pinned = after.iter().filter(|point| point.replicas >= config.max_replicas).count() as f64
        / after.len() as f64;
    if pinned > MAX_PINNED_FRACTION {
        return HealthVerdict::Degraded(format!(
            "pinned at max_replicas {} for {:.0}% of the window",
            config.max_replicas,
            pinned * 100.0
        ));
    }
    if queue_growing(after) {
        return HealthVerdict::Degraded("queue depth trending up after apply".to_owned());
    }
    // Only judge thrashing against a real pre-apply baseline; an empty/short `before` (HPA-status
    // mode, or history lost on restart) would make any post-apply activity look like new oscillation.
    if before.len() >= MIN_AFTER_POINTS
        && oscillation_rate(after) > oscillation_rate(before) + THRASHING_DELTA
    {
        return HealthVerdict::Degraded("scaling oscillation increased after apply".to_owned());
    }
    HealthVerdict::Healthy
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, count) = values.fold((0.0, 0usize), |(sum, count), value| (sum + value, count + 1));
    if count == 0 { 0.0 } else { sum / count as f64 }
}

/// Replica changes per interval (0 when fewer than two points).
fn oscillation_rate(points: &[MetricPoint]) -> f64 {
    if points.len() < 2 {
        return 0.0;
    }
    let changes = points.windows(2).filter(|pair| pair[0].replicas != pair[1].replicas).count();
    changes as f64 / (points.len() - 1) as f64
}

/// True when the workload reports a queue and its tail third is materially larger than its head.
fn queue_growing(points: &[MetricPoint]) -> bool {
    let queue: Vec<f64> = points.iter().filter_map(|point| point.queue_depth).collect();
    if queue.len() < MIN_AFTER_POINTS {
        return false;
    }
    let third = (queue.len() / 3).max(1);
    let head = mean(queue[..third].iter().copied());
    let tail = mean(queue[queue.len() - third..].iter().copied());
    head > 0.0 && tail > head * QUEUE_GROWTH_RATIO
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::*;

    fn config() -> WorkloadConfig {
        WorkloadConfig {
            min_replicas: 2,
            max_replicas: 10,
            target_cpu_pct: 70,
            scale_down_cooldown_s: 300,
        }
    }

    fn point(minute: i64, cpu_util: f64, replicas: i32, queue_depth: Option<f64>) -> MetricPoint {
        MetricPoint {
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                + Duration::minutes(minute),
            cpu_util,
            replicas,
            queue_depth,
        }
    }

    fn window(cpu_util: f64, replicas: i32) -> Vec<MetricPoint> {
        (0..6).map(|m| point(m, cpu_util, replicas, None)).collect()
    }

    #[test]
    fn healthy_when_cpu_and_replicas_settle() {
        let after = window(0.55, 4);
        assert_eq!(evaluate_health(&[], &after, &config(), 0.15), HealthVerdict::Healthy);
    }

    #[test]
    fn insufficient_data_is_inconclusive() {
        let after = vec![point(0, 0.99, 10, None), point(1, 0.99, 10, None)];
        assert_eq!(evaluate_health(&[], &after, &config(), 0.15), HealthVerdict::Inconclusive);
    }

    #[test]
    fn oscillation_against_short_before_is_not_degraded() {
        // `after` thrashes, but `before` is too short to be a real baseline - must not revert.
        let after: Vec<MetricPoint> =
            (0..6).map(|m| point(m, 0.5, 4 + (m as i32 % 2), None)).collect();
        let before = vec![point(-1, 0.5, 4, None)];
        assert_eq!(evaluate_health(&before, &after, &config(), 0.15), HealthVerdict::Healthy);
    }

    #[test]
    fn cpu_saturation_is_degraded() {
        let after = window(0.90, 4);
        let HealthVerdict::Degraded(reason) = evaluate_health(&[], &after, &config(), 0.15) else {
            panic!("expected degraded");
        };
        assert!(reason.contains("cpu"), "{reason}");
    }

    #[test]
    fn pinned_at_max_replicas_is_degraded() {
        let after = window(0.50, 10);
        let HealthVerdict::Degraded(reason) = evaluate_health(&[], &after, &config(), 0.15) else {
            panic!("expected degraded");
        };
        assert!(reason.contains("max_replicas"), "{reason}");
    }

    #[test]
    fn growing_queue_is_degraded() {
        let after: Vec<MetricPoint> =
            (0..6).map(|m| point(m, 0.5, 4, Some(f64::from(m as i32) * 10.0 + 1.0))).collect();
        let HealthVerdict::Degraded(reason) = evaluate_health(&[], &after, &config(), 0.15) else {
            panic!("expected degraded");
        };
        assert!(reason.contains("queue"), "{reason}");
    }

    #[test]
    fn increased_oscillation_is_degraded() {
        let before = window(0.5, 4);
        let after: Vec<MetricPoint> =
            (0..6).map(|m| point(m, 0.5, 4 + (m as i32 % 2), None)).collect();
        let HealthVerdict::Degraded(reason) = evaluate_health(&before, &after, &config(), 0.15)
        else {
            panic!("expected degraded");
        };
        assert!(reason.contains("oscillation"), "{reason}");
    }
}
