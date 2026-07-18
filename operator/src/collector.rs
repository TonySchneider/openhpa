use chrono::{DateTime, Utc};
use k8s_openapi::api::autoscaling::v2::{HorizontalPodAutoscaler, HorizontalPodAutoscalerSpec};
use kube::core::DynamicObject;
use openhpa_core::{MetricPoint, WorkloadConfig};
use serde_json::Value;

/// Read the live autoscaler config from an HPA spec. Missing fields fall back to sane defaults.
pub fn workload_config(hpa: &HorizontalPodAutoscaler) -> WorkloadConfig {
    let spec = hpa.spec.as_ref();
    WorkloadConfig {
        min_replicas: spec.and_then(|s| s.min_replicas).unwrap_or(1),
        max_replicas: spec.map_or(1, |s| s.max_replicas),
        target_cpu_pct: spec.and_then(cpu_target).unwrap_or(80),
        scale_down_cooldown_s: spec.and_then(cooldown_seconds).unwrap_or(300),
    }
}

fn cpu_target(spec: &HorizontalPodAutoscalerSpec) -> Option<i32> {
    spec.metrics.as_ref()?.iter().find_map(|metric| {
        let resource = metric.resource.as_ref()?;
        (resource.name == "cpu").then_some(resource.target.average_utilization?)
    })
}

fn cooldown_seconds(spec: &HorizontalPodAutoscalerSpec) -> Option<i32> {
    spec.behavior.as_ref()?.scale_down.as_ref()?.stabilization_window_seconds
}

/// The Deployment (or other workload) name the HPA scales, used to target Prometheus queries.
pub fn scale_target_name(hpa: &HorizontalPodAutoscaler) -> Option<String> {
    hpa.spec.as_ref().map(|spec| spec.scale_target_ref.name.clone())
}

/// Whether this HPA is created and managed by KEDA for a ScaledObject. The HPA collector skips
/// those: the ScaledObject itself is analyzed (and patched) instead, so reading both would emit
/// two competing recommendations for one workload - and patching a KEDA-managed HPA directly
/// would be fought by the KEDA operator.
pub fn is_keda_managed(hpa: &HorizontalPodAutoscaler) -> bool {
    hpa.metadata
        .owner_references
        .as_ref()
        .is_some_and(|owners| owners.iter().any(|owner| owner.kind == "ScaledObject"))
}

/// Read the live autoscaler config from a KEDA ScaledObject spec. KEDA's defaults differ from the
/// HPA's (minReplicaCount 0, maxReplicaCount 100, cooldownPeriod 300); the CPU target comes from a
/// `cpu` trigger's `metadata.value` (a string in KEDA), defaulting to 80 like the HPA path.
pub fn scaled_object_config(scaled_object: &DynamicObject) -> WorkloadConfig {
    let spec = &scaled_object.data["spec"];
    WorkloadConfig {
        min_replicas: spec["minReplicaCount"].as_i64().map_or(0, |v| v as i32),
        max_replicas: spec["maxReplicaCount"].as_i64().map_or(100, |v| v as i32),
        target_cpu_pct: cpu_trigger_target(spec).unwrap_or(80),
        scale_down_cooldown_s: spec["cooldownPeriod"].as_i64().map_or(300, |v| v as i32),
    }
}

fn cpu_trigger_target(spec: &Value) -> Option<i32> {
    spec["triggers"]
        .as_array()?
        .iter()
        .find(|trigger| trigger["type"] == "cpu")
        .and_then(|trigger| trigger["metadata"]["value"].as_str())
        .and_then(|value| value.trim().parse().ok())
}

/// The workload name a ScaledObject scales (`spec.scaleTargetRef.name`), used for metric queries.
pub fn scaled_object_target_name(scaled_object: &DynamicObject) -> Option<String> {
    scaled_object.data["spec"]["scaleTargetRef"]["name"].as_str().map(str::to_owned)
}

