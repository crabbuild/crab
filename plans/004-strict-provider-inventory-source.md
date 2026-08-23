# Plan 004: Add strict provider inventory adapters as an optional GC source

> **Executor instructions**: This plan is optional for live-list GC but is
> required before advertising inventory-backed deletion. Follow every gate and
> stop on a provider-schema mismatch. Update `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat b738f3b2..HEAD -- crab/src/cost/inventory crab/src/cost/engine.rs crab/src/cost/report.rs crab/src/cost/mod.rs crab/src/cmd/gc crab/src/core/config.rs crab/Cargo.toml Cargo.lock`
> Re-read the provider report files if the diff is non-empty.

## Status

- **Priority**: P2
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/002-durable-bounded-gc-engine.md`,
  `plans/003-persist-shard-closures.md`
- **Category**: security
- **Planned at**: commit `b738f3b2`, 2026-08-22

## Delivery status

The strict inventory contract and fail-closed destructive-source gate are
implemented in `crab/src/cmd/gc/inventory.rs`. Destructive GC remains
live-list-only; provider adapters and source-selection wiring are intentionally
not enabled until each provider's official manifest contract is pinned.

## Why this matters

At large object counts, recursive LIST can dominate cost and wall time, so a
provider inventory is a useful candidate source. The current inventory readers
belong to cost analysis, not deletion: they silently skip malformed rows,
coerce invalid sizes to zero, and sometimes label one format as another. Using
those types to authorize deletion would turn an incomplete report into an
unsafe negative proof. This plan adds a separate strict, scope-pinned adapter
to the durable GC engine; live LIST remains the default until each provider
passes its own evidence gate.

## Current state

- `crab/src/cost/inventory/report/s3.rs:31-108` parses CSV line-by-line,
  ignores malformed rows (`fields.len() < 5`), uses `parse().unwrap_or(0)`, and
  returns a cost `Inventory` without completeness or object-version identity.
- `crab/src/cost/inventory/report/gcs.rs:31-91` parses CSV despite documenting
  GCS Storage Insights Parquet, skips malformed rows, coerces size to zero, and
  reports `schema: parquet` for the CSV parser.
- `crab/src/cost/inventory/report/azure.rs:29-92` has the same skip/zero
  behavior and returns a cost-only `Inventory`.
- `crab/src/cost/inventory/report/mod.rs:1-28` advertises S3 Parquet/ORC/CSV,
  GCS Parquet, and Azure CSV, but the implemented public helpers shown above
  do not provide strict parsing for those formats.
- `crab/src/cost/engine.rs:55-150` selects live/report inventory for pricing and
  records report staleness, not deletion authorization. No current GC caller
  consumes `Inventory` or provider report rows.
- `crab/Cargo.toml:390-391` already includes the `parquet` crate for diff and
  inventory work; do not infer that a dependency means a parser is implemented.

## Strict deletion-source contract

Create a distinct `DeletionInventory`/`GcCandidateSource` type. It must bind:

- provider, bucket/container, exact GC scope/prefix, report manifest identity,
  generation time, and maximum allowed staleness;
- object key, exact byte size, last-modified timestamp, and provider version/
  ETag/delete-marker identity where the provider exposes it;
- expected row/file count and a complete-manifest marker; and
- parser/schema version and a digest of all report members.

Every relevant row must parse and validate. Unknown schema, truncated member,
duplicate identity, missing required column, invalid key/size/timestamp,
wrong bucket/prefix, incomplete manifest, or stale report fails the whole plan.
Never skip a row, convert a parse failure to zero, or label a CSV stream as
Parquet. A provider that cannot provide a stable identity must be live-list-only
for destructive GC; it may still feed cost estimates.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Cost inventory tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cost::inventory` | existing cost behavior remains valid |
| Strict parser tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_inventory` | all provider strict fixtures pass |
| GC adapter tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_source` | source identity/fail-closed tests pass |
| CLI/config tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --locked gc_cli` | source selection is explicit |
| Format | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo fmt --all -- --check` | exit 0 |

Before implementing each provider, re-read its current official inventory
manifest/schema documentation and pin the exact required columns in tests. If
the provider contract differs from the assumptions below, stop and update this
plan rather than accepting a permissive parser.

## Scope

**In scope**:

- new strict source types and parsers adjacent to
  `crab/src/cost/inventory/report/` (keep cost `Inventory` behavior unchanged)
- provider manifest/member validation for S3, GCS, and Azure formats that are
  actually supported by the current provider contract
- `crab/src/cmd/gc/` integration with Plan 002's candidate-source trait
- explicit CLI/config source selection and machine-readable source identity
- strict fixtures, fault tests, runbooks, and docs
- `Cargo.toml`/`Cargo.lock` only if an actually required parser is missing

**Out of scope**:

- changing cost-report totals or making cost analysis fail closed on estimates
- silently upgrading CSV to Parquet or inferring omitted provider fields
- making inventory mandatory for live-list GC
- PB layouts, provider migration, or cache-service deployment
- provider-specific delete APIs beyond identity/conditional-delete support

## Steps

### Step 1: Freeze a source-neutral strict interface

Add a candidate-source trait consumed by Plan 002 with methods for source
identity, bounded candidate batches, completeness proof, and object identity.
Make the engine reject a source whose scope, bucket, report digest, or freshness
does not match the run root snapshot. Keep report rows streaming; do not return
a complete `Inventory` or `Vec` to GC.

