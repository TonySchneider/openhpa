#!/usr/bin/env bash
# A2 real-cluster validation harness (plan 0006). Drives the operator in recommend-only mode against
# synthetic workloads on a real cluster (kind / k3s / managed) so its recommendations can be judged
# for quality + safety. This NEVER enables apply: every run passes --mode=recommend.
#
# Usage:
#   validation/run.sh setup                 # create namespace + apply the synthetic workloads
#   validation/run.sh many-hpas [N]         # add N extra idle HPAs (default 50) for the scale test
#   validation/run.sh run                   # run the operator (recommend-only) until Ctrl-C
#   validation/run.sh recs                  # dump the ScalingRecommendations emitted so far
#   validation/run.sh fault-prometheus-down # scale the Prometheus deployment to 0 (PROM_DEPLOY/PROM_NS)
#   validation/run.sh fault-prometheus-up   # restore it
#   validation/run.sh fault-llm-timeout     # run with the LLM base URL pointed at a black hole
#   validation/run.sh teardown              # delete the namespace + extra HPAs
#
# Config via env (all optional):
#   NAMESPACE          (default ss-validation)
#   OPENHPA_LLM_PROVIDER / OPENHPA_LLM_MODEL   LLM backend (use a REAL key for the quality run)
#   OPENAI_API_KEY / ANTHROPIC_API_KEY            BYO key, read by the operator
#   OPENHPA_PROMETHEUS_URL                       Prometheus base URL (recommended for real history)
#   PROM_DEPLOY / PROM_NS                          Prometheus deployment + namespace for fault tests
set -euo pipefail

NAMESPACE="${NAMESPACE:-ss-validation}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
PROM_DEPLOY="${PROM_DEPLOY:-prometheus-server}"
PROM_NS="${PROM_NS:-monitoring}"

cmd="${1:-}"; shift || true
case "$cmd" in
  setup)
    kubectl create namespace "$NAMESPACE" --dry-run=client -o yaml | kubectl apply -f -
    # Install the CRD straight from the operator so the validation cluster has it.
    cargo run -q --manifest-path "$REPO/Cargo.toml" -p openhpa-operator -- --print-crd | kubectl apply -f -
    kubectl apply -n "$NAMESPACE" -f "$HERE/workloads.yaml"
    echo "applied synthetic workloads to namespace '$NAMESPACE'. Give metrics-server a few minutes to populate."
    ;;
  many-hpas)
    n="${1:-50}"
    for i in $(seq 1 "$n"); do
      kubectl apply -n "$NAMESPACE" -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: { name: bulk-$i }
spec:
  replicas: 3
  selector: { matchLabels: { app: bulk-$i } }
  template:
    metadata: { labels: { app: bulk-$i } }
    spec:
      containers:
        - name: app
          image: registry.k8s.io/pause:3.9
          resources: { requests: { cpu: 25m }, limits: { cpu: 50m } }
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata: { name: bulk-$i }
spec:
  scaleTargetRef: { apiVersion: apps/v1, kind: Deployment, name: bulk-$i }
  minReplicas: 3
  maxReplicas: 12
  metrics:
    - type: Resource
      resource: { name: cpu, target: { type: Utilization, averageUtilization: 70 } }
YAML
    done
    echo "created $n bulk HPAs in '$NAMESPACE' (scale test: watch one reconcile pass stay within the lease)."
    ;;
  run)
    echo "running operator in RECOMMEND-ONLY mode against namespace '$NAMESPACE' (Ctrl-C to stop)"
    RUST_LOG="${RUST_LOG:-info,openhpa_operator=debug}" \
      cargo run --manifest-path "$REPO/Cargo.toml" -p openhpa-operator -- \
        --mode recommend \
        --watch-namespaces "$NAMESPACE" \
        --interval-seconds "${OPENHPA_INTERVAL_SECONDS:-60}" \
        --enable-leader-election=false
    ;;
  fault-llm-timeout)
    # 10.255.255.1 is unroutable: the request hangs until the per-request timeout fires. Watch the
    # operator log a timeout error per workload and keep reconciling (no panic, no recommendation).
    echo "running with LLM base URL pointed at a black hole + a 2s timeout"
    RUST_LOG="${RUST_LOG:-info,openhpa_operator=debug}" \
      OPENHPA_LLM_BASE_URL="http://10.255.255.1:9" \
      OPENHPA_LLM_TIMEOUT_SECONDS=2 \
      cargo run --manifest-path "$REPO/Cargo.toml" -p openhpa-operator -- \
        --mode recommend --watch-namespaces "$NAMESPACE" \
        --interval-seconds "${OPENHPA_INTERVAL_SECONDS:-60}" --enable-leader-election=false
    ;;
  fault-prometheus-down)
    kubectl -n "$PROM_NS" scale deploy "$PROM_DEPLOY" --replicas=0
    echo "scaled $PROM_NS/$PROM_DEPLOY to 0 - now run the operator and confirm it logs the empty fetch + keeps watching (no panic)."
    ;;
  fault-prometheus-up)
    kubectl -n "$PROM_NS" scale deploy "$PROM_DEPLOY" --replicas=1
    ;;
  recs)
    kubectl get scalingrecommendations -A -o wide || true
    echo "--- full specs ---"
    kubectl get scalingrecommendations -A -o yaml || true
    ;;
  teardown)
    kubectl delete namespace "$NAMESPACE" --ignore-not-found
    ;;
  *)
    sed -n '2,40p' "${BASH_SOURCE[0]}"
    exit 1
    ;;
esac
