use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::Result;
use chrono::{TimeZone, Timelike, Utc};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::conditions;
use kube::runtime::wait::await_condition;
use kube::{Api, Client, CustomResourceExt};
use openhpa_core::MetricPoint;
use openhpa_core::forecast::ForecastParams;
use openhpa_operator::controller::{
    Context, analysis_pass, apply_pass, schedule_pass, verify_pass,
};
use openhpa_operator::crd::{
    DiffEntry, ScalingRecommendation, ScalingRecommendationSpec, ScheduleWindowSpec, TargetKind,
    TargetRef,
};
use openhpa_operator::llm::LlmBackend;
use openhpa_operator::metrics::MetricsSource;
use serde_json::{from_value, json};

const CRD_NAME: &str = "scalingrecommendations.openhpa.dev";

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn analysis_emits_recommendation_for_idle_workload() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-analysis";
    setup(&client, namespace, "web", 10, 40, 70).await?;
    let ctx = ctx(client.clone(), vec![namespace.to_owned()]);

    // Pre-seed a multi-hour idle window so the rule engine has real history to judge.
    let now = Utc::now();
    let key = format!("{namespace}/web");
    let points: Vec<MetricPoint> = (0..7)
        .map(|h| MetricPoint {
            timestamp: now - chrono::Duration::hours(6 - h),
            cpu_util: 0.05,
            replicas: 2,
            queue_depth: None,
        })
        .collect();
    let mut history = HashMap::from([(key, points)]);
    analysis_pass(&ctx, &mut history, now).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let rec = recs.get("web").await?;
    // Headroom floor: ceil(2 replicas * 0.05 p95 * 1.5 / 0.70 target) = 1, so min 10 -> 1.
    assert_eq!(rec.spec.config_diff.get("min_replicas").map(|d| d.to), Some(1));
    assert!(!rec.spec.approved);

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn apply_patches_approved_hpa_and_starts_probation() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-apply";
    setup(&client, namespace, "api", 10, 40, 70).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let mut spec = recommendation_spec("api", &[("min_replicas", 10, 3)]);
    spec.approved = true;
    let _ = recs.delete("api", &DeleteParams::default()).await;
    recs.create(&PostParams::default(), &ScalingRecommendation::new("api", spec)).await?;

    let ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    apply_pass(&ctx, Utc::now()).await?;

    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    let hpa = hpas.get("api").await?;
    assert_eq!(hpa.spec.and_then(|s| s.min_replicas), Some(3));

    let rec = recs.get("api").await?;
    let status = rec.status.expect("status set");
    assert_eq!(status.phase.as_deref(), Some("applied"));
    assert!(status.probation_until.is_some(), "probation window started");

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn recommend_only_mode_never_applies_an_approved_recommendation() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-recommend-only";
    setup(&client, namespace, "api", 10, 40, 70).await?;

    // An approved recommendation that *would* lower the floor to 3 if apply were enabled.
    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let mut spec = recommendation_spec("api", &[("min_replicas", 10, 3)]);
    spec.approved = true;
    let _ = recs.delete("api", &DeleteParams::default()).await;
    recs.create(&PostParams::default(), &ScalingRecommendation::new("api", spec)).await?;

    let mut ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    ctx.apply_enabled = false; // recommend-only: the apply pass must be a no-op
    apply_pass(&ctx, Utc::now()).await?;

    // The HPA is untouched and the rec never reaches "applied", even though it is approved.
    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    assert_eq!(
        hpas.get("api").await?.spec.and_then(|s| s.min_replicas),
        Some(10),
        "recommend-only must not patch the HPA floor"
    );
    let phase = recs.get("api").await?.status.and_then(|s| s.phase);
    assert!(phase.is_none(), "recommend-only must not set an apply phase, got {phase:?}");

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn apply_pass_stops_at_the_mutation_deadline() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-apply-deadline";
    let names = ["d1", "d2", "d3"];
    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    for name in names {
        setup(&client, namespace, name, 10, 40, 70).await?;
        let mut spec = recommendation_spec(name, &[("min_replicas", 10, 3)]);
        spec.approved = true;
        let _ = recs.delete(name, &DeleteParams::default()).await;
        recs.create(&PostParams::default(), &ScalingRecommendation::new(name, spec)).await?;
    }

    // interval = 0 ⇒ the per-pass deadline (now + interval) is already reached, so the loop must stop
    // before starting any mutation. With the deadline check removed it would patch all three floors.
    let mut ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    ctx.interval = Duration::from_secs(0);
    apply_pass(&ctx, Utc::now()).await?;

    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    for name in names {
        assert_eq!(
            hpas.get(name).await?.spec.and_then(|s| s.min_replicas),
            Some(10),
            "{name} floor must be untouched once the deadline is reached"
        );
        assert!(
            recs.get(name).await?.status.and_then(|s| s.phase).is_none(),
            "{name} must not be applied past the deadline"
        );
    }

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn schedule_pass_stops_at_the_mutation_deadline() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-schedule-deadline";
    // HPA at the off-peak baseline (3); the active window below wants to raise it to 8.
    setup(&client, namespace, "sched", 3, 40, 70).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let mut spec = recommendation_spec("sched", &[("min_replicas", 10, 3)]);
    spec.approved = true;
    spec.schedule = Some(vec![ScheduleWindowSpec {
        start_cron: "0 0 * * *".to_owned(), // an all-day window, always active
        duration_minutes: 1440,
        min_replicas: 8,
    }]);
    let _ = recs.delete("sched", &DeleteParams::default()).await;
    recs.create(&PostParams::default(), &ScalingRecommendation::new("sched", spec)).await?;
    recs.patch_status(
        "sched",
        &PatchParams::default(),
        &Patch::Merge(&json!({"status": {"phase": "applied"}})),
    )
    .await?;

    // interval = 0 ⇒ deadline already reached: the schedule pass must stop before raising the floor.
    // Without the deadline check it would patch minReplicas 3 -> 8 for the active window.
    let mut ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    ctx.interval = Duration::from_secs(0);
    schedule_pass(&ctx, &HashMap::new(), &mut HashMap::new(), Utc::now()).await?;

    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    assert_eq!(
        hpas.get("sched").await?.spec.and_then(|s| s.min_replicas),
        Some(3),
        "the schedule floor must be untouched once the deadline is reached"
    );
    assert!(
        recs.get("sched").await?.status.and_then(|s| s.schedule_active).is_none(),
        "scheduleActive must not be toggled past the deadline"
    );

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn verify_rolls_back_a_degraded_change() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-rollback";
    // The HPA reflects the already-applied (bad) config: min lowered to 1.
    setup(&client, namespace, "worker", 1, 10, 70).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let mut spec = recommendation_spec("worker", &[("min_replicas", 2, 1)]);
    spec.approved = true;
    let _ = recs.delete("worker", &DeleteParams::default()).await;
    recs.create(&PostParams::default(), &ScalingRecommendation::new("worker", spec)).await?;

    let now = Utc::now();
    let applied_at = now - chrono::Duration::hours(2);
    recs.patch_status(
        "worker",
        &PatchParams::default(),
        &Patch::Merge(&json!({"status": {
            "phase": "applied",
            "appliedAt": applied_at.to_rfc3339(),
            "probationUntil": (now - chrono::Duration::hours(1)).to_rfc3339(),
        }})),
    )
    .await?;

    // After applying, the workload pinned at max_replicas for the whole probation window.
    let after: Vec<MetricPoint> = (0..4)
        .map(|m| MetricPoint {
            timestamp: now - chrono::Duration::minutes(30 - m * 5),
            cpu_util: 0.5,
            replicas: 10,
            queue_depth: None,
        })
        .collect();
    let history = HashMap::from([(format!("{namespace}/worker"), after)]);

    let ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    verify_pass(&ctx, &history, now).await?;

    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    let hpa = hpas.get("worker").await?;
    assert_eq!(hpa.spec.and_then(|s| s.min_replicas), Some(2), "reverted to pre-apply min");

    let rec = recs.get("worker").await?;
    assert_eq!(rec.status.and_then(|s| s.phase).as_deref(), Some("rolledBack"));

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn forecast_creates_schedule_and_drives_hpa() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-forecast";
    setup(&client, namespace, "siteweb", 10, 40, 70).await?;
    let mut ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    ctx.forecast_params = Some(ForecastParams::default());

    // 21 days of a clear daily peak (09:00-17:00 UTC), ending just before `now` (a peak instant).
    let now = Utc.with_ymd_and_hms(2026, 6, 24, 13, 0, 0).unwrap();
    let base = now - chrono::Duration::days(21);
    let points: Vec<MetricPoint> = (0..21 * 24)
        .map(|h| {
            let timestamp = base + chrono::Duration::hours(h);
            let busy = (9..17).contains(&timestamp.hour());
            MetricPoint {
                timestamp,
                cpu_util: if busy { 0.85 } else { 0.15 },
                replicas: 5,
                queue_depth: None,
            }
        })
        .collect();
    let mut history = HashMap::from([(format!("{namespace}/siteweb"), points)]);
    analysis_pass(&ctx, &mut history, now).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let rec = recs.get("siteweb").await?;
    let schedule = rec.spec.schedule.clone().expect("schedule emitted");
    assert!(!schedule.is_empty(), "{schedule:?}");
    let baseline = rec.spec.config_diff.get("min_replicas").expect("baseline").to;
    assert!(baseline < 10, "baseline {baseline}");

    // Approve, apply the lowered baseline, then drive the schedule at the peak instant.
    recs.patch(
        "siteweb",
        &PatchParams::default(),
        &Patch::Merge(&json!({"spec": {"approved": true}})),
    )
    .await?;
    apply_pass(&ctx, now).await?;
    schedule_pass(&ctx, &history, &mut HashMap::new(), now).await?;

    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    let min = hpas.get("siteweb").await?.spec.and_then(|s| s.min_replicas).expect("min set");
    let window_min = schedule.iter().map(|w| w.min_replicas).max().unwrap();
    assert_eq!(min, window_min, "schedule should raise the floor to the window peak");
    assert!(min > baseline);

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn idle_schedule_is_retracted() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-retract";
    setup(&client, namespace, "batch", 3, 20, 70).await?; // HPA already at baseline 3

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let mut spec = recommendation_spec("batch", &[("min_replicas", 10, 3)]);
    spec.approved = true;
    spec.schedule = Some(vec![ScheduleWindowSpec {
        start_cron: "0 0 * * *".to_owned(),
        duration_minutes: 1440,
        min_replicas: 8,
    }]);
    let _ = recs.delete("batch", &DeleteParams::default()).await;
    recs.create(&PostParams::default(), &ScalingRecommendation::new("batch", spec)).await?;
    recs.patch_status(
        "batch",
        &PatchParams::default(),
        &Patch::Merge(&json!({"status": {"phase": "applied"}})),
    )
    .await?;

    // The pre-scaled floor sits idle - recent (fresh) samples so retraction's freshness gate counts
    // them; the latest sample is `now` so it is well within the window_idle freshness bound.
    let now = Utc.with_ymd_and_hms(2026, 6, 24, 12, 0, 0).unwrap();
    let idle: Vec<MetricPoint> = (0..8)
        .map(|m| MetricPoint {
            timestamp: now - chrono::Duration::minutes(7 - m),
            cpu_util: 0.05,
            replicas: 8,
            queue_depth: None,
        })
        .collect();
    let history = HashMap::from([(format!("{namespace}/batch"), idle)]);

    let ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    let mut misses = HashMap::new();
    for _ in 0..8 {
        schedule_pass(&ctx, &history, &mut misses, now).await?;
    }

    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    let min = hpas.get("batch").await?.spec.and_then(|s| s.min_replicas).expect("min set");
    assert_eq!(min, 3, "retraction restores the baseline floor");
    let rec = recs.get("batch").await?;
    assert_eq!(rec.status.and_then(|s| s.schedule_active), Some(false));

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn rolled_back_scheduled_rec_restores_baseline_after_window() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-rollback-schedule";
    // HPA is frozen at the schedule's raised peak (8) after a scalar rollback left the schedule
    // orphaned.
    setup(&client, namespace, "worker", 8, 40, 70).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let mut spec = recommendation_spec("worker", &[("min_replicas", 10, 3)]);
    spec.approved = true;
    spec.schedule = Some(vec![ScheduleWindowSpec {
        start_cron: "0 9 * * 1".to_owned(), // Mondays only
        duration_minutes: 60,
        min_replicas: 8,
    }]);
    let _ = recs.delete("worker", &DeleteParams::default()).await;
    recs.create(&PostParams::default(), &ScalingRecommendation::new("worker", spec)).await?;
    recs.patch_status(
        "worker",
        &PatchParams::default(),
        &Patch::Merge(&json!({"status": {"phase": "rolledBack", "scheduleActive": true}})),
    )
    .await?;

    // A Wednesday afternoon - well outside the Monday window, so the floor must return to baseline.
    let now = Utc.with_ymd_and_hms(2026, 6, 24, 14, 0, 0).unwrap();
    let ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    schedule_pass(&ctx, &HashMap::new(), &mut HashMap::new(), now).await?;

    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    let min = hpas.get("worker").await?.spec.and_then(|s| s.min_replicas).expect("min set");
    assert_eq!(min, 3, "floor must return to baseline after a rolled-back schedule's window");
    let rec = recs.get("worker").await?;
    assert_eq!(rec.status.and_then(|s| s.schedule_active), Some(false));

    cleanup(&client, namespace).await;
    Ok(())
}

