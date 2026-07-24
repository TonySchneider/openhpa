# 2. Requirements

Confirm the following before installing. Items marked *optional* are needed only for the
feature noted.

## 2.1 Cluster

| Requirement | Detail |
| --- | --- |
| Kubernetes | **v1.27 or newer** (the chart sets `kubeVersion: ">=1.27.0-0"`). Works on EKS, GKE, AKS, and self-managed / on-prem clusters. |
| Architecture | The image is built for the cluster node architecture you mirror it to; the default published image is `linux/amd64`. |
| Cluster-admin (install time) | Installing the CRD and the `ClusterRole` / `ClusterRoleBinding` requires cluster-admin. Day-2 operation does not. |

## 2.2 Metrics source

The operator needs workload metrics to analyze. One of the following is required; the first
is strongly recommended.

| Source | Role | Notes |
| --- | --- | --- |
| **Prometheus** *(recommended)* | Real metric history | Backfills weeks of history per workload on every tick and survives operator restarts. Configure with `metrics.prometheusUrl`. Required for predictive forecasting. |
| **metrics-server** | Live utilization | Required for HPAs to function at all, and used by the operator's HPA-status fallback when no Prometheus URL is set. The fallback rebuilds history slowly (one sample per tick), so recommendations take longer to appear. |

Default PromQL queries assume `kube-state-metrics` and cAdvisor metrics are present. If your
Prometheus uses different metric names, override the queries (see
[Configuration reference](./04-configuration-reference.md)).

## 2.3 Optional components

| Component | When you need it |
| --- | --- |
| **KEDA** | Only if you want the operator to tune or schedule KEDA `ScaledObject`s. Not required for plain HPA workloads. |
| **LLM provider account** | Only if you set `llm.provider` to `openai` or `anthropic`. The operator uses *your* key. With `llm.provider=none` the operator runs a deterministic rules-only analysis and needs no LLM. |

## 2.4 Permissions (RBAC)

The chart installs a `ClusterRole` granting:

- read/write on `autoscaling/horizontalpodautoscalers` and `keda.sh/scaledobjects` (the only
  mutating access, used only in apply mode),
- read-only on `apps/deployments` and `metrics.k8s.io/{pods,nodes}`,
- full management of `openhpa.dev/scalingrecommendations`,
- `coordination.k8s.io/leases` for leader election.

OpenHPA needs **no access to Kubernetes Secrets** - the optional LLM key is injected as an
environment variable by the pod spec, never read via the API. The exact rules and the rationale for
each are in [Security](./08-security.md).

## 2.5 Network egress

| Destination | Required? |
| --- | --- |
| LLM provider API (`api.openai.com` / `api.anthropic.com`) | Only with a cloud LLM provider configured. |
| Any OpenHPA-operated endpoint | **Never.** No telemetry, no license server, no phone-home. |

For an air-gapped install, the only egress to plan for is the optional LLM call - omit it
(`llm.provider=none`) and the operator needs no outbound network access.

## 2.6 Resource footprint

The operator is a single low-footprint Rust binary. The chart's defaults:

| | CPU | Memory |
| --- | --- | --- |
| Requests | `50m` | `64Mi` |
| Limits | `250m` | `128Mi` |

These suit small-to-medium clusters; raise the limits if you watch a very large number of
workloads. (No local model is bundled, so there is no large RAM requirement.)

## 2.7 Tooling on the install host

| Tool | Version | Used for |
| --- | --- | --- |
| `helm` | 3.8+ | OCI chart support (`helm install … oci://…`). |
| `kubectl` | matching your cluster | Approving recommendations, inspecting status. |
| `cosign` | 2.x | Verifying the image signature before install. |
| `crane` / `skopeo` / `oras` | any recent | Mirroring the image and chart for an air-gapped install. |
