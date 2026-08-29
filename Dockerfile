# syntax=docker/dockerfile:1.7
#
# Multi-stage image for gha-indie-worker-api-server.
# Prefer linux/arm64 (Apple Silicon / Graviton / Hetzner ARM):
#   docker buildx build --platform linux/arm64 -t gha-indie-worker-api-server:dev .
#
# ores-sops: https://github.com/ORESoftware/ores-sops
# Decrypt at docker run, never at docker build.

FROM rust:1.90-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin gha-indie-worker-api-server \
    && strip "target/release/gha-indie-worker-api-server"

FROM debian:bookworm-slim AS runtime
ARG SOPS_ENV=prod
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && apt-get clean \
    && find /var/lib/apt/lists -mindepth 1 -delete \
    && useradd --system --uid 65532 --no-create-home --shell /usr/sbin/nologin app
COPY --from=build "/src/target/release/gha-indie-worker-api-server" "/usr/local/bin/gha-indie-worker-api-server"
COPY --from=ghcr.io/getsops/sops:v3.10.2-alpine --chmod=0755 /usr/local/bin/sops /usr/local/bin/sops
COPY --chmod=0755 scripts/sops-entrypoint.sh /usr/local/bin/sops-entrypoint.sh
RUN --mount=type=bind,source=.,target=/src,ro \
    mkdir -p /app/secrets \
    && if [ -f /src/env/enc/${SOPS_ENV}.env.enc ]; then \
         cp "/src/env/enc/${SOPS_ENV}.env.enc" /app/secrets/app.env; \
       fi \
    && chown -R 65532:65532 /app /usr/local/bin/gha-indie-worker-api-server
ENV SOPS_SECRETS_FILE=/app/secrets/app.env
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/sops-entrypoint.sh", "/usr/local/bin/gha-indie-worker-api-server"]