fn ctx(client: Client, namespaces: Vec<String>) -> Context {
    Context {
        client,
        llm: LlmBackend::RulesOnly,
        metrics: MetricsSource::HpaStatus,
        namespaces,
        interval: Duration::from_secs(300),
        metrics_window: chrono::Duration::days(14),
        metrics_step: Duration::from_secs(300),
        probation_window: chrono::Duration::minutes(45),
        rollback_enabled: true,
        health_cpu_margin: 0.15,
        apply_enabled: true,
        forecast_params: None,
        llm_concurrency: 4,
        analysis_budget: Duration::from_secs(300),
        cost_per_replica_usd_monthly: 30.0,
        elector: None,
    }
}

fn recommendation_spec(name: &str, diff: &[(&str, i32, i32)]) -> ScalingRecommendationSpec {
    let config_diff: BTreeMap<String, DiffEntry> = diff
        .iter()
        .map(|(field, from, to)| ((*field).to_owned(), DiffEntry { from: *from, to: *to }))
        .collect();
    ScalingRecommendationSpec {
        target_ref: TargetRef { kind: TargetKind::HorizontalPodAutoscaler, name: name.to_owned() },
        approved: false,
        risk_level: "low".to_owned(),
        summary_md: "e2e".to_owned(),
        projected_savings_usd_monthly: None,
        config_diff,
        schedule: None,
    }
}

