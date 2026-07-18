use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::crd::{DiffEntry, ScheduleWindowSpec, TargetKind};

/// Prefix marking the KEDA cron triggers this operator manages, so a re-patch replaces only ours and
/// leaves the workload's real scalers (kafka/prometheus/...) untouched.
const KEDA_CRON_TRIGGER_PREFIX: &str = "openhpa-cron-";

/// Build the merge-patch body that applies a recommendation's config diff, mapping our field names
/// onto the target's spec (HPA `autoscaling/v2` or KEDA `ScaledObject`).
pub fn patch_for(kind: &TargetKind, config_diff: &BTreeMap<String, DiffEntry>) -> Value {
    build_patch(kind, config_diff, |d| d.to)
}

/// Build the inverse merge-patch that restores the pre-apply values (`from`) - the auto-rollback
/// safety net. The rollback target is free: it is already recorded in each `DiffEntry`.
pub fn revert_patch_for(kind: &TargetKind, config_diff: &BTreeMap<String, DiffEntry>) -> Value {
    build_patch(kind, config_diff, |d| d.from)
}

fn build_patch(
    kind: &TargetKind,
    config_diff: &BTreeMap<String, DiffEntry>,
    pick: impl Fn(&DiffEntry) -> i32,
) -> Value {
    match kind {
        TargetKind::HorizontalPodAutoscaler => hpa_patch(config_diff, pick),
        TargetKind::ScaledObject => scaled_object_patch(config_diff, pick),
    }
}

fn hpa_patch(config_diff: &BTreeMap<String, DiffEntry>, pick: impl Fn(&DiffEntry) -> i32) -> Value {
    let mut spec = Map::new();
    if let Some(d) = config_diff.get("min_replicas") {
        spec.insert("minReplicas".to_owned(), json!(pick(d)));
    }
    if let Some(d) = config_diff.get("max_replicas") {
        spec.insert("maxReplicas".to_owned(), json!(pick(d)));
    }
    if let Some(d) = config_diff.get("target_cpu_pct") {
        spec.insert(
            "metrics".to_owned(),
            json!([{
                "type": "Resource",
                "resource": {
                    "name": "cpu",
                    "target": {"type": "Utilization", "averageUtilization": pick(d)},
                },
            }]),
        );
    }
    if let Some(d) = config_diff.get("scale_down_cooldown_s") {
        spec.insert(
            "behavior".to_owned(),
            json!({"scaleDown": {"stabilizationWindowSeconds": pick(d)}}),
        );
    }
    json!({ "spec": spec })
}

fn scaled_object_patch(
    config_diff: &BTreeMap<String, DiffEntry>,
    pick: impl Fn(&DiffEntry) -> i32,
) -> Value {
    let mut spec = Map::new();
    if let Some(d) = config_diff.get("min_replicas") {
        spec.insert("minReplicaCount".to_owned(), json!(pick(d)));
    }
    if let Some(d) = config_diff.get("max_replicas") {
        spec.insert("maxReplicaCount".to_owned(), json!(pick(d)));
    }
    if let Some(d) = config_diff.get("scale_down_cooldown_s") {
        spec.insert("cooldownPeriod".to_owned(), json!(pick(d)));
    }
    json!({ "spec": spec })
}

/// Build the ScaledObject merge-patch that installs the proactive schedule as KEDA `cron` triggers -
/// KEDA does the time-based scaling, the operator doesn't babysit. The workload's existing non-cron
/// (and other-owner) triggers are preserved; only our own cron triggers are replaced.
pub fn schedule_patch_for_keda(
    existing_triggers: &[Value],
    schedule: &[ScheduleWindowSpec],
) -> Value {
    let mut triggers: Vec<Value> =
        existing_triggers.iter().filter(|trigger| !is_managed_cron(trigger)).cloned().collect();
    triggers.extend(schedule.iter().enumerate().map(|(index, window)| cron_trigger(index, window)));
    json!({ "spec": { "triggers": triggers } })
}

/// Whether the live triggers already carry exactly the managed cron triggers this schedule wants
/// (ignoring the workload's own triggers, which we never touch) - so the controller can skip a
/// no-op patch every tick.
pub fn keda_schedule_in_sync(existing_triggers: &[Value], schedule: &[ScheduleWindowSpec]) -> bool {
    let live: Vec<&Value> = existing_triggers.iter().filter(|t| is_managed_cron(t)).collect();
    let desired: Vec<Value> =
        schedule.iter().enumerate().map(|(index, window)| cron_trigger(index, window)).collect();
    live.len() == desired.len() && live.iter().zip(&desired).all(|(live, want)| *live == want)
}

fn is_managed_cron(trigger: &Value) -> bool {
    trigger
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name.starts_with(KEDA_CRON_TRIGGER_PREFIX))
}

fn cron_trigger(index: usize, window: &ScheduleWindowSpec) -> Value {
    json!({
        "type": "cron",
        "name": format!("{KEDA_CRON_TRIGGER_PREFIX}{index}"),
        "metadata": {
            "timezone": "UTC",
            "start": window.start_cron,
            "end": end_cron(&window.start_cron, window.duration_minutes),
            "desiredReplicas": window.min_replicas.to_string(),
        },
    })
}

