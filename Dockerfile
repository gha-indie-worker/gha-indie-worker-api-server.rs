# syntax=docker/dockerfile:1.7
#
# Multi-stage image for gha-indie-worker-api-server.
# Prefer linux/arm64 (Apple Silicon / Graviton / Hetzner ARM):
#   docker buildx build --platform linux/arm64 -t gha-indie-worker-api-server:dev .
#
# ores-sops boundary:
# - never copy plaintext OR ciphertext environment material into an image layer;
# - mount ciphertext read-only at /run/secrets/app.env at runtime;
# - supply the age key through SOPS_AGE_KEY_FILE (preferred) or SOPS_AGE_KEY;
# - the entrypoint decrypts only in process memory before exec'ing the server.

FROM rust:1.90-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin gha-indie-worker-api-server \
    && strip "target/release/gha-indie-worker-api-server"

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && apt-get clean \
    && find /var/lib/apt/lists -mindepth 1 -delete \
    && useradd --system --uid 65532 --no-create-home --shell /usr/sbin/nologin app

COPY --from=build "/src/target/release/gha-indie-worker-api-server" "/usr/local/bin/gha-indie-worker-api-server"
COPY --from=ghcr.io/getsops/sops:v3.10.2-alpine --chmod=0755 /usr/local/bin/sops /usr/local/bin/sops
COPY --chmod=0755 scripts/sops-entrypoint.sh /usr/local/bin/sops-entrypoint.sh

ENV GHA_INDIE_WORKER_API_BIND=0.0.0.0:8080 \
    SOPS_SECRETS_FILE=/run/secrets/app.env \
    HOME=/tmp
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/sops-entrypoint.sh"]
CMD ["/usr/local/bin/gha-indie-worker-api-server"]
