use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};

use crate::domain::{MetricPoint, WorkloadConfig};
use crate::rules::percentile;

const MINUTES_PER_DAY: u32 = 1440;
const DAYS_PER_WEEK: usize = 7;
/// Variance below which a (detrended) series is treated as having no signal - guards the periodicity
/// gate against floating-point roundoff so a pure trend or flat line never reads as a cycle.
const VARIANCE_EPSILON: f64 = 1e-9;

/// Tuning for the seasonal forecaster (built by the operator from its config flags).
#[derive(Debug, Clone)]
pub struct ForecastParams {
    pub min_history_days: i64,
    pub bin_minutes: u32,
    pub quantile: f64,
    pub periodicity_threshold: f64,
    pub prescale_lead_minutes: i64,
}

impl Default for ForecastParams {
    fn default() -> Self {
        Self {
            min_history_days: 14,
            bin_minutes: 60,
            quantile: 0.95,
            periodicity_threshold: 0.3,
            prescale_lead_minutes: 10,
        }
    }
}

/// A deterministic seasonal profile: per weekly (day-of-week, time-of-day) bin, the quantile replica
/// count needed to hold target, plus a week-over-week trend and the periodicity confidence.
#[derive(Debug, Clone)]
pub struct SeasonalForecast {
    pub bins: Vec<f64>,
    pub bin_minutes: u32,
    /// Week-over-week change in demand; the schedule builder extrapolates growth forward.
    pub trend_per_week: f64,
    pub confidence: f64,
}

/// Build a seasonal profile from history, or `None` when the workload isn't periodic enough to
/// forecast (the periodicity gate) or there isn't enough history. Trend is detrended out before the
/// gate so steady growth alone never reads as a cycle, and the autocorrelation must hold at two
/// consecutive multiples of the period so a one-off level shift (high at one lag, decayed at the
/// next) is rejected too.
pub fn build_profile(
    points: &[MetricPoint],
    cfg: &WorkloadConfig,
    params: &ForecastParams,
) -> Option<SeasonalForecast> {
    let (first, last) = (points.first()?, points.last()?);
    let span_minutes = (last.timestamp - first.timestamp).num_minutes();
    if params.bin_minutes == 0
        || span_minutes < params.min_history_days * i64::from(MINUTES_PER_DAY)
    {
        return None;
    }
    // Ceiling division so the grid covers the full 1440 minutes even when bin_minutes doesn't divide
    // it evenly - the trailing partial bin gets its own index instead of aliasing into its
    // neighbor.
    let bins_per_day = MINUTES_PER_DAY.div_ceil(params.bin_minutes).max(1) as usize;
    let bins_per_week = bins_per_day * DAYS_PER_WEEK;

    let detrended = detrend(&regular_series(points, first.timestamp, params.bin_minutes));
    let threshold = params.periodicity_threshold;
    let daily = autocorrelation(&detrended, bins_per_day);
    let weekly = autocorrelation(&detrended, bins_per_week);
    let recurs_daily =
        daily >= threshold && autocorrelation(&detrended, 2 * bins_per_day) >= threshold;
    let recurs_weekly =
        weekly >= threshold && autocorrelation(&detrended, 2 * bins_per_week) >= threshold;
    if !recurs_daily && !recurs_weekly {
        return None;
    }

    let mut buckets: Vec<Vec<f64>> = vec![Vec::new(); bins_per_week];
    for point in points {
        buckets[weekly_bin_index(point.timestamp, params.bin_minutes, bins_per_day)]
            .push(needed_replicas(point, cfg));
    }
    let bins = buckets
        .iter()
        .map(|values| if values.is_empty() { 0.0 } else { percentile(values, params.quantile) })
        .collect();

    Some(SeasonalForecast {
        bins,
        bin_minutes: params.bin_minutes,
        trend_per_week: week_over_week_trend(points, cfg, first.timestamp),
        confidence: daily.max(weekly),
    })
}

/// Replicas needed to hold target at a point: `ceil(replicas * cpu / target)`, never reduced below
/// the current replicas when a queue backlog is present.
pub(crate) fn needed_replicas(point: &MetricPoint, cfg: &WorkloadConfig) -> f64 {
    let target = if cfg.target_cpu_pct > 0 { f64::from(cfg.target_cpu_pct) / 100.0 } else { 1.0 };
    let cpu_based = (f64::from(point.replicas) * point.cpu_util / target).ceil();
    let queue_floor =
        point.queue_depth.filter(|q| *q > 0.0).map_or(0.0, |_| f64::from(point.replicas));
    cpu_based.max(queue_floor).max(0.0)
}

