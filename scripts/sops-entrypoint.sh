#!/bin/sh
# Container entrypoint: decrypt secrets at RUN time, then exec the real command.
#
# Pair with a Dockerfile that sets
#   ENTRYPOINT ["/usr/local/bin/sops-entrypoint.sh"]
#   CMD ["/usr/local/bin/<the server binary>"]
# so this script receives the real command as "$@" and hands off with exec.
#
# Nothing is decrypted at `docker build`. A secret decrypted during a build is
# written into an image layer and stays there: layers are immutable, so a later
# `RUN rm` does not remove it and anyone who can pull the image can recover it.
# `--build-arg` is worse still, since it is recorded in `docker history`.
#
# This image therefore carries only the sops binary. Both the ciphertext and the
# age key arrive at run time:
#
#   ciphertext -> $SOPS_SECRETS_FILE   (default /run/secrets/app.env)
#   age key    -> $SOPS_AGE_KEY or $SOPS_AGE_KEY_FILE
#
# so one image is valid in every environment and no secret-bearing artifact
# travels through a build cache, a provenance attestation, or a registry. See
# ORESoftware/ores-sops docs/consumer-boundary.md.
set -eu

: "${SOPS_SECRETS_FILE:=/run/secrets/app.env}"

# No ciphertext mounted: run the command unchanged. This keeps the same image
# usable where configuration arrives as plain environment variables — a
# Kubernetes env-from-secret, an ECS task secret, `docker run -e`, or a
# host-side `sops exec-env`/`--env-file` injection.
if [ ! -f "$SOPS_SECRETS_FILE" ]; then
  exec "$@"
fi

if [ -z "${SOPS_AGE_KEY:-}" ] && [ -z "${SOPS_AGE_KEY_FILE:-}" ]; then
  if [ "${SOPS_REQUIRE_KEY:-0}" = "1" ]; then
    echo "sops-entrypoint: ciphertext at $SOPS_SECRETS_FILE but no SOPS_AGE_KEY or SOPS_AGE_KEY_FILE (SOPS_REQUIRE_KEY=1)." >&2
    echo "  docker run -e SOPS_AGE_KEY=\"\$(cat ~/.config/sops/age/keys.txt)\" ..." >&2
    echo "  or mount a key and set SOPS_AGE_KEY_FILE=/run/secrets/age.key" >&2
    exit 1
  fi
  echo "sops-entrypoint: no age key supplied; starting without decrypting $SOPS_SECRETS_FILE" >&2
  exec "$@"
fi

command -v sops >/dev/null 2>&1 || {
  echo "sops-entrypoint: sops binary not in image" >&2
  exit 1
}

# `sops -d` writes to stdout, so the plaintext lives only in this shell's memory
# and never touches the filesystem.
#
# --input-type is explicit because the repository's tracked name ends in `.enc`.
# sops infers the store from the file extension, and `sops exec-env` has no
# --input-type flag at all, so a file named `.env.enc` is parsed as JSON and
# fails with "Could not unmarshal input data".
secrets=$(sops --decrypt --input-type dotenv --output-type dotenv "$SOPS_SECRETS_FILE") || {
  echo "sops-entrypoint: failed to decrypt $SOPS_SECRETS_FILE" >&2
  exit 1
}

# Parsed with `read` + `export`, never `eval`. Under eval a decrypted value
# containing `$(...)` or a backtick would execute, turning read access to the
# secrets file into arbitrary code execution inside the container; `export
# "$key=$value"` assigns it literally. Splitting on the first `=` only, via IFS,
# keeps values that themselves contain `=` — URLs, base64, JWTs — intact.
#
# A variable already present in the container environment wins, so an
# orchestrator can still override a single value without re-encrypting.
while IFS='=' read -r key value; do
  case "$key" in
    '' | '#'* | sops_*) continue ;;
    *[!A-Za-z0-9_]* | [0-9]*)
      echo "sops-entrypoint: skipping invalid variable name" >&2
      continue
      ;;
  esac
  if [ -z "$(eval "printf '%s' \"\${$key+x}\"")" ]; then
    export "$key=$value"
  fi
done <<EOF_SECRETS
$secrets
EOF_SECRETS
unset secrets

# The application replaces this shell and becomes PID 1, so `docker stop` and a
# Kubernetes pod deletion deliver SIGTERM straight to it and it gets its full
# terminationGracePeriod to drain and flush.
#
# Deliberately not `sops exec-env`: that runs the command as a child of sops,
# which then holds PID 1 and does not forward signals, so the application never
# sees SIGTERM at all.
exec "$@"
