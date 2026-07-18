use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum TargetKind {
    HorizontalPodAutoscaler,
    ScaledObject,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetRef {
    pub kind: TargetKind,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct DiffEntry {
    pub from: i32,
    pub to: i32,
}

/// One proactive floor-raise window on the CRD: a KEDA-style cron, how long it lasts, and the floor.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleWindowSpec {
    pub start_cron: String,
    pub duration_minutes: i32,
    pub min_replicas: i32,
}

/// The lifecycle phase of a recommendation's status. The string forms are what land in
/// `status.phase`; this enum is the single source of truth for the legal set so the mutating passes
/// can't invent ad-hoc phases. `Degraded` is a known-unhealthy applied change held for re-judgement
/// (rollback disabled) rather than a healthy-looking `applied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pending,
    Applied,
    Verified,
    RolledBack,
    Blocked,
    Degraded,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Verified => "verified",
            Self::RolledBack => "rolledBack",
            Self::Blocked => "blocked",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

/// A scaling recommendation the operator emits for human approval, then applies.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "openhpa.dev",
    version = "v1alpha1",
    kind = "ScalingRecommendation",
    namespaced,
    status = "ScalingRecommendationStatus",
    shortname = "scalerec",
    printcolumn = r#"{"name":"Target","type":"string","jsonPath":".spec.targetRef.name"}"#,
    printcolumn = r#"{"name":"Risk","type":"string","jsonPath":".spec.riskLevel"}"#,
    printcolumn = r#"{"name":"Approved","type":"boolean","jsonPath":".spec.approved"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ScalingRecommendationSpec {
    pub target_ref: TargetRef,
    #[serde(default)]
    pub approved: bool,
    pub risk_level: String,
    pub summary_md: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projected_savings_usd_monthly: Option<f64>,
    pub config_diff: BTreeMap<String, DiffEntry>,
    /// Proactive schedule (from a `PredictablePeak` forecast): raise `minReplicas` per window. The
    /// scalar `config_diff` carries the baseline change; this is the time-based part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Vec<ScheduleWindowSpec>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScalingRecommendationStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<String>,
    /// RFC3339 instant until which an applied change is on probation; after it passes the verify
    /// pass judges health and either marks `verified` or auto-reverts to `rolledBack`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probation_until: Option<String>,
    /// Whether a proactive schedule is currently being enforced; `false` once retracted (its peak
    /// stopped materializing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use kube::CustomResourceExt;

    use super::*;

    #[test]
    fn crd_has_expected_group_and_kind() {
        let crd = ScalingRecommendation::crd();
        assert_eq!(crd.spec.group, "openhpa.dev");
        assert_eq!(crd.spec.names.kind, "ScalingRecommendation");
    }

    #[test]
    fn spec_round_trips_through_camel_case_json() {
        let spec = ScalingRecommendationSpec {
            target_ref: TargetRef {
                kind: TargetKind::HorizontalPodAutoscaler,
                name: "web".to_owned(),
            },
            approved: true,
            risk_level: "low".to_owned(),
            summary_md: "drop replicas".to_owned(),
            projected_savings_usd_monthly: Some(1240.0),
            config_diff: BTreeMap::from([(
                "min_replicas".to_owned(),
                DiffEntry { from: 10, to: 3 },
            )]),
            schedule: None,
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"targetRef\""), "{json}");
        assert!(json.contains("\"configDiff\""), "{json}");

        let back: ScalingRecommendationSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.config_diff.get("min_replicas").unwrap().to, 3);
        assert!(back.approved);
    }
}