/// Weekly grid index for an instant: `day_of_week * bins_per_day + bin_of_day` (Sunday = 0).
pub(crate) fn weekly_bin_index(ts: DateTime<Utc>, bin_minutes: u32, bins_per_day: usize) -> usize {
    let dow = ts.weekday().num_days_from_sunday() as usize;
    let minute_of_day = ts.hour() * 60 + ts.minute();
    let bin_of_day = (minute_of_day / bin_minutes).min(bins_per_day as u32 - 1) as usize;
    dow * bins_per_day + bin_of_day
}

/// Resample the cpu-utilization load into a regular series at `bin_minutes` resolution (empty
/// buckets filled with the overall mean) - the input to the periodicity gate. Using the raw load,
/// not the quantized needed-replicas, keeps a slow ramp from looking cyclic.
fn regular_series(points: &[MetricPoint], t0: DateTime<Utc>, bin_minutes: u32) -> Vec<f64> {
    let mut buckets: BTreeMap<i64, (f64, usize)> = BTreeMap::new();
    let mut total = 0.0;
    for point in points {
        let bucket = (point.timestamp - t0).num_minutes() / i64::from(bin_minutes);
        let entry = buckets.entry(bucket).or_insert((0.0, 0));
        entry.0 += point.cpu_util;
        entry.1 += 1;
        total += point.cpu_util;
    }
    let overall_mean = if points.is_empty() { 0.0 } else { total / points.len() as f64 };
    let max_bucket = buckets.keys().last().copied().unwrap_or(0);
    (0..=max_bucket)
        .map(|bucket| buckets.get(&bucket).map_or(overall_mean, |(sum, n)| sum / *n as f64))
        .collect()
}

/// Subtract the least-squares line so the periodicity gate sees cycles, not steady growth.
fn detrend(series: &[f64]) -> Vec<f64> {
    let (intercept, slope) = least_squares(series);
    series.iter().enumerate().map(|(i, value)| value - (intercept + slope * i as f64)).collect()
}

fn autocorrelation(series: &[f64], lag: usize) -> f64 {
    if lag == 0 || series.len() <= lag + 1 {
        return 0.0;
    }
    pearson(&series[..series.len() - lag], &series[lag..])
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let mean_a = a.iter().sum::<f64>() / n;
    let mean_b = b.iter().sum::<f64>() / n;
    let (mut cov, mut var_a, mut var_b) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        let (dx, dy) = (x - mean_a, y - mean_b);
        cov += dx * dy;
        var_a += dx * dx;
        var_b += dy * dy;
    }
    if var_a < VARIANCE_EPSILON || var_b < VARIANCE_EPSILON {
        return 0.0;
    }
    (cov / (var_a.sqrt() * var_b.sqrt())).clamp(-1.0, 1.0)
}

fn week_over_week_trend(points: &[MetricPoint], cfg: &WorkloadConfig, t0: DateTime<Utc>) -> f64 {
    let mut weeks: BTreeMap<i64, (f64, usize)> = BTreeMap::new();
    for point in points {
        let week = (point.timestamp - t0).num_days() / 7;
        let entry = weeks.entry(week).or_insert((0.0, 0));
        entry.0 += needed_replicas(point, cfg);
        entry.1 += 1;
    }
    if weeks.len() < 2 {
        return 0.0;
    }
    let series: Vec<f64> = weeks.values().map(|(sum, n)| sum / *n as f64).collect();
    least_squares(&series).1
}

/// Least-squares fit over `0..len`, returning `(intercept, slope)`.
fn least_squares(series: &[f64]) -> (f64, f64) {
    let n = series.len() as f64;
    if n < 2.0 {
        return (series.first().copied().unwrap_or(0.0), 0.0);
    }
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = series.iter().sum::<f64>() / n;
    let (mut cov, mut var_x) = (0.0, 0.0);
    for (i, y) in series.iter().enumerate() {
        let dx = i as f64 - mean_x;
        cov += dx * (y - mean_y);
        var_x += dx * dx;
    }
    let slope = if var_x > 0.0 { cov / var_x } else { 0.0 };
    (mean_y - slope * mean_x, slope)
}

