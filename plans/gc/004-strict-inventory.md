# Plan 004: Drive current GC and fsck from strict provider inventories

> **Executor instructions**: Implement only after standalone GC Plan 002's journal engine is
> canonical. Run every verification gate and stop on a STOP condition. Update
> this plan's status in `plans/gc/README.md` when done.
>
> **Drift check (run first)**:
> `git diff --stat ff7792a4..HEAD -- crab/src/cost/inventory crab/src/cmd/gc crab/src/cmd/fsck.rs crab/src/cmd/fsck_store.rs crab/Cargo.toml Cargo.lock crab/docs packages/web/content/docs/cli`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/gc/002-bounded-resumable-engine.md`,
  `plans/gc/003-shard-closures.md`
- **Category**: perf, security
- **Planned at**: commit `ff7792a4`, 2026-08-22

## Why this matters

At high object cardinality, recursive object LIST cannot remain the only
deletion control plane.
Crab already has provider report parsers, but they aggregate rows, split CSV by
comma, skip malformed records, and coerce invalid sizes to zero. This plan adds
strict Parquet inventory adapters to the canonical GC engine and shares the
same validated object/root streams with report-only fsck.

## Current state

- `crab/src/cost/inventory/report/s3.rs:37-93` uses `split(',')`, skips rows
  with too few fields, and calls `parse().unwrap_or(0)`.
- `crab/src/cost/inventory/report/gcs.rs:26-92` parses CSV while recording the
  source schema as Parquet; Azure has the same skip/zero pattern. These are cost
  summaries, not safe deletion inputs.
- `crab/Cargo.toml` and `Cargo.lock` already include `parquet 55`; prefer one
  strict typed format per provider over three partially correct format stacks.
- Standalone GC Plan 002 owns candidate normalization, external-memory join, sealed journals,
  execution, and resume. Inventory is only a `CandidateSource` adapter and must
  not create a second collector.
- Standalone GC Plan 003 supplies current shard/xorb/file reachability without
  recurring shard downloads. This roadmap does not depend on a future layout.

Official provider contracts verified while planning:

- AWS S3 Inventory supports CSV, ORC, and Parquet; `manifest.json` identifies
  source/destination, creation timestamp, schema/format, every data file, size,
  and checksum, with a separate manifest checksum:
  `https://docs.aws.amazon.com/AmazonS3/latest/userguide/storage-inventory-location.html`.
- Google Cloud Storage inventory supports CSV and Parquet. Manifest presence
  means all report shards were generated and records snapshot time, processed
  records, shard count, and shard filenames:
  `https://docs.cloud.google.com/storage/docs/insights/inventory-reports`.
- Azure Blob Inventory supports CSV and Parquet, emits multiple files for large
  runs, and uses the manifest checksum file as the completion marker:
  `https://learn.microsoft.com/azure/storage/blobs/blob-inventory`.

Re-read those official pages and the pinned Parquet API before implementation;
formats and dependency APIs are external contracts.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Inventory tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_inventory` | all matching tests pass |
| Fsck tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked fsck_inventory` | all matching tests pass |
| GC regression set | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib cmd::gc --locked` | all GC tests pass |
| Lockfile review | `git diff -- Cargo.lock crab/Cargo.toml` | no unreviewed dependency change |
| Format | `cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- `crab/src/cmd/gc/inventory/` (create)
- standalone GC Plan 002 candidate-source interfaces under
  `crab/src/cmd/gc/engine/`
- `crab/src/cmd/fsck.rs`, `crab/src/cmd/fsck_store.rs`
- `crab/src/main.rs` for inventory selection/configuration
- existing cost-report parsers only to rename/document them as non-authoritative
  summaries; do not silently change their product output
- `crab/Cargo.toml`, `Cargo.lock` only if unavoidable and reviewed
- provider inventory and GC/fsck runbooks under `crab/docs/` and web docs
- provider fixtures containing synthetic/non-sensitive data

**Out of scope**:

- Supporting every provider export format in the first destructive release.
  Accept strict Parquet only; reject S3 CSV/ORC and GCS/Azure CSV for deletion.
- Making inventory freshness equivalent to real-time state.
- Using cache indexes, billing exports, or sampled cost reports as roots.
- Automatic inventory configuration in customer accounts.
- Any remote-layout migration or unimplemented recipe/receipt format.
- Any live bucket-wide deletion.

## Git workflow

- Branch: `gc/004-strict-inventory`
- Commits: provider-neutral contract, S3, GCS, Azure, fsck/docs.
- Never commit real bucket names, account IDs, credentials, or inventory data.

## Steps

### Step 1: Define one provider-neutral validated report contract

Define `InventoryManifest`, `InventoryIdentity`, and streaming `InventoryRow`
types consumed by standalone GC Plan 002. Required normalized fields are exact key, size,
last-modified, live/delete-marker/version state, provider object identity,
source scope, report snapshot time, shard identity, and report identity.

Validation occurs before any row can enter an executable journal:

- expected source bucket/container and exact configured global/repo prefixes;
- supported provider, schema version, Parquet format, and required typed columns;
- completion marker/manifest, expected shard count/list, and every shard object;
- manifest/shard checksums or pinned provider generation/ETag as available;
- snapshot/freshness policy and non-future timestamps;
- record/count reconciliation where the provider supplies it;
- no unknown relevant object state, duplicate live identity, malformed key,
  invalid timestamp/size, or lossy integer conversion.

