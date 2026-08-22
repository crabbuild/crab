# crab metadb

Inspect, repair, and manage crab's SlateDB metadata subsystem.

## Overview

The metadb subsystem is two SlateDB instances that accelerate Crab's committed
manifest state. A per-repo `file_index_db` at
`{repo_prefix}/file_index_db/` holds generation-pinned file-to-shard records.
A globally shared `chunk_index_db` at `.crab/chunk_index_db/` holds immutable
committed chunk receipts plus a rebuildable point-readable head per chunk. A
local two-tier chunk-index cache (in-memory + SQLite) sits in front of the
remote chunk-index. Every lookup remains a candidate until its manifest,
GC-root, shard, and canonical-origin proof succeeds. This guide is for operators running the
`crab metadb` subcommands to diagnose problems, repair corruption,
and inspect state.

Architecture detail lives in
[docs/architecture/metadata-subsystem.md](../architecture/metadata-subsystem.md).

## Synopsis

```
crab metadb diagnose [--db file_index|chunk_index|both] [--json]
crab metadb rebuild  --db  file_index|chunk_index|both  [--json]
crab metadb compact  [--db file_index|chunk_index|both]
crab metadb cache    stats
crab metadb cache    clear
```

`--db` defaults to `both` everywhere it is accepted.

## Subcommands

### `crab metadb diagnose`

Read-only health snapshot of one or both databases. Reads the
`sys:*` keys (format version, epoch, created_at, and — for
`chunk_index_db` — `gc_generation`) and reports the open state and
path.

```bash
crab metadb diagnose
crab metadb diagnose --db chunk_index
crab metadb diagnose --db file_index --json
```

Safe to run concurrently with a push: `diagnose` opens each SlateDB
in read-only mode, so it does not fence an in-flight writer.
`--json` emits a `DiagnosePayload` structure suitable for scripting.

Use `diagnose` when you want to confirm a database opens cleanly,
check its epoch against the manifest, or verify the remote
`gc_generation` the local cache is being compared against.

### `crab metadb rebuild`

Disaster-recovery tool. Rebuilds acceleration records from only the shards and
Git packs named by the current manifest's segmented indexes. It writes
generation-pinned file records, candidate chunk records, exact Git object locators,
and a generation-index receipt tied to the committed pack/shard index hashes.

```bash
crab metadb rebuild --db chunk_index
crab metadb rebuild --db file_index
crab metadb rebuild --db both
```

Rebuild is idempotent: repeated runs produce the same receipt history and
point-readable heads, and an
interrupted run can be restarted without any special cleanup.

