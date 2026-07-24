# Real-cluster validation run

A **manual quality gate** for contributors: run the operator on a realistic cluster with
metrics-server **and** Prometheus (optionally with a real LLM key), in **recommend-only** mode, over
a sustained period with varied workloads - then judge whether the recommendations are sane/safe and
how the operator behaves under failure. This directory is the harness (`run.sh`, `workloads.yaml`);
fill in the checklist + findings below as you go.

Recommend-only is enforced everywhere here (`--mode=recommend`): nothing is ever applied, so this is
safe to run against a shared cluster.

## Prerequisites

- A cluster: kind / k3s / a managed cluster (managed is closest to a real customer).
- **metrics-server** installed (so HPAs report utilization). On kind it needs `--kubelet-insecure-tls`.
- **Prometheus** reachable in-cluster (set `OPENHPA_PROMETHEUS_URL`) - this gives real rolling
  history that survives restart and is what a customer install uses. Without it the operator falls
  back to slow per-tick HPA-status accumulation (fine for a smoke test, too slow for forecasting).
- A **real** LLM key in `OPENAI_API_KEY` (or `ANTHROPIC_API_KEY`) and `OPENHPA_LLM_PROVIDER` set -
  the whole point is to judge real LLM judgement, not the rules-only fallback.
- `kubectl` pointed at the target cluster; a checkout of this repo (the harness runs the operator via
  `cargo run`, or install the Helm chart with `mode=recommend` instead).

## Run

```bash
export OPENHPA_LLM_PROVIDER=openai OPENAI_API_KEY=sk-...
export OPENHPA_PROMETHEUS_URL=http://prometheus-server.monitoring:9090   # if available

validation/run.sh setup           # namespace + CRD + synthetic workloads
# wait several minutes (and ideally hours/days for forecasting) for history to build
validation/run.sh run             # operator, recommend-only; leave it running
validation/run.sh recs            # in another shell: inspect emitted recommendations
```

The synthetic workloads (`workloads.yaml`) each target one detector so you can compare the operator's
output against a known-correct expectation:

| Workload              | Expected recommendation                                  |
|-----------------------|----------------------------------------------------------|
| `idle-svc`            | idle_window - lower `minReplicas` toward 1-2             |
| `overprovisioned-svc` | overprovisioned - raise `target_cpu_pct`                 |
| `healthy-svc`         | **none** (a recommendation here is a false positive)     |

Forecasting (predictable-peak schedules) needs ≥ 2 weekly cycles of history and is off by default;
to exercise it, run with `--enable-forecasting=true` (via Helm `forecasting.enabled`) on a cluster
with a workload that has a real daily/weekly pattern, over enough days.

## Scenarios to exercise

- **A. Recommendation quality** - let it run; for each workload compare the emitted
  `ScalingRecommendation` (diff, risk, summary, savings) against the table above. Are the diffs sane?
  Is the risk level reasonable? Is the LLM reasoning trustworthy to a DevOps engineer?
- **B. Prometheus down** - `validation/run.sh fault-prometheus-down` (set `PROM_DEPLOY`/`PROM_NS`),
  then run the operator: it must log the empty fetch and keep watching - no panic, no garbage
  recommendation. Restore with `fault-prometheus-up`.
- **C. LLM times out / rate-limits** - `validation/run.sh fault-llm-timeout` points the base URL at a
  black hole with a 2s timeout: the operator must log a timeout per workload and keep reconciling, no
  panic, no recommendation written from a failed call. (A real 429 from the provider should behave
  the same - observe during the quality run.)
- **D. Many HPAs** - `validation/run.sh many-hpas 50` then run: confirm one reconcile pass completes
  within the lease/interval and the operator stays responsive. Note the per-pass wall-clock.
- **E. Restart recovery** - with Prometheus configured, kill and restart the operator: confirm it
  backfills history and resumes (recommendations are stable, not duplicated).
- **F. No unapproved apply** - confirm throughout that no workload is ever mutated (recommend-only):
  HPA specs are unchanged and no recommendation reaches `applied`.

## Observation checklist

- [✗] `idle-svc` -> an idle_window recommendation with a sane lowered floor - **did NOT fire**; surfaced
  as `overprovisioned` (raise target) instead. Structural, see finding S1.
- [✗] `overprovisioned-svc` -> an overprovisioned recommendation - **did NOT fire**; the workload sits at
  ~60% p95, above the 40% threshold. Synthetic workload mis-shaped, see finding S2.
- [✓] `healthy-svc` -> **no** recommendation (no false positive)
- [✓] LLM reasoning + risk levels are trustworthy/defensible (Sonnet 4.6 - reasoning was sharp and honest)
- [ ] Prometheus-down: **not exercised** (no Prometheus in this run; HPA-status fallback) - scenario B
- [✓] LLM timeout/rate-limit: logged, no panic, no recommendation written (scenario C) - both synthetic
  timeout AND a real provider 429 confirmed
