//! Cluster-only coverage for the KEDA ScaledObject path and leader election. `#[ignore]`d;
//! run against kind with `cargo test -p e2e-tests -- --ignored`.
use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::conditions;
use kube::runtime::wait::await_condition;
use kube::{Api, Client, CustomResourceExt};
use openhpa_core::MetricPoint;
use openhpa_operator::controller::{Context, analysis_pass, apply_pass, schedule_pass};
use openhpa_operator::crd::{
    DiffEntry, ScalingRecommendation, ScalingRecommendationSpec, ScheduleWindowSpec, TargetKind,
    TargetRef,
};
use openhpa_operator::leader::LeaderElector;
use openhpa_operator::llm::LlmBackend;
use openhpa_operator::metrics::MetricsSource;
use serde_json::{from_value, json};

const SCALEDOBJECT_CRD: &str = "scaledobjects.keda.sh";

fn scaledobject_api(client: &Client, namespace: &str) -> Api<DynamicObject> {
    let gvk = GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject");
    Api::namespaced_with(client.clone(), namespace, &ApiResource::from_gvk(&gvk))
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn keda_scaledobject_apply_and_schedule() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-keda";
    install_crds(&client).await?;
    ensure_namespace(&client, namespace).await;

    // A minimal ScaledObject with its real (kafka) trigger and a high static floor.
    let api = scaledobject_api(&client, namespace);
    let _ = api.delete("web", &DeleteParams::default()).await;
    let mut object = DynamicObject::new(
        "web",
        &ApiResource::from_gvk(&GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject")),
    );
    object.data = json!({"spec": {
        "scaleTargetRef": {"name": "web"},
        "minReplicaCount": 10,
        "maxReplicaCount": 40,
        "triggers": [{"type": "kafka", "name": "main", "metadata": {}}],
    }});
    api.create(&PostParams::default(), &object).await?;

    // Approved recommendation: lower the floor to 3 and add a proactive window.
    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let mut spec = recommendation_spec(TargetKind::ScaledObject, "web", &[("min_replicas", 10, 3)]);
    spec.approved = true;
    spec.schedule = Some(vec![ScheduleWindowSpec {
        start_cron: "0 9 * * *".to_owned(),
        duration_minutes: 480,
        min_replicas: 8,
    }]);
    let _ = recs.delete("web", &DeleteParams::default()).await;
    recs.create(&PostParams::default(), &ScalingRecommendation::new("web", spec)).await?;

    let ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    apply_pass(&ctx, Utc::now()).await?;

    // Scalar diff patched the KEDA spec; the rec is marked verified (ScaledObject health is not
    // verifiable yet, so it must not dangle in `applied`).
    let object = api.get("web").await?;
    assert_eq!(object.data["spec"]["minReplicaCount"], json!(3));
    assert_eq!(recs.get("web").await?.status.and_then(|s| s.phase).as_deref(), Some("verified"));

    // Schedule pass installs the cron trigger and preserves the real kafka trigger.
    let history = std::collections::HashMap::new();
    let mut misses = std::collections::HashMap::new();
    schedule_pass(&ctx, &history, &mut misses, Utc::now()).await?;
    let triggers = scaledobject_api(&client, namespace).get("web").await?.data["spec"]["triggers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(triggers.iter().any(|t| t["type"] == json!("kafka")), "{triggers:?}");
    assert!(
        triggers.iter().any(|t| t["name"].as_str().is_some_and(|n| n.starts_with("openhpa-cron-"))),
        "{triggers:?}"
    );

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn analysis_recommends_for_a_scaledobject_and_skips_its_managed_hpa() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-keda-analyze";
    install_crds(&client).await?;
    ensure_namespace(&client, namespace).await;

    // A ScaledObject with a high static floor and a cpu trigger, plus the backing HPA KEDA would
    // materialize for it (owned by the ScaledObject). The collector must analyze the ScaledObject
    // and skip the KEDA-managed HPA - one recommendation per workload, never two.
    let api = scaledobject_api(&client, namespace);
    let _ = api.delete("kweb", &DeleteParams::default()).await;
    let mut object = DynamicObject::new(
        "kweb",
        &ApiResource::from_gvk(&GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject")),
    );
    object.data = json!({"spec": {
        "scaleTargetRef": {"name": "kweb"},
        "minReplicaCount": 10,
        "maxReplicaCount": 40,
        "triggers": [{"type": "cpu", "metricType": "Utilization", "metadata": {"value": "70"}}],
    }});
    let created = api.create(&PostParams::default(), &object).await?;
    let uid = created.metadata.uid.clone().expect("created object has a uid");

    let hpas: Api<k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler> =
        Api::namespaced(client.clone(), namespace);
    let _ = hpas.delete("keda-hpa-kweb", &DeleteParams::default()).await;
    hpas.create(
        &PostParams::default(),
        &from_value(json!({
            "apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler",
            "metadata": {
                "name": "keda-hpa-kweb",
                "ownerReferences": [{"apiVersion": "keda.sh/v1alpha1", "kind": "ScaledObject", "name": "kweb", "uid": uid, "controller": true}]
            },
            "spec": {
                "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "kweb"},
                "minReplicas": 10, "maxReplicas": 40,
                "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 70}}}],
            }
        }))?,
    )
    .await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let _ = recs.delete("kweb", &DeleteParams::default()).await;
    let _ = recs.delete("keda-hpa-kweb", &DeleteParams::default()).await;

    // Idle history for BOTH keys: if the HPA-skip ever regresses, the managed HPA would surface
    // its own candidates and a second rec - caught below.
    let ctx = ctx(client.clone(), vec![namespace.to_owned()]);
    let mut history = std::collections::HashMap::from([
        (format!("{namespace}/kweb"), idle_history()),
        (format!("{namespace}/keda-hpa-kweb"), idle_history()),
    ]);
    analysis_pass(&ctx, &mut history, Utc::now()).await?;

    let rec = recs.get("kweb").await?;
    assert_eq!(rec.spec.target_ref.kind, TargetKind::ScaledObject);
    assert_eq!(rec.spec.config_diff.get("min_replicas").map(|d| (d.from, d.to)), Some((10, 1)));
    assert!(
        recs.get_opt("keda-hpa-kweb").await?.is_none(),
        "the KEDA-managed HPA must be skipped, not analyzed as a second workload"
    );

    // Approve -> the existing apply path patches the ScaledObject end-to-end.
    recs.patch(
        "kweb",
        &PatchParams::default(),
        &Patch::Merge(&json!({"spec": {"approved": true}})),
    )
    .await?;
    apply_pass(&ctx, Utc::now()).await?;

    let object = api.get("kweb").await?;
    assert_eq!(object.data["spec"]["minReplicaCount"], json!(1));
    assert_eq!(recs.get("kweb").await?.status.and_then(|s| s.phase).as_deref(), Some("verified"));

    cleanup(&client, namespace).await;
    Ok(())
}

/// Seven samples of sustained idle (5% CPU at 2 replicas over 6h): enough for the idle-window +
/// overprovisioned rules to fire against the min-10 fixtures above.
fn idle_history() -> Vec<MetricPoint> {
    let now = Utc::now();
    (0..7)
        .map(|h| MetricPoint {
            timestamp: now - chrono::Duration::hours(6 - h),
            cpu_util: 0.05,
            replicas: 2,
            queue_depth: None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn leader_election_grants_a_single_holder() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "default";
    let lease = "ss-e2e-leader";
    let leases: Api<k8s_openapi::api::coordination::v1::Lease> =
        Api::namespaced(client.clone(), namespace);
    let _ = leases.delete(lease, &DeleteParams::default()).await;

    let duration = chrono::Duration::seconds(60);
    let a =
        LeaderElector::new(client.clone(), namespace, lease.to_owned(), "a".to_owned(), duration);
    let b =
        LeaderElector::new(client.clone(), namespace, lease.to_owned(), "b".to_owned(), duration);

    let now = Utc::now();
    assert!(a.try_acquire(now).await?, "first acquirer becomes leader");
    assert!(!b.try_acquire(now).await?, "a fresh lease blocks a second holder");
    assert!(a.try_acquire(now + chrono::Duration::seconds(1)).await?, "incumbent renews");

    let _ = leases.delete(lease, &DeleteParams::default()).await;
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

fn recommendation_spec(
    kind: TargetKind,
    name: &str,
    diff: &[(&str, i32, i32)],
) -> ScalingRecommendationSpec {
    let config_diff: BTreeMap<String, DiffEntry> = diff
        .iter()
        .map(|(field, from, to)| ((*field).to_owned(), DiffEntry { from: *from, to: *to }))
        .collect();
    ScalingRecommendationSpec {
        target_ref: TargetRef { kind, name: name.to_owned() },
        approved: false,
        risk_level: "low".to_owned(),
        summary_md: "e2e".to_owned(),
        projected_savings_usd_monthly: None,
        config_diff,
        schedule: None,
    }
}

async fn install_crds(client: &Client) -> Result<()> {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let params = PatchParams::apply("openhpa-e2e").force();
    crds.patch(
        "scalingrecommendations.openhpa.dev",
        &params,
        &Patch::Apply(ScalingRecommendation::crd()),
    )
    .await?;
    // Minimal ScaledObject CRD (open schema) so we can create/patch one without the KEDA operator.
    let scaledobject: CustomResourceDefinition = from_value(json!({
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": SCALEDOBJECT_CRD},
        "spec": {
            "group": "keda.sh", "scope": "Namespaced",
            "names": {"kind": "ScaledObject", "plural": "scaledobjects", "singular": "scaledobject"},
            "versions": [{"name": "v1alpha1", "served": true, "storage": true, "schema": {
                "openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}
            }}],
        }
    }))?;
    crds.patch(SCALEDOBJECT_CRD, &params, &Patch::Apply(&scaledobject)).await?;
    await_condition(crds.clone(), SCALEDOBJECT_CRD, conditions::is_crd_established()).await?;
    await_condition(crds, "scalingrecommendations.openhpa.dev", conditions::is_crd_established())
        .await?;
    Ok(())
}

async fn ensure_namespace(client: &Client, namespace: &str) {
    let namespaces: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client.clone());
    let _ = namespaces
        .create(
            &PostParams::default(),
            &from_value(
                json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": namespace}}),
            )
            .expect("namespace fixture"),
        )
        .await;
}

async fn cleanup(client: &Client, namespace: &str) {
    let namespaces: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(client.clone());
    let _ = namespaces.delete(namespace, &DeleteParams::default()).await;
}
