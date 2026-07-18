use std::collections::BTreeMap;

use crate::domain::{
    Candidate, CandidateKind, ConfigDiff, ConfigDiffEntry, MetricPoint, MetricsSnapshot,
    ScalingSchedule, ScheduleWindow,
};
use crate::forecast::{ForecastParams, build_profile, weekly_window_cron};

const IDLE_CPU_THRESHOLD: f64 = 0.20;
const IDLE_MIN_HOURS: f64 = 4.0;
const OVERPROVISION_P95_THRESHOLD: f64 = 0.40;
const SCALE_LAG_MIN_SECONDS: f64 = 180.0;
const THRASHING_EVENTS_PER_HOUR: f64 = 10.0;
/// High utilization quantile used to size a floor cut, so a brief spike isn't sized to the mean.
const FLOOR_UTIL_QUANTILE: f64 = 0.95;
/// Safety multiplier on the headroom floor so we never size a floor to the bare p95 load.
const FLOOR_HEADROOM_BUFFER: f64 = 1.5;
/// Weeks ahead the forecast extrapolates the week-over-week trend when sizing schedule windows.
const TREND_LOOKAHEAD_WEEKS: f64 = 1.0;

type Detector = fn(&MetricsSnapshot) -> Option<Candidate>;

pub fn run_rules(snapshot: &MetricsSnapshot) -> Vec<Candidate> {
    let detectors: [Detector; 4] =
        [detect_idle_window, detect_overprovisioning, detect_scale_up_lag, detect_thrashing];
    detectors.into_iter().filter_map(|detect| detect(snapshot)).collect()
}

pub(crate) fn percentile(values: &[f64], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    // Clamp so an out-of-range quantile (e.g. a misconfigured --forecast-quantile > 1.0) can never
    // index past the end and panic.
    let pct = pct.clamp(0.0, 1.0);
    let mut ordered = values.to_vec();
    ordered.sort_by(|a, b| a.total_cmp(b));
    let rank = (ordered.len() as f64 - 1.0) * pct;
    let low = rank.floor() as usize;
    if low + 1 >= ordered.len() {
        return ordered[low];
    }
    ordered[low] + (ordered[low + 1] - ordered[low]) * (rank - low as f64)
}

fn median_replicas(points: &[&MetricPoint]) -> f64 {
    let mut replicas: Vec<i32> = points.iter().map(|p| p.replicas).collect();
    replicas.sort_unstable();
    let n = replicas.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        replicas[n / 2] as f64
    } else {
        f64::from(replicas[n / 2 - 1] + replicas[n / 2]) / 2.0
    }
}

fn hours_between(start: &MetricPoint, end: &MetricPoint) -> f64 {
    (end.timestamp - start.timestamp).num_seconds() as f64 / 3600.0
}

/// Replicas needed to serve the observed load at the CPU target, derived from utilization HEADROOM
/// rather than the observed replica count: `ceil(median_replicas * p95_util * buffer / target_frac)`,
/// floored at 1. Sizing from headroom is what lets a service pinned at an over-set `minReplicas` get
/// a real floor cut - the observed replica count equals the floor, so a median-replicas estimate
/// never drops below it.
fn headroom_replicas(points: &[&MetricPoint], target_cpu_pct: i32) -> Option<i32> {
    if points.is_empty() || target_cpu_pct <= 0 {
        return None;
    }
    let utils: Vec<f64> = points.iter().map(|point| point.cpu_util).collect();
    let util_high = percentile(&utils, FLOOR_UTIL_QUANTILE);
    let target_frac = f64::from(target_cpu_pct) / 100.0;
    let needed = (median_replicas(points) * util_high * FLOOR_HEADROOM_BUFFER / target_frac).ceil();
    Some((needed as i32).max(1))
}

