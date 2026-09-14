# gha-indie-worker-api-server.rs

SeaORM JSON API. HTTP is default; TCP and NATS are opt-in features in `src/transport`.

## Laptop / Cloudflare ingress routing

The API server owns the logical ingress contract between Cloudflare Tunnel/Worker traffic and local `ores-compose` sessions. Callers address logical project/session/service names; they never depend on dynamically allocated localhost ports.

Preferred host routing:

```text
https://<service>.<project>.<session>.local.indiebuild.dev/<path>
```

Examples:

```text
api.zed-pkg.pr-481.local.indiebuild.dev/v1/packages
web.zed-pkg.pr-481.local.indiebuild.dev/
admin-api.zed-pkg.pr-481.local.indiebuild.dev/healthz
```

Fallback path routing works with a plain localhost or explicitly admitted tunnel hostname:

```text
/p/<project>/<session>/<service>/<path>
```

For example:

```text
http://127.0.0.1:8080/p/zed-pkg/pr-481/api/v1/packages
```

`src/routing.rs` resolves both forms into a typed `RouteTarget`. `src/ingress.rs` admits the logical route before any lazy-start side effect, then asks a `ProjectRuntime` to ensure the requested `(project, session)` is running.

The returned `ProjectIngress` is validated before it can become a `ProxyPlan`:

- the supervisor generation must be nonzero;
- TCP endpoints must be nonzero loopback addresses;
- Unix-domain sockets must be absolute, traversal-free, and below the trusted runtime root;
- unrelated local sockets such as `/var/run/docker.sock` are rejected.

The proxy plan carries trusted `x-ores-project`, `x-ores-session`, `x-ores-service`, and `x-ores-generation` metadata. Incoming hop-by-hop, forwarding, Cloudflare identity, and `x-ores-*` headers are stripped case-insensitively before trusted metadata is added. Header count and aggregate bytes are bounded, and CR/LF/NUL injection fails closed.

Forwarding headers are rebuilt only from transport-observed context. Client-provided `x-forwarded-*` values are never treated as authority.

Runtime selection remains behind the trusted compose/worker boundary. Public ingress cannot choose Docker vs Podman/containerd/native execution, an executable path, a network provider, a container/private address, or a host port. That policy belongs to `.ores-compose.yaml`, the supervisor capability snapshot, and the worker scheduler.

The default routing suffix and path prefix are configurable with:

```text
GHA_INDIE_WORKER_ROUTING_DOMAIN_SUFFIX=local.indiebuild.dev
GHA_INDIE_WORKER_ROUTING_PATH_PREFIX=/p
```

Cloudflare is intentionally an outer adapter rather than an authority for project routing: the same resolver and trust checks are used for tunneled traffic and direct laptop traffic.

## Verification

The Rust workflow checks the exact revision with locked dependency metadata, formatter checks, all-target/all-feature Clippy with warnings denied, all-feature tests, documentation tests, and no-default-feature tests. A workflow job that never executes repository steps is not considered verification evidence.
