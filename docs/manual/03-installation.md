# 3. Installation

OpenHPA is distributed as a cosign-signed container image and a Helm chart, both published as OCI
artifacts to GitHub Container Registry (GHCR):

| Artifact | Reference |
| --- | --- |
| Image | `ghcr.io/tonyschneider/openhpa` |
| Helm chart | `oci://ghcr.io/tonyschneider/charts/openhpa` |

Both are **public** — no registry login or pull Secret is needed to install. Throughout, substitute
`<version>` with the release you are installing (for example `0.1.0`), `<release>` with your Helm
release name, and `<namespace>` with the target namespace.

## 3.1 Quickstart (recommend-only, no egress)

Two commands take you from nothing to recommendations. The default install is **read-only** — it
never mutates a workload and makes **no outbound network calls**.

```bash
# 1. Install:
helm install <release> oci://ghcr.io/tonyschneider/charts/openhpa \
  --version <version> \
  --namespace <namespace> --create-namespace

# 2. Wait a few minutes while the operator collects metric history and analyzes, then read:
kubectl get scalerec -A
```

Then `kubectl describe scalerec <name> -n <namespace>` shows the reasoning, the proposed diff, and
the estimated saving. See [Usage & workflow](./06-usage.md) to read, approve, and apply them.

> **Want LLM-enriched explanations?** Add `--set llm.provider=openai --set
> llm.apiKey=<your-llm-api-key>` (or `llm.provider=anthropic`). This is the only thing that causes
> any egress — a per-workload metric summary is sent to your chosen provider using your own key. The
> deterministic rule engine still produces the actual recommendations either way. `llm.apiKey` makes
> the chart create a Secret, or point `llm.existingSecret` at one holding the key under `apiKey`.

## 3.2 Verify the image signature

The image is signed with **cosign keyless signing** (Sigstore / GitHub OIDC — no long-lived keys).
Verify that the image was produced by the OpenHPA release workflow before pulling it into your
cluster:

```bash
cosign verify \
  --certificate-identity-regexp '^https://github\.com/tonyschneider/openhpa/\.github/workflows/release\.yml@refs/tags/v.*$' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/tonyschneider/openhpa:<version>
```

A successful verification prints the certificate subject and the matched identity. The two flags
assert *who* signed it (the release workflow on a version tag) and *which* OIDC issuer vouched for
that identity (GitHub Actions); both must match or the command fails.

To pin to an immutable digest, resolve and verify by digest:

```bash
DIGEST=$(crane digest ghcr.io/tonyschneider/openhpa:<version>)
cosign verify \
  --certificate-identity-regexp '^https://github\.com/tonyschneider/openhpa/\.github/workflows/release\.yml@refs/tags/v.*$' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/tonyschneider/openhpa@${DIGEST}
```

The release also publishes an attested SPDX SBOM; verify it with
`cosign verify-attestation --type spdxjson …` against the same identity.

## 3.3 Enable apply mode

By default OpenHPA never mutates a workload. To let it **apply** approved changes (and drive
predictive schedules), switch the operating mode to `apply`:

```bash
helm upgrade <release> oci://ghcr.io/tonyschneider/charts/openhpa \
  --version <version> --namespace <namespace> --reuse-values \
  --set mode=apply
```

Notes:

- `mode=apply` patches **approved** recommendations only; nothing is mutated until you patch a
  recommendation `approved: true`. Leave `mode` at its `recommend` default to keep the operator
  advisory-only.
- Applied HPA changes go on a probation window and are auto-rolled-back if the workload degrades
  (see [Usage](./06-usage.md) and [Operating](./07-operating.md)).
- If your autoscaler specs are managed by a GitOps controller (Argo CD / Flux), a direct apply will
  be reverted on the next sync — prefer recommend-only there. See the README's GitOps section.

### Recommended add-ons

Point the operator at Prometheus for real metric history (strongly recommended):

```bash
  --set metrics.prometheusUrl=http://prometheus.monitoring.svc:9090
```

Run two replicas for availability (leader election is on by default, so only the leader ever
mutates):

```bash
  --set replicaCount=2
```

## 3.4 Air-gapped / private registry

In an air-gapped environment (or when you mirror the artifacts into your own registry), pull access
may be gated and the cluster needs credentials. Authenticate, mirror the verified image and chart
into your internal registry, then install from there.

**Authenticate to a gated registry.** Log your install host in to the registry before pulling the
chart or running `cosign verify` / `crane`:

```bash
echo <token> | docker login ghcr.io -u <github-username> --password-stdin
echo <token> | helm registry login ghcr.io -u <github-username> --password-stdin
```

If the **cluster** also needs credentials to pull the image, create a pull Secret in the install
namespace and reference it via `imagePullSecrets`:

```bash
kubectl create secret docker-registry openhpa-ghcr \
  --namespace <namespace> \
  --docker-server=ghcr.io \
  --docker-username=<github-username> \
  --docker-password=<token>
```

Then add `--set imagePullSecrets[0].name=openhpa-ghcr` to the `helm install` command. (Pulling the
public image needs none of this.)

**1. On a connected host, verify ([§3.2](#32-verify-the-image-signature)) and copy the image** into
your registry. With `crane`:

```bash
crane copy \
  ghcr.io/tonyschneider/openhpa:<version> \
  registry.internal.example.com/openhpa/openhpa:<version>
```

(Equivalent with `skopeo copy docker://… docker://…`.) To carry the signature across, also copy the
cosign artifacts, or re-verify against GHCR before the copy and rely on your internal registry's
controls thereafter.

**2. Pull and re-host the chart:**

```bash
helm pull oci://ghcr.io/tonyschneider/charts/openhpa --version <version>
helm push openhpa-<version>.tgz \
  oci://registry.internal.example.com/openhpa/charts
```

**3. Install from the internal registry**, overriding the image repository:

```bash
helm install <release> \
  oci://registry.internal.example.com/openhpa/charts/openhpa \
  --version <version> \
  --namespace <namespace> --create-namespace \
  --set image.repository=registry.internal.example.com/openhpa/openhpa \
  --set llm.provider=none
```

With `llm.provider=none` (the default), the operator makes **no** outbound calls at all.

## 3.5 Verify the install

```bash
# The operator pod is Running:
kubectl get pods -n <namespace> -l app.kubernetes.io/name=openhpa

# The CRD is registered:
kubectl get crd scalingrecommendations.openhpa.dev

# The operator started cleanly (look for "openhpa operator starting"):
kubectl logs -n <namespace> deploy/openhpa
```

On a healthy start the logs report the configured provider, watched namespaces, whether Prometheus
history is in use, and the operating mode. Recommendations begin to appear once the operator has
accumulated enough metric history — see [Usage and workflow](./06-usage.md) and, if none appear,
[Troubleshooting](./09-troubleshooting.md).
