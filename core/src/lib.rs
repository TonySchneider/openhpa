//! Pure, Kubernetes-free domain logic for OpenHPA: metric snapshots, the rule engine, seasonal
//! forecasting, LLM prompt/parse, and recommendation synthesis. The operator crate wires these to
//! Kubernetes.

pub mod domain;
pub mod forecast;
pub mod llm;
pub mod rules;
pub mod synthesis;

pub use domain::{
    Candidate, CandidateKind, ConfigDiff, ConfigDiffEntry, ESTIMATED_REPLICA_MONTHLY_USD,
    LlmAnalysisOutput, LlmRecommendationItem, MetricPoint, MetricsSnapshot, RiskLevel,
    ScalingEvent, ScalingSchedule, ScheduleWindow, SynthesizedRecommendation, WorkloadConfig,
};
