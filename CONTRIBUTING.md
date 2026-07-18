# Contributing to OpenHPA

Thanks for your interest in improving OpenHPA! This project is maintained by
[Tony Schneider](https://github.com/tonyschneider). Contributions of all sizes are welcome — bug
reports, docs, tests, and code.

## Ground rules

- Be respectful. See the [Code of Conduct](./CODE_OF_CONDUCT.md).
- By contributing, you agree your contribution is licensed under the project's
  [Apache License 2.0](./LICENSE).
- Never report a security issue in a public issue/PR — see [SECURITY.md](./SECURITY.md).

## Development setup

You need Rust `1.91.1` (pinned in [`rust-toolchain.toml`](./rust-toolchain.toml)).

```bash
cargo test -p openhpa-core                          # fast, pure-logic tests (no cluster)
cargo test --workspace                              # all unit tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all                                     # rustfmt; taplo fmt for Cargo.toml
```

Cluster-backed end-to-end tests need a local cluster and run serially:

```bash
kind create cluster --name openhpa-dev
cargo test -p e2e-tests -- --ignored --test-threads=1
```

## Architecture at a glance

- **`core/` (`openhpa-core`)** — pure, Kubernetes-free domain logic. This crate must stay free of
  any Kubernetes or I/O dependency; all cluster access lives in `operator/`. Keeping it pure is what
  makes the analysis fast to unit-test.
- **`operator/` (`openhpa-operator`)** — the kube-rs operator that wires `core` to Kubernetes.
- **`deploy/`** — the Helm chart. **`e2e-tests/`** — cluster-backed tests. **`docs/`** — the manual.

See [docs/architecture.md](./docs/architecture.md) for the module breakdown.

## Code conventions

- **`core` stays Kubernetes-free and side-effect-free.** No `kube`, no network, no filesystem.
- **Typed errors** (`thiserror`) with context — no bare string errors.
- **Narrow visibility** — items are private unless used across modules.
- **No `todo!()` / `unimplemented!()` / `dbg!`** — enforced by workspace clippy lints.
- Validate at the type level where practical.
- `Cargo.toml` files are `taplo`-formatted (aligned, sorted, `workspace = true` deps).

CI runs `fmt --check`, `taplo fmt --check`, `clippy -D warnings`, and the full test suite on every
pull request. Please make sure these pass locally first.

## Pull requests

1. Fork and branch from `main`.
2. Keep the change focused; add or update tests for behaviour changes.
3. Update docs when you change flags, values, RBAC, the CRD, or metrics.
4. Open a PR describing the *why*, not just the *what*.

## Good first issues

- Additional detection rules in the rule engine.
- Post-apply health verification + auto-rollback parity for **KEDA ScaledObjects** (today HPA-only).
- **GitOps PR generation** — turn a `ScalingRecommendation` into a pull request against the source
  manifests (the CRD already carries a field-level diff by design).
- More Prometheus/metrics-server edge-case coverage.