/// A `min_replicas: from -> to` floor-cut candidate with the headroom evidence the LLM judges on.
fn floor_cut_candidate(
    kind: CandidateKind,
    description: String,
    from_min: i32,
    to_min: i32,
    util_p95: f64,
) -> Candidate {
    Candidate {
        kind,
        description,
        evidence: BTreeMap::from([
            ("util_p95".to_owned(), util_p95),
            ("needed_replicas".to_owned(), f64::from(to_min)),
            ("replicas_reduced".to_owned(), f64::from(from_min - to_min)),
        ]),
        proposed_diff: BTreeMap::from([(
            "min_replicas".to_owned(),
            ConfigDiffEntry::new(from_min, to_min),
        )]),
        schedule: None,
    }
}

fn longest_idle_run(points: &[MetricPoint]) -> Vec<&MetricPoint> {
    let mut best: Vec<&MetricPoint> = Vec::new();
    let mut current: Vec<&MetricPoint> = Vec::new();
    for point in points {
        if point.cpu_util < IDLE_CPU_THRESHOLD {
            current.push(point);
            if current.len() > best.len() {
                best = current.clone();
            }
        } else {
            current.clear();
        }
    }
    best
}

fn detect_idle_window(snapshot: &MetricsSnapshot) -> Option<Candidate> {
    let config = &snapshot.config;
    let run = longest_idle_run(&snapshot.points);
    if run.len() < 2 {
        return None;
    }
    let idle_hours = hours_between(run[0], run[run.len() - 1]);
    if idle_hours < IDLE_MIN_HOURS {
        return None;
    }
    let needed = headroom_replicas(&run, config.target_cpu_pct)?;
    if needed >= config.min_replicas {
        return None;
    }
    let util_p95 = percentile(
        &run.iter().map(|point| point.cpu_util).collect::<Vec<_>>(),
        FLOOR_UTIL_QUANTILE,
    );
    let mut candidate = floor_cut_candidate(
        CandidateKind::IdleWindow,
        format!(
            "Idle for {idle_hours:.1}h at p95 {:.0}% CPU; min replicas can drop from {} to {needed} (load headroom).",
            util_p95 * 100.0,
            config.min_replicas
        ),
        config.min_replicas,
        needed,
        util_p95,
    );
    candidate.evidence.insert("idle_hours".to_owned(), idle_hours);
    Some(candidate)
}

fn detect_overprovisioning(snapshot: &MetricsSnapshot) -> Option<Candidate> {
    let config = &snapshot.config;
    if snapshot.points.is_empty() {
        return None;
    }
    let cpus: Vec<f64> = snapshot.points.iter().map(|p| p.cpu_util).collect();
    let p95 = percentile(&cpus, 0.95);
    if p95 >= OVERPROVISION_P95_THRESHOLD {
        return None;
    }
    // Prefer the floor cut (real savings) when load headroom shows the floor is too high; only fall
    // back to a target bump when the floor is already as low as the load allows.
    let point_refs: Vec<&MetricPoint> = snapshot.points.iter().collect();
    if let Some(needed) = headroom_replicas(&point_refs, config.target_cpu_pct)
        && needed < config.min_replicas
    {
        return Some(floor_cut_candidate(
            CandidateKind::Overprovisioned,
            format!(
                "p95 CPU is {:.0}%; min replicas can drop from {} to {needed} (load headroom).",
                p95 * 100.0,
                config.min_replicas
            ),
            config.min_replicas,
            needed,
            p95,
        ));
    }
    let to = (config.target_cpu_pct + 15).min(90);
    Some(Candidate {
        kind: CandidateKind::Overprovisioned,
        description: format!("p95 CPU is {:.0}%; target can rise to {to}%.", p95 * 100.0),
        evidence: BTreeMap::from([("p95_util".to_owned(), p95)]),
        proposed_diff: BTreeMap::from([(
            "target_cpu_pct".to_owned(),
            ConfigDiffEntry::new(config.target_cpu_pct, to),
        )]),
        schedule: None,
    })
}

