# OpenHPA - Operator Manual

This manual covers installation, configuration, and day-2 operation of the OpenHPA operator: an
open-source Kubernetes operator that analyzes your HorizontalPodAutoscalers and KEDA ScaledObjects
and recommends - then, once approved and in apply mode, applies - better autoscaling configuration.

It is written for a competent platform or SRE reader and is intended to be read locally (it ships
with the release, so air-gapped operators have it offline). Every command is copy-pasteable;
substitute the placeholders (`<release>`, `<namespace>`, `<version>`).

**Applies to:** chart and operator `0.1.x` (CRD `openhpa.dev/v1alpha1`).

## Contents

1. [Overview](./01-overview.md) - what the operator does, what it does not, and how it fits.
2. [Requirements](./02-requirements.md) - cluster version, RBAC, metrics, optional components.
3. [Installation](./03-installation.md) - signature verification, Helm install, air-gapped mirror.
4. [Configuration reference](./04-configuration-reference.md) - every Helm value and env flag.
5. [Usage and workflow](./06-usage.md) - reading, approving, and applying recommendations.
6. [Operating](./07-operating.md) - monitoring, high availability, upgrades, run modes.
7. [Security](./08-security.md) - RBAC model, data residency, image signing, egress.
8. [Troubleshooting and FAQ](./09-troubleshooting.md) - common failure modes and fixes.
9. [Uninstall](./10-uninstall.md) - removing the operator and its resources.

The file numbering skips `05` (the former licensing section) - OpenHPA has no licensing.
