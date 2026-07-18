use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use openhpa_core::MetricPoint;
use serde_json::Value;
use tracing::warn;

/// Default PromQL for cpu utilization (0..1) of a deployment's pods, against kube-state-metrics +
/// cAdvisor. `{ns}` / `{deploy}` are interpolated per workload. The `or` is applied to the whole
/// usage/requests ratio so the usage/limits ratio is used ONLY when no requests series exists - a
/// requests-preferred fallback for workloads without CPU requests, never a blend of the two.
const DEFAULT_CPU_QUERY: &str = "avg(rate(container_cpu_usage_seconds_total{namespace=\"{ns}\",pod=~\"{deploy}-.*\"}[5m])) / avg(kube_pod_container_resource_requests{namespace=\"{ns}\",pod=~\"{deploy}-.*\",resource=\"cpu\"}) or avg(rate(container_cpu_usage_seconds_total{namespace=\"{ns}\",pod=~\"{deploy}-.*\"}[5m])) / avg(kube_pod_container_resource_limits{namespace=\"{ns}\",pod=~\"{deploy}-.*\",resource=\"cpu\"})";
const DEFAULT_REPLICAS_QUERY: &str =
    "kube_deployment_status_replicas{namespace=\"{ns}\",deployment=\"{deploy}\"}";

/// Where the operator reads workload metric history. `Prometheus` backfills weeks of real history on
/// every tick (survives restart); `HpaStatus` is the degraded fallback that accumulates one point
/// per tick from the live HPA status (history rebuilds slowly).
pub enum MetricsSource {
    Prometheus(PrometheusSource),
    HpaStatus,
}

impl MetricsSource {
    /// Build the source from config: a non-empty Prometheus URL selects `Prometheus`, otherwise the
    /// `HpaStatus` fallback. PromQL overrides fall back to the kube-state-metrics defaults.
    pub fn from_config(
        prometheus_url: &str,
        cpu: Option<String>,
        replicas: Option<String>,
        queue: Option<String>,
    ) -> Self {
        let url = prometheus_url.trim();
        if url.is_empty() {
            Self::HpaStatus
        } else {
            Self::Prometheus(PrometheusSource::new(
                url.to_owned(),
                PromQlTemplates::from_overrides(cpu, replicas, queue),
            ))
        }
    }

