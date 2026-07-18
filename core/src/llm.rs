use std::fmt::Write as _;

use crate::domain::{Candidate, LlmAnalysisOutput, WorkloadConfig};

/// The system prompt, priced with the configured per-replica cost so the LLM's savings estimates
/// and the deterministic fallback (`synthesis::floor_savings`) agree on the same baseline.
fn system_prompt(cost_per_replica_usd_monthly: f64) -> String {
    format!(
        "You are a Kubernetes autoscaling expert. Below is a customer workload running on \
         Kubernetes with an HPA. Rule-based analysis surfaced these candidates. For each, decide \
         if it should be applied, explain the reasoning in 2-3 sentences a DevOps engineer would \
         respect, and assign a risk level. When a candidate lowers min_replicas, estimate \
         projected_savings_usd_monthly from the replica reduction (each always-on replica costs \
         roughly ${cost_per_replica_usd_monthly}/month of node share unless you have better \
         pricing); never leave it at 0 when replicas are reduced."
    )
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("failed to parse LLM output: {source}: {snippet}")]
    Json {
        #[source]
        source: serde_json::Error,
        snippet: String,
    },
}

/// Build the (system, user) prompt pair for the LLM analysis call.
/// `cost_per_replica_usd_monthly` prices the savings guidance in the system prompt.
pub fn build_prompt(
    workload_desc: &str,
    config: &WorkloadConfig,
    candidates: &[Candidate],
    cost_per_replica_usd_monthly: f64,
) -> (String, String) {
    let mut user = String::new();
    let _ = write!(
        user,
        "WORKLOAD: {workload_desc}\nCURRENT CONFIG:\n  min_replicas: {}\n  max_replicas: {}\n  \
         target_cpu: {}%\n  scale_down_cooldown: {}s\n\nCANDIDATES:\n",
        config.min_replicas,
        config.max_replicas,
        config.target_cpu_pct,
        config.scale_down_cooldown_s
    );
    for (index, candidate) in candidates.iter().enumerate() {
        let _ = writeln!(
            user,
            "  {}. {}: {}",
            index + 1,
            candidate.kind.as_str(),
            candidate.description
        );
    }
    user.push_str(
        "\nOUTPUT STRICT JSON matching:\n{\n  \"recommendations\": [{ \"candidate_kind\": \
         \"<idle_window|scale_lag|overprovisioned|thrashing|predictable_peak>\", \"apply\": true, \"reasoning\": \
         \"...\", \"risk\": \"<low|medium|high>\", \"projected_savings_usd_monthly\": 0 }],\n  \
         \"overall_risk\": \"<low|medium|high>\",\n  \"executive_summary\": \"...\"\n}",
    );
    (system_prompt(cost_per_replica_usd_monthly), user)
}

/// Parse the LLM response, tolerating ```json fenced code blocks.
pub fn parse_llm_output(text: &str) -> Result<LlmAnalysisOutput, ParseError> {
    let trimmed = text.trim();
    let unfenced =
        trimmed.strip_prefix("```json").or_else(|| trimmed.strip_prefix("```")).unwrap_or(trimmed);
    let cleaned = unfenced.strip_suffix("```").unwrap_or(unfenced).trim();
    serde_json::from_str(cleaned)
        .map_err(|source| ParseError::Json { source, snippet: cleaned.chars().take(200).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RiskLevel;

    const RESPONSE: &str = r#"{
      "recommendations": [
        {"candidate_kind": "idle_window", "apply": true, "reasoning": "nightly idle", "risk": "low", "projected_savings_usd_monthly": 1240}
      ],
      "overall_risk": "low",
      "executive_summary": "overprovisioned at night"
    }"#;

    #[test]
    fn parses_bare_json() {
        let out = parse_llm_output(RESPONSE).unwrap();
        assert_eq!(out.recommendations.len(), 1);
        assert_eq!(out.overall_risk, RiskLevel::Low);
        assert_eq!(out.recommendations[0].projected_savings_usd_monthly, Some(1240.0));
    }

    #[test]
    fn parses_fenced_json() {
        let fenced = format!("```json\n{RESPONSE}\n```");
        let out = parse_llm_output(&fenced).unwrap();
        assert_eq!(out.executive_summary, "overprovisioned at night");
    }

    #[test]
    fn rejects_garbage() {
        let err = parse_llm_output("not json at all").unwrap_err();
        assert!(matches!(err, ParseError::Json { .. }));
    }

    #[test]
    fn prompt_contains_workload_and_candidates() {
        let config = WorkloadConfig {
            min_replicas: 10,
            max_replicas: 40,
            target_cpu_pct: 70,
            scale_down_cooldown_s: 300,
        };
        let candidates = vec![Candidate {
            kind: crate::domain::CandidateKind::IdleWindow,
            description: "idle".to_owned(),
            evidence: std::collections::BTreeMap::new(),
            proposed_diff: std::collections::BTreeMap::new(),
            schedule: None,
        }];
        let (system, user) = build_prompt("demo/web", &config, &candidates, 30.0);
        assert!(system.contains("Kubernetes autoscaling expert"));
        assert!(user.contains("demo/web"));
        assert!(user.contains("idle_window"));
    }

    #[test]
    fn prompt_prices_savings_with_the_configured_replica_cost() {
        let config = WorkloadConfig {
            min_replicas: 10,
            max_replicas: 40,
            target_cpu_pct: 70,
            scale_down_cooldown_s: 300,
        };
        let (system, _) = build_prompt("demo/web", &config, &[], 42.5);
        assert!(system.contains("$42.5/month"), "prompt must carry the configured cost: {system}");
    }
}
