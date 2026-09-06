# SeaORM entity expectations for `gha-indie-worker-orm-core`

This server compiles today against an in-memory [`Store`](../src/store.rs) with
exactly the method set the SeaORM implementation will need. This document is the
contract the ORM crate must satisfy so that swapping the implementation changes
`src/store.rs` and nothing else.

Two hard rules, both fleet-wide:

* **This service never runs DDL.** No migrator, no `CREATE`, no `ALTER`, and no
  boot-time schema check that would tempt one. Schema convergence is the
  declarative pipeline's job. `/readyz` only asks whether the pool answers.
* **SeaORM only.** Direct `sqlx`, `tokio-postgres` and `diesel` use is
  forbidden, and `.github/workflows/ci.yml` fails the build if one appears in
  `Cargo.toml`.

Two pools, never three: `DATABASE_URL_CANONICAL` (product data) and
`DATABASE_URL_AUTH` (the federated auth projection). Admin connection strings
are unreachable from this process, and the firewall enforces that as well.

## Cargo wiring

The dependency is declared but **not** in `default`, because the repository is
not published yet:

```toml
[features]
orm-core = ["db", "dep:gha-indie-worker-orm-core"]
```

Enable it with `cargo build --features orm-core` once the crate exists. Until
then `db` pulls in SeaORM alone, so the pools, `/readyz` and the whole HTTP
surface are real while the entities are pending.

## Tables

Timestamps are `timestamptz not null default now()`. Identifiers are `uuid`
(v7 where the server mints them, so they sort by creation). Every tenant-scoped
table carries `org_id` and is expected to enforce row-level security on it.

### `orgs`

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `slug` | `text` unique not null | 1–64 chars of `[a-z0-9._-]` |
| `name` | `text not null` | ≤ 200 characters |
| `verified_domain` | `text` | lowercase DNS name, ≥ 2 labels |
| `seats` | `integer not null default 0` | purchased seat count |
| `created_at` | `timestamptz not null` | |

### `org_members`

| column | type | notes |
|---|---|---|
| `org_id` | `uuid` fk → `orgs.id` | composite pk with `user_id` |
| `user_id` | `uuid` fk → `users.id` | |
| `role` | `text not null` | `owner` \| `admin` \| `member` \| `billing` |
| `joined_at` | `timestamptz not null` | |

A partial unique index or trigger must make "at least one owner per org" true in
the database as well; the server enforces it in
[`domain::orgs::may_remove_member`], but an invariant that only lives in one
process is one deployment away from being false.

### `invitations`

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `org_id` | `uuid` fk → `orgs.id` | |
| `email` | `text not null` | normalised lowercase |
| `role` | `text not null` | the role the invitee receives |
| `state` | `text not null` | `pending` \| `accepted` \| `revoked` \| `expired` |
| `token_digest` | `bytea not null` | SHA-256 of the token; **never the token** |
| `created_at` | `timestamptz not null` | |
| `expires_at` | `timestamptz not null` | |

Index `(org_id, state)` and a unique index on `token_digest`.

### `users`

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `subject` | `text unique not null` | the auth authority's subject |
| `email` | `text` | normalised lowercase |
| `display_name` | `text` | ≤ 200 characters |
| `created_at` | `timestamptz not null` | |

### `onboarding_states`

One row per subject, covering both machines.

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `org_id` | `uuid` | set for the B2B machine |
| `user_id` | `uuid` | set for the B2C machine |
| `kind` | `text not null` | `org` \| `user` |
| `state` | `text not null` | see below |
| `updated_at` | `timestamptz not null` | |

`kind = 'org'`: `created` → `verified_domain` → `seats_allocated` →
`billing_linked` → `active`.
`kind = 'user'`: `signup` → `email_verified` → `workspace_created` → `active`.

A check constraint should require exactly one of `org_id` / `user_id`.

### `runs`

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `org_id` | `uuid` fk → `orgs.id` | |
| `repository` | `text not null` | exactly `owner/repo` |
| `revision` | `char(40) not null` | lowercase hex; **branches and tags are refused** |
| `workflow_path` | `text not null` | under `.github/workflows/` |
| `state` | `text not null` | `queued` \| `running` \| `succeeded` \| `failed` \| `cancelled` |
| `created_at`, `updated_at` | `timestamptz not null` | |

Index `(org_id, created_at desc)`.

### `jobs`

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `run_id` | `uuid` fk → `runs.id` | |
| `name` | `text not null` | bounded portable identifier |
| `profile` | `text not null` | one of the operator-reviewed profiles |
| `needs` | `text[] not null default '{}'` | |
| `state` | `text not null` | same lattice as `runs.state` |
| `sequence` | `integer not null` | deterministic topological position |
| `created_at`, `updated_at` | `timestamptz not null` | |

Unique `(run_id, name)`; index `(run_id, sequence)`.

### `log_chunks`

