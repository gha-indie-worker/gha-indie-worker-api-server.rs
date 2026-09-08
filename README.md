# gha-indie-worker-api-server.rs

SeaORM JSON API. HTTP is default; TCP and NATS are opt-in features in `src/transport`.

`server::startup_plan` is the pure startup boundary: it validates immutable
configuration into an ordered set of typed listener bindings before `run`
performs any I/O. Invalid endpoints fail without producing a partial plan.
