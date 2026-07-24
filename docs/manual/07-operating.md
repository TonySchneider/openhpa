# 7. Operating

Day-2 guidance for running the operator in production.

## 7.1 Monitoring

The operator surfaces its state in three places.

**Logs.** Structured logs, controlled by `RUST_LOG` (default
`info,openhpa_operator=debug`). Each reconcile tick logs the operating mode, whether this replica is
the leader, and the metrics source in use:

```bash
kubectl logs -n <namespace> deploy/openhpa -f
```

Useful log lines to watch for:

| Log message (substring) | Meaning |
| --- | --- |
| `openhpa operator starting` | Startup; reports provider, namespaces, Prometheus, forecasting, mode. |
| `metrics source: Prometheus history backfill` | Prometheus is wired correctly. |
| `metrics source: HPA-status fallback` | No Prometheus URL - history rebuilds slowly. |
| `reconcile tick` | A tick ran; includes `mode=…` and `leader=…`. |
| `created ScalingRecommendation` | A new recommendation was emitted. |
| `applied recommendation` / `auto-reverted degraded recommendation` | Apply / rollback occurred. |
| `not leader; skipping mutating pass` | This replica is a follower (expected with `replicaCount > 1`). |

**Recommendation status.** The authoritative state of any change is the CR's
`status.phase` and `status.detail` (see [Usage §6.5](./06-usage.md)):

```bash
kubectl get scalerec -A
kubectl get scalerec <name> -n <namespace> -o jsonpath='{.status}{"\n"}'
```

**Kubernetes resources.** Inspect the live workload to confirm an applied change:

```bash
kubectl get hpa <name> -n <namespace> \
  -o custom-columns=MIN:.spec.minReplicas,MAX:.spec.maxReplicas,TARGET:.spec.metrics[0].resource.target.averageUtilization
```

## 7.2 High availability

Run two or more replicas and keep leader election enabled (the default):

```bash
helm upgrade <release> oci://ghcr.io/tonyschneider/charts/openhpa \
  --version <version> --namespace <namespace> --reuse-values \
  --set replicaCount=2
```

- Leader election uses a `coordination.k8s.io` Lease (`leaderElection.leaseName`, default
  `openhpa-leader`). Only the leader runs the mutating passes (apply, verify,
  schedule), so multiple replicas never double-apply.
- The lease duration is 3× `intervalSeconds`. Each mutating pass is re-gated on a fresh
  leadership check and bounded by a time budget, so a replica that loses the lease stops
  mutating promptly during a failover.
- Followers still run read-only analysis, so failover is fast - a standby is already warm.

## 7.3 Upgrades

Upgrades follow the same verify-then-install flow as the initial install:

1. **Verify** the new image signature (see [Installation §3.1](./03-installation.md)).
2. **Mirror** the new image and chart if you are air-gapped
   ([§3.3](./03-installation.md)).
3. **Upgrade** in place:

   ```bash
   helm upgrade <release> oci://ghcr.io/tonyschneider/charts/openhpa \
     --version <new-version> --namespace <namespace> --reuse-values
   ```

The CRD ships with the chart. Existing `ScalingRecommendation` resources and their approval state
are preserved across upgrades. The image and chart are public and free; there is nothing to renew.

## 7.4 Run modes

| Mode | How to run it |
| --- | --- |
| **Recommend-only (default)** | `mode=recommend` (the default). The operator watches and emits recommendations but **never mutates a workload**, even an approved one. The safe way to start. |
| **Apply** | Set `mode=apply`. The operator applies approved recommendations and runs the probation -> verify -> rollback net. Nothing is mutated until you approve a recommendation. |
| **Rules-only (no LLM)** | Set `llm.provider=none` (the default). Analysis uses the deterministic rule engine; no external calls are made. Combine with either mode above. |

Promote a rollout by switching `--set mode=apply` on a `helm upgrade` once you trust the advice. See
[Configuration reference §4.8](./04-configuration-reference.md).

## 7.5 Tuning the cadence and safety margins

- **`intervalSeconds`** trades responsiveness against API-server load. The default `300`s is
  appropriate for most clusters; lower it only if you need faster turnaround and your control
  plane has headroom.
- **`safety.probationWindowMinutes`** should be long enough to capture a representative
  traffic sample for the workload. For spiky daily traffic, keep it at or above the default.
- **`safety.healthCpuMargin`** widens or tightens what counts as "degraded." Raise it to
  tolerate more post-change CPU headroom before rolling back; lower it for stricter reverts.