| column | type | notes |
|---|---|---|
| `run_id` | `uuid` fk → `runs.id` | |
| `job_id` | `uuid` fk → `jobs.id` | composite pk with `sequence` |
| `sequence` | `bigint not null` | strictly increasing per job |
| `stream` | `text not null` | `stdout` \| `stderr` |
| `at` | `timestamptz not null` | |
| `text` | `text not null` | ≤ 65536 bytes per chunk |

At most 4096 chunks per job; enforce with a retention job, not at write time.

### `workers`

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `org_id` | `uuid` fk → `orgs.id` | |
| `name` | `text not null` | unique per org |
| `profiles` | `text[] not null` | profiles the pinned image can execute |
| `labels` | `text[] not null default '{}'` | ≤ 32 labels |
| `state` | `text not null` | `registered` \| `idle` \| `busy` \| `draining` \| `offline` |
| `registered_at`, `last_heartbeat_at` | `timestamptz not null` | |

Index `(org_id, state)`.

### `webhook_deliveries`

The in-process ledger in [`domain::webhooks::DeliveryLedger`] makes at-most-once
dispatch true for **one replica**. This table is what makes it true for more
than one; until it exists, keep the deployment single-replica.

| column | type | notes |
|---|---|---|
| `delivery_id` | `uuid` pk | GitHub's `X-GitHub-Delivery` |
| `claimed_at` | `timestamptz not null` | |
| `repository` | `text not null` | |
| `head_sha` | `char(40) not null` | |

Claim with `insert … on conflict do nothing` and treat zero affected rows as a
duplicate. The claim must be inserted **after** the payload is understood, so a
transient failure stays retryable with the same delivery id.

### `embeddings` — model-space v3

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `tenant_id` | `uuid not null` | the organisation; RLS boundary |
| `entity_kind` | `text not null` | `run` \| `job` \| `log_chunk` \| `workflow` \| `regression_finding` \| `document` |
| `entity_id` | `text not null` | ≤ 256 characters |
| `model` | `text not null` | the model that produced the vector |
| `dims` | `integer not null` | the model's **true** width, 1–4096 |
| `vector` | `halfvec(4100)` | zero-padded from `dims` into the fixed slots |
| `tsv` | `tsvector` | lexical companion for hybrid search |
| `created_at` | `timestamptz not null` | |

`vector` needs a raw column type — SeaORM has no native `halfvec`, so declare it
with `column_type = "custom(\"halfvec(4100)\")"` (or the equivalent raw
`ColumnType::custom`) and move values as `Vec<f32>`.

Two things matter here and are easy to get wrong:

* **One physical width, many model widths.** Every vector lives in 4100 slots
  regardless of the model, so the column never has to change when a model does.
  `dims` records the real width, and similarity is only ever computed over the
  first `dims` components — comparing padded tails would silently "succeed"
  across model spaces.
* **`dims` is asserted, not trusted.** The provider's response is checked
  against the configured dimension before anything is stored, because a provider
  that quietly changes width would otherwise corrupt the index.

Unique `(tenant_id, entity_kind, entity_id, model)`; HNSW index on `vector`
using the cosine operator class; GIN index on `tsv`.

### `regression_findings`

| column | type | notes |
|---|---|---|
| `id` | `uuid` pk | |
| `tenant_id` | `uuid not null` | |
| `run_id` | `uuid` fk → `runs.id` | the regressing run, when known |
| `entity_kind` | `text not null` | what the finding points at |
| `entity_id` | `text not null` | |
| `score` | `real not null` | cosine similarity, −1 … 1 |
| `confidence` | `text not null` | `strong` ≥ 0.85, `likely` ≥ 0.70, else `weak` |
| `created_at` | `timestamptz not null` | |

## What `src/store.rs` needs from the ORM crate

Each of these is one query; none of them is a transaction spanning two tables
except where noted.

| store method | shape |
|---|---|
| `slug_taken`, `insert_org`, `org`, `set_org_seats`, `set_org_domain` | orgs |
| `membership`, `members`, `owner_count`, `occupied_seats`, `insert_member` | org_members |
| `insert_invitation`, `invitations` | invitations |
| `user_by_subject`, `insert_user`, `set_display_name`, `memberships_of` | users (+ join) |
| `org_onboarding`, `set_org_onboarding`, `user_onboarding`, `set_user_onboarding` | onboarding_states |
| `insert_run` | runs + jobs, **one transaction** |
| `run`, `runs_of`, `set_run_state` | runs |
| `jobs_of`, `job` | jobs |
| `apply_job_state` | jobs + runs, **one transaction** (the rollup must be atomic) |
| `last_log_sequence`, `append_log`, `logs_of` | log_chunks |
| `insert_worker`, `worker`, `workers_of`, `update_worker` | workers |
| `claim_delivery` | webhook_deliveries, `on conflict do nothing` |
| `upsert_embeddings`, `embeddings_of` | embeddings |
| `push_sync`, `pull_sync`, `sync_clock` | the sync log (table pending) |
