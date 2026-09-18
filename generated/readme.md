# Generated output — do not edit directly

Everything in this directory except this notice is derivative output. Change the human-authored source, run the owning generator, and review the complete regenerated diff. Do not patch a language-specific artifact by hand.

## Current source map

The committed Dart, Gleam, Rust, and TypeScript `env` and `runtime` files are configuration projections generated from the repository-root `.cli-flags.toml` catalog by the flags-2-env toolchain. `.cli-flags.toml` remains the authority for those CLI/environment settings.

A CLI/environment catalog is not an application-domain, IPC, or network protocol. It therefore does **not** require a duplicate TypeSpec model merely to satisfy a file-layout rule. The generated-policy workflow treats this configuration projection separately and requires `.cli-flags.toml` to change whenever those files change.

## Semantic cross-language contracts

For generated job, runner, API, persisted-interchange, or wire types, TypeSpec and JSON Schema/OpenAPI must be independent, human-authored peer authorities. Neither may be generated from the other as the ultimate source of truth. Their independently generated normalized outputs must agree, and a machine-readable reconciliation receipt must be committed under `../contracts/parity/` before derivative output changes are mergeable.

A schema placed below `generated/` is itself derivative output and does not count as the human-authored JSON Schema/OpenAPI authority. Public/client, private/server-only, edge-only, and isomorphic contract surfaces remain separated in the authority sources.

## Service boundary

These configuration projections do not define or expose the API surface. The Rust service and the peer-authority interface repositories own the actual runtime behavior and network contract.

## Regenerate and freeze

Temporarily thaw only this tree before running the documented generator:

```sh
chmod -R u+w generated
```

After regeneration and review, freeze it locally:

```sh
find generated -depth -exec chmod a-w {} +
```

Git stores the regular-versus-executable bit, not arbitrary owner-write bits. A fresh checkout therefore restores ordinary files as writable even if a prior working tree used mode `0444`. CI is the durable merge control; `chmod a-w` is a checkout-local deterrent and is reapplied by the generated-source policy workflow.
