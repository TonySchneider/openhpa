use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use kube::api::{ListParams, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client, ResourceExt};
use openhpa_core::forecast::{ForecastParams, cron_window_active};
use openhpa_core::rules::{detect_predictable_peak, run_rules};
use openhpa_core::synthesis::synthesize;
use openhpa_core::{Candidate, MetricPoint, MetricsSnapshot, ScalingSchedule, WorkloadConfig};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::applier::{keda_schedule_in_sync, patch_for, revert_patch_for, schedule_patch_for_keda};
use crate::collector::{
    current_point, is_keda_managed, scale_target_name, scaled_object_config,
    scaled_object_target_name, workload_config,
};
use crate::crd::{
    DiffEntry, Phase, ScalingRecommendation, ScalingRecommendationSpec, ScheduleWindowSpec,
    TargetKind, TargetRef,
};
use crate::leader::LeaderElector;
use crate::llm::LlmBackend;
use crate::metrics::MetricsSource;
use crate::safety::{HealthVerdict, evaluate_health};

const HISTORY_CAP: usize = 4032; // ~14 days at 5-min resolution
/// Appended to a recommendation's summary when the operator runs in recommend-only mode: it never
/// mutates a workload, so the recommendation stands as advice for a human to apply.
const ADVISORY_SUFFIX: &str = "\n\n_This operator is running in recommend-only mode; apply this change yourself, or run with `--mode=apply` to auto-apply approved recommendations._";
/// A pre-scaled floor whose workload stays below this fraction of target counts as idle.
const GHOST_UTIL_RATIO: f64 = 0.5;
/// Consecutive idle observations inside windows before a schedule is retracted (peak not real).
const RETRACT_AFTER_IDLE_SAMPLES: u32 = 6;

pub struct Context {
    pub client: Client,
    pub llm: LlmBackend,
    pub metrics: MetricsSource,
    pub namespaces: Vec<String>,
    pub interval: Duration,
    pub metrics_window: chrono::Duration,
    pub metrics_step: Duration,
    pub probation_window: chrono::Duration,
    pub rollback_enabled: bool,
    pub health_cpu_margin: f64,
    /// Whether the operator may patch workloads. `false` (recommend-only) skips the mutating apply +
    /// schedule passes entirely, so even an approved recommendation is never applied.
    pub apply_enabled: bool,
    /// Forecast tuning, or `None` when proactive forecasting is disabled.
    pub forecast_params: Option<ForecastParams>,
    /// Max concurrent LLM calls during the analysis pass.
    pub llm_concurrency: usize,
    /// Time budget for the analysis pass's LLM phase; workloads not reached before it elapses are
    /// deferred to the next tick.
    pub analysis_budget: Duration,
    /// Estimated monthly cost (USD) of one always-on replica, pricing the savings estimates.
    pub cost_per_replica_usd_monthly: f64,
    /// Leader elector, or `None` when leader election is disabled (always acts as leader).
    pub elector: Option<LeaderElector>,
}

// Reconcile daemon: intentionally runs until the process is killed; per-tick errors are logged,
// not propagated, so the loop never breaks. Mutating passes (apply/verify) run only on the leader.
#[allow(clippy::infinite_loop)]
pub async fn run(ctx: Context) -> Result<()> {
    if ctx.metrics.is_prometheus() {
        info!("metrics source: Prometheus history backfill");
    } else {
        warn!(
            "metrics source: HPA-status fallback (history rebuilds slowly; set --prometheus-url)"
        );
    }
    let mut history: HashMap<String, Vec<MetricPoint>> = HashMap::new();
    let mut schedule_misses: HashMap<String, u32> = HashMap::new();
    loop {
        let now = Utc::now();
        let leader = is_leader(&ctx, now).await;
        info!(
            leader,
            mode = if ctx.apply_enabled { "apply" } else { "recommend" },
            "reconcile tick"
        );
        if let Err(error) = analysis_pass(&ctx, &mut history, now).await {
            warn!(%error, "analysis pass failed");
        }
        // Analysis (incl. per-workload LLM calls) and each mutating pass can take real time. Each
        // pass is gated on a fresh leadership check and bounded by a time budget shorter than the
        // lease; losing leadership or blowing the budget stops the remaining passes, so an expired
        // or overrunning leader cannot keep patching while a peer takes over.
        if leader {
            // Budget = one reconcile interval, which is half the lease (interval*2); a pass that
            // exceeds it is aborted well before a peer could legitimately take over.
            let budget = ctx.interval;
            let mut proceed = guarded_mutate(
                is_leader(&ctx, Utc::now()).await,
                budget,
                "apply pass",
                apply_pass(&ctx, Utc::now()),
            )
            .await;
            proceed = proceed
                && guarded_mutate(
                    is_leader(&ctx, Utc::now()).await,
                    budget,
                    "verify pass",
                    verify_pass(&ctx, &history, Utc::now()),
                )
                .await;
            if proceed {
                guarded_mutate(
                    is_leader(&ctx, Utc::now()).await,
                    budget,
                    "schedule pass",
                    schedule_pass(&ctx, &history, &mut schedule_misses, Utc::now()),
                )
                .await;
            }
        }
        tokio::time::sleep(jittered_sleep(ctx.interval, now)).await;
    }
}

async fn is_leader(ctx: &Context, now: DateTime<Utc>) -> bool {
    match &ctx.elector {
        None => true,
        Some(elector) => elector
            .try_acquire(now)
            .await
            .inspect_err(|error| warn!(%error, "leader election failed; not acting as leader"))
            .unwrap_or_default(),
    }
}

/// Run a leader-only mutating pass under two guards: skip it entirely when leadership was lost, and
/// abort it if it runs longer than `budget` (kept shorter than the lease, so an overrunning pass
/// can't keep patching past the point a peer could legitimately take over). Returns `true` only when
/// the pass ran to completion as leader within budget; `false` (skipped or timed out) tells the
/// caller to stop the remaining passes for this tick.
async fn guarded_mutate(
    leader: bool,
    budget: Duration,
    label: &str,
    pass: impl Future<Output = Result<()>>,
) -> bool {
    if !leader {
        warn!(pass = label, "not leader; skipping mutating pass");
        return false;
    }
    match tokio::time::timeout(budget, pass).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            warn!(pass = label, %error, "mutating pass failed");
            true
        }
        Err(_) => {
            warn!(pass = label, "mutating pass exceeded its time budget; aborting");
            false
        }
    }
}

