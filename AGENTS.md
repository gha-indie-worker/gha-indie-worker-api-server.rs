# GHA Indie Worker — api-server.rs

## Parent / root agent contract

The fleet-wide parent lives at:

- GitHub: https://github.com/oresoftware/my-ai/AGENTS.md
- Canonical disk path: `~/codes/oresoftware/my-ai/AGENTS.md`
- `~/codes/AGENTS.md` is a symlink to `~/codes/oresoftware/my-ai/AGENTS.md` (installed by `~/codes/oresoftware/my-ai/setup-final.sh`)

When this file and the parent disagree: follow this file for this repository's local layout and tools; follow the parent for org-wide conventions and the functional programming rules.

Canonical `api-server.rs` repository for [`gha-indie-worker`](https://github.com/gha-indie-worker).

- Internal runtimes: Rust, TypeScript, Dart.
- Contracts: JSON Schema in `gha-indie-worker-interfaces`.
- Auth: github.com/shared-auth.
- Sync: github.com/opto-sync.
- Telemetry: github.com/ores-otel.
- Flags: github.com/flags-2-env.
- Packages: github.com/zed-pkg.
- Never use React/JSX or webviews.
- Resolve git conflicts semantically; never rebase, stash, or reset.

## Code style and coding patterns

remember to modularize the rust, typescript and dart - not everything belongs in main.rs, main.ts and main.dart; also follow functional coding principles - fewer side-effects (use pure functions more), more immutability (immutable variables); but for stateful apps like the client or stateful servers like websockets or tcp connections, sometimes classes and oop make more sense than functional programming perse, but we can still adhere to functional programming more than usual. Favor exhaustive pattern matching and use formal methods checking too. Favor composability and re-use , so basically create more utility functions and routines for shared use. You can follow a medium level of D.R.Y. (don't repeat yourself) - in other words you can repeat yourself at medium amount (not too much not too little). Some chaining is totally fine, so either method-chaining (immutable sometimes although with classes can be mutable too for performance), and chaining via the pipe operator is ok in languages like gleamlang.

Functional programming is mostly the following:

+ explicit inputs
+ explicit outputs
+ immutable values
+ pure transformations
+ typed errors
+ explicit state transitions
+ composition
+ effects pushed outward
+ illegal states excluded by types

## api-server.rs — service contract

Everything above still applies. This section adds what is specific to this
service; where the two disagree about this repository's layout, this section
wins.

### The shape of the code

- `src/domain/` is a **functional core**: pure functions, typed errors, explicit
  state transitions, no `async` and no I/O. Business rules live here and nowhere
  else. If you find yourself writing a rule in a route handler, it belongs in
  `src/domain/` with a unit test next to it.
- `src/store.rs` is the only place that mutates state. Every method is "read
  state → call a pure domain function → write the result".
- `src/routes/` parses, calls the core, persists, and renders. Nothing more.
- Match exhaustively. There is no `_ => Ok(state)` catch-all in any state
  machine, on purpose: an event that is not on an edge must be a typed error, so
  a replayed webhook or a double-submitted form cannot silently skip a step.
  Adding a variant should break the build until every machine has decided what
  it means.

### Feature containment (do not break this)

Every fleet dependency is behind a default-on cargo feature **and** confined to
exactly one module. That is what keeps an upstream API change from costing the
whole build:

| feature | dependency | the only module allowed to name it |
|---|---|---|
| `db` | `sea-orm` | `src/transport/db.rs` |
| `otel` | `next-loggers` | `src/telemetry.rs` |
| `ores-mw` | `ores-middleware` | `src/middleware.rs` |
| `ores-rl` | `ores-rl-lib-core` | `src/rate_limit.rs` |
| `shared-auth` | `shared-auth-client` | `src/auth/shared_auth.rs` |
| `interfaces` | `gha-indie-worker-interfaces` | `src/routes/v1/capabilities.rs` |
| `nats-transport` | `async-nats` | `src/transport/nats.rs` |

CI builds the crate with all of them off (`--no-default-features --features
db,tcp-transport`). If you need a fleet type in a new place, put an adapter in
the owning module and depend on the adapter.

### Security invariants

- **Tenancy comes from the token, never the request body.** A resource belonging
  to another tenant is reported as *not found*, not *forbidden* — the API must
  not confirm that an id exists.
- **Opaque rate-limit keys only.** A raw IP, email, cookie or bearer token must
  never reach limiter state, a log line, a cache entry, or a backend keyspace.
- **Secrets never reach a log.** `ApiConfig` implements `Debug` by hand and
  redacts every credential-bearing field. If you add a secret, add its redaction
  in the same commit, and a test that asserts it.
- **No DDL, ever.** This service does not migrate, create or alter anything.
  Schema convergence is the declarative pipeline's job. SeaORM only — direct
  `sqlx`, `tokio-postgres` and `diesel` are refused by CI.
- **Readiness is fail-closed.** `/readyz` reports not-ready without a database.
  Liveness (`/healthz`) touches no dependency, so a database blip never restarts
  a healthy process.

### The gate

```console
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

`docs/orm-core-entities.md` is the contract `gha-indie-worker-orm-core` must
satisfy. Keep it in sync with `src/store.rs`: the store's method list is the
query list.