/// A KEDA-style cron for a weekly window: `MIN HOUR * * DOW`, with `*` when all seven days match.
pub fn weekly_window_cron(days: &[u32], minute_of_day: u32) -> String {
    let dow = if days.len() >= DAYS_PER_WEEK {
        "*".to_owned()
    } else {
        let mut days = days.to_vec();
        days.sort_unstable();
        days.dedup();
        days.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
    };
    format!("{} {} * * {}", minute_of_day % 60, minute_of_day / 60, dow)
}

/// Whether `now` falls inside a window that started at `start_cron` and lasts `duration_minutes`.
/// Handles only the `MIN HOUR * * DOW` shape this crate emits (covers windows crossing midnight).
pub fn cron_window_active(start_cron: &str, duration_minutes: i32, now: DateTime<Utc>) -> bool {
    let Some((minute, hour, days)) = parse_simple_cron(start_cron) else {
        return false;
    };
    if duration_minutes <= 0 {
        return false;
    }
    (0..=duration_minutes / MINUTES_PER_DAY as i32 + 1).any(|offset| {
        let date = (now - Duration::days(i64::from(offset))).date_naive();
        let Some(start) = date.and_hms_opt(hour, minute, 0).map(|naive| naive.and_utc()) else {
            return false;
        };
        days.contains(&start.weekday().num_days_from_sunday())
            && now >= start
            && now < start + Duration::minutes(i64::from(duration_minutes))
    })
}

fn parse_simple_cron(cron: &str) -> Option<(u32, u32, Vec<u32>)> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let minute: u32 = fields[0].parse().ok()?;
    let hour: u32 = fields[1].parse().ok()?;
    if minute > 59 || hour > 23 {
        return None;
    }
    Some((minute, hour, parse_dow(fields[4])?))
}

