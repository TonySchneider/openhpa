# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

First public open-source release of OpenHPA.

### Added
- Deterministic rule engine for HPA / KEDA ScaledObject analysis: idle-floor, overprovisioned,
  thrashing, and scale-lag detection with field-level diffs and estimated cost deltas.
- `ScalingRecommendation` custom resource (`openhpa.dev/v1alpha1`) with a human approval workflow.
- Prometheus history backfill (`query_range`) with a metrics-server / HPA-status fallback.
- Optional seasonal forecasting (off by default) for recurring daily/weekly peaks and proactive
  floor schedules.
- Apply mode with a probation window, post-apply health verification, and automatic rollback for
  HPA targets.
- Optional, off-by-default LLM integration (OpenAI / Anthropic, bring-your-own key) that enriches
  explanations only — never a safety decision. Configurable base URL, timeout, and concurrency.
- Leader election over a `coordination.k8s.io` Lease so `replicaCount: 2+` is safe.
- Helm chart, multi-arch cosign-signed image, and an attested SPDX SBOM in the release pipeline.

### Changed / Removed (from the pre-open-source lineage)
This release removes all commercial licensing from the codebase. See
[docs/adr/0002-open-source-conversion.md](docs/adr/0002-open-source-conversion.md) for the full
list of breaking changes if you are migrating from a pre-open-source build (CRD API group, env var
prefix, Helm value keys, chart/image names, and the removal of the license subsystem).