Add a source selector with current live LIST as the default and an explicit
inventory input (manifest/object path plus provider) for destructive use. Do not
reuse `[cost].inventory_source` implicitly: it is pricing policy and currently
allows `auto/live/report`. If a persistent GC setting is necessary, justify it
against current config/doctor flows before adding it.

**Verify**: source-contract tests reject wrong bucket/prefix, stale identity,
partial manifest, duplicate key/version, and an adapter that reports an
estimate-only `Inventory`. `cargo test -p crab --lib --locked gc_source` passes.

### Step 2: Make S3 parsing strict and format-accurate

Read the current S3 Inventory manifest and selected member schema. Implement
one strict reader for the supported format(s), including manifest checksum,
member count, bucket/key scope, exact size, timestamp, storage class, and
version/delete-marker fields required to avoid deleting a replacement. Stream
Parquet row groups or the documented text format under the engine byte budget;
do not materialize all rows.

Keep the existing permissive CSV helper for cost reports only, or rename it to
make that boundary obvious. The strict reader must return an error on malformed
CSV quoting, invalid integers/timestamps, missing columns, truncated Parquet/
ORC, duplicate identities, and any skipped relevant row. Add fixtures for a
valid multi-member report, wrong bucket, stale manifest, delete marker,
versioned object, malformed row, invalid size, truncated member, and reordered
rows.

**Verify**: S3 strict fixtures pass and a report with one malformed relevant row
fails before any candidate batch is sealed. Cost inventory tests still pass.

### Step 3: Add strict GCS and Azure adapters only for verified schemas

For GCS Storage Insights and Azure Blob Inventory, implement the exact current
provider formats after reading their manifests and schema declarations. Record
provider-specific generation/ETag/version/delete-marker fields when available.
Reject a report that omits a field needed for current object identity or scope
proof. Do not use the current GCS CSV helper as a Parquet parser and do not
silently accept Azure CSV fields with shifted columns.

Use independent fixtures for each provider: valid report, multi-file manifest,
wrong container/bucket, stale report, malformed row, invalid size/timestamp,
duplicate row, deleted/noncurrent object, truncated member, and provider
schema-version mismatch. If a provider cannot satisfy the deletion contract,
leave it supported for cost reporting but mark it destructive-GC unsupported in
the source registry.

**Verify**: strict GCS/Azure tests pass; unsupported formats fail with a
structured configuration error; no parser reports a schema different from the
bytes it consumed.

### Step 4: Integrate inventory candidates into the guarded engine

When the operator explicitly selects inventory, Plan 002 must seal the report
identity and use its stream instead of recursive LIST. Before each delete
batch, validate the report freshness/scope and revalidate object identity via
HEAD/version where the provider permits. A report/list mismatch, replacement
ETag, missing object, or provider inability to pin identity retains or pauses
the batch; it never deletes by key alone.

Retain live LIST as an explicit source and keep its metrics separate from report
metrics. JSON/JSONL must expose provider, report digest, generation time,
members, rows, rejected rows (always zero for an executable strict source), and
whether conditional identity checks were available.

**Verify**: integration tests assert zero recursive LIST calls for inventory
runs, fail closed on stale/partial/mismatched reports, and resume a paused run
after a fresh report is supplied. Live-list tests remain unchanged.

### Step 5: Document provider setup and the non-fallback rule

Update current GC, cost, provider, and operational docs with exact setup,
freshness, supported schema, scope, versioning, and recovery requirements.
State plainly that cost reports are not deletion authorization and that a
provider without strict identity remains live-list-only. Add a diagnostic that
prints source readiness without exposing credentials or report contents.

**Verify**: docs and `crab gc --help` agree; a repository-wide search finds no
claim that the current cost CSV readers are deletion-safe; doc/link checks pass
where configured.

## Test plan

- Strict parser fixtures for every advertised provider/format and every failure
  mode listed above.
- Property tests for row-order independence, duplicate detection, integer/time
  bounds, and bounded row-group memory.
- Source identity and engine tests for stale, partial, wrong-scope, replaced,
  and missing objects.
- Cost regression tests proving permissive estimate behavior is intentionally
  separate from strict GC behavior.
- In-memory object-store integration proving no delete starts before source and
  root proof is sealed.

## Done criteria

- [ ] Strict adapters are a separate type from cost `Inventory` and stream
      bounded candidate batches.
- [ ] Every advertised provider rejects malformed, incomplete, stale, or
      wrong-scope reports; no row is silently skipped or zeroed.
- [ ] Destructive inventory runs pin report identity and object identity, avoid
      recursive LIST, and fail closed when the provider cannot prove identity.
- [ ] Live-list GC remains the default and behavior is unchanged unless the
      operator explicitly selects inventory.
- [ ] Cost reports retain their existing estimate contract and tests.
- [ ] CLI, JSON/JSONL, runbook, provider fixtures, focused tests, and format
      checks pass.

## STOP conditions

- Official provider documentation does not support the assumed manifest or
  required identity fields.
- A parser would need to skip malformed rows, coerce invalid sizes, infer
  missing timestamps, or treat a cost estimate as a complete report.
- A provider cannot pin an object version/ETag strongly enough for delete
  authorization.
- Integrating the source requires a PB layout or an implicit config fallback.
- Any strict fixture can produce an executable plan with a rejected row.
- Any verification command fails twice after a reasonable fix attempt.

## Maintenance notes

Provider inventory schemas and parser dependencies are external contracts. Pin
the exact schema in fixtures and requalify when provider documentation,
`object_store`, Parquet/ORC libraries, or report generation settings change.
Reviewers should ensure the strict and cost paths cannot share a permissive
error branch, and that an inventory source can never silently fall back to live
LIST after a partial report.