/// Derive a single current metric point from the HPA status (utilization is reported 0-100).
pub fn current_point(hpa: &HorizontalPodAutoscaler, now: DateTime<Utc>) -> Option<MetricPoint> {
    let status = hpa.status.as_ref()?;
    let replicas = status.current_replicas.unwrap_or(0);
    let cpu_pct = status.current_metrics.as_ref()?.iter().find_map(|metric| {
        let resource = metric.resource.as_ref()?;
        (resource.name == "cpu").then_some(resource.current.average_utilization?)
    })?;
    Some(MetricPoint {
        timestamp: now,
        cpu_util: f64::from(cpu_pct) / 100.0,
        replicas,
        queue_depth: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hpa_from(json: serde_json::Value) -> HorizontalPodAutoscaler {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn reads_config_from_spec() {
        let hpa = hpa_from(serde_json::json!({
            "apiVersion": "autoscaling/v2",
            "kind": "HorizontalPodAutoscaler",
            "metadata": {"name": "web", "namespace": "demo"},
            "spec": {
                "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"},
                "minReplicas": 10,
                "maxReplicas": 40,
                "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 70}}}],
                "behavior": {"scaleDown": {"stabilizationWindowSeconds": 300}}
            }
        }));
        let config = workload_config(&hpa);
        assert_eq!(config.min_replicas, 10);
        assert_eq!(config.max_replicas, 40);
        assert_eq!(config.target_cpu_pct, 70);
        assert_eq!(config.scale_down_cooldown_s, 300);
    }

    #[test]
    fn defaults_when_fields_absent() {
        let hpa = hpa_from(serde_json::json!({
            "apiVersion": "autoscaling/v2",
            "kind": "HorizontalPodAutoscaler",
            "metadata": {"name": "web", "namespace": "demo"},
            "spec": {"scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"}, "maxReplicas": 5}
        }));
        let config = workload_config(&hpa);
        assert_eq!(config.min_replicas, 1);
        assert_eq!(config.max_replicas, 5);
        assert_eq!(config.target_cpu_pct, 80);
    }

    fn scaled_object_from(spec: serde_json::Value) -> kube::core::DynamicObject {
        use kube::core::{ApiResource, GroupVersionKind};
        let ar =
            ApiResource::from_gvk(&GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject"));
        let mut object = kube::core::DynamicObject::new("web", &ar);
        object.data = serde_json::json!({ "spec": spec });
        object
    }

    #[test]
    fn reads_scaled_object_config_from_spec() {
        let so = scaled_object_from(serde_json::json!({
            "scaleTargetRef": {"name": "web-deploy"},
            "minReplicaCount": 10,
            "maxReplicaCount": 40,
            "cooldownPeriod": 600,
            "triggers": [
                {"type": "kafka", "name": "main", "metadata": {"lagThreshold": "100"}},
                {"type": "cpu", "metricType": "Utilization", "metadata": {"value": "60"}}
            ]
        }));
        let config = scaled_object_config(&so);
        assert_eq!(config.min_replicas, 10);
        assert_eq!(config.max_replicas, 40);
        assert_eq!(config.target_cpu_pct, 60, "the cpu trigger's string value is parsed");
        assert_eq!(config.scale_down_cooldown_s, 600);
        assert_eq!(scaled_object_target_name(&so).as_deref(), Some("web-deploy"));
    }

    #[test]
    fn scaled_object_defaults_match_keda_not_the_hpa() {
        let so = scaled_object_from(serde_json::json!({"scaleTargetRef": {"name": "web"}}));
        let config = scaled_object_config(&so);
        assert_eq!(config.min_replicas, 0, "KEDA minReplicaCount defaults to 0");
        assert_eq!(config.max_replicas, 100, "KEDA maxReplicaCount defaults to 100");
        assert_eq!(config.target_cpu_pct, 80, "no cpu trigger falls back to the HPA-style default");
        assert_eq!(config.scale_down_cooldown_s, 300);
    }

    #[test]
    fn unparsable_cpu_trigger_value_falls_back_to_default() {
        let so = scaled_object_from(serde_json::json!({
            "scaleTargetRef": {"name": "web"},
            "triggers": [{"type": "cpu", "metadata": {"value": "not-a-number"}}]
        }));
        assert_eq!(scaled_object_config(&so).target_cpu_pct, 80);
    }

    #[test]
    fn keda_managed_hpa_is_detected_by_owner_reference() {
        let managed = hpa_from(serde_json::json!({
            "apiVersion": "autoscaling/v2",
            "kind": "HorizontalPodAutoscaler",
            "metadata": {
                "name": "keda-hpa-web", "namespace": "demo",
                "ownerReferences": [{"apiVersion": "keda.sh/v1alpha1", "kind": "ScaledObject", "name": "web", "uid": "u1", "controller": true}]
            },
            "spec": {"scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"}, "maxReplicas": 5}
        }));
        assert!(is_keda_managed(&managed));

        let standalone = hpa_from(serde_json::json!({
            "apiVersion": "autoscaling/v2",
            "kind": "HorizontalPodAutoscaler",
            "metadata": {
                "name": "web", "namespace": "demo",
                "ownerReferences": [{"apiVersion": "apps/v1", "kind": "Deployment", "name": "web", "uid": "u2"}]
            },
            "spec": {"scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"}, "maxReplicas": 5}
        }));
        assert!(
            !is_keda_managed(&standalone),
            "a non-ScaledObject owner must not trigger the skip"
        );
    }
}
