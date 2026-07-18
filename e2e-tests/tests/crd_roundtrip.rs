use std::collections::BTreeMap;

use anyhow::Result;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::runtime::conditions;
use kube::runtime::wait::await_condition;
use kube::{Api, Client, CustomResourceExt};
use openhpa_operator::crd::{
    DiffEntry, ScalingRecommendation, ScalingRecommendationSpec, TargetKind, TargetRef,
};

const CRD_NAME: &str = "scalingrecommendations.openhpa.dev";

#[tokio::test]
#[ignore = "requires a Kubernetes cluster - run with --ignored against kind"]
async fn crd_installs_and_recommendation_round_trips() -> Result<()> {
    let client = Client::try_default().await?;

    // Install (server-side apply) the CRD and wait for it to be established.
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    crds.patch(
        CRD_NAME,
        &PatchParams::apply("openhpa-e2e").force(),
        &Patch::Apply(ScalingRecommendation::crd()),
    )
    .await?;
    await_condition(crds, CRD_NAME, conditions::is_crd_established()).await?;

    // Create a recommendation and read it back.
    let recs: Api<ScalingRecommendation> = Api::namespaced(client, "default");
    let spec = ScalingRecommendationSpec {
        target_ref: TargetRef { kind: TargetKind::HorizontalPodAutoscaler, name: "web".to_owned() },
        approved: false,
        risk_level: "low".to_owned(),
        summary_md: "drop min replicas 10 -> 3".to_owned(),
        projected_savings_usd_monthly: Some(1240.0),
        config_diff: BTreeMap::from([("min_replicas".to_owned(), DiffEntry { from: 10, to: 3 })]),
        schedule: None,
    };
    let _ = recs.delete("e2e-demo", &DeleteParams::default()).await;
    recs.create(&PostParams::default(), &ScalingRecommendation::new("e2e-demo", spec)).await?;

    let got = recs.get("e2e-demo").await?;
    assert_eq!(got.spec.target_ref.name, "web");
    assert_eq!(got.spec.config_diff.get("min_replicas").unwrap().to, 3);

    recs.delete("e2e-demo", &DeleteParams::default()).await?;
    Ok(())
}
