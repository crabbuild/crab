# Cache Service Implementation

This is the internal engineering reference for the Crab cache service. Public
operator-facing docs live under `packages/web/content/docs/cli/cache-service/` and
should avoid source paths and implementation details.

## Source Map

| Area | Source | Notes |
|------|--------|-------|
| Client HTTP API | `crates/crab-cache/src/cache_client.rs` | Calls `/v1/health`, `/v1/{path}`, and `/v1/dedup/query`. |
| Client store wrapper | `crab/src/cache/caching_store.rs` | Routes immutable reads through local cache, remote cache service, then origin. |
| Path classification | `crates/crab-cache/src/path_class.rs` | Shared immutable/mutable path contract for client and service. |
| Active probe | `crates/crab-cache/src/active_probe.rs` | Shared cache-service readiness probe used by `crab doctor` and onboarding. |
| Server bootstrap | `crates/crab-cache-server/src/server.rs` | Opens the cache database, rebuilds dedup index, starts HTTP server. |
| Router | `crates/crab-cache-server/src/state.rs` | Mounts public health/metrics and authenticated object/admin/dedup routes. |
| Handlers | `crates/crab-cache-server/src/handlers.rs` | Implements GET, PUT, dedup query, stats, and eviction endpoints. |
| Auth | `crates/crab-cache-server/src/auth.rs` | PSK, bearer extraction, native/proxy mTLS identity, and policy checks. |
| Config | `crates/crab-cache-server/src/config.rs` | TOML parsing and server defaults. |
| Origin client | `crates/crab-cache-server/src/origin_client.rs` | Builds object-store client from URL and cloud env. |
| Cache store | `crates/crab-cache-server/src/cache_store.rs` | On-disk object storage, metadata, byte accounting, and eviction. |
| Dedup index | `crates/crab-cache-server/src/chunk_index.rs` | Persistent chunk-to-xorb mappings from cached shards. |
| Metrics | `crates/crab-cache-server/src/metrics.rs` | Prometheus counters plus the low-cardinality admin traffic snapshot. |

## Public HTTP Contract

Public routes:

```text
GET /v1/health
GET /v1/health/live
GET /health
GET /health/live
GET /v1/metrics
```

Authenticated routes:

```text
GET  /v1/{path}
HEAD /v1/{path}
PUT  /v1/{path}
POST /v1/dedup/query
GET  /v1/admin/stats
POST /v1/admin/evict
```

`POST /v1/admin/evict` accepts either an `object_type` filter or one exact
immutable `path`; combining both is rejected.

`CacheClient::is_healthy()` must probe `/v1/health`. `/health` remains a
compatibility alias for external probes.

`POST /v1/dedup/query` accepts:

```json
{
  "repo_path": "org/team/repo",
  "chunk_hashes": ["64-hex-chunk-hash"]
}
```

The service authorizes `dedup` on `repo_path` and then applies the configured
`dedup.scope` before consulting the shared chunk index.

The response partitions the request by input index:

```json
{
  "known": [
    {
      "index": 0,
      "xorb_hash": "64-hex-xorb-hash",
      "chunk_index": 7,
      "length": 131072,
      "cache_verified": true
    }
  ],
  "unknown": [1, 2]
}
```

`xorb_hash` uses the canonical `MerkleHash::hex()` form used by
`.crab/xorbs/{first-two-hex}/{hash}` object paths. `chunk_index` is the chunk's ordinal
position inside the xorb metadata, not a byte offset. The service only returns
a chunk in `known` after the referenced xorb is present in the local cache, the
cached xorb parses with the expected aggregate xorb hash, and the referenced
chunk index, hash, and uncompressed length match the xorb metadata and payload.
Indexed chunks without this local cache proof are returned in `unknown`, so
clients repack them instead of trusting index-only entries. Push treats every
`cache_verified` response as a candidate: before shard metadata references an
existing xorb, the CLI verifies the xorb metadata and payload against origin.

## Object Path Contract

The service treats `/v1/{path}` as the object-store key.

Canonical Crab CLI paths:

```text
.crab/xorbs/{64-hex-id}
.crab/shards/{64-hex-id}
```

Repo-local Git pack paths:

