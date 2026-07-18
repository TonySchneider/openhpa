use anyhow::{Result, bail};
use chrono::Duration;
use clap::{ArgAction, Parser, ValueEnum};
use openhpa_core::forecast::ForecastParams;

/// Whether the operator may mutate cluster autoscaling config. `Recommend` only ever emits
/// `ScalingRecommendation` CRDs - it never patches a workload, even an approved one (the safe default
/// for a first rollout). `Apply` additionally patches approved targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum OperatingMode {
    Recommend,
    Apply,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "openhpa-operator")]
pub struct Config {
    /// Comma-separated namespaces to watch; empty = all.
    #[arg(long, env = "OPENHPA_WATCH_NAMESPACES", default_value = "")]
    pub watch_namespaces: String,
    /// Operating mode: `recommend` (only emit recommendations, never mutate - even an approved CR) or
    /// `apply` (also patch approved targets). Recommend-only is the safe rollout default.
    #[arg(long, env = "OPENHPA_MODE", value_enum, default_value_t = OperatingMode::Recommend)]
    pub mode: OperatingMode,
    /// LLM backend: openai | anthropic | none (rules-only). bedrock is planned, not yet wired.
    #[arg(long, env = "OPENHPA_LLM_PROVIDER", default_value = "none")]
    pub llm_provider: String,
    #[arg(long, env = "OPENHPA_LLM_MODEL", default_value = "gpt-4o-mini")]
    pub llm_model: String,
    /// Override the LLM API base URL (empty = the provider's real endpoint); point at an in-cluster
    /// proxy, a local-model sidecar, or a mock.
    #[arg(long, env = "OPENHPA_LLM_BASE_URL")]
    pub llm_base_url: Option<String>,
    /// Per-request timeout (seconds) for LLM calls so a stalled model can't stretch a reconcile pass.
    #[arg(long, env = "OPENHPA_LLM_TIMEOUT_SECONDS", default_value_t = 30)]
    pub llm_timeout_seconds: u64,
    /// Max concurrent LLM calls during the analysis pass (values below 1 are treated as 1).
    #[arg(long, env = "OPENHPA_LLM_CONCURRENCY", default_value_t = 4)]
    pub llm_concurrency: u32,
    /// Time budget (seconds) for the analysis pass's LLM phase; unset = one reconcile interval.
    /// Workloads not reached before the budget elapses are analyzed on the next tick.
    #[arg(long, env = "OPENHPA_ANALYSIS_BUDGET_SECONDS")]
    pub analysis_budget_seconds: Option<u64>,
    /// Estimated monthly cost (USD) of one always-on replica, pricing the projected-savings
    /// estimates (rules fallback + LLM prompt guidance). Must be > 0.
    #[arg(long, env = "OPENHPA_COST_PER_REPLICA_USD_MONTHLY", default_value_t = openhpa_core::ESTIMATED_REPLICA_MONTHLY_USD)]
    pub cost_per_replica_usd_monthly: f64,
    /// Seconds between reconcile ticks.
    #[arg(long, env = "OPENHPA_INTERVAL_SECONDS", default_value_t = 300)]
    pub interval_seconds: u64,
    /// Prometheus base URL for metric history; empty falls back to slow HPA-status accumulation.
    #[arg(long, env = "OPENHPA_PROMETHEUS_URL", default_value = "")]
    pub prometheus_url: String,
    /// Days of metric history to backfill from Prometheus per workload.
    #[arg(long, env = "OPENHPA_METRICS_WINDOW_DAYS", default_value_t = 14)]
    pub metrics_window_days: u32,
    /// Sampling step (seconds) for Prometheus history queries.
    #[arg(long, env = "OPENHPA_METRICS_STEP_SECONDS", default_value_t = 300)]
    pub metrics_step_seconds: u64,
    /// PromQL template overrides (`{ns}` / `{deploy}` placeholders); default to kube-state-metrics.
    #[arg(long, env = "OPENHPA_PROMQL_CPU")]
    pub promql_cpu: Option<String>,
    #[arg(long, env = "OPENHPA_PROMQL_REPLICAS")]
    pub promql_replicas: Option<String>,
    #[arg(long, env = "OPENHPA_PROMQL_QUEUE")]
    pub promql_queue: Option<String>,
    /// Minutes an applied change stays on probation before the verify pass judges its health.
    #[arg(long, env = "OPENHPA_PROBATION_WINDOW_MINUTES", default_value_t = 45)]
    pub probation_window_minutes: u32,
    /// Auto-revert a probationary change when health degrades (the safety net).
    #[arg(long, env = "OPENHPA_ROLLBACK_ENABLED", default_value_t = true, action = ArgAction::Set)]
    pub rollback_enabled: bool,
    /// Headroom over the target CPU fraction tolerated before a change is judged degraded.
    #[arg(long, env = "OPENHPA_HEALTH_CPU_MARGIN", default_value_t = 0.15)]
    pub health_cpu_margin: f64,
    /// Elect a single leader so `replicas: 2+` never double-apply; only the leader mutates.
    #[arg(long, env = "OPENHPA_ENABLE_LEADER_ELECTION", default_value_t = true, action = ArgAction::Set)]
    pub enable_leader_election: bool,
    /// Name of the coordination.k8s.io Lease used for leader election.
    #[arg(long, env = "OPENHPA_LEASE_NAME", default_value = "openhpa-leader")]
    pub lease_name: String,
    /// Namespace for the leader Lease; empty = the operator's own namespace.
    #[arg(long, env = "OPENHPA_LEASE_NAMESPACE", default_value = "")]
    pub lease_namespace: String,
    /// Forecast recurring peaks and pre-scale the floor ahead of them (off until validated).
    #[arg(long, env = "OPENHPA_ENABLE_FORECASTING", default_value_t = false, action = ArgAction::Set)]
    pub enable_forecasting: bool,
    /// Minimum days of history before a forecast is attempted (needs >= 2 weekly cycles).
    #[arg(long, env = "OPENHPA_FORECAST_MIN_HISTORY_DAYS", default_value_t = 14)]
    pub forecast_min_history_days: u32,
    /// Seasonal grid bin size in minutes (168 hourly bins per week by default).
    #[arg(long, env = "OPENHPA_FORECAST_BIN_MINUTES", default_value_t = 60)]
    pub forecast_bin_minutes: u32,
    /// Quantile of per-bin demand used for the forecast (headroom for safety).
    #[arg(long, env = "OPENHPA_FORECAST_QUANTILE", default_value_t = 0.95)]
    pub forecast_quantile: f64,
    /// Minimum lag-24h/7d autocorrelation for a workload to count as periodic (forecast gate).
    #[arg(long, env = "OPENHPA_PERIODICITY_THRESHOLD", default_value_t = 0.3)]
    pub periodicity_threshold: f64,
    /// Minutes of head start to raise the floor before each forecasted peak.
    #[arg(long, env = "OPENHPA_PRESCALE_LEAD_MINUTES", default_value_t = 10)]
    pub prescale_lead_minutes: u32,
    /// Print the ScalingRecommendation CRD as JSON and exit.
    #[arg(long)]
    pub print_crd: bool,
}