fn parse_dow(field: &str) -> Option<Vec<u32>> {
    if field == "*" {
        return Some((0..DAYS_PER_WEEK as u32).collect());
    }
    let mut days = Vec::new();
    for part in field.split(',') {
        match part.split_once('-') {
            Some((start, end)) => {
                for day in start.parse::<u32>().ok()?..=end.parse::<u32>().ok()? {
                    days.push(day % DAYS_PER_WEEK as u32);
                }
            }
            None => days.push(part.parse::<u32>().ok()? % DAYS_PER_WEEK as u32),
        }
    }
    Some(days)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn config() -> WorkloadConfig {
        WorkloadConfig {
            min_replicas: 5,
            max_replicas: 50,
            target_cpu_pct: 70,
            scale_down_cooldown_s: 300,
        }
    }

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 4, 0, 0, 0).unwrap() // a Sunday
    }

    /// `hours` of hourly points; `cpu(hour_of_day, absolute_hour) -> util`.
    fn series(hours: i64, cpu: impl Fn(u32, i64) -> f64) -> Vec<MetricPoint> {
        (0..hours)
            .map(|h| {
                let timestamp = start() + Duration::hours(h);
                MetricPoint {
                    timestamp,
                    cpu_util: cpu(timestamp.hour(), h),
                    replicas: 5,
                    queue_depth: None,
                }
            })
            .collect()
    }

    fn frac(i: i64) -> f64 {
        let x = i as f64 * 0.618_033_988_75;
        x - x.floor()
    }

    fn bin_at(forecast: &SeasonalForecast, when: DateTime<Utc>) -> f64 {
        let bins_per_day = forecast.bins.len() / DAYS_PER_WEEK;
        forecast.bins[weekly_bin_index(when, forecast.bin_minutes, bins_per_day)]
    }

    #[test]
    fn daily_peak_is_recovered() {
        // 21 days, busy 09:00-17:00, quiet otherwise, small deterministic jitter.
        let points = series(21 * 24, |hod, h| {
            let base = if (9..17).contains(&hod) { 0.85 } else { 0.15 };
            base + 0.02 * frac(h)
        });
        let forecast =
            build_profile(&points, &config(), &ForecastParams::default()).expect("periodic");
        let peak = bin_at(&forecast, start() + Duration::hours(13));
        let quiet = bin_at(&forecast, start() + Duration::hours(3));
        assert!(peak > quiet, "peak {peak} quiet {quiet}");
        assert!(forecast.confidence >= 0.3);
    }

    #[test]
    fn single_level_shift_is_not_a_cycle() {
        // One sustained step up halfway through - happened once, must not read as recurring.
        let points = series(21 * 24, |_, h| if h < 21 * 12 { 0.2 } else { 0.8 });
        assert!(build_profile(&points, &config(), &ForecastParams::default()).is_none());
    }

    #[test]
    fn flat_series_is_not_forecast() {
        let points = series(21 * 24, |_, _| 0.5);
        assert!(build_profile(&points, &config(), &ForecastParams::default()).is_none());
    }

    #[test]
    fn white_noise_is_not_forecast() {
        let points = series(21 * 24, |_, h| 0.3 + 0.5 * frac(h * 7 + 3));
        assert!(build_profile(&points, &config(), &ForecastParams::default()).is_none());
    }

    #[test]
    fn pure_growth_trend_is_not_a_cycle() {
        let points = series(21 * 24, |_, h| 0.1 + 0.0005 * h as f64);
        assert!(build_profile(&points, &config(), &ForecastParams::default()).is_none());
    }

    #[test]
    fn weekly_pattern_is_recovered() {
        // Busy only on weekdays (Mon-Fri), quiet on weekends.
        let points = series(28 * 24, |_, h| {
            let timestamp = start() + Duration::hours(h);
            let weekday = timestamp.weekday().num_days_from_sunday();
            if (1..=5).contains(&weekday) { 0.8 } else { 0.1 }
        });
        let forecast =
            build_profile(&points, &config(), &ForecastParams::default()).expect("periodic");
        // Wednesday noon (busy) vs Sunday noon (quiet). start() is a Sunday.
        let busy = bin_at(&forecast, start() + Duration::days(3) + Duration::hours(12));
        let quiet = bin_at(&forecast, start() + Duration::hours(12));
        assert!(busy > quiet, "busy {busy} quiet {quiet}");
    }

    #[test]
    fn needed_replicas_holds_target() {
        let point =
            MetricPoint { timestamp: start(), cpu_util: 0.9, replicas: 10, queue_depth: None };
        // ceil(10 * 0.9 / 0.7) = ceil(12.857) = 13.
        assert_eq!(needed_replicas(&point, &config()), 13.0);
    }

    #[test]
    fn trend_tracks_week_over_week_growth() {
        // Daily cycle whose amplitude grows each week.
        let points = series(28 * 24, |hod, h| {
            let week = (h / (24 * 7)) as f64;
            let base = if (9..17).contains(&hod) { 0.4 } else { 0.1 };
            base + 0.1 * week + 0.01 * frac(h)
        });
        let forecast =
            build_profile(&points, &config(), &ForecastParams::default()).expect("periodic");
        assert!(forecast.trend_per_week > 0.0, "trend {}", forecast.trend_per_week);
    }

    #[test]
    fn cron_round_trips_for_a_weekday_window() {
        let cron = weekly_window_cron(&[1, 2, 3, 4, 5], 8 * 60 + 30);
        assert_eq!(cron, "30 8 * * 1,2,3,4,5");
        // Wednesday 2026-01-07 at 09:00 is inside an 08:30 + 120m weekday window.
        let inside = Utc.with_ymd_and_hms(2026, 1, 7, 9, 0, 0).unwrap();
        assert!(cron_window_active(&cron, 120, inside));
        // Sunday is not a matching day.
        let sunday = Utc.with_ymd_and_hms(2026, 1, 4, 9, 0, 0).unwrap();
        assert!(!cron_window_active(&cron, 120, sunday));
        // Wednesday 12:00 is past the window end.
        let after = Utc.with_ymd_and_hms(2026, 1, 7, 12, 0, 0).unwrap();
        assert!(!cron_window_active(&cron, 120, after));
    }

    #[test]
    fn daily_cron_uses_wildcard_day() {
        assert_eq!(weekly_window_cron(&[0, 1, 2, 3, 4, 5, 6], 60), "0 1 * * *");
    }

    #[test]
    fn non_divisor_bin_size_covers_the_full_day() {
        // 50 doesn't divide 1440; the grid must cover all 1440 minutes (ceil), not truncate the last
        // partial bin. Goes through build_profile (not the recomputed formula) so reverting the
        // ceiling-division fix makes bins.len() wrong and fails this test.
        let bin = 50u32;
        let points = series(21 * 24, |hour, h| {
            let base = if (9..17).contains(&hour) { 0.85 } else { 0.15 };
            base + 0.02 * frac(h)
        });
        let params = ForecastParams { bin_minutes: bin, ..ForecastParams::default() };
        let forecast = build_profile(&points, &config(), &params).expect("periodic");
        let expected = MINUTES_PER_DAY.div_ceil(bin) as usize * DAYS_PER_WEEK;
        assert_eq!(forecast.bins.len(), expected); // 29 * 7 = 203, not 28 * 7 = 196
    }
}