```text
{repo-prefix}/packs/{pack-name}.pack
{repo-prefix}/packs/{pack-name}.idx
{repo-prefix}/packs/{pack-name}.meta
```

Versioned SlateDB metadata paths:

```text
{repo-prefix}/file_index_db/wal/{wal-id}.sst
{repo-prefix}/file_index_db/compacted/{sst-id}.sst
{repo-prefix}/file_index_db/manifest/{manifest-id}.manifest
{repo-prefix}/file_index_db/compactions/{compactions-id}.compactions
.crab/chunk_index_db/wal/{wal-id}.sst
.crab/chunk_index_db/compacted/{sst-id}.sst
.crab/chunk_index_db/manifest/{manifest-id}.manifest
.crab/chunk_index_db/compactions/{compactions-id}.compactions
```

Xorb and shard identities must be 64 hex characters. Pack names can be
non-hex; the cache store derives an internal storage ID from object type plus
URL pack name. SlateDB metadata is keyed by a hash of the full object-store
path and stored as generic metadata, not as the removed Crab `/file-index`
object type. Directory/list discovery under `file_index_db` and
`chunk_index_db` stays mutable and goes to origin.

For policy checks, canonical `.crab/*` objects use `.crab` as the synthetic
repo path.

## CacheStore Behavior

`CacheStore` stores objects under:

```text
{cache_root}/
├── cache.sqlite
├── xorbs/{hh}/{id}
├── shards/{hh}/{id}
├── packs/{hh}/{internal-id}
└── metadata/{hh}/{path-id}
```

Metadata key:

```text
[object_type_u8, storage_id_32]
```

Metadata value:

```text
size: u64
last_access: u64
access_count: u64
cached_at: u64
```

`CacheStore::put` verifies raw `blake3(body)` against an expected hash and is
kept for tests and body-addressed objects. HTTP object paths use
`put_unverified` for xorb and shard objects because production path identities
are Crab domain IDs, not necessarily raw body hashes.

Byte accounting is idempotent. Replacing an existing object updates
`current_bytes` by net size delta.

`cache.sqlite` stores object type, size, last access, access count, cached-at
time, and the chunk-to-xorb dedup index. The cache service opens separate
SQLite connections for object metadata and dedup lookups so HTTP reads,
eviction, and shard ingestion do not share one in-process mutex.

## Dedup Index

The service populates the `chunk_index` table in `cache.sqlite` when shard
objects are cached.

Production shard ingestion uses `ShardReader` and
`MDBShardInfo::read_all_xorb_blocks_full` to insert:

```text
chunk_hash -> xorb_hash, chunk_index, unpacked_segment_bytes
```

On startup, the service recursively scans:

```text
{cache_root}/shards/{hh}/{hash}
```

Only production Crab shard bytes are ingested. Unparseable shard files are
skipped with warnings so one corrupt shard does not prevent the cache service
from starting.

## Auth And Policy Internals

Auth identities:

| Mechanism | Principal |
|-----------|-----------|
| PSK | `psk-client` |
| Bearer | Raw bearer token string |
| Native mTLS | `mtls-sha256:<leaf-certificate-fingerprint>` |
| Proxy mTLS | `X-Client-CN` header from trusted proxy |

Policy rules are additive. Principal matching is exact. Repo matching supports
simple `*` suffix matching and exact matching.

`POST /v1/dedup/query` is repo-scoped. It checks whether the principal has
`dedup` permission on the supplied repo path, then applies `dedup.scope` before
reading the shared chunk index.

## Operational Implementation Notes

- Health readiness probes origin with a short timeout and caches the result
  briefly to avoid probe storms.
- Request-timeout middleware returns `408` only for elapsed request timers;
  other middleware failures are logged and returned as `500`.
- Mutable path handling is controlled by `server.mutable_path_mode`.
  Strict mode rejects mutable reads; transparent mode streams origin responses
  to the client without committing them to cache.
- Push warming is best-effort and must not fail a successful origin push.
  The push pipeline calls the cache-service warm path only after origin
  durability is established, so warming existing shards must not replay an
  origin PUT. Oversized warm requests are rejected at the HTTP body-limit
  boundary before cache writes are recorded. Xorb, shard, pack, pack-index, and
  metadata warm bodies stream into same-directory temp files before atomic cache
  commit. Xorbs are validated from the temp file by reading footer, metadata,
  and compressed chunk ranges; shards are validated with an incremental xet data
  hash before commit, then the committed shard is parsed to populate the dedup
  index.
