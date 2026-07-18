# OpenHPA

**Open-source HPA & KEDA tuning for Kubernetes.**

OpenHPA analyzes how your Kubernetes HorizontalPodAutoscalers and KEDA ScaledObjects behave over
time and recommends safer, more efficient autoscaling configuration — `minReplicas`, `maxReplicas`,
CPU utilization targets, and scale-down/cooldown settings. Every recommendation is backed by
measurable evidence from your own metrics, written as a Kubernetes custom resource you read with
`kubectl`.

It runs **entirely in your cluster**. In its default mode it is **read-only** and makes **no
outbound network calls** — it reads metrics, writes recommendations, and stops there. You decide
whether to apply a change (by hand, through GitOps, or by letting OpenHPA apply it behind a
probation + auto-rollback safety net).

- **Deterministic first.** A rule engine, not a model, makes every recommendation and every safety
  decision. It is fully explainable and reproducible.
- **Optional LLM, off by default.** If you configure a provider and your own API key, an LLM adds a
  plain-language explanation and risk narrative on top of the deterministic result. It is never
  required, and it is never in the safety path.
- **No licensing, no tiers, no telemetry.** Apache-2.0. Everything works out of the box.

> **Scope.** OpenHPA tunes *horizontal scaling behaviour* (how many replicas, and when). It does
> **not** rightsize CPU/memory *requests and limits* — for that, a tool like
> [KRR](https://github.com/robusta-dev/krr) is a good complement. The two solve different problems
> and compose well.

---

## Table of contents

- [How it works](#how-it-works)
- [What evidence backs a recommendation](#what-evidence-backs-a-recommendation)
- [Quickstart (read-only)](#quickstart-read-only)
- [Applying changes safely](#applying-changes-safely)
- [What OpenHPA reads, writes, and sends](#what-openhpa-reads-writes-and-sends)
- [Kubernetes permissions (RBAC)](#kubernetes-permissions-rbac)
- [Rules-only vs. optional LLM](#rules-only-vs-optional-llm)
- [Relationship with GitOps (Argo CD / Flux)](#relationship-with-gitops-argo-cd--flux)
- [Current limitations](#current-limitations)
- [Development](#development)
- [Contributing](#contributing)
- [Security](#security)
- [Maintainer](#maintainer)
- [License](#license)

---

## How it works

```
watch HPA / ScaledObject + read metrics
        │
        ▼
deterministic rule engine  ──►  candidate changes (idle floor, overprovisioned, thrashing, scale lag)
        │
        ▼
(optional) LLM explanation using your own key — narrative + risk, never a safety gate
        │
        ▼
ScalingRecommendation CRD   ──►  you review with `kubectl` (status: pending)
        │
        ▼
approve  ──►  [apply mode] operator patches the target ──► probation window ──► verify health
                                                                                   │
                                                                          healthy? keep : auto-rollback
```

1. **Watch.** The operator watches HPAs and KEDA ScaledObjects in the namespaces you configure and
   reads their live spec (min/max replicas, target, cooldown).
2. **Collect.** It reads autoscaling metrics in-cluster over a rolling window — from **Prometheus**
   (weeks of real history via `query_range`, survives restarts) or, as a fallback, by accumulating
   the **metrics-server / HPA status** each tick.
3. **Analyze.** A deterministic rule engine detects candidates: idle floors, overprovisioning,
   thrashing, and scaling lag. An optional seasonal forecaster (off by default) detects recurring
   daily/weekly peaks.
4. **Recommend.** Results are written as `ScalingRecommendation` custom resources with
   `status: pending`, viewable with `kubectl get scalerec -A`.
5. **Approve.** A human sets `approved: true` (via `kubectl patch` or GitOps).
6. **Apply** *(only in `--mode=apply`)*. The operator patches the HPA / ScaledObject, holds the
   change on a **probation window**, then **verifies** health and **auto-rolls-back** if the
   workload degrades.

## What evidence backs a recommendation

Each recommendation records the concrete signal that produced it, so you can judge it without
trusting a black box:

- **Idle floor** — the p95 of observed demand over the window implies a much lower `minReplicas`
  than is configured; the diff shows the current vs. proposed floor.
- **Overprovisioned** — sustained CPU well under the target with headroom to raise the utilization
  target.
- **Thrashing** — frequent scale up/down oscillation that a longer cooldown would damp.
- **Scale lag** — the workload repeatedly saturates before scaling catches up (a target or floor
  adjustment).

The proposed change is expressed as a field-level diff (e.g. `min_replicas: 10 → 3`) plus an
estimated monthly cost delta derived from a per-replica cost you configure. Nothing is applied
until you approve it.

## Quickstart (read-only)

The image and chart are public — no registry login required. This install is **recommend-only** and
makes **no outbound calls**:

```bash
helm install openhpa oci://ghcr.io/tonyschneider/charts/openhpa \
  --namespace openhpa --create-namespace

# recommendations appear after a few reconcile ticks
kubectl get scalerec -A
kubectl get scalerec <name> -n <ns> -o yaml    # read the diff + evidence
```

For real, restart-surviving history, point OpenHPA at Prometheus:

```bash
helm upgrade openhpa oci://ghcr.io/tonyschneider/charts/openhpa \
  --reuse-values --set metrics.prometheusUrl=http://prometheus-server.monitoring:9090
```

Full install, air-gapped mirroring, and configuration are in [docs/](./docs/manual/) (rendered at
**https://openhpa.dev/docs**).

## Applying changes safely

By default OpenHPA never mutates a workload. To let it apply approved recommendations:

```bash
helm upgrade openhpa oci://ghcr.io/tonyschneider/charts/openhpa \
  --reuse-values --set mode=apply
# then approve a specific recommendation:
kubectl patch scalerec <name> -n <ns> --type merge -p '{"spec":{"approved":true}}'
```

When it applies a change to an HPA, OpenHPA:

1. patches the target and records `appliedAt` + a **probation window**;
2. after probation, judges post-apply health against the pre-apply baseline;
3. marks it `verified` if healthy, or **auto-reverts** to the exact prior config (`rolledBack`) if
   the workload degraded. Rollback runs regardless of mode — it only ever restores config OpenHPA
   itself set.

Leader election (a `coordination.k8s.io` Lease) makes `replicaCount: 2+` safe — only the leader
mutates.

## What OpenHPA reads, writes, and sends

| | Detail |
|---|---|
| **Reads** | HPA & ScaledObject specs/status; Deployment metadata; CPU/replica/queue metrics from Prometheus or metrics-server; its own `ScalingRecommendation` CRDs. |
| **Writes** | `ScalingRecommendation` CRDs (always); HPA/ScaledObject `spec` patches (**only** in `--mode=apply`, **only** for approved recommendations); a leader-election Lease. |
| **Sends off-cluster** | **Nothing by default.** Only if you enable the optional LLM: a per-workload summary (config + metric candidates, no secrets) is sent to the provider you choose, using your own API key. See [docs/manual/08-security.md](./docs/manual/08-security.md). |

OpenHPA has no phone-home, no telemetry, and no license server. It works fully air-gapped.

## Kubernetes permissions (RBAC)

The chart installs a `ClusterRole` with least privilege. Notably, **OpenHPA needs no access to
Secrets** — the only sensitive value (an optional LLM key) is injected as an environment variable by
the pod spec, never read via the Kubernetes API.

| Resource | Verbs | Why |
|---|---|---|
| `autoscaling/horizontalpodautoscalers` | get, list, watch, **patch, update** | read config; apply approved changes (apply mode) |
| `keda.sh/scaledobjects` | get, list, watch, **patch, update** | same, for KEDA |
| `apps/deployments` | get, list, watch | read-only context for analysis |
| `metrics.k8s.io/pods,nodes` | get, list | metrics-server fallback |
| `openhpa.dev/scalingrecommendations(/status)` | full | own its recommendation CRDs |
| `coordination.k8s.io/leases` | get, create, update, patch | leader election |

`patch`/`update` on autoscalers are the only mutating grants; drop apply mode and OpenHPA is
effectively read-only. See [docs/manual/08-security.md](./docs/manual/08-security.md).

## Rules-only vs. optional LLM

- **Rules-only (default).** `llm.provider=none`. Fully deterministic, zero egress, works
  air-gapped. Recommendations carry the rule-derived diff and a templated explanation.
- **Optional LLM.** Set `llm.provider=openai|anthropic` and provide your own key. The LLM enriches
  the explanation and risk narrative; the deterministic rule engine still produces the actual
  change and still makes every safety decision. You can point `llm.baseUrl` at an in-cluster proxy
  or a local model.

Anthropic Bedrock via a VPC endpoint (for isolated clusters) is planned, not yet wired.

## Relationship with GitOps (Argo CD / Flux)

If your HPA / ScaledObject specs are owned by Git and reconciled by Argo CD or Flux, a **direct
change OpenHPA applies in `--mode=apply` will be reverted** by the GitOps controller on its next
sync. This is expected — Git is the source of truth.

The recommended pattern today is: **run OpenHPA in recommend-only mode** and treat each
`ScalingRecommendation` as a proposal to fold back into your manifests (by hand or via automation).
The recommendation model — a declarative CRD carrying a field-level diff — is deliberately designed
so that **GitOps-native pull-request generation can be built on top of it later** without changing
the analysis engine. That is a roadmap direction, not a current feature; see
[docs/adr/0002-open-source-conversion.md](./docs/adr/0002-open-source-conversion.md).

## Current limitations

- **GitOps.** Direct apply conflicts with Git-owned specs (above). PR generation is not yet built.
- **KEDA verification.** ScaledObject changes are marked `verified` at apply time; post-apply health
  verification and auto-rollback are implemented for **HPA targets only** so far.
- **Vertical rightsizing is out of scope.** OpenHPA does not touch container CPU/memory
  requests/limits (use KRR or VPA for that).
- **Forecasting is opt-in and conservative.** It only acts on workloads with a genuine periodic
  signal and enough history; flat/random workloads are left reactive.
- **Bedrock / local-model** integrations are planned, not shipped.

## Development

```bash
cargo test -p openhpa-core          # fast, pure-logic tests (no cluster)
cargo test --workspace              # all unit tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all                     # taplo fmt for Cargo.toml

# run against a local cluster
kind create cluster --name openhpa-dev
cargo run -p openhpa-operator
cargo run -p openhpa-operator -- --print-crd    # emit the CRD JSON

docker build -t openhpa .           # multi-stage → distroless, nonroot

# cluster-backed end-to-end tests (need a kind cluster; run serially)
cargo test -p e2e-tests -- --ignored --test-threads=1
```

- **`core/`** (`openhpa-core`) — pure, Kubernetes-free domain logic: metric snapshots, the rule
  engine, seasonal forecasting, LLM prompt/parse, recommendation synthesis. Fast to compile + test.
- **`operator/`** (`openhpa-operator`) — the kube-rs operator: CRD types, collector, metrics
  sources, LLM backends, applier, safety/verify, leader election, and the reconcile controller.
- **`deploy/`** — the Helm chart. **`e2e-tests/`** — cluster-backed tests. **`docs/`** — the manual.

See [CONTRIBUTING.md](./CONTRIBUTING.md) and [docs/architecture.md](./docs/architecture.md).

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](./CONTRIBUTING.md) (build, test, and code
conventions) and our [Code of Conduct](./CODE_OF_CONDUCT.md). Good first areas: additional detection
rules, KEDA verification parity, and GitOps PR generation.

## Security

Please report vulnerabilities privately per [SECURITY.md](./SECURITY.md) — do not open a public
issue for security reports.

## Maintainer

OpenHPA is created and maintained by **Tony Schneider** — https://github.com/tonyschneider.

## License

Licensed under the [Apache License 2.0](./LICENSE). See [NOTICE](./NOTICE).
