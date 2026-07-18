//! End-to-end coverage for the live LLM path: a `wiremock` server stands in for the OpenAI API
//! (pointed at via the configurable base URL), so the operator exercises the real HTTP call +
//! `parse_llm_output` + synthesis without a real key. `#[ignore]`d - run against kind with
//! `cargo test -p e2e-tests -- --ignored`.
use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::conditions;
use kube::runtime::wait::await_condition;
use kube::{Api, Client, CustomResourceExt};
use openhpa_core::MetricPoint;
use openhpa_operator::controller::{Context, analysis_pass};
use openhpa_operator::crd::ScalingRecommendation;
use openhpa_operator::llm::LlmBackend;
use openhpa_operator::metrics::MetricsSource;
use serde_json::{Value, from_value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CRD_NAME: &str = "scalingrecommendations.openhpa.dev";

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn analysis_uses_the_llm_verdict_to_shape_the_recommendation() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-mock-llm";
    setup(&client, namespace, "web").await?;

    // The idle history surfaces two rule candidates, both a min_replicas floor cut from load
    // headroom (min 10 -> 1): idle_window and overprovisioned. The mocked LLM approves only
    // idle_window, rejects overprovisioned, and returns a medium overall risk + a savings figure -
    // none of which the rules-only fallback would produce - so the assertions prove the live LLM
    // reply (not the fallback) shaped the result.
    let verdict = json!({
        "recommendations": [
            {"candidate_kind": "idle_window", "apply": true, "reasoning": "Idle overnight; the floor can drop safely.", "risk": "medium", "projected_savings_usd_monthly": 1234.0},
            {"candidate_kind": "overprovisioned", "apply": false, "reasoning": "Headroom is intentional; leave the target.", "risk": "low", "projected_savings_usd_monthly": 0}
        ],
        "overall_risk": "medium",
        "executive_summary": "Mocked LLM verdict for the e2e."
    })
    .to_string();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_completion(&verdict)))
        .expect(1)
        .mount(&server)
        .await;

    let ctx = ctx(client.clone(), namespace, openai_backend(&server.uri(), Duration::from_secs(5)));
    let mut history = HashMap::from([(format!("{namespace}/web"), idle_history())]);
    analysis_pass(&ctx, &mut history, Utc::now()).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let rec = recs.get("web").await?;
    assert_eq!(rec.spec.config_diff.get("min_replicas").map(|d| d.to), Some(1));
    assert!(
        !rec.spec.config_diff.contains_key("target_cpu_pct"),
        "only idle_window was approved, so no target bump may appear: {:?}",
        rec.spec.config_diff
    );
    assert_eq!(rec.spec.risk_level, "medium", "risk_level must come from the LLM overall_risk");
    assert_eq!(rec.spec.projected_savings_usd_monthly, Some(1234.0));
    assert!(
        !rec.spec.summary_md.contains("recommend-only"),
        "in apply mode a rec is auto-appliable, so it carries no advisory note: {}",
        rec.spec.summary_md
    );

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn malformed_llm_reply_errors_without_writing_a_recommendation() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-mock-llm-bad";
    setup(&client, namespace, "web").await?;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_completion("not valid json")))
        .expect(1)
        .mount(&server)
        .await;

    let ctx = ctx(client.clone(), namespace, openai_backend(&server.uri(), Duration::from_secs(5)));
    let mut history = HashMap::from([(format!("{namespace}/web"), idle_history())]);
    let err = analysis_pass(&ctx, &mut history, Utc::now()).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("parsing LLM output"),
        "malformed reply must surface a parse error: {err:#}"
    );

    // No recommendation must be written on the error path (no crash, no half-baked apply).
    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    assert!(
        recs.get_opt("web").await?.is_none(),
        "no recommendation may be created on a parse error"
    );

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn a_rate_limited_llm_is_retried_and_the_analysis_succeeds() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-mock-llm-429";
    setup(&client, namespace, "web").await?;

    let verdict = json!({
        "recommendations": [
            {"candidate_kind": "idle_window", "apply": true, "reasoning": "Idle overnight.", "risk": "low", "projected_savings_usd_monthly": 270.0}
        ],
        "overall_risk": "low",
        "executive_summary": "Retry e2e verdict."
    })
    .to_string();
    // First request is rate-limited, the retry succeeds. The two expectations together prove the
    // pass made exactly 2 requests (verified when the MockServer drops).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_completion(&verdict)))
        .expect(1)
        .mount(&server)
        .await;

    let ctx = ctx(client.clone(), namespace, openai_backend(&server.uri(), Duration::from_secs(5)));
    let mut history = HashMap::from([(format!("{namespace}/web"), idle_history())]);
    analysis_pass(&ctx, &mut history, Utc::now()).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let rec = recs.get("web").await?;
    assert_eq!(rec.spec.config_diff.get("min_replicas").map(|d| d.to), Some(1));
    assert_eq!(rec.spec.projected_savings_usd_monthly, Some(270.0));

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn a_persistent_429_fails_the_pass_with_the_provider_error_body() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-mock-llm-quota";
    setup(&client, namespace, "web").await?;

    // Always 429: the pass must give up after bounded attempts (expect(3) caps the retry loop)
    // and the error must carry the provider's body so rate-limit vs exhausted-quota is visible.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "insufficient_quota: billing hard limit reached", "type": "insufficient_quota"}
        })))
        .expect(3)
        .mount(&server)
        .await;

    let ctx = ctx(client.clone(), namespace, openai_backend(&server.uri(), Duration::from_secs(5)));
    let mut history = HashMap::from([(format!("{namespace}/web"), idle_history())]);
    let err = analysis_pass(&ctx, &mut history, Utc::now()).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("insufficient_quota: billing hard limit reached"),
        "the provider error body must be surfaced: {err:#}"
    );

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    assert!(
        recs.get_opt("web").await?.is_none(),
        "no recommendation may be created when the LLM is exhausted"
    );

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn llm_fan_out_is_capped_and_faster_than_sequential() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-mock-llm-fanout";
    setup(&client, namespace, "w1").await?;
    for name in ["w2", "w3", "w4"] {
        add_workload(&client, namespace, name).await?;
    }

    let verdict = json!({
        "recommendations": [
            {"candidate_kind": "idle_window", "apply": true, "reasoning": "Idle.", "risk": "low", "projected_savings_usd_monthly": null}
        ],
        "overall_risk": "low",
        "executive_summary": "Fan-out e2e verdict."
    })
    .to_string();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_completion(&verdict))
                .set_delay(Duration::from_secs(1)),
        )
        .expect(4)
        .mount(&server)
        .await;

    let mut ctx =
        ctx(client.clone(), namespace, openai_backend(&server.uri(), Duration::from_secs(10)));
    ctx.llm_concurrency = 2;
    let mut history: HashMap<_, _> = ["w1", "w2", "w3", "w4"]
        .into_iter()
        .map(|name| (format!("{namespace}/{name}"), idle_history()))
        .collect();
    let started = std::time::Instant::now();
    analysis_pass(&ctx, &mut history, Utc::now()).await?;
    let elapsed = started.elapsed();

    // Four 1s replies under a cap of 2 must take two waves: clearly under the ~4s sequential
    // floor (proves real concurrency - fails on the old sequential code) while still at least
    // two waves of wall-clock (proves the cap held; uncapped would finish in ~1s).
    assert!(elapsed >= Duration::from_secs(2), "cap of 2 must force two waves: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(3800), "fan-out must beat sequential: {elapsed:?}");

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    for name in ["w1", "w2", "w3", "w4"] {
        assert!(recs.get_opt(name).await?.is_some(), "missing recommendation for {name}");
    }

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn analysis_budget_defers_workloads_to_the_next_tick() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-mock-llm-budget";
    setup(&client, namespace, "w1").await?;
    for name in ["w2", "w3"] {
        add_workload(&client, namespace, name).await?;
    }

    let verdict = json!({
        "recommendations": [
            {"candidate_kind": "idle_window", "apply": true, "reasoning": "Idle.", "risk": "low", "projected_savings_usd_monthly": null}
        ],
        "overall_risk": "low",
        "executive_summary": "Budget e2e verdict."
    })
    .to_string();
    // Exactly 3 requests across BOTH passes (verified on drop): deferred workloads are analyzed
    // once on the next tick, already-recommended ones are deduped, nothing is analyzed twice.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_completion(&verdict))
                .set_delay(Duration::from_millis(700)),
        )
        .expect(3)
        .mount(&server)
        .await;

    let history_for = |namespace: &str| -> HashMap<String, Vec<MetricPoint>> {
        ["w1", "w2", "w3"]
            .into_iter()
            .map(|name| (format!("{namespace}/{name}"), idle_history()))
            .collect()
    };

    // Pass 1: serial fan-out with a 1s budget against 700ms replies - w1 always completes, w3's
    // deadline check always lands at >= 1.4s, so w3 is always deferred (not an error).
    let mut ctx =
        ctx(client.clone(), namespace, openai_backend(&server.uri(), Duration::from_secs(10)));
    ctx.llm_concurrency = 1;
    ctx.analysis_budget = Duration::from_secs(1);
    let mut history = history_for(namespace);
    analysis_pass(&ctx, &mut history, Utc::now()).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    assert!(recs.get_opt("w1").await?.is_some(), "the first workload fits the budget");
    // On the old unbounded sequential code every workload would have a recommendation here.
    assert!(
        recs.get_opt("w3").await?.is_none(),
        "the budget must defer workloads it did not reach"
    );

    // Pass 2 (the next tick, ample budget): the deferred workloads are picked up.
    ctx.analysis_budget = Duration::from_secs(300);
    let mut history = history_for(namespace);
    analysis_pass(&ctx, &mut history, Utc::now()).await?;
    for name in ["w1", "w2", "w3"] {
        assert!(recs.get_opt(name).await?.is_some(), "missing recommendation for {name}");
    }

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn recommend_only_mode_still_runs_the_llm_and_marks_the_rec_advisory() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-mock-llm-recommend-only";
    setup(&client, namespace, "web").await?;

    // An OpenAI backend is configured in recommend-only mode: the LLM still runs, so the mock must
    // see exactly one request (expect(1), verified on drop) and the live verdict (not the
    // rules-only fallback) must shape the rec. Because the operator won't auto-apply in this mode,
    // the summary is marked advisory for a human to apply.
    let verdict = json!({
        "recommendations": [
            {"candidate_kind": "idle_window", "apply": true, "reasoning": "Idle overnight; the floor can drop safely.", "risk": "medium", "projected_savings_usd_monthly": 1234.0},
            {"candidate_kind": "overprovisioned", "apply": false, "reasoning": "Headroom is intentional.", "risk": "low", "projected_savings_usd_monthly": 0}
        ],
        "overall_risk": "medium",
        "executive_summary": "Mocked LLM verdict for the recommend-only e2e."
    })
    .to_string();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_completion(&verdict)))
        .expect(1)
        .mount(&server)
        .await;

    let mut ctx =
        ctx(client.clone(), namespace, openai_backend(&server.uri(), Duration::from_secs(5)));
    ctx.apply_enabled = false; // recommend-only: recs are advisory, never auto-applied
    let mut history = HashMap::from([(format!("{namespace}/web"), idle_history())]);
    analysis_pass(&ctx, &mut history, Utc::now()).await?;

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let rec = recs.get("web").await?;
    assert_eq!(rec.spec.config_diff.get("min_replicas").map(|d| d.to), Some(1));
    // The live verdict approved only idle_window and returned a medium risk + a savings figure -
    // none of which the rules-only fallback (low risk, no savings) would produce.
    assert!(
        !rec.spec.config_diff.contains_key("target_cpu_pct"),
        "only idle_window was approved, so no target bump may appear: {:?}",
        rec.spec.config_diff
    );
    assert_eq!(rec.spec.risk_level, "medium", "risk_level must come from the live LLM verdict");
    assert_eq!(rec.spec.projected_savings_usd_monthly, Some(1234.0));
    assert!(
        rec.spec.summary_md.contains("recommend-only") && rec.spec.summary_md.contains("apply"),
        "a recommend-only rec must carry the advisory note: {}",
        rec.spec.summary_md
    );

    cleanup(&client, namespace).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn a_stalled_llm_times_out_without_writing_a_recommendation() -> Result<()> {
    let client = Client::try_default().await?;
    let namespace = "ss-e2e-mock-llm-timeout";
    setup(&client, namespace, "web").await?;

    // The reply is delayed well past the configured request timeout, so the call must error out
    // rather than stretch the pass indefinitely.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_completion("{}"))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&server)
        .await;

    let ctx =
        ctx(client.clone(), namespace, openai_backend(&server.uri(), Duration::from_millis(200)));
    let mut history = HashMap::from([(format!("{namespace}/web"), idle_history())]);
    analysis_pass(&ctx, &mut history, Utc::now()).await.unwrap_err();

    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    assert!(recs.get_opt("web").await?.is_none(), "no recommendation may be created on a timeout");

    cleanup(&client, namespace).await;
    Ok(())
}

