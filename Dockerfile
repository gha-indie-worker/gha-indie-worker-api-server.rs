# syntax=docker/dockerfile:1.7
#
# gha-indie-worker-api-server — production container image.
#
# Multi-stage. The toolchain stage is well over a gigabyte; nothing from it
# reaches the runtime stage except one stripped binary, so the published image
# is debian:bookworm-slim plus that binary plus sops.
#
# BUILD
#
#   # arm64 is the default target. Both clusters are aarch64 — Graviton on the
#   # AWS/EC2 side, CAX on Hetzner — and building natively there is markedly
#   # faster than emulating amd64 through QEMU. Nothing here is arm-specific:
#   # pass a --platform list for a multi-arch index.
#   docker buildx build --platform linux/arm64 -t ghcr.io/gha-indie-worker/gha-indie-worker-api-server:dev --load .
#
#   # This crate resolves 1 dependency from git remotes. If any of them is
#   # private, pass a token as a BuildKit secret. It is tmpfs-mounted for the
#   # duration of one RUN and applied through process-scoped GIT_CONFIG_*
#   # variables, so it is never written to ~/.gitconfig and never lands in a
#   # layer or in `docker history` — unlike --build-arg.
#   GH_TOKEN="$(gh auth token)" docker buildx build \
#     --platform linux/arm64 --secret id=gh_token,env=GH_TOKEN \
#     -t ghcr.io/gha-indie-worker/gha-indie-worker-api-server:dev --load .
#
#   # OCI image index with provenance and SBOM attestations:
#   docker buildx build --platform linux/arm64 --push \
#     --provenance=true --sbom=true \
#     --output type=image,oci-mediatypes=true,name=ghcr.io/gha-indie-worker/gha-indie-worker-api-server:dev .
#
#   `just docker-build` / `just docker-run` / `just docker-push` wrap all three.
#
# DEPENDENCIES — <function dep_note at 0xebdfad9225f0>
#
# SECRETS — see the block above ENTRYPOINT, and ORESoftware/ores-sops
# docs/consumer-boundary.md. Nothing is decrypted at build time.

########################################
# Stage 0 — toolchain and cargo-chef
########################################
FROM rust:1-bookworm AS chef
# git: dependencies are resolved from git remotes.
# cmake, build-essential, perl: aws-lc-sys and ring, pulled in by rustls, build
# native code. pkg-config and libssl-dev cover any transitive openssl-sys.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ca-certificates pkg-config libssl-dev build-essential cmake perl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    cargo install cargo-chef --locked

########################################
# Stage 1 — plan: derive the dependency recipe
########################################
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

########################################
# Stage 2 — build
########################################
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

# (1) Compile ONLY the dependency graph. This layer is keyed on recipe.json —
#     that is, on the manifests — so a source-only change reuses it. Because
#     /app/target is a real directory here and not a --mount=type=cache, the
#     compiled dependencies are baked into the layer and survive a cold CI
#     runner, which a host-local BuildKit cache does not. That matters directly
#     to the GitHub Actions minutes budget.
# NOTE: no Cargo.lock is tracked on this branch, so the build cannot pass
# --locked and two builds of the same commit can resolve different dependency
# versions. For a binary crate the lockfile belongs in version control: commit
# one and add --locked to both cargo invocations below.
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=secret,id=gh_token \
    set -eu; \
    if [ -s /run/secrets/gh_token ]; then \
      t="$(cat /run/secrets/gh_token)"; \
      export GIT_CONFIG_COUNT=2; \
      export GIT_CONFIG_KEY_0="url.https://x-access-token:${t}@github.com/.insteadOf"; \
      export GIT_CONFIG_VALUE_0="https://github.com/"; \
      export GIT_CONFIG_KEY_1="url.https://x-access-token:${t}@github.com/.insteadOf"; \
      export GIT_CONFIG_VALUE_1="ssh://git@github.com/"; \
    fi; \
    cargo chef cook --release --recipe-path recipe.json

# (2) Bring in the real source and compile just this crate against the cooked
#     dependency artifacts already in /app/target.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=secret,id=gh_token \
    set -eu; \
    if [ -s /run/secrets/gh_token ]; then \
      t="$(cat /run/secrets/gh_token)"; \
      export GIT_CONFIG_COUNT=2; \
      export GIT_CONFIG_KEY_0="url.https://x-access-token:${t}@github.com/.insteadOf"; \
      export GIT_CONFIG_VALUE_0="https://github.com/"; \
      export GIT_CONFIG_KEY_1="url.https://x-access-token:${t}@github.com/.insteadOf"; \
      export GIT_CONFIG_VALUE_1="ssh://git@github.com/"; \
    fi; \
    cargo build --release --bin gha-indie-worker-api-server; \
    strip target/release/gha-indie-worker-api-server; \
    cp target/release/gha-indie-worker-api-server /usr/local/bin/gha-indie-worker-api-server

