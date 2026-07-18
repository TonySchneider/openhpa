# 4. Configuration reference

Every Helm value maps to an environment variable on the operator container (set by the
chart's Deployment template). You normally configure via Helm values; the env column is
useful for local runs and for understanding the pod spec.

## 4.1 Scope and scheduling

| Helm value | Env var | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `OPENHPA_MODE` | `recommend` | `recommend` (only emit `ScalingRecommendation`s, never mutate a workload — even an approved one) or `apply` (also patch approved targets). Start in `recommend`; switch to `apply` once the advice is trusted. |
| `watchNamespaces` | `OPENHPA_WATCH_NAMESPACES` | `""` (all) | Comma-separated namespaces to watch. Empty watches all namespaces. Example: `prod,api`. |
| `intervalSeconds` | `OPENHPA_INTERVAL_SECONDS` | `300` | Seconds between reconcile ticks. Also drives the leader-lease duration (3× this value). |
| `replicaCount` | — | `1` | Operator replicas. `2+` is safe with leader election (only the leader mutates). |
| `costPerReplicaUsdMonthly` | `OPENHPA_COST_PER_REPLICA_USD_MONTHLY` | `30` | Estimated monthly cost (USD) of one always-on replica, pricing the projected-savings **estimates** (see §6.2). Set it to your real per-replica node share; must be > 0. |

## 4.2 Metrics

| Helm value | Env var | Default | Meaning |
| --- | --- | --- | --- |
| `metrics.prometheusUrl` | `OPENHPA_PROMETHEUS_URL` | `""` | Prometheus base URL. Set it for real history backfill; empty falls back to slow HPA-status accumulation. Example: `http://prometheus.monitoring.svc:9090`. |
| `metrics.windowDays` | `OPENHPA_METRICS_WINDOW_DAYS` | `14` | Days of history to backfill per workload from Prometheus. |
| `metrics.stepSeconds` | `OPENHPA_METRICS_STEP_SECONDS` | `300` | Sampling step (seconds) for Prometheus history queries. |
| `metrics.promqlCpu` | `OPENHPA_PROMQL_CPU` | *(built-in)* | PromQL override for CPU utilization. Supports `{ns}` / `{deploy}` placeholders. Defaults to a kube-state-metrics + cAdvisor query. |
| `metrics.promqlReplicas` | `OPENHPA_PROMQL_REPLICAS` | *(built-in)* | PromQL override for replica count. |
| `metrics.promqlQueue` | `OPENHPA_PROMQL_QUEUE` | *(built-in)* | PromQL override for queue depth (queue-based workloads). |

## 4.3 Safety (probation and rollback)

| Helm value | Env var | Default | Meaning |
| --- | --- | --- | --- |
| `safety.probationWindowMinutes` | `OPENHPA_PROBATION_WINDOW_MINUTES` | `45` | Minutes an applied change stays on probation before the verify pass judges its health. Clamped to a minimum of 1. |
| `safety.rollbackEnabled` | `OPENHPA_ROLLBACK_ENABLED` | `true` | Auto-revert a probationary change when health degrades. When `false`, a degraded change is held in `degraded` for re-judgement instead of reverted. |
| `safety.healthCpuMargin` | `OPENHPA_HEALTH_CPU_MARGIN` | `0.15` | Headroom over the target CPU fraction tolerated before a change is judged degraded. |

## 4.4 Leader election (high availability)

| Helm value | Env var | Default | Meaning |
| --- | --- | --- | --- |
| `leaderElection.enabled` | `OPENHPA_ENABLE_LEADER_ELECTION` | `true` | Elect a single leader so `replicaCount: 2+` never double-applies; only the leader mutates. |
| `leaderElection.leaseName` | `OPENHPA_LEASE_NAME` | `openhpa-leader` | Name of the `coordination.k8s.io` Lease. |
| `leaderElection.leaseNamespace` | `OPENHPA_LEASE_NAMESPACE` | `""` (release ns) | Namespace for the Lease; empty uses the operator's own namespace. |

## 4.5 Predictive forecasting

Off by default. Requires Prometheus history and a genuinely periodic workload; flat or random
workloads are left reactive.

| Helm value | Env var | Default | Meaning |
| --- | --- | --- | --- |
| `forecasting.enabled` | `OPENHPA_ENABLE_FORECASTING` | `false` | Forecast recurring peaks and pre-scale the floor ahead of them. |
| `forecasting.minHistoryDays` | `OPENHPA_FORECAST_MIN_HISTORY_DAYS` | `14` | Minimum days of history before a forecast is attempted (needs ≥ 2 weekly cycles). |
| `forecasting.binMinutes` | `OPENHPA_FORECAST_BIN_MINUTES` | `60` | Seasonal grid bin size in minutes (168 hourly bins per week by default). |
| `forecasting.quantile` | `OPENHPA_FORECAST_QUANTILE` | `0.95` | Quantile of per-bin demand used for the forecast (safety headroom). Clamped to `[0, 1]`. |
| `forecasting.periodicityThreshold` | `OPENHPA_PERIODICITY_THRESHOLD` | `0.3` | Minimum lag-24h/7d autocorrelation for a workload to count as periodic. |
| `forecasting.prescaleLeadMinutes` | `OPENHPA_PRESCALE_LEAD_MINUTES` | `10` | Minutes of head start to raise the floor before each forecasted peak. |

## 4.6 LLM provider (bring-your-own key)

| Helm value | Env var | Default | Meaning |
| --- | --- | --- | --- |
| `llm.provider` | `OPENHPA_LLM_PROVIDER` | `openai` | `openai`, `anthropic`, or `none` (deterministic rules-only). |
| `llm.model` | `OPENHPA_LLM_MODEL` | `gpt-4o-mini` | Model name passed to the provider. Any model the provider exposes works (e.g. `gpt-4o`, `claude-sonnet-4-6`). |
| `llm.baseUrl` | `OPENHPA_LLM_BASE_URL` | `""` (provider default) | Override the API base URL — point at an in-cluster proxy, a local-model sidecar, or a mock. |
| `llm.timeoutSeconds` | `OPENHPA_LLM_TIMEOUT_SECONDS` | `30` | Per-request timeout; a stalled model can't stretch a reconcile pass (a timed-out call is logged and skipped, no recommendation written). |
| `llm.concurrency` | `OPENHPA_LLM_CONCURRENCY` | `4` | Max concurrent LLM calls during the analysis pass (values below 1 are treated as 1). |
| `llm.analysisBudgetSeconds` | `OPENHPA_ANALYSIS_BUDGET_SECONDS` | `""` (one reconcile interval) | Time budget for the analysis pass's LLM phase. Workloads not reached before it elapses are analyzed on the next tick (the deferred count is logged). |
| `llm.apiKey` | `OPENAI_API_KEY` *(see note)* | `""` | Your API key. Setting it makes the chart create a Secret. |
| `llm.existingSecret` | — | `""` | Name of an existing Secret holding the key under `apiKey`, used instead of `llm.apiKey`. |

> **Note.** The chart injects the key as the `OPENAI_API_KEY` environment variable sourced
> from the Secret key `apiKey`, **for both providers** — so a single `apiKey` value is all you
> set regardless of `llm.provider`. (For local CLI runs outside the chart, set only the env var
> matching your provider — if both `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` are exported the
> operator uses `OPENAI_API_KEY` first.) `bedrock` is planned but not yet wired.

## 4.7 Resources and service account

| Helm value | Default | Meaning |
| --- | --- | --- |
| `image.repository` | `ghcr.io/tonyschneider/openhpa` | Image repository (override for a mirrored registry). |
| `image.tag` | `""` (chart appVersion) | Image tag. |
| `image.pullPolicy` | `IfNotPresent` | Image pull policy. |
| `serviceAccount.name` | `openhpa` | Service account the operator runs as. |
| `resources.requests` | `cpu: 50m`, `memory: 64Mi` | Resource requests. |
| `resources.limits` | `cpu: 250m`, `memory: 128Mi` | Resource limits. |

## 4.8 Run modes

The operator's run mode is the `mode` value (§4.1):

- **`recommend` (default)** — a pure advisor. It watches, analyzes, and writes
  `ScalingRecommendation`s, but **never mutates a workload**, even one whose recommendation you
  have approved. This is the safe way to start a rollout: trust the advice before granting apply.
- **`apply`** — additionally patches approved targets, then runs the
  probation → verify → auto-rollback safety net.

Switch with `--set mode=apply` on a `helm upgrade`. See [Operating §7.4](./07-operating.md) for
the recommend → apply promotion path.