/// A canned OpenAI chat-completion whose message content is `content` (the operator parses content
/// as the strict-JSON `LlmAnalysisOutput`).
fn chat_completion(content: &str) -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": content}}]})
}

fn openai_backend(base_url: &str, timeout: Duration) -> LlmBackend {
    LlmBackend::from_config(
        "openai",
        "gpt-4o-mini",
        Some("test-key".to_owned()),
        Some(base_url.to_owned()),
        timeout,
    )
    .expect("openai backend builds")
}

/// Seven samples of sustained idle (5% CPU at 2 replicas over 6h), enough for the idle-window +
/// overprovisioned rules to fire against the min-10 HPA below.
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

fn ctx(client: Client, namespace: &str, llm: LlmBackend) -> Context {
    Context {
        client,
        llm,
        metrics: MetricsSource::HpaStatus,
        namespaces: vec![namespace.to_owned()],
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

/// Install the CRD, ensure the namespace, and create a Deployment + an idle min-10 HPA to analyze.
async fn setup(client: &Client, namespace: &str, name: &str) -> Result<()> {
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

    let _ = recs_cleanup(client, namespace).await;
    add_workload(client, namespace, name).await
}

/// Create one Deployment + an idle min-10 HPA for the analysis pass to pick up.
async fn add_workload(client: &Client, namespace: &str, name: &str) -> Result<()> {
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
    hpas.create(&PostParams::default(), &hpa(name, 10, 40, 70)).await?;
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

async fn recs_cleanup(client: &Client, namespace: &str) -> Result<()> {
    let recs: Api<ScalingRecommendation> = Api::namespaced(client.clone(), namespace);
    let _ = recs.delete_collection(&DeleteParams::default(), &ListParams::default()).await;
    Ok(())
}

async fn cleanup(client: &Client, namespace: &str) {
    let _ = recs_cleanup(client, namespace).await;
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let _ = namespaces.delete(namespace, &DeleteParams::default()).await;
}