########################################
# Stage 3 — distroless runtime (opt-in: --target runtime-distroless)
########################################
# Smaller than the default stage and carries no shell or package manager at
# all. The trade-off is that nothing in it can run sops, so this variant cannot
# decrypt anything itself: the environment has to arrive already-plaintext, from
# a Kubernetes env-from-secret or a host-side `sops exec-env` / `--env-file`
# injection. Use it where the platform owns the secret store outright.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime-distroless
COPY --from=builder --chown=65532:65532 /usr/local/bin/gha-indie-worker-api-server /usr/local/bin/gha-indie-worker-api-server
ENV GHA_INDIE_WORKER_API_BIND=0.0.0.0:8080
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/gha-indie-worker-api-server"]

########################################
# Stage 4 — runtime (default)
########################################
FROM debian:bookworm-slim AS runtime
# ca-certificates: rustls and sea-orm need the system trust store to reach
# Postgres, Supabase and any upstream over TLS. libssl3 covers a transitive
# openssl-sys that links dynamically.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 \
 && apt-get clean && rm -rf /var/lib/apt/lists/*

# sops, copied from its own published multi-arch image rather than curled and
# checksummed: the right architecture is selected automatically for whatever
# --platform this image is built for, and there is no per-arch SHA table to keep
# current. sops is a static Go binary, so the alpine-built one runs here.
COPY --chmod=0755 --from=ghcr.io/getsops/sops:v3.10.2-alpine /usr/local/bin/sops /usr/local/bin/sops
COPY --chmod=0755 scripts/sops-entrypoint.sh /usr/local/bin/sops-entrypoint.sh
COPY --from=builder /usr/local/bin/gha-indie-worker-api-server /usr/local/bin/gha-indie-worker-api-server

# The bind address is set explicitly because this service's own default is a
# LOOPBACK address. Inside a container that accepts no traffic from outside the
# container's network namespace, so the pod would pass its own liveness probe
# and refuse every request arriving through the Service. Overridable as usual;
# no source change was needed.
ENV GHA_INDIE_WORKER_API_BIND=0.0.0.0:8080 \
    SOPS_SECRETS_FILE=/run/secrets/app.env \
    HOME=/tmp
EXPOSE 8080

# This service exposes no health route that the audit could find. Add one
# (`/healthz` liveness, `/readyz` readiness) before giving the Deployment
# probes, rather than pointing a probe at `/`.

# --- ores-sops: decrypt at `docker run`, never at `docker build` -------------
#
# The image ships the sops binary and nothing else secret-shaped. Both halves
# arrive at run time, which is what makes one image valid in every environment:
#
#   ciphertext -> $SOPS_SECRETS_FILE   (default /run/secrets/app.env)
#   age key    -> $SOPS_AGE_KEY, or a file at $SOPS_AGE_KEY_FILE
#
# Local, against the repository's own encrypted dev profile:
#
#   docker run --rm -p 8080:8080 \
#     -v "$PWD/env/enc/dev.env.enc:/run/secrets/app.env:ro" \
#     -e SOPS_AGE_KEY="$(cat ~/.config/sops/age/keys.txt)" \
#     ghcr.io/gha-indie-worker/gha-indie-worker-api-server:dev
#
# Kubernetes — project the ciphertext from a Secret or ConfigMap and supply the
# key from a Secret. Prefer the file form for the key: an environment variable
# is visible to anyone who can read the pod spec or run `docker inspect`,
# whereas a projected Secret lands on tmpfs.
#
#   volumeMounts:
#     - { name: sops-enc, mountPath: /run/secrets/app.env, subPath: app.env, readOnly: true }
#     - { name: sops-age, mountPath: /run/secrets/age, readOnly: true }
#   env:
#     - { name: SOPS_AGE_KEY_FILE, value: /run/secrets/age/key }
#     - { name: SOPS_REQUIRE_KEY,  value: "1" }   # fail closed once wired up
#
# Stronger still, and the preferred production path: leave the age key out of
# the container entirely and let a KMS/workload identity decrypt — that is a
# .sops.yaml change, not an entrypoint change. Or decrypt in an initContainer
# into an `emptyDir: { medium: Memory }` so the app never holds the key at all.
#
# With no ciphertext mounted the entrypoint runs the command unchanged, so this
# image also works where config arrives as plain environment variables.
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/sops-entrypoint.sh"]
CMD ["/usr/local/bin/gha-indie-worker-api-server"]