impl Config {
    pub fn namespaces(&self) -> Vec<String> {
        self.watch_namespaces
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }

    /// Per-request LLM timeout as a `std::time::Duration` for the reqwest client builder.
    pub fn llm_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.llm_timeout_seconds)
    }

    /// LLM fan-out width for the analysis pass, clamped so at least one call is in flight.
    pub fn llm_concurrency(&self) -> usize {
        self.llm_concurrency.max(1) as usize
    }

    /// Analysis-pass LLM time budget; defaults to one reconcile interval so a cold pass over many
    /// workloads can't stretch a tick unboundedly.
    pub fn analysis_budget(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.analysis_budget_seconds.unwrap_or(self.interval_seconds),
        )
    }

    /// Whether the operator may patch workloads. `false` in `recommend` mode (never mutate).
    pub fn apply_enabled(&self) -> bool {
        self.mode == OperatingMode::Apply
    }

    pub fn forecast_params(&self) -> ForecastParams {
        ForecastParams {
            min_history_days: i64::from(self.forecast_min_history_days),
            bin_minutes: self.forecast_bin_minutes,
            // Clamp so a misconfigured quantile can't drive percentile() out of bounds.
            quantile: self.forecast_quantile.clamp(0.0, 1.0),
            periodicity_threshold: self.periodicity_threshold,
            prescale_lead_minutes: i64::from(self.prescale_lead_minutes),
        }
    }

    /// Probation window, clamped to a positive minimum so apply and verify can't both run in one
    /// tick and rubber-stamp a change with zero post-apply data.
    pub fn probation_window(&self) -> Duration {
        Duration::minutes(i64::from(self.probation_window_minutes.max(1)))
    }

    /// API key for the configured LLM provider, read from that provider's env var.
    pub fn resolve_api_key(&self) -> Result<Option<String>> {
        select_api_key(&self.llm_provider, |var| std::env::var(var).ok())
    }

    /// Per-replica monthly cost for savings estimates; a zero or negative cost is a
    /// misconfiguration, not a clampable value, so it is rejected at startup.
    pub fn cost_per_replica_usd_monthly(&self) -> Result<f64> {
        if self.cost_per_replica_usd_monthly > 0.0 {
            Ok(self.cost_per_replica_usd_monthly)
        } else {
            bail!(
                "--cost-per-replica-usd-monthly must be > 0 (got {})",
                self.cost_per_replica_usd_monthly
            )
        }
    }
}

