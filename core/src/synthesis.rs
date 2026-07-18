use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::domain::{
    Candidate, CandidateKind, ConfigDiff, LlmAnalysisOutput, LlmRecommendationItem,
    SynthesizedRecommendation,
};

/// Deterministic monthly-savings estimate from a candidate's `min_replicas` reduction, used when the
/// LLM did not supply a figure of its own. `None` for non-floor-cut diffs or zero/negative cuts.
fn floor_savings(candidate: &Candidate, cost_per_replica_usd_monthly: f64) -> Option<f64> {
    let cut = candidate.proposed_diff.get("min_replicas")?;
    (cut.from > cut.to).then(|| f64::from(cut.from - cut.to) * cost_per_replica_usd_monthly)
}

/// Merge the rule candidates the LLM approved into one recommendation.
/// `cost_per_replica_usd_monthly` prices the deterministic floor-cut savings estimate.
pub fn synthesize(
    candidates: &[Candidate],
    output: &LlmAnalysisOutput,
    cost_per_replica_usd_monthly: f64,
) -> SynthesizedRecommendation {
    let by_kind: BTreeMap<CandidateKind, &LlmRecommendationItem> =
        output.recommendations.iter().map(|item| (item.candidate_kind, item)).collect();

    let applied: Vec<(&Candidate, &LlmRecommendationItem)> = candidates
        .iter()
        .filter_map(|candidate| {
            by_kind.get(&candidate.kind).filter(|item| item.apply).map(|item| (candidate, *item))
        })
        .collect();

    let mut config_diff = ConfigDiff::new();
    for (candidate, _) in &applied {
        for (field, change) in &candidate.proposed_diff {
            config_diff.insert(field.clone(), *change);
        }
    }

    let savings: Vec<f64> = applied
        .iter()
        .filter_map(|(candidate, item)| {
            item.projected_savings_usd_monthly
                .or_else(|| floor_savings(candidate, cost_per_replica_usd_monthly))
        })
        .collect();
    let projected_savings_usd_monthly: Option<f64> =
        (!savings.is_empty()).then(|| savings.iter().sum());

    let mut summary_md = output.executive_summary.clone();
    for (candidate, item) in &applied {
        let _ = write!(
            summary_md,
            "\n\n- **{}** ({} risk): {}",
            candidate.kind.as_str(),
            item.risk.as_str(),
            item.reasoning
        );
    }
    if projected_savings_usd_monthly.is_some() {
        let _ = write!(
            summary_md,
            "\n\n_Projected savings are an estimate priced at \
             ${cost_per_replica_usd_monthly}/replica/month (configurable: \
             `--cost-per-replica-usd-monthly`)._"
        );
    }

    SynthesizedRecommendation {
        config_diff,
        summary_md,
        risk_level: output.overall_risk,
        projected_savings_usd_monthly,
        schedule: applied.iter().find_map(|(candidate, _)| candidate.schedule.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ConfigDiffEntry, RiskLevel};

    fn candidate(kind: CandidateKind, field: &str, from: i32, to: i32) -> Candidate {
        Candidate {
            kind,
            description: String::new(),
            evidence: BTreeMap::new(),
            proposed_diff: BTreeMap::from([(field.to_owned(), ConfigDiffEntry::new(from, to))]),
            schedule: None,
        }
    }

    fn item(kind: CandidateKind, apply: bool, savings: Option<f64>) -> LlmRecommendationItem {
        LlmRecommendationItem {
            candidate_kind: kind,
            apply,
            reasoning: "because".to_owned(),
            risk: RiskLevel::Low,
            projected_savings_usd_monthly: savings,
        }
    }

    #[test]
    fn keeps_only_approved_and_sums_savings() {
        let candidates = vec![
            candidate(CandidateKind::IdleWindow, "min_replicas", 10, 3),
            candidate(CandidateKind::Overprovisioned, "target_cpu_pct", 70, 85),
        ];
        let output = LlmAnalysisOutput {
            recommendations: vec![
                item(CandidateKind::IdleWindow, true, Some(1000.0)),
                item(CandidateKind::Overprovisioned, false, Some(500.0)),
            ],
            overall_risk: RiskLevel::Low,
            executive_summary: "summary".to_owned(),
        };

        let rec = synthesize(&candidates, &output, 30.0);
        assert_eq!(rec.config_diff.len(), 1);
        assert_eq!(*rec.config_diff.get("min_replicas").unwrap(), ConfigDiffEntry::new(10, 3));
        assert_eq!(rec.projected_savings_usd_monthly, Some(1000.0));
        assert_eq!(rec.risk_level, RiskLevel::Low);
        assert!(rec.summary_md.contains("idle_window"));
    }

    #[test]
    fn floor_cut_gets_deterministic_savings_when_llm_gives_none() {
        // A min_replicas cut the LLM approved but left without a savings figure: synthesis fills a
        // deterministic estimate (3 replicas saved * $30) so the floor cut never shows $0.
        let candidates = vec![candidate(CandidateKind::Overprovisioned, "min_replicas", 4, 1)];
        let output = LlmAnalysisOutput {
            recommendations: vec![item(CandidateKind::Overprovisioned, true, None)],
            overall_risk: RiskLevel::Medium,
            executive_summary: "summary".to_owned(),
        };
        assert_eq!(
            synthesize(&candidates, &output, 30.0).projected_savings_usd_monthly,
            Some(90.0)
        );
    }

    #[test]
    fn floor_cut_savings_scale_linearly_with_the_replica_cost() {
        // Same 4 -> 1 cut priced at $100/replica/month: 3 x 100. The flag, not a constant,
        // drives the estimate.
        let candidates = vec![candidate(CandidateKind::Overprovisioned, "min_replicas", 4, 1)];
        let output = LlmAnalysisOutput {
            recommendations: vec![item(CandidateKind::Overprovisioned, true, None)],
            overall_risk: RiskLevel::Medium,
            executive_summary: "summary".to_owned(),
        };
        assert_eq!(
            synthesize(&candidates, &output, 100.0).projected_savings_usd_monthly,
            Some(300.0)
        );
    }

    #[test]
    fn savings_are_labeled_as_an_estimate_in_the_summary() {
        let candidates = vec![candidate(CandidateKind::Overprovisioned, "min_replicas", 4, 1)];
        let output = LlmAnalysisOutput {
            recommendations: vec![item(CandidateKind::Overprovisioned, true, None)],
            overall_risk: RiskLevel::Medium,
            executive_summary: "summary".to_owned(),
        };
        let with_savings = synthesize(&candidates, &output, 30.0);
        assert!(
            with_savings.summary_md.contains("estimate priced at $30/replica/month"),
            "summary must label the savings figure: {}",
            with_savings.summary_md
        );

        // No savings figure -> no estimate disclaimer.
        let no_savings = synthesize(
            &[candidate(CandidateKind::Overprovisioned, "target_cpu_pct", 70, 85)],
            &output,
            30.0,
        );
        assert!(
            !no_savings.summary_md.contains("estimate"),
            "no savings, no disclaimer: {}",
            no_savings.summary_md
        );
    }

    #[test]
    fn target_bump_has_no_deterministic_savings() {
        // A non-floor-cut diff with no LLM figure stays None (no fabricated savings).
        let candidates = vec![candidate(CandidateKind::Overprovisioned, "target_cpu_pct", 70, 85)];
        let output = LlmAnalysisOutput {
            recommendations: vec![item(CandidateKind::Overprovisioned, true, None)],
            overall_risk: RiskLevel::Low,
            executive_summary: "summary".to_owned(),
        };
        assert_eq!(synthesize(&candidates, &output, 30.0).projected_savings_usd_monthly, None);
    }

    #[test]
    fn no_approved_items_yields_empty_diff_and_no_savings() {
        let candidates = vec![candidate(CandidateKind::IdleWindow, "min_replicas", 10, 3)];
        let output = LlmAnalysisOutput {
            recommendations: vec![item(CandidateKind::IdleWindow, false, Some(1000.0))],
            overall_risk: RiskLevel::Medium,
            executive_summary: "summary".to_owned(),
        };

        let rec = synthesize(&candidates, &output, 30.0);
        assert!(rec.config_diff.is_empty());
        assert_eq!(rec.projected_savings_usd_monthly, None);
    }

    #[test]
    fn carries_schedule_from_approved_predictable_peak() {
        let mut peak = candidate(CandidateKind::PredictablePeak, "min_replicas", 10, 2);
        peak.schedule = Some(vec![crate::domain::ScheduleWindow {
            start_cron: "0 9 * * *".to_owned(),
            duration_minutes: 480,
            min_replicas: 8,
        }]);
        let output = LlmAnalysisOutput {
            recommendations: vec![item(CandidateKind::PredictablePeak, true, None)],
            overall_risk: RiskLevel::Low,
            executive_summary: "summary".to_owned(),
        };

        let rec = synthesize(&[peak], &output, 30.0);
        let schedule = rec.schedule.expect("schedule carried through");
        assert_eq!(schedule.len(), 1);
        assert_eq!(schedule[0].min_replicas, 8);
    }

    #[test]
    fn drops_schedule_when_candidate_not_approved() {
        let mut peak = candidate(CandidateKind::PredictablePeak, "min_replicas", 10, 2);
        peak.schedule = Some(vec![crate::domain::ScheduleWindow {
            start_cron: "0 9 * * *".to_owned(),
            duration_minutes: 480,
            min_replicas: 8,
        }]);
        let output = LlmAnalysisOutput {
            recommendations: vec![item(CandidateKind::PredictablePeak, false, None)],
            overall_risk: RiskLevel::Low,
            executive_summary: "summary".to_owned(),
        };
        assert!(synthesize(&[peak], &output, 30.0).schedule.is_none());
    }
}
