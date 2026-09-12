# gha-indie-worker-api-server.rs

The JSON API for **GHA Indie Worker** (`api.indiebuild.dev`) — the SaaS control
plane around `gha-indie-worker.rs` (the fixed-profile, immutable-SHA worker) and
`gha-clone-server.rs` (the fail-closed workflow planner).

It speaks four carriers over one domain model: HTTP, WebSocket, TCP and NATS.

```
                  ┌─ HTTP  /v1/*        (stateless JSON)
   one domain ────┼─ WS    /v1/ws       (run logs, presence, chat)
   model, four    ├─ TCP   length-prefixed JSON frames, same commands
   carriers       └─ NATS  giw.<env>.<domain>.<event>
```

## What it does

* **Organisations and onboarding.** Two explicit state machines — B2B
  (`Created → VerifiedDomain → SeatsAllocated → BillingLinked → Active`) and
  B2C (`Signup → EmailVerified → WorkspaceCreated → Active`) — plus members
  (`owner` / `admin` / `member` / `billing`) and single-use invitations.
* **Plans, runs, jobs, logs.** A workflow is compiled to operator-reviewed
  profiles; a run is only accepted for a **fully supported** plan at an
  **immutable 40-hex commit SHA**. Everything unsupported is reported, never
  approximated.
* **Workers.** Registration, capability matching, heartbeats, presence.
* **Webhooks.** GitHub `X-Hub-Signature-256` verified in constant time, with
  bounded delivery-id de-duplication.
* **Chat.** `/v1/chat/*` reverse-proxied to `ORES_CHAT_API_BASE` with the
  verified actor injected as headers.
* **Embeddings and regression correlation.** OpenAI-compatible provider behind a
  trait; vectors zero-padded into the fleet's fixed `halfvec(4100)` slots.
* **Sync.** `/v1/sync/*` push and pull of causal envelopes for opto-sync.

## Design commitments

These are the decisions the code is organised around, and the reasons they are
worth the structure.

**A functional core with effects at the edges.** Everything in `src/domain/` is
pure: values in, values out, typed errors, no `async` and no I/O. Every state
machine is a total function over `(state, event)` with **no `_ => Ok(state)`
catch-all**, so a replayed webhook or a double-clicked button is a typed
conflict rather than a silently skipped step. `src/store.rs` is the only place
that mutates, and each mutation is "read state → call a pure function → write
the result".

**Fail-closed where it counts, fail-open where it doesn't.** `/readyz` reports
not-ready without a database, so a process that cannot persist is taken out of
the load balancer. A run is refused unless every job is independently
executable. But a NATS outage only degrades observability, and a chat upstream
being down never fails a request that already succeeded.

**One verified actor, two authorities.** shared-auth introspection *or* a
Supabase/Neon JWT, collapsed to one `VerifiedActor { subject, org_id, roles,
scopes, source }`. Nothing downstream knows which authority spoke.

**Tenancy comes from the token, never the body.** A run's organisation is the
token's organisation. A run id belonging to another tenant is reported as *not
found*, not *forbidden*, so the API does not confirm that an id exists.

**Opaque rate-limit keys.** A raw IP, email, cookie or bearer token never
reaches limiter state, a log line, a cache entry, or a backend keyspace — only
an HMAC digest does. Identity beats network location: an authenticated caller is
keyed by subject, so a shared egress is not punished and a rotating address buys
no extra budget.

**Feature containment.** Every fleet dependency lives behind a default-on cargo
feature *and* in exactly one module. An upstream API change costs one flag, not
the build — and CI proves that by building the crate with all of them off.

| feature | default | dependency | confined to |
|---|---|---|---|
| `db` | on | `sea-orm` | `src/transport/db.rs` |
| `otel` | on | `next-loggers` | `src/telemetry.rs` |
| `ores-mw` | on | `ores-middleware` | `src/middleware.rs` |
| `ores-rl` | on | `ores-rl-lib-core` | `src/rate_limit.rs::ores_rl` |
| `shared-auth` | on | `shared-auth-client` | `src/auth/shared_auth.rs` |
| `interfaces` | on | `gha-indie-worker-interfaces` | `src/routes/v1/capabilities.rs` |
| `tcp-transport` | on | — | `src/transport/tcp.rs` |
| `nats-transport` | on | `async-nats` | `src/transport/nats.rs` |
| `orm-core` | **off** | `gha-indie-worker-orm-core` | not published yet |

## Startup

Configuration is resolved by flags-2-env (`src/flags.rs`) against the embedded
`.cli-flags.toml`: unknown options, invalid values and stray positionals fail
closed without echoing their values, and the resolved map is parsed by
`ApiConfig::from_map`.

`server::startup_plan` is the pure startup boundary: it validates immutable
configuration into an ordered set of typed listener bindings before `run`
performs any I/O. Invalid endpoints fail without producing a partial plan, and
the NATS endpoint is redacted from the plan's `Debug` output.

`src/web_api_plane.rs` binds the service to the fleet four-avenue policy catalog
(`k8s-web-api-data-plane`), and `transport::nats::envelope` is the broker-free
product request envelope on `dd.remote.web_api.gha-indie-worker.request`,
alongside the `giw.<env>.<domain>.<event>` domain-event publisher.

## Running it

