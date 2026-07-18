# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues, pull requests, or
discussions.**

Instead, use GitHub's private vulnerability reporting:

1. Go to the **Security** tab of the repository.
2. Click **Report a vulnerability** (GitHub Private Vulnerability Reporting).

Please include a description, reproduction steps, affected version(s), and impact. You will receive
an acknowledgement, and we will work with you on a fix and coordinated disclosure. Please give a
reasonable window to address the issue before any public disclosure.

## Supported versions

OpenHPA is pre-1.0. Security fixes are made against the latest released minor version and `main`.

## Security model (what to keep in mind)

OpenHPA may be granted access to production clusters, so its trust posture matters:

- **Least privilege.** The operator's `ClusterRole` grants read on autoscalers/deployments/metrics
  and read-write **only** on autoscalers/ScaledObjects (for apply mode) and its own CRDs. It needs
  **no access to Kubernetes Secrets.**
- **No egress by default.** With `llm.provider=none` (the default) OpenHPA makes no outbound network
  calls. It has no telemetry and no phone-home.
- **Optional LLM egress is explicit.** If you enable an LLM provider, per-workload metric summaries
  (no secrets) are sent to that provider using your own API key. You choose the endpoint
  (`llm.baseUrl` can point at an in-cluster proxy or local model).
- **Safe by default.** The operator defaults to recommend-only mode and never mutates a workload
  unless you set `--mode=apply` *and* approve a specific recommendation. Applied changes are held on
  probation and auto-rolled-back if the workload degrades.
- **Signed artifacts.** Release images are multi-arch and cosign-signed (keyless/OIDC) with an
  attested SPDX SBOM; verify them before pulling.

See [docs/manual/08-security.md](./docs/manual/08-security.md) for the full model.
