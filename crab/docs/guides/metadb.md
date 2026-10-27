# crab metadb

Inspect Capsule v2 authority, verify reachable content, and maintain checkpoints.

## Repository authority

Metadata commands require a valid `v2/root`. They capture the root and
transaction-consistent ref heads, then use the catalogs selected by that view.
Missing or corrupt authority returns an error. Commands never interpret an
absent root as a manifest-v1 repository or reconstruct authority by listing
unrelated bucket objects.

## Synopsis

```bash
crab metadb diagnose [--deep] [--json]
crab metadb rebuild [--json]
crab metadb owner [--once] [--interval SECONDS] [--jsonl]
crab metadb cache stats
crab metadb cache clear
crab doctor --metadb [--json]
```

The former `--db` selector and `metadb compact` command are removed. File,
shard, xorb, visibility, and Git metadata form one authenticated correctness
unit. `crab optimize indexes diagnose` and `rebuild` call the same implementations.
Shard compaction remains a separate `crab compact` operation.

## Diagnose

```bash
crab metadb diagnose --json
crab metadb diagnose --deep --json
crab doctor --metadb --json
```

The default diagnosis authenticates root and ref-head state without downloading
stable capsule, checkpoint, or Git-pack bodies. It reports `protocol: capsule-v2`,
generation, root and state digests, visible refs and capsules, and checkpoint
presence. `doctor --metadb` reports the same authority and local cache health.
Configuration, storage, authority, and cache errors produce non-zero exits;
they are not reported as uninitialized databases or absent caches.

Deep diagnosis opens one captured view, authenticates its metadata and Git
visibility, verifies catalogued shards and xorbs, and reconstructs and checks the
reachable Git closure. A final activity probe rejects the result if repository
state changed during verification. Retry against a stable repository view.
Diagnosis does not change remote authority or acquire a writer fence.

## Rebuild

```bash
crab metadb rebuild --json
```

Rebuild acquires repository maintenance and GC writer fences before capturing
its root. It verifies the complete content closure before publishing a
checkpoint, and releases its leases on success or error. Publication uses
compare-and-swap against that exact root. A conflicting root change returns an
error; concurrent ref updates remain as an authenticated suffix.

An empty repository, or a checkpoint already covering the visible capsule
frontier, needs no new checkpoint. The JSON result reports
`checkpoint_published` and verified file, shard, xorb, pack, and Git-object counts.
Existing index-write counters remain zero: this command does not write SlateDB
indexes. Repeating rebuild reuses immutable content-addressed objects.

Rebuild cannot restore missing or corrupt authoritative metadata or payloads.
Recover those bytes from verified retained history, a replica, or a backup,
then rerun deep diagnosis. It never invents file bindings by scanning a bucket.

## Repository owner

```bash
crab metadb owner --interval 30 --jsonl
crab metadb owner --once
```

Run one continuous owner under a process supervisor with the repository's
configuration and credentials. It requires valid Capsule authority before
claiming the exclusive owner lease. The lease renews every third of its TTL;
cooperative cancellation drains work before release.

The continuous owner waits for unchanged root/ref activity for at least one
polling interval and five seconds. At 32 visible capsules it checkpoints the
frontier; a multi-pack checkpoint can also trigger physical repacking. `--once`
eagerly maintains a nonempty Git frontier. An already checkpointed empty suffix
is a no-op. The captured view is reused for exact-root publication.

An unchanged completed view is reused for up to ten minutes before full
maintenance revalidation. Quiet polls still capture independently mutable ref
heads, so request cost scales with ref count. Stable pack bodies are not
fetched on each poll. No manifest, locator, graph, or legacy receipt is written.

JSONL reports `protocol`, action, maintenance reason, next eligibility, inventory
availability, generation, pack counts, and elapsed time. Existing locator,
catalog-layer, and commit-graph work counters are zero under Capsule. When
`inventory_loaded` is false, zero pack counts mean inventory was not loaded.

## Local cache

```bash
crab metadb cache stats
```

Stats reports the bucket-scoped SQLite path, database-file size, chunk count,
installed shard count, and GC generation cursor. It uses the existing read-only
inspection API and SQL counts, without loading all chunk rows or opening a
writer-capable shared handle. A missing file reports an absent cache; unreadable
or invalid files return errors without replacement or repair. The size is the
main SQLite file, excluding WAL and shared-memory sidecars.

```bash
crab metadb cache clear
```

Stop Crab processes using this bucket before clearing. The current command
removes the local SQLite database file, forcing a cold cache on the next
operation. It does not modify remote content. Coordinating clear with live
SQLite handles and sidecars remains a separate lifecycle hardening task.

See [metadata diagnostics](https://crab.build/docs/cli/diagnostics/metadata-database)
for the operator workflow.