fn hpa_apis(ctx: &Context) -> Vec<Api<HorizontalPodAutoscaler>> {
    if ctx.namespaces.is_empty() {
        vec![Api::all(ctx.client.clone())]
    } else {
        ctx.namespaces.iter().map(|ns| Api::namespaced(ctx.client.clone(), ns)).collect()
    }
}

fn scaled_object_apis(ctx: &Context) -> Vec<Api<DynamicObject>> {
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject"));
    if ctx.namespaces.is_empty() {
        vec![Api::all_with(ctx.client.clone(), &ar)]
    } else {
        ctx.namespaces.iter().map(|ns| Api::namespaced_with(ctx.client.clone(), ns, &ar)).collect()
    }
}

/// One workload that cleared rule detection and the existing-rec dedupe, queued for LLM judgement.
struct AnalysisItem {
    key: String,
    namespace: String,
    name: String,
    snapshot: MetricsSnapshot,
    candidates: Vec<Candidate>,
    target_kind: TargetKind,
}

pub async fn analysis_pass(
    ctx: &Context,
    history: &mut HashMap<String, Vec<MetricPoint>>,
    now: DateTime<Utc>,
) -> Result<()> {
    // Analysis always runs the same way regardless of mode. In recommend-only mode the operator
    // never mutates a workload, so each recommendation is annotated as advisory (a human applies
    // it). In apply mode approved recommendations are auto-applied by the apply pass.
    let apply_available = ctx.apply_enabled;
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<AnalysisItem> = Vec::new();
    for api in hpa_apis(ctx) {
        for hpa in api.list(&ListParams::default()).await?.items {
            let (Some(namespace), name) = (hpa.namespace(), hpa.name_any()) else {
                continue;
            };
            if is_keda_managed(&hpa) {
                continue; // the owning ScaledObject is analyzed instead (one rec per workload)
            }
            let key = format!("{namespace}/{name}");
            seen.insert(key.clone());
            let deployment = scale_target_name(&hpa).unwrap_or_else(|| name.clone());
            let fallback = current_point(&hpa, now);
            update_history(ctx, history, &key, &namespace, &deployment, fallback, now).await?;
            queue_analysis(
                ctx,
                history,
                &mut pending,
                key,
                namespace,
                name,
                workload_config(&hpa),
                TargetKind::HorizontalPodAutoscaler,
            )
            .await?;
        }
    }
    for api in scaled_object_apis(ctx) {
        let scaled_objects = match api.list(&ListParams::default()).await {
            Ok(list) => list.items,
            // The KEDA CRD is absent (no KEDA in this cluster) - nothing to analyze.
            Err(kube::Error::Api(error)) if error.code == 404 => continue,
            Err(error) => return Err(error.into()),
        };
        for scaled_object in scaled_objects {
            let (Some(namespace), name) = (scaled_object.namespace(), scaled_object.name_any())
            else {
                continue;
            };
            let key = format!("{namespace}/{name}");
            seen.insert(key.clone());
            let deployment =
                scaled_object_target_name(&scaled_object).unwrap_or_else(|| name.clone());
            let fallback = keda_hpa_point(ctx, &namespace, &name, now).await;
            update_history(ctx, history, &key, &namespace, &deployment, fallback, now).await?;
            queue_analysis(
                ctx,
                history,
                &mut pending,
                key,
                namespace,
                name,
                scaled_object_config(&scaled_object),
                TargetKind::ScaledObject,
            )
            .await?;
        }
    }
    // drop cached history for workloads that no longer exist so the map can't grow unbounded.
    // `seen` is already complete here, so a deferred or failed LLM phase below can't evict the
    // history of a workload that was merely not reached this tick.
    evict_absent(history, &seen);

    // Rule detection above is cheap; the LLM judgement is the slow part (~seconds per workload),
    // so it fans out under a bounded concurrency cap and a per-pass time budget. Workloads not
    // reached before the deadline are deferred to the next tick, mirroring the mutating passes'
    // deadline pattern.
    let deadline = mutation_deadline(now, ctx.analysis_budget);
    let total = pending.len();
    let outcomes: Vec<Result<bool>> = stream::iter(pending.into_iter().map(|item| async move {
        if Utc::now() >= deadline {
            return Ok(false);
        }
        analyze_and_recommend(ctx, &item, apply_available).await.map(|()| true)
    }))
    .buffer_unordered(ctx.llm_concurrency)
    .collect()
    .await;
    let deferred = outcomes.iter().filter(|outcome| matches!(outcome, Ok(false))).count();
    if deferred > 0 {
        warn!(
            pass = "analysis",
            deferred, total, "analysis budget reached; remaining workloads analyzed next tick"
        );
    }
    // Surface the first error only after every workload was attempted (or deferred), so one bad
    // LLM reply can't starve the rest of the fleet of recommendations.
    outcomes.into_iter().collect::<Result<Vec<_>>>()?;
    Ok(())
}

