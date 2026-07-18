# syntax=docker/dockerfile:1

# ---- builder ----
# Pinned to the workspace toolchain (rust-toolchain.toml). reqwest/kube use rustls-tls,
# so no OpenSSL is linked and a glibc distroless runtime needs no extra system libraries.
FROM rust:1.91.1-bookworm AS builder
WORKDIR /usr/src/openhpa
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/openhpa/target \
    cargo build --release --locked -p openhpa-operator \
    && cp target/release/openhpa-operator /usr/local/bin/openhpa-operator

# ---- runtime ----
# Distroless: no shell, no package manager, runs as an unprivileged user. Minimal attack surface
# for an in-cluster operator. cosign-signed at release (see .github/workflows/release.yml).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /usr/local/bin/openhpa-operator /usr/local/bin/openhpa-operator
# Numeric uid:gid of the distroless `nonroot` user. A non-numeric USER fails the kubelet's
# runAsNonRoot verification (CreateContainerConfigError); numeric lets any cluster verify it.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/openhpa-operator"]
