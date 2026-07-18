//! Guards that the Helm chart keeps the runtime wiring the operator depends on. These would
//! otherwise be invisible to the rest of the test suite: deleting the rule/env passes everything
//! else but breaks a deployed operator.
use std::fs;
use std::path::Path;

fn chart_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/helm/openhpa").join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

#[test]
fn chart_version_tracks_the_workspace_version() {
    // The chart `version` + `appVersion` must match the crate (workspace) version, so a `v*` tag
    // ships the chart that matches the binary. Catches a half-finished version bump.
    let version = env!("CARGO_PKG_VERSION");
    let chart = chart_file("Chart.yaml");
    assert!(
        chart.contains(&format!("version: {version}")),
        "Chart.yaml `version` must match the workspace version {version}:\n{chart}"
    );
    assert!(
        chart.contains(&format!("appVersion: \"{version}\"")),
        "Chart.yaml `appVersion` must match the workspace version {version}:\n{chart}"
    );
}

#[test]
fn rbac_does_not_grant_secrets_access() {
    // Least privilege: OpenHPA never reads Secrets via the Kubernetes API. The LLM API key (the only
    // sensitive value) is injected as an environment variable by the Deployment, never read from the
    // Secrets API by the operator. This guards against a Secrets grant creeping back into the chart.
    let rbac = chart_file("templates/rbac.yaml");
    assert!(
        !rbac.contains("\"secrets\""),
        "the ClusterRole must NOT grant access to secrets (least privilege):\n{rbac}"
    );
}

#[test]
fn deployment_wires_the_reconcile_interval() {
    // The reconcile interval (and, transitively, the lease duration) must be tunable from values.
    let deployment = chart_file("templates/deployment.yaml");
    assert!(
        deployment.contains("OPENHPA_INTERVAL_SECONDS"),
        "deployment must set the reconcile-interval env var"
    );
    assert!(
        deployment.contains(".Values.intervalSeconds"),
        "the interval env var must be rendered from values.yaml"
    );
}

#[test]
fn deployment_defaults_to_recommend_only_mode() {
    // Safe by default: a fresh `helm install` must never mutate a workload. The chart must default
    // the operating mode to recommend-only so apply is an explicit opt-in.
    let values = chart_file("values.yaml");
    assert!(
        values.contains("mode: recommend"),
        "values.yaml must default the operating mode to recommend:\n{values}"
    );
}

#[test]
fn chart_readme_leads_with_the_public_quickstart() {
    // The chart README renders as the Artifact Hub package page: it must lead with the public
    // quickstart and link to the docs site. OpenHPA has no tiers or licensing.
    let readme = chart_file("README.md");
    assert!(
        readme.contains("helm install openhpa oci://ghcr.io/tonyschneider/charts/openhpa"),
        "the chart README must lead with the public OCI quickstart:\n{readme}"
    );
}
