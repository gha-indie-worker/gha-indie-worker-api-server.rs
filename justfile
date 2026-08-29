# --- docker (ORES canonical; rationale lives in the Dockerfile header) -------
# arm64 first: both clusters are aarch64 (Graviton on AWS, CAX on Hetzner).

docker_image    := env_var_or_default("DOCKER_IMAGE", "ghcr.io/gha-indie-worker/gha-indie-worker-api-server")
docker_tag      := env_var_or_default("DOCKER_TAG", "dev")
docker_platform := env_var_or_default("DOCKER_PLATFORM", "linux/arm64")

# Build locally. GH_TOKEN, when set, is handed to BuildKit as a secret for the
# private git dependencies; it is never written into a layer.
docker-build:
    #!/usr/bin/env bash
    set -euo pipefail
    args=(buildx build --platform "{{docker_platform}}" \
          -t "{{docker_image}}:{{docker_tag}}" --load .)
    if [ -n "${GH_TOKEN:-}" ]; then
      args+=(--secret id=gh_token,env=GH_TOKEN)
    elif command -v gh >/dev/null 2>&1 && GH_TOKEN="$(gh auth token 2>/dev/null)" \
         && [ -n "$GH_TOKEN" ] && [ "$GH_TOKEN" != "from-env" ]; then
      export GH_TOKEN
      args+=(--secret id=gh_token,env=GH_TOKEN)
    fi
    docker "${args[@]}"

# Run the image with an encrypted profile mounted read-only and the age key
# supplied at run time. Decryption happens inside the container at start-up;
# env/dec is never involved and no plaintext is written anywhere.
docker-run env="dev":
    #!/usr/bin/env bash
    set -euo pipefail
    args=(run --rm -p 8080:8080)
    enc="env/enc/{{env}}.env.enc"
    if [ -f "$enc" ]; then
      if command -v ores-sops >/dev/null 2>&1; then ores-sops verify; fi
      args+=(-v "$PWD/$enc:/run/secrets/app.env:ro")
      if [ -n "${SOPS_AGE_KEY:-}" ]; then
        args+=(-e "SOPS_AGE_KEY=$SOPS_AGE_KEY")
      else
        key="${SOPS_AGE_KEY_FILE:-$HOME/.config/sops/age/keys.txt}"
        [ -r "$key" ] || { echo "no age key: set SOPS_AGE_KEY or SOPS_AGE_KEY_FILE" >&2; exit 1; }
        args+=(-v "$key:/run/secrets/age.key:ro" -e SOPS_AGE_KEY_FILE=/run/secrets/age.key)
      fi
    else
      echo "note: $enc not present; starting with the ambient environment only" >&2
    fi
    docker "${args[@]}" "{{docker_image}}:{{docker_tag}}"

# Push an OCI image index with provenance and SBOM attestations.
docker-push tag=docker_tag:
    #!/usr/bin/env bash
    set -euo pipefail
    args=(buildx build --platform "{{docker_platform}}" --push \
          --provenance=true --sbom=true \
          --output "type=image,oci-mediatypes=true,name={{docker_image}}:{{tag}}" .)
    if [ -n "${GH_TOKEN:-}" ]; then args+=(--secret id=gh_token,env=GH_TOKEN); fi
    docker "${args[@]}"

# Prove the build context carries no plaintext, no ciphertext and no key
# material — the CI contract in ores-sops docs/consumer-boundary.md.
docker-context-audit:
    #!/usr/bin/env bash
    set -euo pipefail
    missing=0
    for pat in '.env' '.env.*' '**/*.env' 'env/dec' 'env/dec/**' 'env/enc' 'env/enc/**'                '**/*.pem' '**/*.key' '**/*.p8'; do
      if ! grep -qxF -- "$pat" .dockerignore; then
        echo ".dockerignore is missing the required exclusion: $pat" >&2
        missing=1
      fi
    done
    [ "$missing" -eq 0 ] || exit 1
    if grep -qE '^[^#]*COPY[^#]*env/enc' Dockerfile; then
      echo "Dockerfile COPYs env/enc: ciphertext must not enter the build context" >&2
      exit 1
    fi
    if grep -qE '^[[:space:]]*RUN.*sops.*(-d|--decrypt)' Dockerfile; then
      echo "Dockerfile decrypts at build time: the plaintext would be baked into a layer" >&2
      exit 1
    fi
    echo "docker context exclusions OK"