- [✓] Many HPAs: one pass stays within the interval; operator responsive (scenario D)
- [ ] Restart recovers history and is stable: **not exercised** (HPA-status fallback rebuilds from
  scratch; real backfill needs Prometheus) - scenario E
- [✓] No workload ever mutated; nothing reaches `applied` (scenario F)

## Update - 2026-06-07, plan 0011 C1+C2 fix (S1/F2 resolved)

Re-ran the quality scenario on the same kind cluster + `claude-sonnet-4-6` after the C1 headroom
fix. **The headline floor cut now fires on the HPA-status source (no Prometheus):**

- `idle-svc` (min=4, p95 CPU 0%) -> `Overprovisioned` recommendation, diff **`min_replicas: 4 -> 1`**,
  risk low, **projected savings $90/month** (3 replicas × the $30/replica estimate). The LLM summary
  agrees: *"Reducing min_replicas to 1 eliminates approximately $90/month … HPA remains fully armed to
  scale out."* This replaces the old `$0` target-bump no-op - S1 is resolved.
- Savings is deterministic (synthesis fills `replicas_reduced × $30` when the LLM gives no figure), so a
  floor cut is never `$0`; the prompt also tells the LLM the assumption so its number/text agree.
- **C2 (F2) verified:** ran with `--llm-provider anthropic` while **both** `OPENAI_API_KEY` and
  `ANTHROPIC_API_KEY` were exported - the operator selected the Anthropic key and succeeded (no
  `env -u OPENAI_API_KEY` workaround). The old code would have sent the OpenAI key to Anthropic.

S2 (overprovisioned-svc still at ~60%, doesn't trip the <40% detector) and S3 (Prometheus-backed
idle/forecast re-validation) remain follow-ups (plan 0011 C5/C6). Original gate findings below.

## Findings

> Cluster: `kind-openhpa-dev` (k8s v1.35.0, single node, metrics-server +`--kubelet-insecure-tls`)
> • Date: 2026-06-07 • Duration: short smoke (quality run 4 ticks @ 30s; fault/scale ticks)
> • metrics source: **HPA-status fallback** (no Prometheus) • LLM: **Anthropic `claude-sonnet-4-6`**
> (BYO key, recommend-only, `--mode=recommend`)

**Gate verdict: PARTIAL - mechanics pass, headline value-prop recommendation does not fire on the default
metrics source.** The operator's safety + robustness properties are solid (no mutation, graceful
failures, sub-second reconcile throughput, no false positives, trustworthy LLM reasoning). But the core
"lower your over-set floor -> savings" recommendation never fired for the canonical idle workload, and the
idle/forecast/savings paths cannot be exercised without Prometheus. Per this plan's rule ("garbage here =
stop and fix before any client"), this is a decision point before relying on apply/savings with a client.

- **Recommendation quality (A):** Only `idle-svc` produced a recommendation; `overprovisioned-svc`
  (~60% util) and `healthy-svc` (~200% util, HPA maxed at 10) correctly produced **none** - zero false
  positives. The `idle-svc` rec: detector `overprovisioned` (p95 CPU 0% < 40%), diff
  `target_cpu_pct 70->85`, risk `low`, **projected savings $0**. Sonnet 4.6's summary was excellent and
  *honest*: it explicitly stated the target-only change "produces no actual reduction" and that the real
  lever is `min_replicas` (drop 4->1-2 for 50-75% savings) - which the rule engine did **not** put in the
  applyable diff. So: reasoning trustworthy, but the machine-actionable output for the headline case is a
  $0 no-op.
  - **gpt-4o cross-check (provider independence):** re-ran the same workload on OpenAI `gpt-4o` once the
    account was funded - identical diff (`target_cpu_pct 70->85`, risk low, **$0**), confirming S1 is a
    rule-engine property, not an LLM one. gpt-4o's prose was *weaker*: it claimed raising the target
    "reduces excessive resource allocation" (it does not, for a floored idle service) and never flagged
    the $0 no-op or the min_replicas lever that Sonnet 4.6 caught. Sonnet is the better judge here.
- **Prometheus down (B):** not exercised - this run used the HPA-status fallback. Validate on a cluster
  with Prometheus (`OPENHPA_PROMETHEUS_URL`) before relying on the backfill path.
