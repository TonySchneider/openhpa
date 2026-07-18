# Architecture

OpenHPA is a Cargo workspace split into a pure domain crate and a Kubernetes operator crate. The
split is deliberate: everything that can be reasoned about and tested without a cluster lives in
`core`, and all Kubernetes I/O lives in `operator`.

## Crates

### `core/` — `openhpa-core` (Kubernetes-free)

Pure, side-effect-free domain logic. No `kube`, no network, no filesystem. Fast to compile and
unit-test.

- **`domain`** — metric snapshots, workload config, candidates, recommendation types, cost constant.
- **`rules`** — the deterministic rule engine: idle-floor, overprovisioned, thrashing, and scale-lag
  detection, plus `detect_predictable_peak` for the optional forecaster.
- **`forecast`** — periodicity-gated daily/weekly peak prediction and schedule-window construction.
- **`llm`** — prompt construction and strict-JSON parsing of an LLM analysis reply (provider-agnostic
  at this layer).
- **`synthesis`** — combines rule candidates and the (optional) LLM verdict into a final
  recommendation with a field-level diff and estimated savings.

### `operator/` — `openhpa-operator` (kube-rs)

Wires `core` to Kubernetes.

- **`crd`** — `ScalingRecommendation` CRD types (`openhpa.dev/v1alpha1`): `TargetRef`, `DiffEntry`,
  `ScheduleWindowSpec`, and the status (`phase`, `probationUntil`, `scheduleActive`).
- **`collector`** — reads `WorkloadConfig` + a fallback metric point from each HPA and each KEDA
  ScaledObject. KEDA-managed HPAs are skipped so a KEDA workload produces one recommendation.
- **`metrics`** — `MetricsSource`: `Prometheus` (`query_range` history backfill, survives restart) or
  the `HpaStatus` per-tick accumulation fallback (in-memory).
- **`llm`** — `LlmBackend`: `OpenAi` / `Anthropic` (via `reqwest`) or `RulesOnly`. Configurable base
  URL, per-request timeout, bounded concurrency, and a per-pass time budget.
- **`applier`** — pure `patch_for` / `revert_patch_for` producing HPA / KEDA merge-patches.
- **`safety`** — pure `evaluate_health` → `HealthVerdict` for the apply → probation → verify net.
- **`leader`** — single-leader election over a `coordination.k8s.io` Lease.
- **`controller`** — the reconcile loop: analysis pass (emit CRDs; all replicas) + leader-only apply
  pass + verify pass (auto-rollback) + schedule pass (drive/retract proactive windows).
- **`config`** — clap/env configuration, including `--mode=recommend|apply`.

### `deploy/` — Helm chart. `e2e-tests/` — cluster-backed tests. `docs/` — the manual.

## Reconcile passes

1. **Analysis** (all replicas) — collect metrics, run rules (+ optional forecast), and create
   `ScalingRecommendation` CRDs for new candidates. Existing recommendations (and human decisions on
   them) are left alone.
2. **Apply** (leader only, `--mode=apply`) — patch approved targets; HPA changes start a probation
   window.
3. **Verify** (leader only) — after probation, judge HPA health and mark `verified` or auto-revert to
   the exact pre-apply config. Runs regardless of mode (it only ever restores config OpenHPA set).
4. **Schedule** (leader only, `--mode=apply`) — drive proactive floor windows for periodic workloads
   and retract a schedule whose forecasted peak never materializes.

Each mutating pass re-confirms leadership immediately before running and is bounded by a time budget
shorter than the lease, so a lost or overrunning leader cannot keep patching while a peer takes over.

## Design decisions

- [ADR 0001](./adr/0001-in-cluster-operator.md) — in-cluster operator (Rust) over a central service.
- [ADR 0002](./adr/0002-open-source-conversion.md) — open-source conversion and the recommendation
  model.