fn detect_scale_up_lag(snapshot: &MetricsSnapshot) -> Option<Candidate> {
    let config = &snapshot.config;
    let mut start: Option<&MetricPoint> = None;
    let mut prev: Option<&MetricPoint> = None;
    for point in &snapshot.points {
        let Some(queue) = point.queue_depth else {
            start = None;
            prev = None;
            continue;
        };
        let rising = prev.is_some_and(|p| {
            p.queue_depth.is_some_and(|pq| queue > pq) && point.replicas == p.replicas
        });
        if rising {
            let anchor = start.or(prev);
            start = anchor;
            if let Some(st) = anchor {
                let lag = (point.timestamp - st.timestamp).num_seconds() as f64;
                if lag >= SCALE_LAG_MIN_SECONDS {
                    let to = (config.target_cpu_pct - 15).max(20);
                    return Some(Candidate {
                        kind: CandidateKind::ScaleLag,
                        description: format!(
                            "Queue grew for {lag:.0}s without scaling; lower target to {to}%."
                        ),
                        evidence: BTreeMap::from([("lag_seconds".to_owned(), lag)]),
                        proposed_diff: BTreeMap::from([(
                            "target_cpu_pct".to_owned(),
                            ConfigDiffEntry::new(config.target_cpu_pct, to),
                        )]),
                        schedule: None,
                    });
                }
            }
        } else {
            start = None;
        }
        prev = Some(point);
    }
    None
}

fn detect_thrashing(snapshot: &MetricsSnapshot) -> Option<Candidate> {
    let config = &snapshot.config;
    let (Some(first), Some(last)) = (snapshot.points.first(), snapshot.points.last()) else {
        return None;
    };
    if snapshot.scaling_events.is_empty() {
        return None;
    }
    let mut window_hours = hours_between(first, last);
    if window_hours <= 0.0 {
        window_hours = 24.0;
    }
    let events_per_hour = snapshot.scaling_events.len() as f64 / window_hours;
    if events_per_hour <= THRASHING_EVENTS_PER_HOUR {
        return None;
    }
    let to = config.scale_down_cooldown_s * 2;
    Some(Candidate {
        kind: CandidateKind::Thrashing,
        description: format!(
            "{events_per_hour:.1} scaling events/hour; double scale-down cooldown to {to}s."
        ),
        evidence: BTreeMap::from([("events_per_hour".to_owned(), events_per_hour)]),
        proposed_diff: BTreeMap::from([(
            "scale_down_cooldown_s".to_owned(),
            ConfigDiffEntry::new(config.scale_down_cooldown_s, to),
        )]),
        schedule: None,
    })
}

/// Forecast-driven detector (run separately from `run_rules`, gated behind `--enable-forecasting`).
/// When the workload is periodic, propose lowering the static floor to the off-peak baseline and a
/// schedule that raises it during each recurring peak window. `None` when the periodicity gate in
/// `forecast::build_profile` rejects the workload.
pub fn detect_predictable_peak(
    snapshot: &MetricsSnapshot,
    params: &ForecastParams,
) -> Option<Candidate> {
    let config = &snapshot.config;
    let forecast = build_profile(&snapshot.points, config, params)?;
    let bins_per_day = forecast.bins.len() / 7;

    let nonzero: Vec<f64> = forecast.bins.iter().copied().filter(|value| *value > 0.0).collect();
    if nonzero.is_empty() {
        return None;
    }
    // Off-peak floor, never raised above the current static min (forecasting only lowers the floor;
    // HPA still handles reactive scale-up). Always carried as the diff so the scheduler can restore it.
    let baseline = (percentile(&nonzero, 0.2).ceil() as i32).max(1).min(config.min_replicas);

    // Extrapolate week-over-week growth one week forward (growth only, never shrink the forecast) so
    // a rising workload's windows track the trend; empty bins stay empty.
    let growth = forecast.trend_per_week.max(0.0) * TREND_LOOKAHEAD_WEEKS;
    let projected: Vec<f64> =
        forecast.bins.iter().map(|&bin| if bin > 0.0 { bin + growth } else { 0.0 }).collect();

    let schedule = build_schedule(
        &projected,
        bins_per_day,
        forecast.bin_minutes,
        baseline,
        config.max_replicas,
        params.prescale_lead_minutes,
    );
    if schedule.is_empty() {
        return None;
    }
    debug_assert!(
        schedule.iter().all(|w| w.min_replicas <= config.max_replicas),
        "schedule floor must never exceed max_replicas"
    );

    let proposed_diff = ConfigDiff::from([(
        "min_replicas".to_owned(),
        ConfigDiffEntry::new(config.min_replicas, baseline),
    )]);
    let peak = schedule.iter().map(|window| window.min_replicas).max().unwrap_or(baseline);
    Some(Candidate {
        kind: CandidateKind::PredictablePeak,
        description: format!(
            "Recurring peak forecast (confidence {:.0}%): hold baseline {baseline}, pre-scale to {peak} across {} window(s).",
            forecast.confidence * 100.0,
            schedule.len()
        ),
        evidence: BTreeMap::from([
            ("confidence".to_owned(), forecast.confidence),
            ("windows".to_owned(), schedule.len() as f64),
            ("baseline".to_owned(), f64::from(baseline)),
        ]),
        proposed_diff,
        schedule: Some(schedule),
    })
}

