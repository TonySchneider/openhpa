# OpenHPA

Open-source Kubernetes operator that analyzes how your HPA / KEDA workloads behave over time and
recommends safer, more efficient autoscaling configuration. It watches your
HorizontalPodAutoscalers and KEDA ScaledObjects, analyzes real metrics with a deterministic rule
engine, and writes recommendations you read with `kubectl`. In `apply` mode it can also apply
approved changes behind a probation + auto-rollback safety net.

**Runs entirely in your cluster.** Metrics and analysis happen locally - no data leaves your
cluster, no SaaS, no phone-home. An optional, off-by-default LLM integration (your own key) enriches
the human-readable explanations; it is never required for a safety decision.

## Quickstart (recommend-only, no egress)

```bash
helm install openhpa oci://ghcr.io/tonyschneider/charts/openhpa \
  --namespace openhpa --create-namespace

kubectl get scalerec -A          # recommendations appear after a few minutes
```

This default install is **read-only**: it never mutates a workload and makes no outbound network
calls. To enable the optional LLM explanations, add `--set llm.provider=openai --set
llm.apiKey=<your-key>`. To let the operator apply approved changes, add `--set mode=apply`.

Full install, apply-mode, and air-gapped instructions are at **https://openhpa.dev/docs**.

## Links

- **Documentation:** https://openhpa.dev/docs
- **Source:** https://github.com/tonyschneider/openhpa
- **Image:** `ghcr.io/tonyschneider/openhpa` (cosign-signed, multi-arch)
- **Chart:** `oci://ghcr.io/tonyschneider/charts/openhpa`

See the [configuration reference](https://openhpa.dev/docs) for every Helm value.