Rebuild is also the repair path after a crash between manifest CAS and
post-CAS acceleration indexing. It never scans or advertises orphan shards
outside the current manifest. See
[When to use `rebuild`](#when-to-use-rebuild) below for the specific
scenarios.

`--json` emits a `RebuildReport` summarizing shards, Git packs, and entries.
`crab doctor --metadb` reports the generation receipt, repo/bucket registry
completeness, and Git locator availability, and names the applicable repair
command.

### `crab metadb compact`

Request immediate SlateDB compaction on one or both databases.

```bash
crab metadb compact
crab metadb compact --db chunk_index
```

Currently a **no-op**. SlateDB drives compaction in the background
on its own schedule, and the public `slatedb` crate does not expose
an imperative trigger. The command is kept so operator runbooks can
call it without "unknown subcommand" errors, and so it becomes
real work the moment SlateDB exposes the API. The command logs a
warning explaining this and exits successfully.

### `crab metadb cache stats`

Report on the local chunk-index cache.

```bash
crab metadb cache stats
```

Prints the SQLite cache path, on-disk size, entry count, installed
shard count, and the `cache_gc_generation` cursor. Useful before
running `cache clear` (to know what you're wiping) and for
troubleshooting "push is slow / no dedup" reports.

### `crab metadb cache clear`

Wipe the local chunk-index SQLite file at
`~/.cache/crab/buckets/{bucket-hash}/chunk-index.sqlite`.

```bash
crab metadb cache clear
```

Forces a cold re-warm on the next operation: the next push falls
through to `chunk_index_db` on every classification miss until the
cache refills. Useful when the cache is suspected of drift or
corruption, or when you want to force a clean starting point for
a benchmark. The remote state is untouched — only the local file
is removed.

## When to use `rebuild`

Use rebuild when an index is corrupt, incomplete, or missed its repairable
post-CAS update. Typical triggers:

- `crab metadb diagnose` reports a manifest or WAL read failure.
- `crab push` reports that refs committed but post-CAS MetaDB indexing needs
  repair.
- A `{repo_prefix}/file_index_db/` or `.crab/chunk_index_db/`
  prefix was accidentally deleted from the bucket.

Unversioned rows inside the current SlateDB namespaces are retired only after
every manifest-named shard has been verified and rebuilt successfully. Objects
from pre-SlateDB test layouts such as `{repo_prefix}/file-index/{hash}` remain
outside this migration and are ignored.

## Troubleshooting

| Symptom | Likely cause | Action |
|---------|--------------|--------|
| `push` fails with `MetaDbError::Open` | SlateDB manifest unreadable or S3 credentials / path wrong | Run `crab doctor --metadb` to see which database failed and the underlying error. Check the AWS credentials for the session and that the `.crab/chunk_index_db/` and `{repo_prefix}/file_index_db/` prefixes are readable. If the error is corruption, `crab metadb rebuild --db <affected>`. |
| `hydrate` reports `FileNotFoundInFileIndexDb` | Either the file was never pushed, or `file_index_db` is missing entries | First verify the file was pushed: look for a shard entry naming that `file_hash` under `.crab/shards/`. If the shard exists but the entry is missing, `crab metadb rebuild --db file_index` repopulates from the shards. |
| Push is slow and xet says nothing deduped | Local cache is empty or was wiped | Run `crab pull` (or `crab fetch`) to warm the cache via shard sync, then retry. Verify with `crab metadb cache stats` — after a pull, the entry count and installed shard count should both be non-zero. |
| `crab metadb cache stats` shows the cache was wiped unexpectedly | Remote `sys:gc_generation` drifted beyond `cache_gc_grace` after `crab gc` ran | Expected behavior. GC bumps the remote generation so stale clients know to re-validate; when the drift exceeds the grace window the cache is wiped. The cache refills on the next `crab pull` / `crab push`. Increase `metadb.chunk_index.cache_gc_grace` if you want more headroom. |
| `compact` appears to do nothing | It is a no-op at present | See the subcommand description above. SlateDB runs background compaction; there is no imperative trigger today. |

For anything not covered here, `crab doctor --metadb` gives a
per-database tabular report (path, open state, epoch, SSTable count,
WAL segment count, shard count via S3 LIST) that usually isolates
the problem to one database.

## Configuration

All tunables live under the `[metadb]` section of the crab config
(`.crab/config.toml` or the user-level config at
`~/.config/crab/config.toml`) and can be overridden by
`CRAB_METADB_*` environment variables.

```toml
[metadb.file_index]
# path                  = "{repo_prefix}/file_index_db/"   # derived
compaction_threshold    = 4
wal_flush_size          = 4194304                          # 4 MiB
bloom_bits_per_key      = 10

[metadb.chunk_index]
# path                  = ".crab/chunk_index_db/"        # derived
compaction_threshold    = 4
wal_flush_size          = 4194304                          # 4 MiB
bloom_bits_per_key      = 10
# local_path            = "~/.cache/crab/..."             # derived
in_memory_ceiling_bytes = 1073741824                        # 1 GiB
cache_gc_grace          = 3
```

Leave a field unset to use the derived default shown in comments.

### Environment variables

Each TOML field is mirrored by an `CRAB_METADB_*` environment
variable:

| Variable | Maps to |
|----------|---------|
| `CRAB_METADB_FILE_INDEX_PATH` | `metadb.file_index.path` |
| `CRAB_METADB_FILE_INDEX_COMPACTION_THRESHOLD` | `metadb.file_index.compaction_threshold` |
| `CRAB_METADB_FILE_INDEX_WAL_FLUSH_SIZE` | `metadb.file_index.wal_flush_size` |
| `CRAB_METADB_FILE_INDEX_BLOOM_BITS_PER_KEY` | `metadb.file_index.bloom_bits_per_key` |
| `CRAB_METADB_CHUNK_INDEX_PATH` | `metadb.chunk_index.path` |
| `CRAB_METADB_CHUNK_INDEX_COMPACTION_THRESHOLD` | `metadb.chunk_index.compaction_threshold` |
| `CRAB_METADB_CHUNK_INDEX_WAL_FLUSH_SIZE` | `metadb.chunk_index.wal_flush_size` |
| `CRAB_METADB_CHUNK_INDEX_BLOOM_BITS_PER_KEY` | `metadb.chunk_index.bloom_bits_per_key` |
| `CRAB_METADB_CHUNK_INDEX_LOCAL_PATH` | `metadb.chunk_index.local_path` |
| `CRAB_METADB_CHUNK_INDEX_IN_MEMORY_CEILING_BYTES` | `metadb.chunk_index.in_memory_ceiling_bytes` |
| `CRAB_METADB_CHUNK_INDEX_CACHE_GC_GRACE` | `metadb.chunk_index.cache_gc_grace` |

Env vars win over TOML; malformed values are logged and skipped
rather than failing the command.

There is no `metadata.backend` option and no `chunk_index.shard_count`
option. SlateDB is the only backend, and `chunk_index_db` is always
a single instance (the earlier 16-way sharding idea was dropped).

## Writer lifecycle and bootstrap

Fresh repositories do not need any setup. The first `crab push`
to a clean bucket auto-creates both SlateDB instances via
`Db::open`'s create-if-missing semantics. There is no
`crab metadb bootstrap`, no `crab metadb init`, and no
migration command.

Push planning uses read-only SlateDB readers. After manifest CAS, a push that
has metadata changes closes its reader, opens a short-lived writer directly on
the canonical origin, commits generation-pinned records and queued stale-row
tombstones, then closes it. Code-only and no-op pushes do not open a metadata
writer. This prevents a large push from holding the bucket-global writer fence
during classification and upload.

The Git object locator uses the same short-lived writer model. It disables
SlateDB's periodic collector because the dependency's first timer tick would
scan remote state on every publication. A writer that advances exact locator
coverage across a 32-generation boundary instead runs one foreground collection
after its reader checkpoint is published and the database is closed. SlateDB's
five-minute minimum age remains in force.

If you are upgrading from a pre-spec build that wrote
`{repo_prefix}/file-index/{hash}` objects, those legacy remote
objects are garbage after the cutover and must be wiped manually
before pushing with the new binary:

```bash
# remote: wipe the legacy per-file index
aws s3 rm --recursive s3://{bucket}/{repo_prefix}/file-index/

# local: optional, force the SQLite chunk-index cache to re-warm
crab metadb cache clear
```

Then push with the new binary. The hard-cutover section of
[RELEASE_NOTES.md](../../RELEASE_NOTES.md) covers this in more
detail.

Current builds have no redb-backed chunk-index path. Older
`chunk-index.redb` files are ignored; remove them manually only if you
need to reclaim disk from a pre-SQLite client.
