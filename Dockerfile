# syntax=docker/dockerfile:1
#
# Multi-stage image for gha-indie-worker-api-server.
# Prefer linux/arm64:
#   docker buildx build --platform linux/arm64 -t gha-indie-worker-api-server:dev .
#   docker run --rm --platform linux/arm64 \
#     -e SOPS_AGE_KEY="$(cat ~/.config/sops/age/keys.txt)" gha-indie-worker-api-server:dev
#
# ores-sops (https://github.com/ORESoftware/ores-sops):
#   env/enc/dev.env.enc and env/enc/prod.env.enc — ciphertext, committed
#   env/dec/<name>.env — plaintext, gitignored
# Decrypt at `docker run`, never at `docker build`. Age key via SOPS_AGE_KEY
# or SOPS_AGE_KEY_FILE. Orchestrator env (including OTEL_*) wins over secrets.
#
# ores-otel (https://github.com/ores-otel):
#   The app exports OTLP in-process. Default collector is
#   dd-otel-collector.observability.svc.cluster.local (HTTP/protobuf :4318).
#   Wrong port silently drops spans. Do not EXPOSE 4317/4318.
#   The *-sidecar.rs image is a separate loopback probe helper on
#   127.0.0.1:9090 — do not bake it into this image, do not publish :9090.

############################
# Stage 1 — build + strip
############################
FROM rust:1.90-bookworm AS build
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=cache,target=/src/target,id=gha-indie-worker-api-server-target-${TARGETARCH},sharing=locked \
    cargo build --release --bin gha-indie-worker-api-server \
    && strip "target/release/gha-indie-worker-api-server" \
    && cp "target/release/gha-indie-worker-api-server" "/usr/local/bin/gha-indie-worker-api-server"

############################
# Stage 2 — slim runtime + sops
############################
FROM debian:bookworm-slim AS runtime
ARG SOPS_ENV=prod
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && apt-get clean \
    && find /var/lib/apt/lists -mindepth 1 -delete \
    && useradd --system --uid 65532 --no-create-home --shell /usr/sbin/nologin app
COPY --from=build "/usr/local/bin/gha-indie-worker-api-server" "/usr/local/bin/gha-indie-worker-api-server"
COPY --from=ghcr.io/getsops/sops:v3.10.2-alpine --chmod=0755 /usr/local/bin/sops /usr/local/bin/sops
COPY --chmod=0755 scripts/sops-entrypoint.sh /usr/local/bin/sops-entrypoint.sh
# Ciphertext is optional. Bind-mount the repo so a missing env/enc does not
# fail the build; when present it is renamed to .env so sops can infer dotenv.
RUN --mount=type=bind,source=.,target=/src,ro \
    mkdir -p /app/secrets \
    && if [ -f /src/env/enc/${SOPS_ENV}.env.enc ]; then \
         cp "/src/env/enc/${SOPS_ENV}.env.enc" /app/secrets/app.env; \
       fi \
    && chown -R 65532:65532 /app /usr/local/bin/gha-indie-worker-api-server
ENV SOPS_SECRETS_FILE=/app/secrets/app.env \
    OTEL_SERVICE_NAME=gha-indie-worker-api-server \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://dd-otel-collector.observability.svc.cluster.local:4318 \
    RUST_LOG=info
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/sops-entrypoint.sh", "/usr/local/bin/gha-indie-worker-api-server"]
