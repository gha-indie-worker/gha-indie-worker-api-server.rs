#!/bin/sh
# Decrypt ores-sops ciphertext at RUN time, then exec the real command.
#
# Pair with a Dockerfile that sets
#   ENTRYPOINT ["/usr/local/bin/sops-entrypoint.sh", "<the real binary>"]
# so this script receives the real command as "$@" and hands off with exec.
#
# Never decrypt during `docker build`: that writes secrets into an immutable
# layer. The image carries only ciphertext (env/enc/<dev|prod>.env.enc, copied
# to $SOPS_SECRETS_FILE) plus the sops binary. The age key arrives at
# `docker run` via SOPS_AGE_KEY or SOPS_AGE_KEY_FILE. Plaintext lives only in
# this process's memory — never on disk, never in a layer.
#
# See https://github.com/ORESoftware/ores-sops
set -eu

: "${SOPS_SECRETS_FILE:=/app/secrets/app.env}"

if [ ! -f "$SOPS_SECRETS_FILE" ]; then
  exec "$@"
fi

if [ -z "${SOPS_AGE_KEY:-}" ] && [ -z "${SOPS_AGE_KEY_FILE:-}" ]; then
  if [ "${SOPS_REQUIRE_KEY:-0}" = "1" ]; then
    echo "sops-entrypoint: no SOPS_AGE_KEY or SOPS_AGE_KEY_FILE set (SOPS_REQUIRE_KEY=1)." >&2
    echo "  docker run -e SOPS_AGE_KEY=\"\$(cat ~/.config/sops/age/keys.txt)\" ..." >&2
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

# Parsed with read + export, never eval. Split on the first '=' only so
# URLs, base64 and JWTs stay intact. Orchestrator-set variables win.
while IFS='=' read -r key value; do
  case "$key" in
    '' | '#'* | sops_*) continue ;;
    *[!A-Za-z0-9_]* | [0-9]*) echo "sops-entrypoint: skipping invalid variable name" >&2; continue ;;
  esac
  if [ -z "$(eval "printf '%s' \"\${$key+x}\"")" ]; then
    export "$key=$value"
  fi
done <<EOF_SECRETS
$secrets
EOF_SECRETS
unset secrets

# Application becomes PID 1 so docker stop / k8s SIGTERM reach it directly.
# Not `sops exec-env`: that keeps sops as PID 1 and does not forward signals.
exec "$@"
