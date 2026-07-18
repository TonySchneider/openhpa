use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Default monthly cost of one always-on replica's node share, used to estimate floor-cut savings
/// when the LLM has no specific instance pricing. A rough on-demand figure, not a quote - real
/// per-replica cost varies wildly, so it is configurable (`--cost-per-replica-usd-monthly`); this
/// constant is only the default.
pub const ESTIMATED_REPLICA_MONTHLY_USD: f64 = 30.0;

/// Current autoscaler config for a workload (read from the live HPA / ScaledObject spec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadConfig {
    pub min_replicas: i32,
    pub max_replicas: i32,
    pub target_cpu_pct: i32,
    pub scale_down_cooldown_s: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricPoint {
    pub timestamp: DateTime<Utc>,
    /// CPU utilization as a fraction in [0, 1].
    pub cpu_util: f64,
    pub replicas: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_depth: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingEvent {
    pub timestamp: DateTime<Utc>,
    pub from_replicas: i32,
    pub to_replicas: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub config: WorkloadConfig,
    pub points: Vec<MetricPoint>,
    #[serde(default)]
    pub scaling_events: Vec<ScalingEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    IdleWindow,
    ScaleLag,
    Overprovisioned,
    Thrashing,
    PredictablePeak,
}

impl CandidateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdleWindow => "idle_window",
            Self::ScaleLag => "scale_lag",
            Self::Overprovisioned => "overprovisioned",
            Self::Thrashing => "thrashing",
            Self::PredictablePeak => "predictable_peak",
        }
    }
}

/// One proactive floor-raise window: a KEDA-style `start_cron`, how long it lasts, and the floor to
/// hold during it. The forecaster emits these to pre-scale ahead of a recurring peak.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleWindow {
    pub start_cron: String,
    pub duration_minutes: i32,
    pub min_replicas: i32,
}

/// A proactive scaling schedule: raise `min_replicas` only during each forecasted peak window.
pub type ScalingSchedule = Vec<ScheduleWindow>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// A single `from -> to` change to one config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDiffEntry {
    pub from: i32,
    pub to: i32,
}

impl ConfigDiffEntry {
    pub fn new(from: i32, to: i32) -> Self {
        Self { from, to }
    }
}

pub type ConfigDiff = BTreeMap<String, ConfigDiffEntry>;

/// A rule-detected optimization opportunity, before LLM judgement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub description: String,
    #[serde(default)]
    pub evidence: BTreeMap<String, f64>,
    pub proposed_diff: ConfigDiff,
    /// Proactive schedule (only `PredictablePeak` carries one) raised alongside the scalar diff.
    #[serde(default)]
    pub schedule: Option<ScalingSchedule>,
}

/// One item of the LLM's judgement over the candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRecommendationItem {
    pub candidate_kind: CandidateKind,
    pub apply: bool,
    pub reasoning: String,
    pub risk: RiskLevel,
    #[serde(default)]
    pub projected_savings_usd_monthly: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmAnalysisOutput {
    pub recommendations: Vec<LlmRecommendationItem>,
    pub overall_risk: RiskLevel,
    pub executive_summary: String,
}

/// Final, synthesized recommendation persisted to the `ScalingRecommendation` CRD.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthesizedRecommendation {
    pub config_diff: ConfigDiff,
    pub summary_md: String,
    pub risk_level: RiskLevel,
    #[serde(default)]
    pub projected_savings_usd_monthly: Option<f64>,
    /// Proactive schedule carried through from an approved `PredictablePeak` candidate.
    #[serde(default)]
    pub schedule: Option<ScalingSchedule>,
}
