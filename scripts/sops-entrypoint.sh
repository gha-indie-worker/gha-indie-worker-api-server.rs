#!/bin/sh
# Decrypt ores-sops ciphertext at RUN time, then exec the real command.
#
# Ciphertext is mounted read-only at $SOPS_SECRETS_FILE (default
# /run/secrets/app.env); it is never copied into the image. The age key arrives
# at runtime through SOPS_AGE_KEY_FILE (preferred) or SOPS_AGE_KEY. Plaintext
# exists only in this process environment before exec.
set -eu

: "${SOPS_SECRETS_FILE:=/run/secrets/app.env}"

if [ ! -f "$SOPS_SECRETS_FILE" ]; then
  exec "$@"
fi

if [ -z "${SOPS_AGE_KEY:-}" ] && [ -z "${SOPS_AGE_KEY_FILE:-}" ]; then
  if [ "${SOPS_REQUIRE_KEY:-0}" = "1" ]; then
    echo "sops-entrypoint: no SOPS_AGE_KEY or SOPS_AGE_KEY_FILE set (SOPS_REQUIRE_KEY=1)." >&2
    exit 1
  fi
  echo "sops-entrypoint: no age key supplied; starting without decrypting $SOPS_SECRETS_FILE" >&2
  exec "$@"
fi

command -v sops >/dev/null 2>&1 || { echo "sops-entrypoint: sops binary not in image" >&2; exit 1; }

secrets=$(sops --decrypt --input-type dotenv --output-type dotenv "$SOPS_SECRETS_FILE") || {
  echo "sops-entrypoint: failed to decrypt $SOPS_SECRETS_FILE" >&2
  exit 1
}

# Parse with read + export, never eval. Split on the first '=' only so URLs,
# base64 values and JWTs stay intact. Orchestrator-set variables win.
while IFS='=' read -r key value; do
  case "$key" in
    '' | '#'* | sops_*) continue ;;
    *[!A-Za-z0-9_]* | [0-9]*) echo "sops-entrypoint: skipping invalid variable name" >&2; continue ;;
  esac
  if ! printenv "$key" >/dev/null 2>&1; then
    export "$key=$value"
  fi
done <<EOF_SECRETS
$secrets
EOF_SECRETS
unset secrets

# Application becomes PID 1 so docker stop / Kubernetes SIGTERM reach it.
exec "$@"