/// Install the CRD, ensure the namespace, and create a Deployment + HPA the passes can act on.
async fn setup(
    client: &Client,
    namespace: &str,
    name: &str,
    min: i32,
    max: i32,
    target: i32,
) -> Result<()> {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    crds.patch(
        CRD_NAME,
        &PatchParams::apply("openhpa-e2e").force(),
        &Patch::Apply(ScalingRecommendation::crd()),
    )
    .await?;
    await_condition(crds, CRD_NAME, conditions::is_crd_established()).await?;

    let namespaces: Api<Namespace> = Api::all(client.clone());
    let _ = namespaces
        .create(
            &PostParams::default(),
            &from_value(
                json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": namespace}}),
            )?,
        )
        .await;

    let deployments: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let deployment: Deployment = from_value(json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app": name}},
            "template": {"metadata": {"labels": {"app": name}}, "spec": {"containers": [{"name": "app", "image": "registry.k8s.io/pause:3.9"}]}},
        }
    }))?;
    let _ = deployments.create(&PostParams::default(), &deployment).await;

    let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    let _ = hpas.delete(name, &DeleteParams::default()).await;
    hpas.create(&PostParams::default(), &hpa(name, min, max, target)).await?;
    Ok(())
}

fn hpa(name: &str, min: i32, max: i32, target: i32) -> HorizontalPodAutoscaler {
    from_value(json!({
        "apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler",
        "metadata": {"name": name},
        "spec": {
            "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": name},
            "minReplicas": min,
            "maxReplicas": max,
            "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": target}}}],
        }
    }))
    .expect("valid HPA fixture")
}

async fn cleanup(client: &Client, namespace: &str) {
    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let _ = recs.delete_collection(&DeleteParams::default(), &ListParams::default()).await;
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let _ = namespaces.delete(namespace, &DeleteParams::default()).await;
}
