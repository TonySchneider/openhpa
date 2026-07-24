# 8. Security

OpenHPA is designed for a security-conscious platform team: minimal privilege, no data exfiltration,
and a signed supply chain.

## 8.1 RBAC - least privilege

The chart installs one `ClusterRole` (and a `ClusterRoleBinding` to the operator's service account).
Every rule and its justification:

| API group | Resources | Verbs | Why |
| --- | --- | --- | --- |
| `autoscaling` | `horizontalpodautoscalers` | `get, list, watch, patch, update` | Read and tune HPAs. One of only two mutating grants. |
| `keda.sh` | `scaledobjects` | `get, list, watch, patch, update` | Read and tune KEDA ScaledObjects. The other mutating grant. |
| `apps` | `deployments` | `get, list, watch` | Read-only context for the workload behind an HPA. |
| `metrics.k8s.io` | `pods, nodes` | `get, list` | Read live utilization metrics. Read-only. |
| `openhpa.dev` | `scalingrecommendations`, `scalingrecommendations/status` | `get, list, watch, create, update, patch, delete` | Own the recommendation CRs the operator itself creates. |
| `coordination.k8s.io` | `leases` | `get, create, update, patch` | Leader election so `replicaCount: 2+` never double-applies. |

The operator's **only** write access to your workloads is to HPAs and ScaledObjects, and only in
`apply` mode. It cannot modify Deployments or touch workload pods.

**No access to Secrets.** OpenHPA requests no `secrets` RBAC at all. The only sensitive value (an
optional LLM API key) is injected into the pod as an environment variable by the Deployment spec -
the operator never reads it, or any other Secret, through the Kubernetes API.

### Narrowing the blast radius

- Set `watchNamespaces` to the specific namespaces you want tuned. (The ClusterRole is
  cluster-scoped because HPAs may live anywhere; the operator only acts on the namespaces you list.)
- Run in the default `recommend` mode to remove the two mutating grants from the operator's
  effective behaviour entirely - it becomes a read-only advisor.

## 8.2 Data residency

- All metric collection, analysis, apply, and rollback happen **in your cluster**. No workload
  metrics, configuration, or cluster identifiers are sent anywhere by OpenHPA itself.
- The operator keeps no external datastore. Metric history lives in memory and is rebuilt from your
  Prometheus (or HPA status) each run; recommendations live as CRs in your cluster.
- There is no telemetry, no license server, and no phone-home of any kind.

## 8.3 Egress

The operator makes **no outbound calls** except, optionally, to the LLM provider you configure:

| Provider setting | Outbound calls |
| --- | --- |
| `llm.provider=none` (default) | None. Fully air-gapped. |
| `llm.provider=openai` | HTTPS to `api.openai.com` (or your `llm.baseUrl`), carrying the analysis prompt and your API key. |
| `llm.provider=anthropic` | HTTPS to `api.anthropic.com` (or your `llm.baseUrl`), same. |

If your security policy forbids any egress, run with `llm.provider=none`; the rule engine still
produces recommendations deterministically, and no LLM decision is ever in the safety path.

> When a cloud LLM provider is configured, the analysis prompt includes your workload's autoscaling
> configuration and summarized metric statistics (no Secrets, no raw logs). Review your provider's
> data handling, point `llm.baseUrl` at an in-cluster proxy or local model, or use
> `llm.provider=none`, if that is sensitive in your environment.

## 8.4 Supply chain and runtime hardening

- **Signed image + SBOM.** Every release image is cosign-signed via keyless OIDC (no long-lived
  signing keys) and ships an attested SPDX SBOM. Verify both before install - see
  [Installation §3.2](./03-installation.md).
- **Distroless, non-root.** The image is a distroless base with no shell or package manager. The
  container runs as a non-root user with `readOnlyRootFilesystem: true`,
  `allowPrivilegeEscalation: false`, and all Linux capabilities dropped.
- **Small, memory-safe binary.** OpenHPA is a single Rust binary; the source is open under
  Apache-2.0 and auditable.