/// Run rules (+ forecast) over one workload's snapshot and queue it for LLM judgement, unless it
/// has no candidates or already has a recommendation (human decisions are left alone).
#[allow(clippy::too_many_arguments)]
async fn queue_analysis(
    ctx: &Context,
    history: &HashMap<String, Vec<MetricPoint>>,
    pending: &mut Vec<AnalysisItem>,
    key: String,
    namespace: String,
    name: String,
    config: WorkloadConfig,
    target_kind: TargetKind,
) -> Result<()> {
    let snapshot = MetricsSnapshot {
        config,
        points: history.get(&key).cloned().unwrap_or_default(),
        scaling_events: Vec::new(),
    };
    let mut candidates = run_rules(&snapshot);
    if let Some(params) = ctx.forecast_params.as_ref()
        && let Some(peak) = detect_predictable_peak(&snapshot, params)
    {
        candidates.push(peak);
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let recs: Api<ScalingRecommendation> = Api::namespaced(ctx.client.clone(), &namespace);
    if recs.get_opt(&name).await?.is_some() {
        return Ok(()); // leave existing recommendations (and human decisions) alone
    }
    pending.push(AnalysisItem { key, namespace, name, snapshot, candidates, target_kind });
    Ok(())
}

/// Fallback metric point for a ScaledObject in HPA-status mode: KEDA materializes a backing HPA
/// named `keda-hpa-<name>` whose status carries current replicas + CPU utilization. Returns `None`
/// with Prometheus (no fallback needed) or when that HPA is missing (e.g. KEDA not yet reconciled).
async fn keda_hpa_point(
    ctx: &Context,
    namespace: &str,
    name: &str,
    now: DateTime<Utc>,
) -> Option<MetricPoint> {
    if ctx.metrics.is_prometheus() {
        return None;
    }
    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(ctx.client.clone(), namespace);
    let hpa = hpas.get_opt(&format!("keda-hpa-{name}")).await.ok()??;
    current_point(&hpa, now)
}

/// Judge one workload's candidates with the LLM and create its `ScalingRecommendation`.
/// `apply_available` only controls the advisory note appended to the summary when the operator runs
/// in recommend-only mode.
async fn analyze_and_recommend(
    ctx: &Context,
    item: &AnalysisItem,
    apply_available: bool,
) -> Result<()> {
    let output = ctx
        .llm
        .analyze(
            &item.key,
            &item.snapshot.config,
            &item.candidates,
            ctx.cost_per_replica_usd_monthly,
        )
        .await?;
    let mut recommendation =
        synthesize(&item.candidates, &output, ctx.cost_per_replica_usd_monthly);
    if recommendation.config_diff.is_empty() && recommendation.schedule.is_none() {
        return Ok(());
    }
    if !apply_available {
        recommendation.summary_md.push_str(ADVISORY_SUFFIX);
    }
    let config_diff = recommendation
        .config_diff
        .iter()
        .map(|(field, change)| (field.clone(), DiffEntry { from: change.from, to: change.to }))
        .collect();
    let spec = ScalingRecommendationSpec {
        target_ref: TargetRef { kind: item.target_kind.clone(), name: item.name.clone() },
        approved: false,
        risk_level: recommendation.risk_level.as_str().to_owned(),
        summary_md: recommendation.summary_md,
        projected_savings_usd_monthly: recommendation.projected_savings_usd_monthly,
        config_diff,
        schedule: recommendation.schedule.map(into_schedule_specs),
    };
    let recs: Api<ScalingRecommendation> = Api::namespaced(ctx.client.clone(), &item.namespace);
    match recs.create(&PostParams::default(), &ScalingRecommendation::new(&item.name, spec)).await {
        Ok(_) => info!(workload = %item.key, "created ScalingRecommendation"),
        // Another replica created it first this tick; benign under leader-less analysis.
        Err(kube::Error::Api(error)) if error.code == 409 => {
            info!(workload = %item.key, "ScalingRecommendation already created by another replica");
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Refresh the metric history cache for one workload. With Prometheus, append the latest samples
/// (backfilling the full window on first sight, so a restart recovers history immediately); without
/// it, accumulate the single `fallback` status point per tick (the HPA's own status, or the
/// KEDA-managed HPA's for a ScaledObject).
async fn update_history(
    ctx: &Context,
    history: &mut HashMap<String, Vec<MetricPoint>>,
    key: &str,
    namespace: &str,
    deployment: &str,
    fallback: Option<MetricPoint>,
    now: DateTime<Utc>,
) -> Result<()> {
    match &ctx.metrics {
        MetricsSource::Prometheus(_) => {
            let since = history
                .get(key)
                .and_then(|cache| cache.last())
                .map_or(now - ctx.metrics_window, |point| point.timestamp);
            let fetched =
                ctx.metrics.history(namespace, deployment, since, ctx.metrics_step).await?;
            // surface any empty fetch - both a workload that never had data (no CPU
            // requests/limits, so the default query's denominator is absent) AND one that had
            // history and then lost all series mid-stream. Not gated on prior history so a
            // mid-stream loss is observable rather than silently stalling.
            if fetched.is_empty() {
                warn!(workload = %key, "prometheus returned no points; check CPU requests/limits or PromQL templates");
            }
            let cache = history.entry(key.to_owned()).or_default();
            for point in fetched {
                if cache.last().is_none_or(|last| point.timestamp > last.timestamp) {
                    cache.push(point);
                }
            }
            trim(cache);
        }
        MetricsSource::HpaStatus => {
            let Some(point) = fallback else {
                return Ok(());
            };
            let cache = history.entry(key.to_owned()).or_default();
            cache.push(point);
            trim(cache);
        }
    }
    Ok(())
}

fn trim(points: &mut Vec<MetricPoint>) {
    if points.len() > HISTORY_CAP {
        let overflow = points.len() - HISTORY_CAP;
        points.drain(0..overflow);
    }
}

pub async fn apply_pass(ctx: &Context, now: DateTime<Utc>) -> Result<()> {
    if !ctx.apply_enabled {
        return Ok(()); // recommend-only mode: never mutate a workload, even an approved CR
    }
    let deadline = mutation_deadline(now, ctx.interval);
    let recs: Api<ScalingRecommendation> = Api::all(ctx.client.clone());
    let items = recs.list(&ListParams::default()).await?.items;
    let total = items.len();
    for (index, rec) in items.into_iter().enumerate() {
        if Utc::now() >= deadline {
            warn!(
                pass = "apply",
                skipped = total - index,
                total,
                "mutation deadline reached; remaining recommendations retried next tick"
            );
            break;
        }
        let phase = rec.status.as_ref().and_then(|s| s.phase.as_deref());
        if !rec.spec.approved
            || matches!(phase, Some("applied" | "verified" | "rolledBack" | "degraded"))
        {
            continue;
        }
        let Some(namespace) = rec.namespace() else { continue };
        let name = rec.name_any();
        let recs_ns: Api<ScalingRecommendation> = Api::namespaced(ctx.client.clone(), &namespace);

        let TargetRef { kind, name: target } = rec.spec.target_ref;
        let patch = patch_for(&kind, &rec.spec.config_diff);
        if let Err(error) = apply_patch(&ctx.client, &kind, &namespace, &target, &patch).await {
            warn!(workload = %format!("{namespace}/{name}"), %error, "apply failed");
            patch_status(
                &recs_ns,
                &name,
                json!({"phase": Phase::Failed.as_str(), "detail": format!("apply failed: {error}")}),
            )
            .await?;
            continue;
        }
        // HPA applies go on probation for the verify pass. ScaledObject health verification isn't
        // supported yet (no live-config read), so mark it verified now rather than leave it dangling
        // in `applied` forever.
        let status = match kind {
            TargetKind::HorizontalPodAutoscaler => json!({
                "phase": Phase::Applied.as_str(),
                "appliedAt": now.to_rfc3339(),
                "probationUntil": (now + ctx.probation_window).to_rfc3339(),
                "detail": "patched HPA",
            }),
            TargetKind::ScaledObject => json!({
                "phase": Phase::Verified.as_str(),
                "appliedAt": now.to_rfc3339(),
                "detail": "patched ScaledObject (health verification not supported)",
            }),
        };
        patch_status(&recs_ns, &name, status).await?;
        info!(workload = %format!("{namespace}/{name}"), "applied recommendation");
    }
    Ok(())
}

/// After the probation window, judge the workload's health and either mark `verified` or auto-revert
/// to the pre-apply config (`rolledBack`). Rollback runs regardless of operating mode - it only ever
/// restores config the operator itself set. Only HPA targets are verified today (reading live
/// ScaledObject config is a follow-up).
///
/// Deliberately NOT gated on `apply_enabled`: unlike the apply/schedule passes (which patch *new*
/// config and are skipped in recommend-only mode), this pass only ever *restores* a change the
/// operator itself previously applied - a safety revert, never a new mutation. So if a deploy is
/// switched apply -> recommend with a probationary change still pending, the auto-rollback safety net
/// still fires. (In a fresh recommend-only deploy nothing ever reaches `applied`, so this is inert.)
pub async fn verify_pass(
    ctx: &Context,
    history: &HashMap<String, Vec<MetricPoint>>,
    now: DateTime<Utc>,
) -> Result<()> {
    let recs: Api<ScalingRecommendation> = Api::all(ctx.client.clone());
    for rec in recs.list(&ListParams::default()).await?.items {
        let status = rec.status.as_ref();
        // Re-judge both freshly-applied changes and ones held `degraded` (rollback was off) so a
        // recovered workload reaches `verified` and a still-degraded one can be reverted later.
        if !matches!(status.and_then(|s| s.phase.as_deref()), Some("applied" | "degraded")) {
            continue;
        }
        if rec.spec.target_ref.kind != TargetKind::HorizontalPodAutoscaler {
            continue; // ScaledObjects are marked verified at apply time (no live-config read yet)
        }
        let Some(probation_until) = status.and_then(|s| s.probation_until.as_deref()) else {
            continue;
        };
        let Some(applied_at) = status.and_then(|s| s.applied_at.as_deref()) else { continue };
        let (Ok(probation_until), Ok(applied_at)) =
            (parse_rfc3339(probation_until), parse_rfc3339(applied_at))
        else {
            continue;
        };
        if now < probation_until {
            continue; // still on probation
        }
        let Some(namespace) = rec.namespace() else { continue };
        let name = rec.name_any();
        let target = rec.spec.target_ref.name.clone();
        let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(ctx.client.clone(), &namespace);
        let Some(hpa) = hpas.get_opt(&target).await? else { continue };
        let config = workload_config(&hpa);
        let key = format!("{namespace}/{name}");
        // the apply-tick sample (timestamp == applied_at) reflects pre-apply state, so it belongs
        // in `before`; only strictly-later samples count as post-apply.
        let (before, after) =
            split_before_after(history.get(&key).cloned().unwrap_or_default(), applied_at);
        let recs_ns: Api<ScalingRecommendation> = Api::namespaced(ctx.client.clone(), &namespace);
        let verdict = evaluate_health(&before, &after, &config, ctx.health_cpu_margin);
        match decide_verify(verdict, ctx.rollback_enabled) {
            VerifyAction::Verify => {
                patch_status(&recs_ns, &name, json!({"phase": Phase::Verified.as_str(), "detail": "healthy after probation", "probationUntil": Value::Null})).await?;
                info!(workload = %key, "verified recommendation");
            }
            VerifyAction::Extend => {
                // too few post-apply samples to judge - stay on probation, don't rubber-stamp.
                patch_status(&recs_ns, &name, json!({"phase": Phase::Applied.as_str(), "detail": "awaiting post-apply data", "probationUntil": (now + ctx.probation_window).to_rfc3339()})).await?;
            }
            VerifyAction::HoldDegraded(reason) => {
                // keep probation set so the rec is re-judged each cycle (and reverted once
                // rollback is re-enabled) instead of freezing as a healthy-looking `applied`.
                warn!(workload = %key, %reason, "degraded but rollback disabled; holding for re-judgement");
                patch_status(&recs_ns, &name, json!({"phase": Phase::Degraded.as_str(), "detail": reason, "probationUntil": (now + ctx.probation_window).to_rfc3339()})).await?;
            }
            VerifyAction::Revert { reason } => {
                // the schedule owns min_replicas during its window - revert the other fields only
                // so a rollback can't stomp the active proactive floor mid-peak.
                let diff = revert_diff(&rec.spec.config_diff, rec.spec.schedule.is_some());
                if !diff.is_empty() {
                    let revert = revert_patch_for(&rec.spec.target_ref.kind, &diff);
                    apply_patch(
                        &ctx.client,
                        &rec.spec.target_ref.kind,
                        &namespace,
                        &target,
                        &revert,
                    )
                    .await?;
                }
                patch_status(&recs_ns, &name, json!({"phase": Phase::RolledBack.as_str(), "detail": reason, "probationUntil": Value::Null})).await?;
                warn!(workload = %key, "auto-reverted degraded recommendation");
            }
        }
    }
    Ok(())
}

/// Drive proactive schedules (leader-only, apply-mode only). For KEDA targets, install/refresh the
/// cron triggers; for HPA targets the operator itself raises `minReplicas` inside a window and
/// restores the baseline outside it, retracting a schedule whose forecasted peak never materializes.
pub async fn schedule_pass(
    ctx: &Context,
    history: &HashMap<String, Vec<MetricPoint>>,
    misses: &mut HashMap<String, u32>,
    now: DateTime<Utc>,
) -> Result<()> {
    if !ctx.apply_enabled {
        return Ok(()); // recommend-only mode: never drive schedules (they patch minReplicas)
    }
    let deadline = mutation_deadline(now, ctx.interval);
    let recs: Api<ScalingRecommendation> = Api::all(ctx.client.clone());
    let freshness = chrono::Duration::from_std(ctx.metrics_step.max(ctx.interval) * 3)
        .unwrap_or_else(|_| chrono::Duration::minutes(15));
    let mut seen: HashSet<String> = HashSet::new();
    let items = recs.list(&ListParams::default()).await?.items;
    let (total, mut completed) = (items.len(), true);
    for (index, rec) in items.into_iter().enumerate() {
        if Utc::now() >= deadline {
            warn!(
                pass = "schedule",
                skipped = total - index,
                total,
                "mutation deadline reached; remaining schedules driven next tick"
            );
            completed = false;
            break;
        }
        let Some(windows) = rec.spec.schedule.clone() else { continue };
        let status = rec.status.as_ref();
        let phase = status.and_then(|s| s.phase.as_deref());
        // `degraded` recs are still applied (held for re-judgement); `rolledBack` scheduled recs need
        // their floor restored to baseline at window end. Both keep being driven here.
        if !rec.spec.approved
            || !matches!(phase, Some("applied" | "verified" | "degraded" | "rolledBack"))
        {
            continue;
        }
        if status.and_then(|s| s.schedule_active) == Some(false) {
            continue; // retracted - leave it restored to baseline
        }
        let Some(namespace) = rec.namespace() else { continue };
        let name = rec.name_any();
        let key = format!("{namespace}/{name}");
        seen.insert(key.clone());
        let target = rec.spec.target_ref.name.clone();
        if rec.spec.target_ref.kind == TargetKind::ScaledObject {
            // KEDA's cron scaler self-manages the time-based scaling; auto-retraction stays HPA-only
            // until reading live ScaledObject config lands (a follow-up).
            reconcile_keda_schedule(&ctx.client, &namespace, &target, &windows).await?;
            continue;
        }

        let recs_ns: Api<ScalingRecommendation> = Api::namespaced(ctx.client.clone(), &namespace);
        let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(ctx.client.clone(), &namespace);
        let Some(hpa) = hpas.get_opt(&target).await? else { continue };
        let config = workload_config(&hpa);
        // The off-peak floor to restore comes from the recommendation, never the live min (which is
        // the raised floor while inside a window) - otherwise the floor would ratchet upward.
        let Some(baseline) = rec.spec.config_diff.get("min_replicas").map(|d| d.to) else {
            warn!(workload = %key, "schedule present without a min_replicas baseline; skipping");
            continue;
        };
        let active = active_window_min(&windows, now);

        if phase == Some("rolledBack") {
            // a scalar rollback must not orphan the schedule's floor. Hold the raised floor
            // through an active window (never stomp it mid-peak), then restore baseline and retire
            // the schedule once the window ends - so minReplicas returns to baseline, not pinned high.
            match floor_action(true, active, baseline, config.min_replicas) {
                FloorAction::Set(min) => {
                    apply_patch(
                        &ctx.client,
                        &TargetKind::HorizontalPodAutoscaler,
                        &namespace,
                        &target,
                        &json!({"spec": {"minReplicas": min}}),
                    )
                    .await?;
                }
                FloorAction::Retire(min) => {
                    if config.min_replicas != min {
                        apply_patch(
                            &ctx.client,
                            &TargetKind::HorizontalPodAutoscaler,
                            &namespace,
                            &target,
                            &json!({"spec": {"minReplicas": min}}),
                        )
                        .await?;
                    }
                    patch_status(&recs_ns, &name, json!({"scheduleActive": false, "detail": "schedule retired: baseline restored after rollback"})).await?;
                    info!(workload = %key, "restored baseline and retired schedule after rollback");
                }
                FloorAction::Keep => {}
            }
            continue;
        }

        // Only fresh idle samples count toward retraction; a busy or stale sample resets the streak.
        if active.is_some() {
            let idle = window_idle(history.get(&key), &config, now, freshness);
            match next_miss_count(misses.get(&key).copied().unwrap_or(0), idle) {
                Some(count) if count >= RETRACT_AFTER_IDLE_SAMPLES => {
                    apply_patch(
                        &ctx.client,
                        &TargetKind::HorizontalPodAutoscaler,
                        &namespace,
                        &target,
                        &json!({"spec": {"minReplicas": baseline}}),
                    )
                    .await?;
                    patch_status(&recs_ns, &name, json!({"scheduleActive": false, "detail": "schedule retracted: forecasted peak not materializing"})).await?;
                    misses.remove(&key);
                    warn!(workload = %key, "retracted proactive schedule");
                    continue;
                }
                Some(count) => {
                    misses.insert(key.clone(), count);
                }
                None => {
                    misses.remove(&key);
                }
            }
        } else {
            misses.remove(&key);
        }

        if let FloorAction::Set(min) = floor_action(false, active, baseline, config.min_replicas) {
            apply_patch(
                &ctx.client,
                &TargetKind::HorizontalPodAutoscaler,
                &namespace,
                &target,
                &json!({"spec": {"minReplicas": min}}),
            )
            .await?;
            info!(workload = %key, min, "schedule adjusted minReplicas");
        }
        if status.and_then(|s| s.schedule_active).is_none() {
            patch_status(&recs_ns, &name, json!({"scheduleActive": true})).await?;
        }
    }
    // drop miss counters for recs that no longer exist or are no longer being driven - only after a
    // complete pass, so a deadline-truncated pass doesn't reset the streaks of recs it never reached.
    if completed {
        evict_absent(misses, &seen);
    }
    Ok(())
}

fn into_schedule_specs(windows: ScalingSchedule) -> Vec<ScheduleWindowSpec> {
    windows
        .into_iter()
        .map(|window| ScheduleWindowSpec {
            start_cron: window.start_cron,
            duration_minutes: window.duration_minutes,
            min_replicas: window.min_replicas,
        })
        .collect()
}

/// The highest floor any window active at `now` demands, or `None` when outside every window.
fn active_window_min(windows: &[ScheduleWindowSpec], now: DateTime<Utc>) -> Option<i32> {
    windows
        .iter()
        .filter(|window| cron_window_active(&window.start_cron, window.duration_minutes, now))
        .map(|window| window.min_replicas)
        .max()
}

/// Whether the latest sample shows a pre-scaled floor running idle. `None` when there is no
/// sufficiently fresh sample, so a metrics gap is treated as "unknown" rather than idle.
fn window_idle(
    history: Option<&Vec<MetricPoint>>,
    config: &WorkloadConfig,
    now: DateTime<Utc>,
    max_age: chrono::Duration,
) -> Option<bool> {
    let point = history.and_then(|points| points.last())?;
    if now - point.timestamp > max_age {
        return None;
    }
    let ceiling = f64::from(config.target_cpu_pct) / 100.0 * GHOST_UTIL_RATIO;
    Some(point.cpu_util < ceiling)
}

/// What to do with an HPA's `minReplicas` for a scheduled rec this tick.
#[derive(Debug, PartialEq, Eq)]
enum FloorAction {
    Set(i32),
    /// Restore the baseline and retire the schedule (stop managing it).
    Retire(i32),
    Keep,
}

/// Pure floor decision. A live rec (`rolled_back=false`) raises to the active window floor and
/// restores baseline outside any window. A `rolled_back` rec holds its raised floor through an active
/// window (never stomped mid-peak) and, once the window ends, retires to baseline so `minReplicas`
/// is never pinned high forever.
fn floor_action(
    rolled_back: bool,
    active: Option<i32>,
    baseline: i32,
    current_min: i32,
) -> FloorAction {
    if rolled_back {
        return match active {
            Some(window) if current_min != window => FloorAction::Set(window),
            Some(_) => FloorAction::Keep,
            None => FloorAction::Retire(baseline),
        };
    }
    let target = active.unwrap_or(baseline);
    if current_min == target { FloorAction::Keep } else { FloorAction::Set(target) }
}

/// Next consecutive-idle miss count, or `None` to reset the streak. Only a fresh idle sample
/// (`Some(true)`) extends it; a busy reading or a stale/missing sample (`Some(false)` / `None`)
/// resets it, so flaky metrics during a real peak can't accumulate into a retraction.
fn next_miss_count(current: u32, idle: Option<bool>) -> Option<u32> {
    match idle {
        Some(true) => Some(current + 1),
        Some(false) | None => None,
    }
}

/// Drop map entries whose key was not observed this tick, so per-workload state can't grow unbounded
/// as workloads/recommendations are deleted.
fn evict_absent<V>(map: &mut HashMap<String, V>, present: &HashSet<String>) {
    map.retain(|key, _| present.contains(key));
}

/// The instant a mutating pass must stop starting new work, so a single pass over thousands of
/// workloads can't overrun the lease. The budget is one reconcile interval (half the lease); the
/// per-pass timeout in `guarded_mutate` is the backstop for a single mutation that hangs past it.
fn mutation_deadline(started: DateTime<Utc>, budget: Duration) -> DateTime<Utc> {
    started + chrono::Duration::from_std(budget).unwrap_or_else(|_| chrono::Duration::minutes(5))
}

/// The reconcile sleep with a small deterministic jitter (0..10% of the interval) added, so multiple
/// replicas don't reconcile and renew their leases in lockstep and stampede the API server. The
/// jitter is derived from the wall clock, so it needs no RNG dependency.
fn jittered_sleep(base: Duration, now: DateTime<Utc>) -> Duration {
    let span = base.as_millis() as u64 / 10;
    if span == 0 {
        return base;
    }
    base + Duration::from_millis(u64::from(now.timestamp_subsec_nanos()) % (span + 1))
}

async fn reconcile_keda_schedule(
    client: &Client,
    namespace: &str,
    name: &str,
    windows: &[ScheduleWindowSpec],
) -> Result<()> {
    let gvk = GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject");
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &ar);
    let Some(scaled_object) = api.get_opt(name).await? else { return Ok(()) };
    let existing = scaled_object
        .data
        .get("spec")
        .and_then(|spec| spec.get("triggers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !keda_schedule_in_sync(&existing, windows) {
        let patch = schedule_patch_for_keda(&existing, windows);
        api.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
        info!(workload = %format!("{namespace}/{name}"), "applied KEDA schedule triggers");
    }
    Ok(())
}

fn parse_rfc3339(value: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(value).map(|dt| dt.with_timezone(&Utc))
}

/// What the verify pass should do with a probationary recommendation - an explicit transition so the
/// phase machine can't drift into contradictory states.
enum VerifyAction {
    Verify,
    Extend,
    HoldDegraded(String),
    Revert { reason: String },
}

/// Pure verify-pass decision: maps a health verdict + rollback policy to the next action.
fn decide_verify(verdict: HealthVerdict, rollback_enabled: bool) -> VerifyAction {
    match verdict {
        HealthVerdict::Healthy => VerifyAction::Verify,
        HealthVerdict::Inconclusive => VerifyAction::Extend,
        HealthVerdict::Degraded(reason) if rollback_enabled => VerifyAction::Revert { reason },
        HealthVerdict::Degraded(reason) => VerifyAction::HoldDegraded(reason),
    }
}

/// The config diff to revert, excluding `min_replicas` when the rec carries a schedule (the schedule
/// owns the floor during its window).
fn revert_diff(
    config_diff: &BTreeMap<String, DiffEntry>,
    has_schedule: bool,
) -> BTreeMap<String, DiffEntry> {
    config_diff
        .iter()
        .filter(|(field, _)| !(has_schedule && field.as_str() == "min_replicas"))
        .map(|(field, entry)| (field.clone(), *entry))
        .collect()
}

/// Split cached history into pre-apply (`<= applied_at`) and post-apply (`> applied_at`) windows.
/// The apply-instant sample reflects the old state, so it belongs in `before`.
fn split_before_after(
    points: Vec<MetricPoint>,
    applied_at: DateTime<Utc>,
) -> (Vec<MetricPoint>, Vec<MetricPoint>) {
    points.into_iter().partition(|point| point.timestamp <= applied_at)
}

/// Merge-patch an HPA (typed) or a KEDA ScaledObject (dynamic client - KEDA has no `k8s-openapi`
/// type). Used by both the apply pass and the rollback path.
async fn apply_patch(
    client: &Client,
    kind: &TargetKind,
    namespace: &str,
    name: &str,
    patch: &Value,
) -> Result<()> {
    let params = PatchParams::default();
    match kind {
        TargetKind::HorizontalPodAutoscaler => {
            let api: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
            api.patch(name, &params, &Patch::Merge(patch)).await?;
        }
        TargetKind::ScaledObject => {
            let gvk = GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject");
            let ar = ApiResource::from_gvk(&gvk);
            let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &ar);
            api.patch(name, &params, &Patch::Merge(patch)).await?;
        }
    }
    Ok(())
}

async fn patch_status(recs: &Api<ScalingRecommendation>, name: &str, status: Value) -> Result<()> {
    recs.patch_status(name, &PatchParams::default(), &Patch::Merge(&json!({"status": status})))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn window(cron: &str, duration_minutes: i32, min_replicas: i32) -> ScheduleWindowSpec {
        ScheduleWindowSpec { start_cron: cron.to_owned(), duration_minutes, min_replicas }
    }

    fn config() -> WorkloadConfig {
        WorkloadConfig {
            min_replicas: 2,
            max_replicas: 20,
            target_cpu_pct: 70,
            scale_down_cooldown_s: 300,
        }
    }

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 7, hour, minute, 0).unwrap() // a Wednesday
    }

    fn point(timestamp: DateTime<Utc>, cpu_util: f64) -> MetricPoint {
        MetricPoint { timestamp, cpu_util, replicas: 8, queue_depth: None }
    }

    /// A client whose API server is unreachable. Construction does NOT connect (kube connects
    /// lazily), so a pass that early-returns before touching the API succeeds, while one that does
    /// reach the API fails fast - which is exactly what the recommend-mode gate tests rely on.
    fn offline_client() -> Client {
        let config = kube::Config::new("http://127.0.0.1:1".parse().expect("uri"));
        Client::try_from(config).expect("offline client builds without connecting")
    }

    fn test_ctx(client: Client, apply_enabled: bool) -> Context {
        Context {
            client,
            llm: LlmBackend::RulesOnly,
            metrics: MetricsSource::HpaStatus,
            namespaces: vec![],
            interval: std::time::Duration::from_secs(300),
            metrics_window: chrono::Duration::days(14),
            metrics_step: std::time::Duration::from_secs(300),
            probation_window: chrono::Duration::minutes(45),
            rollback_enabled: true,
            health_cpu_margin: 0.15,
            apply_enabled,
            forecast_params: None,
            llm_concurrency: 4,
            analysis_budget: std::time::Duration::from_secs(300),
            cost_per_replica_usd_monthly: 30.0,
            elector: None,
        }
    }

    #[test]
    fn active_window_min_picks_highest_active_floor() {
        let windows = vec![window("0 9 * * *", 120, 5), window("30 9 * * *", 60, 8)];
        assert_eq!(active_window_min(&windows, at(9, 45)), Some(8)); // inside both
        assert_eq!(active_window_min(&windows, at(9, 15)), Some(5)); // inside the first only
        assert_eq!(active_window_min(&windows, at(12, 0)), None); // outside all
    }

    #[test]
    fn window_idle_requires_a_fresh_sample() {
        let now = at(9, 30);
        let max_age = Duration::minutes(15);
        let idle = vec![point(now - Duration::minutes(2), 0.10)];
        let busy = vec![point(now - Duration::minutes(2), 0.60)];
        let stale = vec![point(now - Duration::hours(2), 0.10)];
        assert_eq!(window_idle(Some(&idle), &config(), now, max_age), Some(true));
        assert_eq!(window_idle(Some(&busy), &config(), now, max_age), Some(false));
        assert_eq!(window_idle(Some(&stale), &config(), now, max_age), None);
        assert_eq!(window_idle(None, &config(), now, max_age), None);
    }

    #[test]
    fn into_schedule_specs_maps_fields() {
        let specs = into_schedule_specs(vec![openhpa_core::ScheduleWindow {
            start_cron: "0 9 * * *".to_owned(),
            duration_minutes: 480,
            min_replicas: 8,
        }]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].start_cron, "0 9 * * *");
        assert_eq!(specs[0].min_replicas, 8);
    }

    fn diff(pairs: &[(&str, i32, i32)]) -> BTreeMap<String, DiffEntry> {
        pairs
            .iter()
            .map(|(k, from, to)| ((*k).to_owned(), DiffEntry { from: *from, to: *to }))
            .collect()
    }

    #[test]
    fn decide_verify_maps_verdicts_to_actions() {
        assert!(matches!(decide_verify(HealthVerdict::Healthy, true), VerifyAction::Verify));
        assert!(matches!(decide_verify(HealthVerdict::Inconclusive, true), VerifyAction::Extend));
        assert!(matches!(
            decide_verify(HealthVerdict::Degraded("x".to_owned()), true),
            VerifyAction::Revert { .. }
        ));
        // rollback disabled holds for re-judgement instead of freezing.
        assert!(matches!(
            decide_verify(HealthVerdict::Degraded("x".to_owned()), false),
            VerifyAction::HoldDegraded(_)
        ));
    }

    #[test]
    fn revert_diff_keeps_schedule_owned_min_replicas() {
        let config_diff = diff(&[("min_replicas", 10, 3), ("target_cpu_pct", 70, 85)]);
        // with a schedule, the revert must not touch min_replicas (the schedule owns the floor).
        let scheduled = revert_diff(&config_diff, true);
        assert!(!scheduled.contains_key("min_replicas"));
        assert!(scheduled.contains_key("target_cpu_pct"));
        // Without a schedule, every field reverts.
        assert_eq!(revert_diff(&config_diff, false).len(), 2);
    }

    #[test]
    fn split_excludes_apply_instant_from_after() {
        let applied_at = at(10, 0);
        let points = vec![
            point(at(9, 55), 0.5), // before
            point(at(10, 0), 0.5), // apply instant -> before
            point(at(10, 5), 0.5), // after
        ];
        let (before, after) = split_before_after(points, applied_at);
        assert_eq!(before.len(), 2);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].timestamp, at(10, 5));
    }

    #[test]
    fn floor_action_restores_baseline_after_rolled_back_window() {
        // Rolled-back rec, inside its window: hold the raised floor (raise to it, or keep if already).
        assert_eq!(floor_action(true, Some(8), 3, 3), FloorAction::Set(8));
        assert_eq!(floor_action(true, Some(8), 3, 8), FloorAction::Keep);
        // Rolled-back rec, window over: retire to baseline (so the floor isn't pinned high forever).
        assert_eq!(floor_action(true, None, 3, 8), FloorAction::Retire(3));
        // Live rec: raise inside the window, restore baseline outside, no-op when already correct.
        assert_eq!(floor_action(false, Some(8), 3, 3), FloorAction::Set(8));
        assert_eq!(floor_action(false, None, 3, 8), FloorAction::Set(3));
        assert_eq!(floor_action(false, None, 3, 3), FloorAction::Keep);
    }

    #[test]
    fn miss_count_only_grows_on_consecutive_fresh_idle() {
        assert_eq!(next_miss_count(2, Some(true)), Some(3)); // fresh idle extends the streak
        assert_eq!(next_miss_count(5, Some(false)), None); // busy resets
        assert_eq!(next_miss_count(5, None), None); // stale/missing also resets (no silent accrual)
    }

    #[test]
    fn evict_absent_drops_unseen_keys() {
        let mut map = HashMap::from([("ns/live".to_owned(), 1u32), ("ns/dead".to_owned(), 9)]);
        let present = HashSet::from(["ns/live".to_owned()]);
        evict_absent(&mut map, &present);
        assert_eq!(map.keys().collect::<Vec<_>>(), vec![&"ns/live".to_owned()]);
    }

    #[test]
    fn jittered_sleep_stays_within_ten_percent() {
        let base = std::time::Duration::from_secs(300);
        for nanos in [0u32, 123_456_789, 999_999_999] {
            let now = DateTime::from_timestamp(1_700_000_000, nanos).unwrap();
            let slept = jittered_sleep(base, now);
            assert!(
                slept >= base && slept <= base + std::time::Duration::from_secs(30),
                "{slept:?}"
            );
        }
        let zero = std::time::Duration::ZERO;
        assert_eq!(jittered_sleep(zero, DateTime::from_timestamp(0, 0).unwrap()), zero);
    }

    #[test]
    fn mutation_deadline_is_start_plus_budget() {
        let started = at(10, 0);
        assert_eq!(
            mutation_deadline(started, std::time::Duration::from_secs(300)),
            started + Duration::seconds(300)
        );
    }

    #[tokio::test]
    async fn guarded_mutate_skips_the_pass_when_not_leader() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let ran = AtomicBool::new(false);
        let proceed = guarded_mutate(false, std::time::Duration::from_secs(1), "test", async {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(!proceed, "a lost lease must stop the remaining passes");
        assert!(!ran.load(Ordering::SeqCst), "the pass body must not run when not leader");

        let proceed = guarded_mutate(true, std::time::Duration::from_secs(1), "test", async {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(proceed && ran.load(Ordering::SeqCst), "as leader the pass runs to completion");
    }

    #[tokio::test]
    async fn guarded_mutate_aborts_a_pass_over_its_budget() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // A 50ms budget against a pass that would block for 5s: the timeout fires (~50ms) and the
        // inner future is cancelled, so the pass never completes.
        let finished = AtomicBool::new(false);
        let proceed = guarded_mutate(true, std::time::Duration::from_millis(50), "test", async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            finished.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(!proceed, "a pass over budget aborts and stops the remaining passes");
        assert!(!finished.load(Ordering::SeqCst), "the over-budget pass must not complete");
    }

    #[tokio::test]
    async fn recommend_mode_apply_and_schedule_passes_never_touch_the_api() {
        // apply_enabled=false must early-return before any API call. The client is unreachable, so if
        // the gate regresses the pass would try to list and error - failing these unwraps. Runs in
        // plain `cargo test` (no cluster), unlike the kind-only no-apply e2e.
        let ctx = test_ctx(offline_client(), false);
        let now = Utc::now();
        apply_pass(&ctx, now).await.unwrap();
        schedule_pass(&ctx, &HashMap::new(), &mut HashMap::new(), now).await.unwrap();
    }

    #[tokio::test]
    async fn apply_mode_reaches_the_api_without_any_license() {
        // Counterpart: with apply enabled the gate does NOT short-circuit, so the pass reaches the
        // unreachable API and errors - proving the no-op above is the gate, not a dead client. OpenHPA
        // has no licensing: `--mode=apply` is the ONLY thing that enables mutation, so this reaching
        // the API is the regression guard that apply works with no license present.
        let ctx = test_ctx(offline_client(), true);
        apply_pass(&ctx, Utc::now()).await.unwrap_err();
    }
}
