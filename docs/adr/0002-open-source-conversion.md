# ADR 0002 - Open-source conversion and the recommendation model

Status: Accepted

## Context

OpenHPA began as a closed-source, self-hosted operator with an offline license that gated the
*apply* path (analysis was always unrestricted). To make the project a genuinely useful and
trustworthy open-source tool — and a clean public artifact — the commercial layer had to go, and the
public identity had to be consistent.

## Decision

Release OpenHPA under **Apache-2.0** with **no licensing, tiers, trials, or telemetry**. Every
implemented capability works out of the box.

Concretely:

- **Remove the license subsystem entirely** — offline verification, license issuance tooling,
  expiry/grace, cluster limits, and feature gates. Apply is now gated **only** by the explicit
  `--mode=apply` flag plus per-recommendation human approval (the pre-existing safety gates).
- **Drop the Kubernetes `Secrets` RBAC grant.** It existed solely to read the license Secret. The
  operator no longer reads any Secret via the Kubernetes API; the optional LLM key is injected as an
  environment variable by the pod spec.
- **Reposition around deterministic analysis.** The rule engine — not an LLM — makes every
  recommendation and every safety decision. The LLM is optional, off by default, and only enriches
  explanations. This is both the honest description of how the system works and the safer default.
- **Keep the CRD-based recommendation model** (a declarative resource carrying a field-level diff)
  as the stable core. It is deliberately shaped so that **GitOps-native pull-request generation can
  be layered on later** without touching the analysis engine.

## Breaking changes from the pre-open-source lineage

If you are migrating from a pre-open-source build, note:

| Area | Before | After |
|---|---|---|
| CRD API group | `stepscale.io` | `openhpa.dev` |
| Env var prefix | `STEPSCALE_*` | `OPENHPA_*` |
| License env/values | `*_LICENSE_*`, `license.*` | **removed** |
| Helm chart name | `stepscale-autoscaler` | `openhpa` |
| Container image | `ghcr.io/stepscale/...` | `ghcr.io/tonyschneider/openhpa` |
| Chart OCI path | `.../charts/stepscale-autoscaler` | `.../charts/openhpa` |
| Crates / binary | `stepscale-*` | `openhpa-*` |
| Lease name | `stepscale-autoscaler-leader` | `openhpa-leader` |
| RBAC | granted `secrets: get` | no Secrets access |

Because the CRD API group changed, this is a new resource type — there is no in-place upgrade from a
pre-open-source install; install fresh.

## Consequences

- Anyone can run every feature; contribution friction drops to zero.
- The security posture improves (no Secrets access, no license-server trust, egress off by default).
- A future GitOps PR-generation mode is an additive feature on the existing CRD, not a rewrite.
