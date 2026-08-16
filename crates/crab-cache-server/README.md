# crab-cache-server

`crab-cache-server` is the server-side composition boundary for Crab's shared
cache service. It stores immutable objects on local disk, proxies misses to an
object-store origin, exposes authenticated cache and dedup HTTP APIs, and
maintains the metadata and eviction state needed to run the service safely.

## Why it exists

Organizations often have many Crab clients reading the same immutable shards
and xorbs. A shared cache reduces origin traffic and can answer cross-repo
chunk dedup queries, while keeping repository mutation and credentials out of
the cache itself. The server also gives operators explicit preflight,
evidence, and onboarding checks instead of treating a listening port as proof
of a healthy deployment.

## Architecture

```text
authenticated client
        │
        ▼
HTTP router / auth middleware / limits
        │
        ├── immutable cache store ── SQLite metadata + local files
        ├── origin client ────────── S3/GCS/Azure-compatible object store
        ├── dedup index ───────────── chunk presence and locations
        ├── evictor ───────────────── high/low watermarks
        └── metrics / health / admin
```

Public health and metrics routes include `/health`, `/health/live`,
`/v1/health`, `/v1/health/live`, and `/v1/metrics`. Authenticated routes
include capabilities, authorization checks, dedup queries, admin stats and
eviction, plus `/v1/{path}` for immutable GET, HEAD, and PUT operations.
Mutable paths are rejected by default; transparent origin proxying is an
explicit configuration choice and still does not cache mutations.

The binary is bounded by request timeouts, concurrency limits, and a maximum
object size. Authentication supports mTLS, bearer, and PSK modes. A PSK is
configured as a BLAKE3 hash, never as the raw secret.

## Configuration and usage

`CacheServerConfig` reads TOML sections for the server, TLS, auth, origin,
cache, dedup, eviction, and logging settings. A minimal reverse-proxy setup
looks like:

```toml
[server]
listen_addr = "127.0.0.1:8443"
mutable_path_mode = "strict"

[auth]
mechanism = "psk"
# Replace with the BLAKE3 hash of the deployment PSK.
psk_hash = "0000000000000000000000000000000000000000000000000000000000000000"

[origin]
url = "s3://example-bucket"

[cache]
root = "/data/crab-cache"
max_bytes = 1099511627776

[dedup]
scope = "all"
```

TLS can be configured in `[tls]` or terminated by a trusted reverse proxy.
Run readiness checks before serving traffic:

```text
cargo run -p crab-cache-server -- --config cache.toml check --json
cargo run -p crab-cache-server -- --config cache.toml serve
```

The CLI also provides `evidence` verification/gating and `onboarding`
render/check/probe commands for repeatable deployment proof.

## Boundaries

- [`crab-cache`](../crab-cache/README.md) defines client-facing cache keys,
  capabilities, and HTTP semantics.
- [`crab-cache-store`](../crab-cache-store/README.md) is the client-side
  read-through adapter; it can fall back to origin when this service is down.
- [`crab-storage`](../crab-storage/README.md) owns provider-neutral origin
  access, while this crate owns server lifecycle and cache persistence.