/// Coalesce bins whose forecast exceeds the baseline into windows, starting `lead_minutes` early,
/// merging identical windows across days into one cron entry. The window floor is clamped to
/// `max_replicas` (a low target can push the raw forecast above the ceiling), and the lead is
/// clamped to the start of the day, so a peak that begins at midnight (bin 0) gets no head start.
fn build_schedule(
    bins: &[f64],
    bins_per_day: usize,
    bin_minutes: u32,
    baseline: i32,
    max_replicas: i32,
    lead_minutes: i64,
) -> ScalingSchedule {
    let lead = i32::try_from(lead_minutes.max(0)).unwrap_or(i32::MAX);
    let mut groups: BTreeMap<(u32, i32, i32), Vec<u32>> = BTreeMap::new();
    for day in 0..7u32 {
        let mut bin = 0;
        while bin < bins_per_day {
            let needed = bins[day as usize * bins_per_day + bin].ceil() as i32;
            if needed <= baseline {
                bin += 1;
                continue;
            }
            let mut end = bin;
            let mut peak = needed;
            while end < bins_per_day
                && bins[day as usize * bins_per_day + end].ceil() as i32 > baseline
            {
                peak = peak.max(bins[day as usize * bins_per_day + end].ceil() as i32);
                end += 1;
            }
            let raw_start = bin as i32 * bin_minutes as i32;
            let start_minute = (raw_start - lead).max(0);
            let duration = (end - bin) as i32 * bin_minutes as i32 + (raw_start - start_minute);
            groups
                .entry((start_minute as u32, duration, peak.min(max_replicas)))
                .or_default()
                .push(day);
            bin = end;
        }
    }
    groups
        .into_iter()
        .map(|((start_minute, duration_minutes, min_replicas), days)| ScheduleWindow {
            start_cron: weekly_window_cron(&days, start_minute),
            duration_minutes,
            min_replicas,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Timelike, Utc};

    use super::*;
    use crate::domain::{ScalingEvent, WorkloadConfig};

    fn config() -> WorkloadConfig {
        WorkloadConfig {
            min_replicas: 10,
            max_replicas: 40,
            target_cpu_pct: 70,
            scale_down_cooldown_s: 300,
        }
    }

    fn point(hour: i64, cpu_util: f64, replicas: i32, queue_depth: Option<f64>) -> MetricPoint {
        MetricPoint {
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::hours(hour),
            cpu_util,
            replicas,
            queue_depth,
        }
    }

    #[test]
    fn idle_and_overprovisioned_detected() {
        // 8h idle at 8% CPU with 3 replicas, then steady 30% - p95 < 40%, idle run >= 4h.
        let mut points: Vec<MetricPoint> = (0..8).map(|h| point(h, 0.08, 3, None)).collect();
        points.extend((8..24).map(|h| point(h, 0.30, 12, None)));
        let snapshot = MetricsSnapshot { config: config(), points, scaling_events: vec![] };

        let kinds: Vec<CandidateKind> = run_rules(&snapshot).iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&CandidateKind::IdleWindow), "kinds: {kinds:?}");
        assert!(kinds.contains(&CandidateKind::Overprovisioned), "kinds: {kinds:?}");
    }

    #[test]
    fn idle_window_drops_min_replicas() {
        // 6h idle at 5% CPU running 2 replicas, floor pinned at 10. The headroom floor is
        // ceil(2 * 0.05 * 1.5 / 0.70) = 1, so min replicas drops 10 -> 1 (not the observed-replica
        // median of 2 the old logic produced).
        let points: Vec<MetricPoint> = (0..6).map(|h| point(h, 0.05, 2, None)).collect();
        let snapshot = MetricsSnapshot { config: config(), points, scaling_events: vec![] };

        let candidates = run_rules(&snapshot);
        let idle = candidates.iter().find(|c| c.kind == CandidateKind::IdleWindow).unwrap();
        let diff = idle.proposed_diff.get("min_replicas").unwrap();
        assert_eq!(*diff, ConfigDiffEntry::new(10, 1));
        assert_eq!(idle.evidence.get("replicas_reduced"), Some(&9.0));
    }

    #[test]
    fn overprovisioned_floor_cut_from_headroom() {
        // A service pinned at min=4 but idle (~5% CPU) over a short (<4h) window. The idle_window
        // detector is 4h-gated, so the headline floor cut must come from detect_overprovisioning:
        // the old code emitted only a $0 target bump; headroom logic emits a real min_replicas cut.
        let cfg = WorkloadConfig {
            min_replicas: 4,
            max_replicas: 20,
            target_cpu_pct: 70,
            scale_down_cooldown_s: 300,
        };
        let points: Vec<MetricPoint> = (0..3).map(|h| point(h, 0.05, 4, None)).collect();
        let snapshot = MetricsSnapshot { config: cfg, points, scaling_events: vec![] };

        let candidates = run_rules(&snapshot);
        let over = candidates
            .iter()
            .find(|c| c.kind == CandidateKind::Overprovisioned)
            .expect("candidates");
        assert_eq!(*over.proposed_diff.get("min_replicas").unwrap(), ConfigDiffEntry::new(4, 1));
        assert!(!over.proposed_diff.contains_key("target_cpu_pct"));
        assert_eq!(over.evidence.get("replicas_reduced"), Some(&3.0));
        assert!(
            !candidates.iter().any(|c| c.kind == CandidateKind::IdleWindow),
            "idle_window is 4h-gated and must not fire on a 2h window: {candidates:?}"
        );
    }

    #[test]
    fn thrashing_detected_from_event_rate() {
        // 1h window, 20 scaling events -> 20/hour > 10.
        let points = vec![point(0, 0.6, 10, None), point(1, 0.6, 10, None)];
        let events: Vec<ScalingEvent> = (0..20)
            .map(|_| ScalingEvent {
                timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                from_replicas: 10,
                to_replicas: 11,
            })
            .collect();
        let snapshot = MetricsSnapshot { config: config(), points, scaling_events: events };

        let candidates = run_rules(&snapshot);
        assert_eq!(candidates.iter().filter(|c| c.kind == CandidateKind::Thrashing).count(), 1);
    }

    #[test]
    fn optimal_workload_has_no_candidates() {
        let points: Vec<MetricPoint> = (0..24).map(|h| point(h, 0.62, 12, None)).collect();
        let snapshot = MetricsSnapshot { config: config(), points, scaling_events: vec![] };
        assert_eq!(run_rules(&snapshot).len(), 0);
    }

    #[test]
    fn scale_lag_detected_from_rising_queue() {
        // queue rising for 4 minutes (>180s) while replicas flat.
        let points = vec![
            MetricPoint {
                timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                cpu_util: 0.6,
                replicas: 10,
                queue_depth: Some(1.0),
            },
            MetricPoint {
                timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 2, 0).unwrap(),
                cpu_util: 0.6,
                replicas: 10,
                queue_depth: Some(5.0),
            },
            MetricPoint {
                timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 4, 0).unwrap(),
                cpu_util: 0.6,
                replicas: 10,
                queue_depth: Some(9.0),
            },
        ];
        let snapshot = MetricsSnapshot { config: config(), points, scaling_events: vec![] };

        let candidates = run_rules(&snapshot);
        let lag = candidates.iter().find(|c| c.kind == CandidateKind::ScaleLag).unwrap();
        assert_eq!(*lag.proposed_diff.get("target_cpu_pct").unwrap(), ConfigDiffEntry::new(70, 55));
    }

    fn daily_points(busy: impl Fn(u32) -> f64) -> Vec<MetricPoint> {
        let base = Utc.with_ymd_and_hms(2026, 1, 4, 0, 0, 0).unwrap();
        (0..21 * 24)
            .map(|h| {
                let timestamp = base + Duration::hours(h);
                MetricPoint {
                    timestamp,
                    cpu_util: busy(timestamp.hour()),
                    replicas: 5,
                    queue_depth: None,
                }
            })
            .collect()
    }

    #[test]
    fn predictable_peak_emits_schedule_and_lowers_baseline() {
        let points = daily_points(|hour| if (9..17).contains(&hour) { 0.85 } else { 0.15 });
        let snapshot = MetricsSnapshot { config: config(), points, scaling_events: vec![] };

        let candidate =
            detect_predictable_peak(&snapshot, &ForecastParams::default()).expect("forecast");
        assert_eq!(candidate.kind, CandidateKind::PredictablePeak);
        let schedule = candidate.schedule.expect("schedule");
        assert!(!schedule.is_empty(), "{schedule:?}");
        assert!(schedule.iter().all(|window| window.min_replicas > 2), "{schedule:?}");
        // Off-peak baseline recommended below the static floor of 10.
        let baseline = candidate.proposed_diff.get("min_replicas").expect("baseline lowering");
        assert_eq!(baseline.from, 10);
        assert!(baseline.to < 10, "{baseline:?}");
    }

    #[test]
    fn flat_workload_emits_no_forecast() {
        let points = daily_points(|_| 0.5);
        let snapshot = MetricsSnapshot { config: config(), points, scaling_events: vec![] };
        assert!(detect_predictable_peak(&snapshot, &ForecastParams::default()).is_none());
    }

    #[test]
    fn out_of_range_quantile_does_not_panic() {
        // a quantile > 1.0 must be clamped, not index past the end of percentile().
        let points = daily_points(|hour| if (9..17).contains(&hour) { 0.85 } else { 0.15 });
        let snapshot = MetricsSnapshot { config: config(), points, scaling_events: vec![] };
        let params = ForecastParams { quantile: 1.5, ..ForecastParams::default() };
        assert!(detect_predictable_peak(&snapshot, &params).is_some());
    }

    #[test]
    fn window_floor_never_exceeds_max_replicas() {
        // a low target inflates needed_replicas above max_replicas; the window must be clamped.
        let config = WorkloadConfig {
            min_replicas: 10,
            max_replicas: 40,
            target_cpu_pct: 10,
            scale_down_cooldown_s: 300,
        };
        let points = daily_points(|hour| if (9..17).contains(&hour) { 0.85 } else { 0.05 });
        let snapshot = MetricsSnapshot { config, points, scaling_events: vec![] };
        let schedule = detect_predictable_peak(&snapshot, &ForecastParams::default())
            .unwrap()
            .schedule
            .unwrap();
        assert!(schedule.iter().all(|window| window.min_replicas <= 40), "{schedule:?}");
        assert!(schedule.iter().any(|window| window.min_replicas == 40), "expected a clamped peak");
    }
}
