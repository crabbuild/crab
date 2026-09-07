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
configured as a BLAKE3 hash, never as the raw secret. `auth.psk_hash` must
contain exactly 64 ASCII hexadecimal characters; malformed input returns a
configuration error before service startup.

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
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-cache-dev \
  cargo run -p crab-cache-server -- --config cache.toml check --json
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-cache-dev \
  cargo run -p crab-cache-server -- --config cache.toml serve
```

Use a unique target directory for the checkout on the mounted workspace volume.

Startup constructs the authorization policy and origin client before opening or
evicting cache data. Invalid origin configuration therefore leaves the cache
untouched and starts no maintenance task. Successful client construction does
not prove origin connectivity; the readiness check also probes the origin.

If Tokio cannot initialize its runtime, serve, check, and onboarding probe
return exit code 1 with the underlying startup error on stderr. They do not
begin asynchronous service or probe work in that case.

The CLI also provides `evidence` verification/gating and `onboarding`
render/check/probe commands for repeatable deployment proof.

For JSON and text reports, success includes writing the complete output and
flushing stdout. A reported output error returns exit code 1 even when the
checks pass. If onboarding render cannot print its result, already-written
bundle files remain available. Evidence files use the same JSON writer as
stdout; this is not an atomic publication or power-loss durability guarantee.

## Eviction failures

Eviction updates metadata and byte counters only after removing the payload or
confirming that it is already absent. A filesystem removal error leaves the
entry accounted for and returns an error; admin eviction responds with HTTP 500.
The error retains the cache path and underlying I/O cause. Invalid-object
cleanup uses the same removal policy.

Removal reports object count separately from bytes. Deleting an empty object
counts as one eviction with zero bytes; deleting it again reports zero objects.
Batch eviction sums actual removal results instead of counting stale candidates.

## Shutdown ownership

Administrative eviction and staged upload/origin publication use a service-owned
blocking worker. Admission allows one such mutation at a time; cancelling a queued request removes it, while an
already admitted mutation finishes before server shutdown returns. Cancellation
does not roll back an admitted eviction or publication. The worker owns staged
files through commit; cancelled queued publications remove their temporary files.
Startup recovery and other synchronous disk reads are outside this worker's drain.

Periodic eviction runs disk/SQLite work on a blocking worker, one batch at a
time. Evictor shutdown stops polling and waits for any admitted batch before
releasing its cache reference. A shutdown signal takes priority over a queued
nudge. Dropping the handle alone does not request shutdown.

Signal registration happens before runtime dependencies are prepared. A
registration error returns through the server error path. TLS keeps signal
waiting inside the serving future, so a failed bind leaves no signal task.

| Transport | Normal shutdown |
| --- | --- |
| Plain HTTP | Stop accepting, then wait for in-flight requests without a hard deadline |
| TLS / mTLS | Stop accepting, then apply the configured drain timeout |

Abandoning the entire server future is a separate lifecycle concern; normal
shutdown tests do not prove completion of every dependency-owned task.

## Boundaries

- [`crab-cache`](../crab-cache/README.md) defines client-facing cache keys,
  capabilities, and HTTP semantics.
- [`crab-cache-store`](../crab-cache-store/README.md) is the client-side
  read-through adapter; it can fall back to origin when this service is down.
- [`crab-storage`](../crab-storage/README.md) owns provider-neutral origin
  access, while this crate owns server lifecycle and cache persistence.
