# 9. Troubleshooting and FAQ

Start by reading the operator logs and the recommendation's `status.detail`:

```bash
kubectl logs -n <namespace> deploy/openhpa --tail=200
kubectl get scalerec <name> -n <namespace> -o jsonpath='{.status}{"\n"}'
```

## 9.1 No recommendations appear

A few normal causes, in order of likelihood:

1. **History is still warming up.** Without Prometheus, the operator accumulates one sample
   per tick from HPA status, so it can take many ticks (hours) before there is enough history
   to analyze. Fix by pointing at Prometheus: `--set metrics.prometheusUrl=…`. The logs show
   `metrics source: HPA-status fallback` when no Prometheus URL is set.
2. **Prometheus returns no points.** The log line
   `prometheus returned no points; check CPU requests/limits or PromQL templates` means the
   default query found nothing — usually because the workload has no CPU requests/limits (the
   default query's denominator), or your Prometheus uses non-standard metric names. Set CPU
   requests on the workload, or override the queries with `metrics.promqlCpu` /
   `metrics.promqlReplicas` / `metrics.promqlQueue` (using `{ns}` / `{deploy}` placeholders).
3. **No optimization opportunity.** If a workload is already well-tuned, the rule engine emits
   nothing and no recommendation is created. This is expected.
4. **Not watching that namespace.** Check `watchNamespaces`. Empty means all namespaces; a
   non-empty list restricts to exactly those.
5. **A recommendation already exists.** The operator never overwrites an existing
   recommendation for a workload. Check `kubectl get scalerec -A`.

## 9.2 LLM errors

- The operator falls back to a deterministic **rules-only** analysis if the LLM is not
  configured (`llm.provider=none`). Recommendations still appear, with the summary
  *"Rule-based recommendation (no LLM configured)."*
- If a configured provider returns an error (bad key, rate limit, network), the analysis pass
  logs the error and that tick produces no LLM-judged recommendation; the loop continues and
  retries next tick. Verify the key Secret exists and holds the key under `apiKey`, and that
  egress to the provider is allowed.
- `llm provider 'openai' requires an API key` at startup means `llm.provider` is set but no
  key was supplied. Set `llm.apiKey` or `llm.existingSecret`, or switch to
  `llm.provider=none`.

## 9.3 An approved recommendation is not being applied

| Symptom | Cause | Fix |
| --- | --- | --- |
| Approved recommendation stays `pending`, nothing patched | Operator is in `recommend` mode (the default). | Set `--set mode=apply` on a `helm upgrade`. |
| Applied change immediately reverts on the next sync | A GitOps controller (Argo CD / Flux) owns the spec. | Expected — Git is the source of truth. Run recommend-only and fold the change into your manifests. See [§9.5](#95-a-change-was-applied-but-immediately-rolled-back) and the README's GitOps section. |
| `apply failed: … is forbidden` | Missing RBAC (see [§9.4](#94-rbac-errors)). | Re-install the chart with the `ClusterRole`/`ClusterRoleBinding`. |

## 9.4 RBAC errors

Log lines like `apply failed: … is forbidden: User "system:serviceaccount:…" cannot patch
resource …` mean the service account lacks a grant — usually because the `ClusterRole` /
`ClusterRoleBinding` was not installed (e.g. a templated-manifests-only deploy without
cluster-admin). Re-run the Helm install with cluster-admin so the RBAC objects are created,
and confirm:

```bash
kubectl auth can-i patch horizontalpodautoscalers \
  --as=system:serviceaccount:<namespace>:openhpa -n <namespace>
```

## 9.5 A change was applied but immediately rolled back

The verify pass judged the workload **degraded** after probation (for example CPU pushed past
the target plus `safety.healthCpuMargin`). The recommendation moves to `rolledBack` and
`status.detail` records why. This is the safety net working as designed. If you believe the
change is actually fine, you can widen `safety.healthCpuMargin` or lengthen
`safety.probationWindowMinutes` so a brief post-change spike is not misread.

## 9.6 A ScaledObject change did not apply / no rollback

KEDA ScaledObject targets are marked `verified` at apply time and are **not** health-verified
or auto-rolled-back yet (reading live ScaledObject config back is a follow-up). If the patch
itself did not take effect, confirm KEDA is installed, the `ScaledObject` exists in the
target namespace, and the operator has the `keda.sh/scaledobjects` grant (it does by default).

## 9.7 Predictive schedule keeps retracting

If a forecasted peak does not actually materialize, the operator retracts the schedule after
several consecutive idle samples inside the window and restores the baseline floor
(`status.detail: "schedule retracted: forecasted peak not materializing"`). This is expected
for workloads whose pattern has changed. Forecasting also requires enough history
(`forecasting.minHistoryDays`) and a genuinely periodic signal
(`forecasting.periodicityThreshold`); flat or random workloads stay reactive by design.

## 9.8 FAQ

**Does the operator change anything without approval?** No. Nothing is applied until
`spec.approved: true`. Auto-rollback only ever undoes a change the operator itself applied.

**Does it work air-gapped?** Yes — with `llm.provider=none` (the default) there is no outbound
traffic at all.

**Can I run it purely as an advisor?** Yes — the default `recommend` mode never mutates a workload.
Even in `apply` mode, nothing happens until you approve a recommendation. See
[Operating §7.4](./07-operating.md).

**Will two replicas double-apply?** No — leader election ensures only the leader mutates.