Nothing is required. With an empty environment the server binds
`0.0.0.0:8080`, serves `/healthz`, `/metrics` and `/v1/capabilities`, refuses
every authenticated route with `401`, and reports itself **not ready**.

```console
cargo run
curl -s localhost:8080/healthz | jq
curl -s localhost:8080/v1/capabilities | jq
curl -si localhost:8080/v1/users/me | head -1   # 401, by design
```

With a database and an auth authority:

```console
cp .env.example .env    # then fill in what you have
DATABASE_URL_CANONICAL=postgres://… \
SHARED_AUTH_BASE=https://auth.indiebuild.dev \
SHARED_AUTH_INTROSPECT_SECRET=… \
  cargo run
```

Secrets in a real deployment come from `env/enc/*.env.enc` through ores-sops and
are decrypted at `docker run`, never at `docker build`.

## The gate

```console
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Plus the containment check CI runs:

```console
cargo test --no-default-features --features db,tcp-transport
```

## API surface

Unauthenticated: `GET /healthz`, `GET /readyz`, `GET /metrics`,
`GET /v1/capabilities`, and `POST /v1/webhooks/github` (which authenticates
itself with an HMAC). Everything else needs a bearer.

| method | path | scope |
|---|---|---|
| `POST` | `/v1/orgs` | `orgs:write` |
| `GET` | `/v1/orgs/{id}` | `orgs:read` |
| `GET POST` | `/v1/orgs/{id}/invitations` | `orgs:read` / `orgs:write` |
| `GET POST` | `/v1/orgs/{id}/members` | `orgs:read` / invitation token |
| `GET PATCH` | `/v1/users/me` | — (any verified actor) |
| `POST` | `/v1/onboarding/org/advance` | `orgs:write` |
| `POST` | `/v1/onboarding/user/advance` | — |
| `POST` | `/v1/plans` | `plans:write` |
| `GET POST` | `/v1/runs` | `runs:read` / `runs:write` |
| `GET` | `/v1/runs/{id}`, `/v1/runs/{id}/jobs` | `runs:read` |
| `POST` | `/v1/runs/{id}/jobs/{job}/events` | `runs:write` |
| `GET POST` | `/v1/runs/{id}/logs` | `runs:read` / `runs:write` |
| `GET POST` | `/v1/workers` | `workers:read` / `workers:write` |
| `GET` | `/v1/workers/{id}` | `workers:read` |
| `POST` | `/v1/workers/{id}/events` | `workers:write` |
| `POST` | `/v1/embeddings/index` | `embeddings:write` |
| `POST` | `/v1/embeddings/search` | `embeddings:read` |
| `POST` | `/v1/regressions/correlate` | `embeddings:read` |
| `ANY` | `/v1/chat/{surface}/…` | surface-dependent |
| `POST GET` | `/v1/sync/push`, `/v1/sync/pull` | `sync:rw` |
| `GET` | `/v1/ws` | bearer, or a `hello` frame |

JSON is `snake_case` on the way in and on the way out. Errors are RFC 9457
problem documents:

```json
{ "type": "urn:gha-indie-worker:api:not_found", "title": "not_found", "status": 404, "detail": "not found" }
```

`GET /v1/capabilities` publishes the live scope vocabulary, the executable
profiles, the limits, the contract revision, and the **explicit exclusions** —
so a client never has to discover an unsupported construct by having a run fail.

## Stateful carriers

WebSocket (`/v1/ws`) and TCP (`GHA_INDIE_WORKER_API_TCP_BIND`) speak the same
command vocabulary; a client picks a carrier, not a protocol.

```jsonc
// client → server
{"command":"hello","token":"…"}          // TCP always; WS only without a bearer
{"command":"subscribe_run","run_id":"…"}
{"command":"ping"}

// server → client
{"frame":"welcome","subject":"…","protocol":"giw.ws.v1"}
{"frame":"event","event":{"type":"log_appended","run_id":"…","sequence":7}}
{"frame":"lagged","missed":12}           // bounded channel; a slow client is told
```

TCP frames are a big-endian `u32` length followed by that many bytes of JSON.
The length is checked against the 64 KiB bound *before* a buffer is allocated,
so a hostile peer cannot ask the process to allocate four gigabytes.

Subscriptions are explicit: a socket that subscribes to nothing receives
nothing. Presence and chat topics are confined to the actor's own organisation.

## Layout

```
src/
  domain/      pure core — orgs, users, onboarding, plans, runs, workers,
               webhooks, chat, embeddings, sync
  routes/      HTTP surfaces (health + v1/*)
  transport/   db (SeaORM), http (+ graceful shutdown), ws, tcp, nats
  auth/        dual auth → one VerifiedActor; JWKS cache; guards
  store.rs     the effect boundary in front of the domain
  middleware.rs  ores-middleware installation
  rate_limit.rs  opaque principals + bounded window
  telemetry.rs   ores-otel → tracing bridge
  config.rs      env only, redacted Debug
docs/orm-core-entities.md   the SeaORM contract for gha-indie-worker-orm-core
```

## Related

`gha-indie-worker-interfaces` (contracts) · `gha-indie-worker-lib-core` (query
builders) · `gha-indie-worker-orm-core` (entities) ·
`gha-indie-worker-web-server.rs` (`app.` / `user.` / `org.` / `m.`) ·
`shared-auth` · `ores-middleware` · `ores-rate-limit` · `ores-otel` ·
`opto-sync` · `ores-chat`.