- Range reads are served directly from warm cache. On cold range miss, the
  service streams the full immutable origin object into a same-directory temp
  file, validates and atomically commits it, then slices the requested response
  range from the cached file. If local cache commit fails after validation, the
  request is still served from the staged temp file when the bytes are still
  available.
- `GET /v1/admin/stats` returns cache inventory plus aggregate traffic counters
  for cache hits, misses, body origin fetches, metadata origin HEAD requests,
  coalesced misses, served bytes, transparent mutable proxy GET/HEAD/byte
  counts, mutable proxy stream errors, metadata object count, in-flight miss
  fills, startup and runtime cache-store integrity repair counters,
  per-object-type traffic, and dedup-index health. The same repair events are
  exported as `cache_integrity_repair_total` with `phase` and `event` labels.
- `crab-cache-server --config <file> check` is the server-side startup
  preflight. It validates listen bind, TLS PEM loading, auth posture, policy
  loading, cache metadata open, chunk-index rebuild, cache budget, and origin
  reachability without serving traffic or printing secrets.
- Policy loading validates non-empty rules, principals, repos, actions,
  trailing-only repo wildcards, and the supported action set (`read`, `write`,
  `dedup`, `admin`) before the server binds.
- Preflight JSON includes redacted policy diagnostics: rule count, repo-pattern
  count, and configured actions. It does not include principal values.
- Native mTLS is enabled by setting `tls.client_ca_path`. The rustls handshake
  requires a client certificate signed by that CA bundle, and auth ignores
  spoofable proxy identity headers in this mode.
- The Crab CLI cache client can add a private cache-service CA and present a
  client certificate/key for native mTLS. The data path and `crab doctor`
  share the same client identity contract.
- `crab doctor` expands a configured `[cache]` client into separate
  cache-service health, object-route auth, and optional admin-stats readiness
  checks. The admin check verifies cache byte budget and dedup-index rebuild
  state when the client credential has `admin`; credentials without `admin`
  get a warning instead of a hard failure.
- `POST /v1/dedup/query` carries the caller's repo prefix. The service
  authorizes the `dedup` action for that repo and enforces `dedup.scope` before
  reading the shared chunk index. Push classification trusts only
  `cache_verified` service hits before falling through to the remote
  `chunk_index_db`; index-only service entries are returned unknown. Because
  the cache server verifies the cached xorb hash plus referenced chunk metadata
  before setting `cache_verified`, the CLI can avoid index-only false positives.
  The service proof is still not a publish boundary: push verifies origin
  durability before skipping an xorb upload in every cache-service mode.
- The push pipeline opens MetaDb over the cache-aware `ObjectStore` when the
  cache wrapper is available. Versioned `file_index_db` and
  `.crab/chunk_index_db` GETs route through the cache service; SlateDB list,
  head, delete, and mutable discovery paths stay direct to origin.
- Existing-shard verification and file-index shard proof reads use the
  cache-aware store when configured. Duplicate pushes should not fetch shard
  bodies from origin once the shard is warm.
- The push pipeline loads the advisory `commit-graph-summary` only after a
  fast-forward probe needs the shallow-client fallback. New-ref, delete, and
  locally provable update pushes should not read that mutable object.
- Uncontended push locks use strict-create/update CAS tokens returned by the
  object store. The lock body is read only for contention, expired-lock reclaim,
  heartbeat renewal, or stale-token release fallback.
- Manifest pointer creates and updates cache the ETag returned by conditional
  PUT. Existing-manifest pushes still read the mutable manifest once at the
  start to establish the CAS base; successful writes should not re-read it.
- `CRAB_CACHE_SERVICE_URL` is the client env override for
  `cache.service_url`. Empty values are ignored so an unset CI secret does not
  erase a TOML-configured endpoint.
- `CRAB_CACHE_ORIGIN_URL`, `CRAB_CACHE_TLS_CERT`, and `CRAB_CACHE_TLS_KEY`
  are server env overrides. There is no `CRAB_CACHE_PSK_HASH` override.
- S3-compatible object-store setup normalizes uppercase AWS env vars for the
  object-store client.