Inventory age only extends retention. A candidate must be old enough relative
to both the report snapshot and execution grace; objects absent because they
were created after the snapshot can never become candidates from that report.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_inventory_contract`
→ valid fixture streams rows; every completeness/scope/schema/freshness error
invalidates the source before journal seal.

### Step 2: Implement strict S3 Parquet ingestion

Read and pin `manifest.json` plus its checksum/version, validate source bucket,
creation timestamp, `fileFormat=Parquet`, declared schema, every file key/size/
MD5, and optional version/delete-marker fields. Stream only required Parquet
columns by row group under byte budget. S3 inventory order is unspecified;
standalone GC Plan 002 partitions/sorts it. Decode object keys exactly once and reject a key
outside the authorized scope.

Explicitly reject CSV and ORC in the destructive adapter even though the cost
report parser understands CSV and AWS offers ORC. The error must tell the
operator to configure Parquet or use live-list dry-run diagnostics.

**Verify**: S3 fixtures cover valid multi-file report, checksum mismatch,
wrong bucket/prefix, missing file, wrong format/schema, delete markers,
versions, invalid typed values, truncated Parquet, and row-order changes.

### Step 3: Implement strict GCS Parquet ingestion

Pin the manifest object generation, validate report configuration source and
destination, manifest presence, snapshot time, records processed, shard count,
and exact shard names. Require typed `project`, `bucket`, `name`, `size`,
creation/update time, generation, metageneration, and deletion state needed by
the policy. Pin every report shard generation/ETag available from HEAD before
reading; reconcile count after all shards.

Reject CSV for destructive use. Do not label CSV bytes as Parquet or infer a
header/default delimiter.

**Verify**: GCS fixtures cover valid multi-shard report, manifest count/name
drift, wrong source/scope, changed generation during read, stale/future report,
deleted/noncurrent object, invalid types, and truncated Parquet.

### Step 4: Implement strict Azure Parquet ingestion

Require the manifest checksum completion object, validate manifest digest,
account/container/rule/prefix/schema, and every inventory file in large
multi-file output. Pin blob ETag/version where exposed. Parse Parquet timestamp
millis and blob/version/snapshot/deletion fields without lossy coercion. Reject
CSV in the destructive adapter.

**Verify**: Azure fixtures cover valid multi-file report, missing completion
checksum, manifest mismatch, wrong container/rule/prefix, changed ETag, blob
versions/snapshots, invalid timestamp/length, and truncated Parquet.

### Step 5: Select inventory explicitly for the current Unified layout

Add explicit configuration/CLI input for a provider inventory manifest object
identity. Do not discover “latest” by broad LIST. The sealed journal records the
exact manifest and shard identities. Current repo prefixes and
`.crab/{shards,xorbs}` remain the only supported namespaces. Fixed-partition
live LIST remains supported for current deployments, while inventory is an
explicit alternative for large namespaces. Emit measured size/cost warnings
and never silently switch source after a failure.

The operator can request live LIST or inventory, and structured output says
which source was used. A stale inventory is a retention delay, not permission
to weaken freshness.

**Verify**: source-selection tests show no implicit fallback, no latest-report
guess, wrong-scope rejection, and exact identities in the journal.

### Step 6: Share the validated streams with report-only fsck

Make fsck consume the same inventory rows and current-layout reachability
streams to report missing required objects, unexpected objects, shard-closure/
registry drift, file-index drift, and provider identity changes. Fsck report mode
does not seal an executable delete journal and performs zero repair writes.
Repair remains an explicit existing recovery/doctor action.

There must be one parser contract and one mark/join engine in this standalone
GC track; do not import contracts from unimplemented layout plans.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked fsck_inventory`
→ deterministic corruption fixtures produce qualified findings and zero writes.

## Test plan

- Provider-neutral eligibility/freshness/scope properties.
- Strict S3, GCS, and Azure manifests plus Parquet row-group fixtures.
- Truncation, checksum/generation drift, duplicates, invalid types, and report
  changes during read.
- External-memory reports above RAM budget.
- No-fallback source selection and exact journal identity.
- Shared fsck report path with a write-counting store proving zero mutations.

## Done criteria

- [ ] Only strict, complete, scope-matched, fresh Parquet reports authorize
  inventory-driven deletion.
- [ ] Parse/manifest/shard errors are fatal; no row is skipped or coerced.
- [ ] Report and row streams stay within declared memory/open-file budgets.
- [ ] Inventory-backed destructive GC performs no recursive namespace LIST.
- [ ] Live LIST and inventory never silently substitute for one another.
- [ ] Fsck reuses the same validated streams without repair writes.
- [ ] Existing cost-report parsers are clearly non-authoritative.
- [ ] Targeted tests, GC tests, format, lockfile review, and `git diff --check`
  pass.

## STOP conditions

- Official provider documentation or pinned dependency source disagrees with
  a planned field/completeness assumption.
- A provider report lacks enough scope/completion/object identity to authorize
  deletion; keep that provider unsupported instead of guessing.
- A new parser dependency is needed without license, maintenance, and lockfile
  review.
- A malformed relevant row would be skipped, defaulted, or logged-and-continued.
- Inventory configuration would require broad customer-account mutation without
  explicit operator authority.

## Maintenance notes

Provider formats change. Retain official-contract links and fixture versions,
and requalify on `object_store`, Parquet, or provider schema changes. Supporting
another format is a new strict adapter, not a permissive branch in these ones.