    /// Metric history for one workload since `since`, sampled at `step`. The `HpaStatus` fallback
    /// has no external store and returns nothing here (the controller accumulates it per tick).
    pub async fn history(
        &self,
        namespace: &str,
        deployment: &str,
        since: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<MetricPoint>> {
        match self {
            Self::Prometheus(source) => source.history(namespace, deployment, since, step).await,
            Self::HpaStatus => Ok(Vec::new()),
        }
    }

    pub fn is_prometheus(&self) -> bool {
        matches!(self, Self::Prometheus(_))
    }
}

/// PromQL templates with `{ns}` / `{deploy}` placeholders. `queue` is optional (no default).
pub struct PromQlTemplates {
    cpu: String,
    replicas: String,
    queue: Option<String>,
}

impl PromQlTemplates {
    fn from_overrides(
        cpu: Option<String>,
        replicas: Option<String>,
        queue: Option<String>,
    ) -> Self {
        Self {
            cpu: cpu.unwrap_or_else(|| DEFAULT_CPU_QUERY.to_owned()),
            replicas: replicas.unwrap_or_else(|| DEFAULT_REPLICAS_QUERY.to_owned()),
            queue,
        }
    }
}

pub struct PrometheusSource {
    http: reqwest::Client,
    base_url: String,
    templates: PromQlTemplates,
}

impl PrometheusSource {
    fn new(base_url: String, templates: PromQlTemplates) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_owned(),
            templates,
        }
    }

    async fn history(
        &self,
        namespace: &str,
        deployment: &str,
        since: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<MetricPoint>> {
        let end = Utc::now();
        let cpu = self
            .query_range(&interpolate(&self.templates.cpu, namespace, deployment), since, end, step)
            .await?;
        let replicas = self
            .query_range(
                &interpolate(&self.templates.replicas, namespace, deployment),
                since,
                end,
                step,
            )
            .await?;
        let queue = match &self.templates.queue {
            Some(template) => {
                self.query_range(&interpolate(template, namespace, deployment), since, end, step)
                    .await?
            }
            None => Vec::new(),
        };
        Ok(assemble_points(cpu, &replicas, &queue))
    }

    async fn query_range(
        &self,
        query: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<(i64, f64)>> {
        let start = start.timestamp().to_string();
        let end = end.timestamp().to_string();
        let step = step.as_secs().to_string();
        let body: Value = self
            .http
            .get(format!("{}/api/v1/query_range", self.base_url))
            .query(&[("query", query), ("start", &start), ("end", &end), ("step", &step)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if matrix_series_count(&body) > 1 {
            warn!(%query, "prometheus query returned multiple series; summing across them");
        }
        parse_matrix_series(&body)
    }
}

fn matrix_series_count(body: &Value) -> usize {
    body.get("data")
        .and_then(|data| data.get("result"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn interpolate(template: &str, namespace: &str, deployment: &str) -> String {
    template.replace("{ns}", namespace).replace("{deploy}", deployment)
}

/// Parse a Prometheus `query_range` matrix response into time-sorted `(unix_secs, value)` pairs,
/// summing across every returned series at each timestamp - a single-series result is
/// unchanged, while sharded kube-state-metrics / per-partition metrics are aggregated rather than
/// silently reduced to one arbitrary series. An empty result set is not an error.
fn parse_matrix_series(body: &Value) -> Result<Vec<(i64, f64)>> {
    let result = body
        .get("data")
        .and_then(|data| data.get("result"))
        .and_then(Value::as_array)
        .context("prometheus response missing data.result array")?;
    let mut summed: BTreeMap<i64, f64> = BTreeMap::new();
    for series in result {
        let values = series
            .get("values")
            .and_then(Value::as_array)
            .context("prometheus series missing values")?;
        for pair in values {
            if let Some((timestamp, sample)) = parse_pair(pair)? {
                *summed.entry(timestamp).or_insert(0.0) += sample;
            }
        }
    }
    Ok(summed.into_iter().collect())
}

/// Parse one `[timestamp, "sample"]` pair. Non-finite samples (Prometheus emits the string `"NaN"`,
/// e.g. when a CPU request is unset and the ratio divides by zero) are skipped (`Ok(None)`) so they
/// never poison `cpu_util`; only a genuinely malformed pair is an error.
fn parse_pair(pair: &Value) -> Result<Option<(i64, f64)>> {
    let pair = pair.as_array().context("prometheus value pair is not an array")?;
    let timestamp =
        pair.first().and_then(Value::as_f64).context("prometheus value missing timestamp")?;
    let raw = pair.get(1).and_then(Value::as_str).context("prometheus value missing sample")?;
    let sample: f64 =
        raw.parse().with_context(|| format!("prometheus sample is not a float: {raw}"))?;
    Ok(sample.is_finite().then_some((timestamp as i64, sample)))
}

/// Join the three series into time-sorted `MetricPoint`s over the UNION of their timestamps, so an
/// empty or partial cpu series no longer collapses the whole timeline and a replicas gap no
/// longer fabricates `replicas=0` against live cpu. cpu and replicas are carried forward from
/// their last known sample (cpu defaults to 0.0 until first seen); points before the first known
/// replicas count are dropped, since a `MetricPoint` needs a real replica count. A cpu-only gap is
/// encoded as `cpu_util = 0.0` (carried forward) while the timeline keeps advancing; the
/// `update_history` warning fires only when every series is empty (a total fetch loss).
fn assemble_points(
    cpu: Vec<(i64, f64)>,
    replicas: &[(i64, f64)],
    queue: &[(i64, f64)],
) -> Vec<MetricPoint> {
    let cpu_at: BTreeMap<i64, f64> = cpu.into_iter().collect();
    let replicas_at: BTreeMap<i64, f64> = replicas.iter().copied().collect();
    let queue_at: HashMap<i64, f64> = queue.iter().copied().collect();
    let mut timestamps: BTreeSet<i64> = BTreeSet::new();
    timestamps.extend(cpu_at.keys().copied());
    timestamps.extend(replicas_at.keys().copied());
    timestamps.extend(queue_at.keys().copied());

    let mut last_cpu = 0.0;
    let mut last_replicas: Option<i32> = None;
    timestamps
        .into_iter()
        .filter_map(|timestamp| {
            if let Some(value) = cpu_at.get(&timestamp) {
                last_cpu = *value;
            }
            if let Some(value) = replicas_at.get(&timestamp) {
                last_replicas = Some(value.round() as i32);
            }
            Some(MetricPoint {
                timestamp: DateTime::from_timestamp(timestamp, 0)?,
                cpu_util: last_cpu,
                replicas: last_replicas?,
                queue_depth: queue_at.get(&timestamp).copied(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_matrix_series_values() {
        let body = json!({
            "status": "success",
            "data": {
                "resultType": "matrix",
                "result": [{"metric": {}, "values": [[1700000000, "0.42"], [1700000300, "0.55"]]}],
            }
        });
        assert_eq!(
            parse_matrix_series(&body).unwrap(),
            vec![(1700000000, 0.42), (1700000300, 0.55)]
        );
    }

    #[test]
    fn non_finite_samples_are_skipped() {
        let body = json!({
            "data": {"result": [{"metric": {}, "values": [[1700000000, "NaN"], [1700000300, "0.5"]]}]}
        });
        assert_eq!(parse_matrix_series(&body).unwrap(), vec![(1700000300, 0.5)]);
    }

    #[test]
    fn malformed_sample_is_still_an_error() {
        let body = json!({"data": {"result": [{"metric": {}, "values": [[1700000000, "oops"]]}]}});
        assert!(parse_matrix_series(&body).unwrap_err().to_string().contains("not a float"));
    }

    #[test]
    fn empty_result_is_not_an_error() {
        let body = json!({"status": "success", "data": {"resultType": "matrix", "result": []}});
        assert!(parse_matrix_series(&body).unwrap().is_empty());
    }

    #[test]
    fn missing_result_array_is_an_error() {
        let body = json!({"status": "success", "data": {}});
        let err = parse_matrix_series(&body).unwrap_err();
        assert!(err.to_string().contains("data.result"), "{err}");
    }

    #[test]
    fn assembles_points_aligning_replicas_and_queue() {
        let cpu = vec![(1700000000, 0.42), (1700000300, 0.55)];
        let replicas = [(1700000000, 3.0), (1700000300, 4.0)];
        let queue = [(1700000300, 12.0)];
        let points = assemble_points(cpu, &replicas, &queue);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].replicas, 3);
        assert_eq!(points[0].queue_depth, None);
        assert_eq!(points[1].replicas, 4);
        assert_eq!(points[1].queue_depth, Some(12.0));
    }

    #[test]
    fn missing_replicas_carry_forward_never_zero() {
        // cpu at three grid points, replicas only at the 1st and 3rd (gap at the 2nd) - the gap
        // carries the last-known count forward, never fabricating replicas=0 against live cpu.
        let cpu = vec![(1700000000, 0.42), (1700000300, 0.55), (1700000600, 0.6)];
        let replicas = [(1700000000, 3.0), (1700000600, 7.0)];
        let points = assemble_points(cpu, &replicas, &[]);
        assert_eq!(points.iter().map(|p| p.replicas).collect::<Vec<_>>(), vec![3, 3, 7]);
        assert!(points.iter().all(|p| p.replicas > 0));
    }

    #[test]
    fn empty_cpu_uses_the_replicas_timeline() {
        // an empty cpu series must NOT zero out history when replicas data is present.
        let points = assemble_points(vec![], &[(1700000000, 3.0), (1700000300, 4.0)], &[]);
        assert_eq!(points.len(), 2, "{points:?}");
        assert_eq!(points.iter().map(|p| p.replicas).collect::<Vec<_>>(), vec![3, 4]);
    }

    #[test]
    fn cpu_loss_mid_stream_keeps_advancing() {
        // cpu present only at the first grid point; the timeline must still advance over the
        // later replicas samples (carrying the last cpu forward) instead of collapsing to one point.
        let cpu = vec![(1700000000, 0.5)];
        let replicas = [(1700000000, 3.0), (1700000300, 3.0), (1700000600, 3.0)];
        let points = assemble_points(cpu, &replicas, &[]);
        assert_eq!(points.len(), 3, "{points:?}");
        assert!(points.iter().all(|p| (p.cpu_util - 0.5).abs() < f64::EPSILON));
    }

    #[test]
    fn leading_points_without_replicas_are_dropped() {
        // No replicas sample until the 2nd grid point - the 1st cpu point has no real count to use.
        let cpu = vec![(1700000000, 0.42), (1700000300, 0.55)];
        let points = assemble_points(cpu, &[(1700000300, 4.0)], &[]);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].replicas, 4);
    }

    #[test]
    fn sums_across_multiple_series() {
        // Sharded KSM / per-partition metric: two series at the same timestamps are summed.
        let body = json!({"data": {"result": [
            {"metric": {"shard": "a"}, "values": [[1700000000, "2"], [1700000300, "3"]]},
            {"metric": {"shard": "b"}, "values": [[1700000000, "5"], [1700000300, "4"]]},
        ]}});
        assert_eq!(parse_matrix_series(&body).unwrap(), vec![(1700000000, 7.0), (1700000300, 7.0)]);
        assert_eq!(matrix_series_count(&body), 2);
    }

    #[test]
    fn default_cpu_query_tolerates_missing_requests() {
        // denominator falls back to CPU limits when requests are unset.
        assert!(DEFAULT_CPU_QUERY.contains("kube_pod_container_resource_limits"));
    }

    #[test]
    fn interpolate_replaces_placeholders() {
        let out = interpolate("x{namespace=\"{ns}\",deployment=\"{deploy}\"}", "prod", "web");
        assert_eq!(out, "x{namespace=\"prod\",deployment=\"web\"}");
    }

    #[test]
    fn empty_url_selects_hpa_status_fallback() {
        let source = MetricsSource::from_config("  ", None, None, None);
        assert!(!source.is_prometheus());
    }

    #[test]
    fn url_selects_prometheus_with_default_templates() {
        let source = MetricsSource::from_config("http://prom:9090/", None, None, None);
        assert!(source.is_prometheus());
        let MetricsSource::Prometheus(source) = source else { unreachable!() };
        assert_eq!(source.base_url, "http://prom:9090");
        assert_eq!(source.templates.cpu, DEFAULT_CPU_QUERY);
        assert!(source.templates.queue.is_none());
    }

    #[test]
    fn promql_overrides_take_precedence() {
        let source = MetricsSource::from_config(
            "http://prom",
            Some("custom_cpu".to_owned()),
            None,
            Some("q".to_owned()),
        );
        let MetricsSource::Prometheus(source) = source else { unreachable!() };
        assert_eq!(source.templates.cpu, "custom_cpu");
        assert_eq!(source.templates.replicas, DEFAULT_REPLICAS_QUERY);
        assert_eq!(source.templates.queue.as_deref(), Some("q"));
    }
}