## Verification Commands

```bash
cargo test -p crab --lib cache_service
cargo test -p crab-cache --lib active_probe::tests
cargo test -p crab cache:: --lib
cargo test -p crab --lib cmd::doctor::tests
cargo test -p crab-cache-server --lib preflight::tests
cargo test -p crab-cache-server --test cache_service_integration
cargo test -p crab-cache-server --test cache_server_preflight_cli
cargo check -p crab-cache-server --bin crab-cache-server
python3 scripts/e2e/run_cache_service_mtls_smoke.py
python3 scripts/e2e/run_cache_service_rustfs_smoke.py
python3 scripts/e2e/run_cache_service_rustfs_smoke.py --audit-report <report.json>
CACHE_SERVICE_RUSTFS_REPORT=<report.json> make cache-service-verify-smoke-report
crab-cache-server evidence verify --report <report.json> --json
crab-cache-server evidence summarize --report <report.json> --json
```

For end-to-end verification, start a real object-store origin, start
`crab-cache-server`, configure a disposable Crab repository with `[cache]`,
then verify:

- `/v1/health`, `/health`, and `/health/live` succeed.
- `crab-cache-server --config <file> check --json --profile enterprise`
  reports startup readiness with redacted origin URLs and secret-free auth
  details. RustFS smoke runs it with `--trusted-proxy-boundary` and stores the
  JSON as `cache_server_preflight_json` in the evidence bundle.
- RustFS smoke stores `cache_service_evidence_manifest` with SHA-256 hashes for
  `report.json`, `cache_server_preflight_json`, and retained copies of the
  smoke harness and report verifier. The verifier recomputes those hashes
  before accepting retained evidence.
- Report artifact references and evidence manifest file records are stored
  relative to the `report.json` directory. Moving the retained evidence
  directory does not invalidate `crab-cache-server evidence verify` or
  `crab-cache-server evidence summarize`.
- RustFS smoke starts `crab-cache-server` from private runnable TOML files but
  only retains redacted TOML/YAML artifacts. The retained verifier rejects
  default PSK values, PSK hashes, and policy principals in those artifacts.
- `crab-cache-server evidence verify --report <report.json>` validates retained
  evidence without requiring a live server config, so enterprise support can
  audit a retained evidence directory directly.
- `crab-cache-server evidence summarize --report <report.json>` emits the same
  manifest-backed verdict plus concise cache-hit, origin-avoidance, dedup,
  route-contract coverage, and enterprise-posture proof for customer handoff.
- `crab-cache-server evidence doctor --verification <release-verify.json>`
  groups failed release checks by operator action. Route-contract failures
  include compact expected/actual counts plus missing, unexpected, or retired
  route names in both JSON and text output.
- Native mTLS rejects clients without certificates, loads `policy_path`,
  enforces repo/admin actions for CA-signed clients, and lets a Crab repo
  configured with `cache.service_auth = "mtls"` pass `crab doctor --json`.
- Repeated immutable full-object, `HEAD`, and range reads report the expected
  `MISS`/`HIT` statuses.
- Oversized push-warming requests return `413` without storing cache bytes.
- Malformed `Range` requests return `400` without cache or origin traffic.
- The origin-counting proxy reports one RustFS/S3 GET per cold object, including
  concurrent cold misses.
- `crab doctor --json` in a configured client repo reports cache-service
  health, auth, and admin readiness without leaking PSK or bearer values.
- `crab push` reports a healthy cache service.
- Push warming stores shard and xorb objects.
- Admin stats show cached bytes, cache hits, cache misses, body origin fetches,
  metadata origin HEAD requests, and coalesced misses split by object type.
- Admin stats show dedup-index cardinality, startup rebuild status, configured
  scope, and the last shard-ingestion error if one occurred.
- A cold `crab hydrate --all` with a fresh client cache reads through the
  service and populates it from RustFS/S3.
- A second `crab hydrate --all` from another fresh client cache hits the cache
  service while RustFS/S3 origin GETs stay flat.
- A second overlapping push exercises dedup queries while xorb, shard,
  versioned metadata, advisory commit-graph, and uncontended lock origin GET
  deltas stay zero; the only origin GET is the single mutable manifest CAS-base
  read.
