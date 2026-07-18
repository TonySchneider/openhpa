use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use kube::{Client, CustomResourceExt};
use openhpa_operator::config::Config;
use openhpa_operator::controller::{Context, run};
use openhpa_operator::crd::ScalingRecommendation;
use openhpa_operator::leader::{self, LeaderElector};
use openhpa_operator::llm::LlmBackend;
use openhpa_operator::metrics::MetricsSource;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    let cfg = Config::parse();

    if cfg.print_crd {
        println!("{}", serde_json::to_string_pretty(&ScalingRecommendation::crd())?);
        return Ok(());
    }

    let llm = LlmBackend::from_config(
        &cfg.llm_provider,
        &cfg.llm_model,
        cfg.resolve_api_key()?,
        cfg.llm_base_url.clone(),
        cfg.llm_timeout(),
    )?;
    let metrics = MetricsSource::from_config(
        &cfg.prometheus_url,
        cfg.promql_cpu.clone(),
        cfg.promql_replicas.clone(),
        cfg.promql_queue.clone(),
    );
    info!(
        provider = %cfg.llm_provider,
        namespaces = ?cfg.namespaces(),
        prometheus = metrics.is_prometheus(),
        leader_election = cfg.enable_leader_election,
        forecasting = cfg.enable_forecasting,
        mode = ?cfg.mode,
        apply_enabled = cfg.apply_enabled(),
        "openhpa operator starting"
    );

    let client = Client::try_default().await?;
    let interval = Duration::from_secs(cfg.interval_seconds);
    let elector = cfg.enable_leader_election.then(|| {
        let namespace = resolve_namespace(&cfg.lease_namespace);
        let identity = std::env::var("HOSTNAME").unwrap_or_else(|_| cfg.lease_name.clone());
        let lease_duration = leader::lease_duration_for_interval(cfg.interval_seconds);
        LeaderElector::new(
            client.clone(),
            &namespace,
            cfg.lease_name.clone(),
            identity,
            lease_duration,
        )
    });
    let ctx = Context {
        client,
        llm,
        metrics,
        namespaces: cfg.namespaces(),
        interval,
        metrics_window: chrono::Duration::days(i64::from(cfg.metrics_window_days)),
        metrics_step: Duration::from_secs(cfg.metrics_step_seconds),
        probation_window: cfg.probation_window(),
        rollback_enabled: cfg.rollback_enabled,
        health_cpu_margin: cfg.health_cpu_margin,
        apply_enabled: cfg.apply_enabled(),
        forecast_params: cfg.enable_forecasting.then(|| cfg.forecast_params()),
        llm_concurrency: cfg.llm_concurrency(),
        analysis_budget: cfg.analysis_budget(),
        cost_per_replica_usd_monthly: cfg.cost_per_replica_usd_monthly()?,
        elector,
    };
    run(ctx).await
}

/// Resolve the namespace for the leader Lease: the configured value, else the operator's own
/// namespace from the projected service-account token, else `default`.
fn resolve_namespace(configured: &str) -> String {
    if !configured.is_empty() {
        return configured.to_owned();
    }
    std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .map(|ns| ns.trim().to_owned())
        .ok()
        .filter(|ns| !ns.is_empty())
        .unwrap_or_else(|| "default".to_owned())
}