- **LLM timeout / rate-limit (C):** PASS, two ways.
  - Synthetic: base URL -> black hole + `OPENHPA_LLM_TIMEOUT_SECONDS=2` -> `analysis pass failed
    error=error sending request …` at ~2.0s, 3 ticks / 2 failures, **0 panics, 0 recs written**, loop
    kept reconciling.
  - Real provider 429 (bonus): the configured OpenAI key returned `429 insufficient_quota` on **every**
    call; the operator logged `analysis pass failed error=… 429 Too Many Requests`, kept reconciling, no
    panic, no rec. Real-provider rate-limit handling is therefore also confirmed.
  - Finding F1: `error_for_status()` collapses the 429 - the log shows only "429 Too Many Requests", not
    the provider body that distinguishes *rate-limit* (retry) from *insufficient_quota* (billing). No
    retry/backoff. Surface the response body + a bounded retry.
- **Many HPAs (D), per-pass wall-clock:** 50 bulk HPAs added (53 total). A rules-only analysis pass
  (list + rules + **30 ScalingRecommendation CR writes**) completed in **~189ms** (tick 16:39:01.200 ->
  last rec 16:39:01.388), 0 panics. The operator's own reconcile throughput is sub-second and well within
  any lease. Finding D1: the **LLM fan-out is sequential, O(N)** - single Sonnet call latency measured
  **~10.5s**, so a *cold* pass over N new candidates ≈ N×10s (≈ 8 min for 50) before any CR exists; once
  CRs exist they are skipped, so steady-state matches the rules-only number. Bound it (concurrency cap /
  batching / per-pass time budget) before fleets of hundreds. (The A5 per-mutation deadline guards only
  the *mutating* passes, which are off in recommend mode; the analysis pass is currently unbounded.)
- **Restart recovery (E):** not exercised. On the HPA-status fallback, in-memory history rebuilds from
  scratch on every start (observed). Real restart-recovery (Prometheus `query_range` backfill) needs
  Prometheus - validate there.
- **No-apply confirmation (F):** PASS. Patched the `idle-svc` rec `approved: true` while the operator ran
  in recommend mode; over 2+ ticks the HPA spec was **byte-identical** before/after (min=4, target=70),
  the rec stayed `approved=true` with an **empty `status.phase`** (never `applied`/`verified`), and there
  was zero apply-pass activity in the log. Recommend mode never mutates, even an approved CR.

### Structural findings (the important ones for a client decision)

- **S1 - the idle_window detector cannot fire from live HPA-status data.** It proposes
  `suggested_min = median(observed replicas during the idle run)` and only emits when that is **below** the
  configured `minReplicas`. But the HPA floors `replicas ≥ minReplicas`, so an idle service pinned at its
  floor always shows `replicas == min` -> `suggested_min == min` -> no candidate. The detector can only fire
  from **Prometheus history that recorded a period when replicas were genuinely below the current floor**
  (e.g. before the floor was raised). Consequence: on the HPA-status fallback (the default for a
  no-Prometheus install) the headline "lower your floor" recommendation **never fires**; idle services
  surface only as `overprovisioned: raise target` ($0). Fix candidate: derive `suggested_min` from load
  headroom (cpu/target), not observed replicas.
- **S2 - `overprovisioned-svc` is mis-shaped.** Its busy-loop holds ~60% CPU utilization, above the 40%
  p95 overprovision threshold, so it never trips the detector it was built to exercise. Lower its duty
  cycle so p95 < 40%.
- **S3 - idle (≥4h) and forecast (≥2 weekly cycles) paths are unreachable on a fresh kind smoke.** Both
  need long real history that only Prometheus provides. The idle/forecast/savings value-prop is therefore
  **unvalidated** by this run and MUST be validated on EKS (or any cluster) **with Prometheus** and real
  traffic before those claims back a client engagement.
- **F2 - BYO-key provider mismatch footgun.** `main.rs` reads `OPENAI_API_KEY` first, else
  `ANTHROPIC_API_KEY`, regardless of `--llm-provider`. A customer who sets both keys and selects
  `anthropic` will send the OpenAI key to Anthropic (401). The validation run worked around it with
  `env -u OPENAI_API_KEY`. Key the credential off the selected provider.

### Prompt/rule tuning to do

1. **(S1, highest value)** Make the idle/overprovisioned detectors recommend a `min_replicas` reduction
   derived from observed load headroom - so the savings recommendation actually fires for floor-pinned
   idle services, which is the product's headline pitch.
2. **(S2)** Reshape `overprovisioned-svc` to sit < 40% p95 so the harness exercises that detector.
3. **(F1)** Surface the provider error body and add a bounded 429 retry/backoff.
4. **(D1)** Bound the analysis-pass LLM fan-out (concurrency cap or per-pass time budget).
5. **(F2)** Select the API key by the configured provider, not by env-var precedence.
6. **(S3)** Re-run this gate on a Prometheus-backed cluster with a workload that is genuinely
   over-floored and one with a daily/weekly pattern, to validate idle_window + forecasting + savings.

## Teardown

```bash
validation/run.sh teardown
```
