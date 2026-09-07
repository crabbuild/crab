# crab-cache-server

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-cache-server/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns the cache-service binary, HTTP admission, origin composition, disk persistence, and maintenance tasks. crab-cache defines shared route/client contracts; repository mutation remains outside the cache service.

## Read first

1. `crates/crab-cache-server/src/lib.rs` — service modules.
2. `crates/crab-cache-server/src/server.rs` — `prepare_server / run_server`: listener/TLS startup and shutdown.
3. `crates/crab-cache-server/src/state.rs` — `build_router / AppState`: router and shared state.
4. `crates/crab-cache-server/src/handlers.rs` — `read_object / write_object`: request parsing and cache/origin decisions.
5. `crates/crab-cache-server/src/cache_store.rs` — `ServerObjectKey`: disk payload and SQLite accounting.

Trace one path: `crates/crab-cache-server/src/bin/crab_cache.rs` → `run_server` in
`crates/crab-cache-server/src/server.rs` → `build_router` in
`crates/crab-cache-server/src/state.rs` → handlers in
`crates/crab-cache-server/src/handlers.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Service startup | `crates/crab-cache-server/src/bin/crab_cache.rs` | `crates/crab-cache-server/src/server.rs` |
| Route or authorization | `crates/crab-cache-server/src/state.rs` | `crates/crab-cache/src/path_class.rs` |
| Disk retention | `crates/crab-cache-server/src/cache_store.rs` | `crates/crab-cache-server/src/evictor.rs` |

## Invariants

- Construct policy and origin dependencies before cache recovery, eviction, or
  background task startup. Await `PreparedServer::shutdown` after preparation
  succeeds; dropping an evictor handle does not stop its task.
  Sources: `crates/crab-cache-server/src/server.rs`, `crates/crab-cache-server/src/evictor.rs`.
- Handle fallible Tokio runtime creation at the binary boundary for serve,
  check, and onboarding probe; resource exhaustion is a startup error, not an
  `expect` panic. Source: `crates/crab-cache-server/src/bin/crab_cache.rs`.
- Register shutdown signals before preparing runtime dependencies. TLS signal
  waiting stays inside the serving future; listener failure must not detach it.
  Preserve the separate HTTP and TLS drain policies.
  Source: `crates/crab-cache-server/src/server.rs`.
- Command success includes output completion. Propagate JSON serialization,
  text/newline writes, and flush errors; keep stdout and evidence-file JSON on
  one writer. Output failures return normal errors instead of printing panics.
  Source: `crates/crab-cache-server/src/bin/crab_cache.rs`.
- Validate ASCII before byte-indexed hex decoding. Malformed PSK config and cache hashes must return errors rather than panic on UTF-8 boundaries.
  Sources: `crates/crab-cache-server/src/config.rs`, `crates/crab-cache-server/src/cache_store.rs`.

- Separate public health/metrics routes from authenticated object/admin routes; inspect router composition before moving middleware.
  Source: `crates/crab-cache-server/src/state.rs`.
- Path parsing and immutable admission precede cache/origin operations; reject invalid paths rather than normalizing them into a different object.
  Source: `crates/crab-cache-server/src/handlers.rs`.
- Payload integrity and persistent accounting must agree after put/eviction; inspect cache-store and evictor paths together.
  Source: `crates/crab-cache-server/src/cache_store.rs`.
- Failed payload removal must retain metadata, byte accounting, and eviction
  counters. Indexed eviction and invalid-object cleanup share `remove_cache_file`;
  preserve its typed I/O cause. Source: `crates/crab-cache-server/src/cache_store.rs`.
- Use removal's `EvictStats` for both count and bytes. Zero bytes can mean an
  empty object; a stale candidate contributes no eviction. Source:
  `crates/crab-cache-server/src/cache_store.rs`.
- Administrative eviction uses `CacheStore::run_mutation`. Keep the admission
  permit in the blocking worker and serialize tracker closure with spawning;
  cancelled requests must not detach mutations from `PreparedServer::shutdown`.
  Upload/origin publication remains a separate synchronous ownership path.
- Periodic eviction runs blocking work off the async executor. Shutdown signals
  the loop and joins the admitted batch; do not abort its outer task and detach
  a mutation. Source: `crates/crab-cache-server/src/evictor.rs`.

## Features and platform

No declared Cargo features. Binary `crab-cache-server` is declared at src/bin/crab_cache.rs. Disk/SQLite and loopback networking support are needed for relevant tests; TLS/live origin/eviction qualification needs the dedicated workflow.

## Verification

The Unix TLS startup fixture requires `openssl` to generate temporary certificates.
Inline state/handlers tests exercise routes and range errors; cache_store tests cover hash mismatch and persistence. Full service qualification is defined in `.github/workflows/cache-service.yml`.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-cache-server --locked --lib state::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-cache-server --locked --lib cache_store::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-cache-server/README.md` — usage and detailed contracts.
- `crates/crab-cache-server/Cargo.toml` — dependency and feature authority.

Read `crates/crab-cache-server/src/config.rs` and `crates/crab-cache-server/src/preflight.rs` before changing operational requirements.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
