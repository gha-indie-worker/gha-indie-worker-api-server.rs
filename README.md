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

Fallback path routing works with a plain localhost or tunnel hostname:

```text
/p/<project>/<session>/<service>/<path>
```

For example:

```text
http://127.0.0.1:8080/p/zed-pkg/pr-481/api/v1/packages
```

`src/routing.rs` resolves both forms into a typed `RouteTarget`. `src/ingress.rs` then asks a `ProjectRuntime` to ensure the requested `(project, session)` is running and returns a `ProxyPlan` pointing at that session's Rust load balancer. The plan carries `x-ores-project`, `x-ores-session`, and `x-ores-service` headers so the project load balancer can select the correct logical service while `ores-compose` remains free to use dynamic ports, Unix sockets, Docker networks, Podman/other OCI networks, or host processes internally.

The default routing suffix and path prefix are configurable with:

```text
GHA_INDIE_WORKER_ROUTING_DOMAIN_SUFFIX=local.indiebuild.dev
GHA_INDIE_WORKER_ROUTING_PATH_PREFIX=/p
```

Cloudflare is intentionally an outer adapter rather than an authority for project routing: the same resolver is used for tunneled traffic and direct laptop traffic.
