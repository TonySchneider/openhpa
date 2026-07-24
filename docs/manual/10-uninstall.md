# 10. Uninstall

Removing the operator is a Helm uninstall plus an explicit CRD cleanup. Uninstalling does
**not** change any autoscaling configuration already applied to your workloads - your HPAs
and ScaledObjects keep whatever values were last set.

## 10.1 Remove the operator

```bash
helm uninstall <release> --namespace <namespace>
```

This removes the Deployment, ServiceAccount, ClusterRole/ClusterRoleBinding, the
leader-election Lease, and any chart-created Secret (the optional LLM key).

## 10.2 Remove the CRD and recommendations

Helm does **not** delete CRDs it installed (a deliberate safety default), so the
`ScalingRecommendation` CRD and any remaining recommendation resources stay until you remove
them explicitly.

Inspect what remains first:

```bash
kubectl get scalingrecommendations -A
```

Then delete the CRD. **This deletes every `ScalingRecommendation` in the cluster** (the
recommendation history, not your live autoscalers):

```bash
kubectl delete crd scalingrecommendations.openhpa.dev
```

If you only want to clear recommendations but keep the CRD (for example, before a
reinstall), delete the resources instead:

```bash
kubectl delete scalingrecommendations --all -A
```

## 10.3 What is left behind

| Item | Removed by `helm uninstall`? | Notes |
| --- | --- | --- |
| Operator Deployment / pods | Yes | - |
| ServiceAccount, ClusterRole, ClusterRoleBinding | Yes | - |
| Leader-election Lease | Yes | - |
| Chart-created Secret (optional LLM key) | Yes | A Secret you created yourself and referenced via `existingSecret` is **not** removed - delete it manually if desired. |
| `ScalingRecommendation` CRD + resources | **No** | Remove explicitly (10.2). |
| Applied HPA / ScaledObject changes | **No** | Your autoscalers keep their last-applied values. Revert manually if you want the pre-openhpa configuration back. |

## 10.4 Reverting applied configuration

If you want a workload back to its configuration from before a openhpa change, the previous
values are recorded in the recommendation's `configDiff` (the `from` side) **before** you
delete the CRs:

```bash
kubectl get scalerec <name> -n <namespace> -o jsonpath='{.spec.configDiff}{"\n"}'
```

Apply those `from` values to the HPA/ScaledObject manually (or via your GitOps source of
truth) before uninstalling, so you retain the record.
