# 6. Usage and workflow

This section walks the full lifecycle of a recommendation: how it appears, how to read it,
how to approve it, and what the operator does after approval.

## 6.1 List recommendations

Recommendations are namespaced custom resources. List them with the `scalingrecommendations`
plural or the `scalerec` short name:

```bash
# All namespaces:
kubectl get scalingrecommendations -A

# One namespace:
kubectl get scalerec -n <namespace>
```

The printed columns come from the CRD:

```
NAME    TARGET   RISK     APPROVED   PHASE
web     web      low      false      pending
api     api      medium   true       verified
```

| Column | Source | Meaning |
| --- | --- | --- |
| `TARGET` | `.spec.targetRef.name` | The HPA (or ScaledObject) the recommendation tunes. |
| `RISK` | `.spec.riskLevel` | `low` / `medium` / `high`, from the analysis. |
| `APPROVED` | `.spec.approved` | Whether a human has approved it. |
| `PHASE` | `.status.phase` | Lifecycle state (see §6.5). |

A recommendation is created with the **same name** as its target HPA, in that HPA's
namespace. The operator leaves existing recommendations alone, so your approval and any
manual edits are never overwritten.

## 6.2 Read a recommendation

```bash
kubectl get scalerec <name> -n <namespace> -o yaml
```

Key fields under `spec`:

| Field | Meaning |
| --- | --- |
| `targetRef` | `{ kind: HorizontalPodAutoscaler | ScaledObject, name }` - what gets patched. |
| `riskLevel` | `low` / `medium` / `high`. |
| `summaryMd` | Human-readable Markdown summary of the reasoning. With `llm.provider=none` the rules-only output notes that no LLM was configured; in recommend-only mode the summary notes that the change is advisory and should be applied by a human. |
| `projectedSavingsUsdMonthly` | Estimated monthly saving, when the analysis produced one. This is an **estimate**, priced at `costPerReplicaUsdMonthly` (default `$30`/replica/month) when the LLM has no better figure - set that value to your real per-replica cost, and treat the number as directional, not an invoice. The `summaryMd` repeats the pricing basis. |
| `configDiff` | A map of `field -> { from, to }`: the exact change. Fields: `min_replicas`, `max_replicas`, `target_cpu_pct`, `scale_down_cooldown_s`. |
| `schedule` | Optional list of predictive windows: `{ startCron, durationMinutes, minReplicas }`. Present only for forecasted recurring peaks. |

Read the `summaryMd` for the *why*, and `configDiff` for the *what*. For example, a diff of
`min_replicas: { from: 10, to: 3 }` with `target_cpu_pct: { from: 50, to: 70 }` proposes
lowering the floor and raising the CPU target - trading idle headroom for cost.

## 6.3 Approve a recommendation

Approval is a single edit to `spec.approved`. The operator applies it on the next tick.

```bash
kubectl patch scalerec <name> -n <namespace> \
  --type merge -p '{"spec":{"approved":true}}'
```

You can review the diff first and approve in one motion, or edit interactively with
`kubectl edit scalerec <name> -n <namespace>`. Until `approved` is `true`, nothing is
changed on the live workload.

## 6.4 What apply does

On the next tick after approval, the leader operator:

1. **Patches the target.** For an HPA it sets `minReplicas`, `maxReplicas`, the CPU
   `averageUtilization` target, and/or the scale-down stabilization window, per the
   `configDiff`. For a KEDA ScaledObject it sets `minReplicaCount`, `maxReplicaCount`, and/or
   `cooldownPeriod`.
2. **Records the apply.** `status.phase` becomes `applied`, `status.appliedAt` is stamped,
   and `status.probationUntil` is set to now + the probation window.

Apply happens only in `apply` mode. In the default `recommend` mode an approved recommendation is
left `pending` and never patched onto the workload.

## 6.5 Probation, verification, and auto-rollback

After the probation window (default 45 minutes), the verify pass re-reads the workload's
health and decides:

| Verdict | Action | Resulting phase |
| --- | --- | --- |
| Healthy | Keep the change. | `verified` |
| Inconclusive (too little post-apply data) | Extend probation. | stays `applied` |
| Degraded, rollback enabled | Revert to the previous config (`from` values). | `rolledBack` |
| Degraded, rollback disabled | Hold for re-judgement (revert later if you re-enable rollback). | `degraded` |

The rollback restores exactly the values the operator changed, and runs **regardless of operating
mode** - it only ever undoes the operator's own change, so switching a deploy from apply back to
recommend can never strand a degraded workload mid-probation.

> Health verification (and therefore auto-rollback) currently applies to **HPA** targets.
> KEDA ScaledObject applies are marked `verified` at apply time, because reading live
> ScaledObject config back is a follow-up.

### Phase reference

| Phase | Meaning |
| --- | --- |
| `pending` | Created, awaiting approval (or approved, not yet applied this tick). |
| `applied` | Patched onto the workload; on probation. |
| `verified` | Healthy after probation. The terminal happy state. |
| `rolledBack` | Auto-reverted after the change degraded health. |
| `degraded` | Degraded but held (rollback was disabled); re-judged each cycle. |
| `failed` | The apply call itself failed (see `status.detail` and the logs). |

## 6.6 Predictive schedule windows

When forecasting is enabled and a workload shows a strong recurring peak, the recommendation
carries a `schedule`: one or more windows, each a KEDA-style `startCron`, a
`durationMinutes`, and the `minReplicas` floor to hold during it. After approval:

- **KEDA ScaledObject targets:** the operator installs the windows as KEDA `cron` triggers
  (named `openhpa-cron-N`, timezone UTC). KEDA then performs the time-based scaling; your
  own (non-cron) triggers are preserved untouched.
- **HPA targets:** the operator itself raises `minReplicas` to the window floor inside each
  window and restores the baseline outside it.

`status.scheduleActive` reflects whether a schedule is currently being enforced. If a
forecasted peak repeatedly fails to materialize (the pre-scaled floor sits idle), the
operator **retracts** the schedule and restores the baseline, recording the reason in
`status.detail`.

To inspect the active schedule on a KEDA target:

```bash
kubectl get scaledobject <name> -n <namespace> \
  -o jsonpath='{.spec.triggers[?(@.type=="cron")]}{"\n"}'
```