/// Env var holding the API key for a provider; `None` for the keyless `none` / unknown backends.
fn api_key_env_var(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

/// Resolve the API key for `provider` via `lookup`, keyed strictly off the configured provider (not
/// env-var precedence), erroring if the matching var is unset. `none`/unknown providers need no key
/// (`Ok(None)`); the LLM backend rejects an unknown provider downstream.
fn select_api_key(
    provider: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    let Some(var) = api_key_env_var(provider) else {
        return Ok(None);
    };
    match lookup(var) {
        Some(key) => Ok(Some(key)),
        None => {
            bail!("llm provider '{provider}' requires the {var} environment variable to be set")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_rules_only_and_all_namespaces() {
        let cfg = Config::parse_from(["openhpa-operator"]);
        assert_eq!(cfg.llm_provider, "none");
        assert!(cfg.namespaces().is_empty());
    }

    #[test]
    fn selects_key_for_the_configured_provider() {
        let key = select_api_key("openai", |var| {
            (var == "OPENAI_API_KEY").then(|| "sk-openai".to_owned())
        })
        .unwrap();
        assert_eq!(key, Some("sk-openai".to_owned()));
    }

    #[test]
    fn anthropic_provider_ignores_a_present_openai_key() {
        // Both keys set: provider=anthropic must use the Anthropic key, never fall back to OpenAI.
        let key = select_api_key("anthropic", |var| match var {
            "OPENAI_API_KEY" => Some("sk-openai".to_owned()),
            "ANTHROPIC_API_KEY" => Some("sk-anthropic".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(key, Some("sk-anthropic".to_owned()));
    }

    #[test]
    fn missing_provider_key_errors_naming_the_var() {
        // provider=anthropic with only OPENAI_API_KEY present must error, not silently send the
        // OpenAI key to Anthropic (the old OPENAI-first precedence bug).
        let err = select_api_key("anthropic", |var| {
            (var == "OPENAI_API_KEY").then(|| "sk-openai".to_owned())
        })
        .unwrap_err();
        assert!(err.to_string().contains("ANTHROPIC_API_KEY"), "{err}");
    }

    #[test]
    fn none_provider_needs_no_key() {
        assert_eq!(select_api_key("none", |_| None).unwrap(), None);
    }

    #[test]
    fn mode_defaults_to_recommend_and_apply_enables_apply() {
        // Safe-by-default: the binary must never mutate unless apply is explicitly requested, so a
        // direct run or an omitted Helm value can't patch a workload.
        let cfg = Config::parse_from(["openhpa-operator"]);
        assert_eq!(cfg.mode, OperatingMode::Recommend);
        assert!(!cfg.apply_enabled(), "the default mode must never enable apply");

        let cfg = Config::parse_from(["openhpa-operator", "--mode", "recommend"]);
        assert_eq!(cfg.mode, OperatingMode::Recommend);
        assert!(!cfg.apply_enabled(), "recommend mode must never enable apply");

        let cfg = Config::parse_from(["openhpa-operator", "--mode", "apply"]);
        assert_eq!(cfg.mode, OperatingMode::Apply);
        assert!(cfg.apply_enabled(), "apply mode enables apply");
    }

    #[test]
    fn llm_endpoint_defaults_and_overrides() {
        let cfg = Config::parse_from(["openhpa-operator"]);
        assert!(cfg.llm_base_url.is_none(), "base URL defaults to the provider's real endpoint");
        assert_eq!(cfg.llm_timeout(), std::time::Duration::from_secs(30));

        let cfg = Config::parse_from([
            "openhpa-operator",
            "--llm-base-url",
            "http://prom-proxy:8080",
            "--llm-timeout-seconds",
            "5",
        ]);
        assert_eq!(cfg.llm_base_url.as_deref(), Some("http://prom-proxy:8080"));
        assert_eq!(cfg.llm_timeout(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn hardening_defaults() {
        let cfg = Config::parse_from(["openhpa-operator"]);
        assert!(cfg.prometheus_url.is_empty());
        assert_eq!(cfg.metrics_window_days, 14);
        assert!(cfg.promql_cpu.is_none());
        assert_eq!(cfg.probation_window_minutes, 45);
        assert!(cfg.rollback_enabled);
        assert!((cfg.health_cpu_margin - 0.15).abs() < f64::EPSILON);
        assert!(cfg.enable_leader_election);
        assert_eq!(cfg.lease_name, "openhpa-leader");
    }

    #[test]
    fn forecasting_defaults_off_with_sane_params() {
        let cfg = Config::parse_from(["openhpa-operator"]);
        assert!(!cfg.enable_forecasting);
        let params = cfg.forecast_params();
        assert_eq!(params.min_history_days, 14);
        assert_eq!(params.bin_minutes, 60);
        assert!((params.periodicity_threshold - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn forecast_quantile_is_clamped() {
        let cfg = Config::parse_from(["openhpa-operator", "--forecast-quantile", "1.5"]);
        assert!((cfg.forecast_params().quantile - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn llm_concurrency_defaults_and_clamps_to_at_least_one() {
        let cfg = Config::parse_from(["openhpa-operator"]);
        assert_eq!(cfg.llm_concurrency(), 4);

        let cfg = Config::parse_from(["openhpa-operator", "--llm-concurrency", "0"]);
        assert_eq!(cfg.llm_concurrency(), 1);
    }

    #[test]
    fn analysis_budget_defaults_to_one_interval() {
        let cfg = Config::parse_from(["openhpa-operator", "--interval-seconds", "120"]);
        assert_eq!(cfg.analysis_budget(), std::time::Duration::from_secs(120));

        let cfg = Config::parse_from(["openhpa-operator", "--analysis-budget-seconds", "30"]);
        assert_eq!(cfg.analysis_budget(), std::time::Duration::from_secs(30));
    }

    #[test]
    fn cost_per_replica_defaults_to_30_and_rejects_nonpositive_values() {
        let cfg = Config::parse_from(["openhpa-operator"]);
        assert!((cfg.cost_per_replica_usd_monthly().unwrap() - 30.0).abs() < f64::EPSILON);

        let cfg =
            Config::parse_from(["openhpa-operator", "--cost-per-replica-usd-monthly", "55.5"]);
        assert!((cfg.cost_per_replica_usd_monthly().unwrap() - 55.5).abs() < f64::EPSILON);

        for bad in ["=0", "=-12.5"] {
            let arg = format!("--cost-per-replica-usd-monthly{bad}");
            let cfg = Config::parse_from(["openhpa-operator", &arg]);
            let err = cfg.cost_per_replica_usd_monthly().unwrap_err();
            assert!(err.to_string().contains("must be > 0"), "{err}");
        }
    }

    #[test]
    fn probation_window_has_a_positive_minimum() {
        let cfg = Config::parse_from(["openhpa-operator", "--probation-window-minutes", "0"]);
        assert_eq!(cfg.probation_window(), chrono::Duration::minutes(1));
    }

    #[test]
    fn bool_flags_accept_explicit_false() {
        let cfg = Config::parse_from([
            "openhpa-operator",
            "--enable-leader-election=false",
            "--rollback-enabled",
            "false",
        ]);
        assert!(!cfg.enable_leader_election);
        assert!(!cfg.rollback_enabled);
    }

    #[test]
    fn namespaces_split_trims_and_drops_empties() {
        let cfg =
            Config::parse_from(["openhpa-operator", "--watch-namespaces", "default, prod ,,api"]);
        assert_eq!(
            cfg.namespaces(),
            vec!["default".to_owned(), "prod".to_owned(), "api".to_owned()]
        );
    }
}