/// Derive the KEDA cron `end` from a window's `start` cron + duration, clamped to the same day
/// (sub-day windows; a window running to midnight ends at 23:59).
fn end_cron(start_cron: &str, duration_minutes: i32) -> String {
    let fields: Vec<&str> = start_cron.split_whitespace().collect();
    let minute: i32 = fields.first().and_then(|f| f.parse().ok()).unwrap_or(0);
    let hour: i32 = fields.get(1).and_then(|f| f.parse().ok()).unwrap_or(0);
    let day_of_week = fields.get(4).copied().unwrap_or("*");
    let end = (hour * 60 + minute + duration_minutes).min(23 * 60 + 59);
    format!("{} {} * * {}", end % 60, end / 60, day_of_week)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(pairs: &[(&str, i32, i32)]) -> BTreeMap<String, DiffEntry> {
        pairs
            .iter()
            .map(|(k, from, to)| (k.to_string(), DiffEntry { from: *from, to: *to }))
            .collect()
    }

    #[test]
    fn hpa_patch_maps_min_and_target() {
        let patch = patch_for(
            &TargetKind::HorizontalPodAutoscaler,
            &diff(&[("min_replicas", 10, 3), ("target_cpu_pct", 70, 85)]),
        );
        assert_eq!(patch["spec"]["minReplicas"], json!(3));
        assert_eq!(
            patch["spec"]["metrics"][0]["resource"]["target"]["averageUtilization"],
            json!(85)
        );
    }

    #[test]
    fn scaled_object_patch_uses_keda_fields() {
        let patch = patch_for(
            &TargetKind::ScaledObject,
            &diff(&[("min_replicas", 10, 3), ("scale_down_cooldown_s", 300, 600)]),
        );
        assert_eq!(patch["spec"]["minReplicaCount"], json!(3));
        assert_eq!(patch["spec"]["cooldownPeriod"], json!(600));
    }

    #[test]
    fn revert_hpa_patch_restores_from_values() {
        let diff = diff(&[("min_replicas", 10, 3), ("target_cpu_pct", 70, 85)]);
        let patch = revert_patch_for(&TargetKind::HorizontalPodAutoscaler, &diff);
        assert_eq!(patch["spec"]["minReplicas"], json!(10));
        assert_eq!(
            patch["spec"]["metrics"][0]["resource"]["target"]["averageUtilization"],
            json!(70)
        );
    }

    #[test]
    fn revert_scaled_object_patch_restores_from_values() {
        let diff = diff(&[("min_replicas", 10, 3), ("scale_down_cooldown_s", 300, 600)]);
        let patch = revert_patch_for(&TargetKind::ScaledObject, &diff);
        assert_eq!(patch["spec"]["minReplicaCount"], json!(10));
        assert_eq!(patch["spec"]["cooldownPeriod"], json!(300));
    }

    fn window(start_cron: &str, duration_minutes: i32, min_replicas: i32) -> ScheduleWindowSpec {
        ScheduleWindowSpec { start_cron: start_cron.to_owned(), duration_minutes, min_replicas }
    }

    #[test]
    fn keda_schedule_emits_cron_trigger_with_derived_end() {
        let patch = schedule_patch_for_keda(&[], &[window("0 9 * * 1-5", 480, 8)]);
        let trigger = &patch["spec"]["triggers"][0];
        assert_eq!(trigger["type"], json!("cron"));
        assert_eq!(trigger["metadata"]["start"], json!("0 9 * * 1-5"));
        assert_eq!(trigger["metadata"]["end"], json!("0 17 * * 1-5"));
        assert_eq!(trigger["metadata"]["desiredReplicas"], json!("8"));
    }

    #[test]
    fn keda_schedule_preserves_real_triggers_and_replaces_managed_ones() {
        let existing = vec![
            json!({"type": "kafka", "name": "main", "metadata": {}}),
            json!({"type": "cron", "name": "openhpa-cron-0", "metadata": {}}),
        ];
        let patch = schedule_patch_for_keda(&existing, &[window("0 6 * * *", 120, 5)]);
        let triggers = patch["spec"]["triggers"].as_array().unwrap();
        assert_eq!(triggers.len(), 2, "{triggers:?}");
        assert!(triggers.iter().any(|t| t["type"] == json!("kafka")));
        let managed: Vec<&Value> = triggers
            .iter()
            .filter(|t| t["name"].as_str().is_some_and(|n| n.starts_with("openhpa-cron-")))
            .collect();
        assert_eq!(managed.len(), 1);
        assert_eq!(managed[0]["metadata"]["desiredReplicas"], json!("5"));
    }

    #[test]
    fn keda_in_sync_detects_when_a_patch_is_needed() {
        let schedule = [window("0 9 * * 1-5", 480, 8)];
        // No managed cron yet -> a patch is needed.
        let kafka = json!({"type": "kafka", "name": "main"});
        assert!(!keda_schedule_in_sync(std::slice::from_ref(&kafka), &schedule));
        // Already-applied managed cron alongside the real trigger -> in sync, no patch.
        let applied = schedule_patch_for_keda(std::slice::from_ref(&kafka), &schedule);
        let triggers: Vec<Value> = applied["spec"]["triggers"].as_array().unwrap().clone();
        assert!(keda_schedule_in_sync(&triggers, &schedule));
        // A different desired floor is out of sync.
        assert!(!keda_schedule_in_sync(&triggers, &[window("0 9 * * 1-5", 480, 9)]));
    }
}
