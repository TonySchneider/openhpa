# ADR 0001 - In-cluster operator (Rust) over a central service

Status: Accepted

## Context

Autoscaling analysis needs a workload's real metric history. There are two broad shapes for a tool
that does this:

1. A **central service** that ingests each cluster's metrics (via cross-account access or a metrics
   push) and analyzes them remotely.
2. An **in-cluster agent** that reads metrics locally and does the whole job where the data already
   lives.

The category standard for Kubernetes autoscaling / rightsizing tooling is the in-cluster agent, for
good reasons: a central service means shipping cluster metrics off-site, operating a multi-tenant
ingest pipeline and time-series database, and (often) holding cross-account credentials — all of
which are a trust and operational burden, and a larger attack surface.

## Decision

Build OpenHPA as an **in-cluster Kubernetes operator** that runs entirely in the user's cluster. It
reads metrics, runs a deterministic rule engine (plus an optional, user-configured LLM for
explanations only), emits `ScalingRecommendation` custom resources, and — in apply mode — patches
approved targets behind a probation + auto-rollback safety net. No data leaves the cluster; there is
no central service, no phone-home, and it works air-gapped.

Implement it in **Rust** with kube-rs: a small, memory-safe, low-footprint binary is a good citizen
in someone else's cluster and an easy thing to reason about in a security review. Structure it as a
workspace: a pure, Kubernetes-free `core` crate (domain model, rule engine, forecasting, LLM
prompt/parse, synthesis) and an `operator` crate that wires `core` to Kubernetes.

## Consequences

Positive:
- Data never leaves the cluster — no central trust objection, no data plane to operate.
- Cloud-agnostic (EKS / GKE / AKS / on-prem); works air-gapped.
- Small, safe, low-footprint binary; easy to defend in review.
- The pure `core` crate is fast to unit-test without a cluster.

Costs:
- Users run an operator pod in-cluster — but that is the category norm and what applying changes
  requires anyway.
- Outcomes can't be measured centrally (by design — we don't see the data).
- Version drift across environments; mitigated by a signed release channel and diagnostics.

## Alternatives considered

- **Central service, cross-account metrics pull** — rejected: ships data out and runs central infra.
- **Central service with a metrics push + hosted TSDB** — better, but still off-cluster; rejected in
  favor of fully local.
- **Go / kubebuilder or Python / kopf** — viable; Rust chosen for footprint, safety, and the
  security story.
