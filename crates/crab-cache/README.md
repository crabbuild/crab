# crab-cache

`crab-cache` provides the cache contracts and implementations shared by
Crab's read paths. It supports hash-verified local disk caching, an optional
HTTP cache-service client, path classification, service capabilities, dedup
queries, prefetch profiles, and Xet-specific chunk hints.

## Why it exists

Crab reads immutable shards, xorbs, chunks, manifests, and workflow artifacts
repeatedly. A cache must be safe to trust: content-addressed objects need
integrity verification, manifests need freshness handling, and an unavailable
remote cache must not become an availability dependency. Keeping those rules
here lets `crab-read`, VFS, fetch, and workflow code use the same behavior.

## Architecture

```text
origin object store
        │
        ├── LocalCache       atomic files, hash checks, LRU limits
        └── CacheClient      optional authenticated HTTP service
                │
                └── capabilities, immutable GET/HEAD/range, dedup query
```

`CacheKey` distinguishes chunks, shards, xorbs, manifests, and stage entries.
`LocalCache::get_or_fetch` coalesces fills, verifies content-addressed data
on read and write, and atomically renames completed files. Manifest entries
use ETags rather than a content hash.

`clean_cache` is the shared explicit cleanup boundary. It streams recognized
payload layouts through pinned private directories, retaining unknown subtrees,
SQLite databases and side files, profiles, and unpublished temporaries. Active
read descriptors and concurrent publishers are skipped using nonblocking locks.
Reports count actual removals (or eligible files in a dry run) and separately
count retained, busy, and unsafe entries. It does not create missing roots.
Unix parent-directory locks coordinate payload publication and deletion; native
Windows support, SQLite ownership, and reservation protection across all
maintenance entry points remain open.

Decoded-range stats, prune, and verify now use pinned private directories and
the same fixed range-layout ownership policy as cleanup. Unknown files,
database files, live subtrees, and unpublished temporaries are retained.
Prune previews and applied removals both skip active readers. Verification
holds the parent mutation lock and an exclusive payload lease while streaming
one descriptor, and removes only entries it successfully checked as corrupt.
It checks CRC/offsets and also Blake3 identity for the `crab-chunk` namespace;
xorb-range keys are not decoded-content hashes. Busy entries are not checked
or reported as valid. Dropping an async scan cancels its blocking worker via a
child token. This does not qualify catalog reservation protection, database
ownership, complete physical accounting, or bounded-time LRU reconciliation.

Product stats use `xet_chunk_cache_stats_in_root` to pin the configured cache
root before traversing decoded ranges, rather than treating its parent as
ambient. Standalone range-directory statistics retain their narrower ownership
contract. Neither scan opens a database or creates missing paths; this does not
establish full-family health or physical disk accounting.

Object-cache stats, prune, targeted eviction, and verification use that same
private boundary. The three eviction loops are consolidated, and stats/verify
stream recognized objects instead of collecting an inventory first. Object
stats include chunks, shards, xorbs, stages, and manifest counts; decoded
ranges remain a separate report. Unknown filenames no longer become corrupt
objects merely because they appear beneath a hash-prefix directory. Stages
and manifests retain their logical-key semantics and are not hash-verified.

Full-file xorb checks share one descriptor-owning worker implementation with
maintenance. They validate aggregate identity, compressed chunks, and the
footer's serialized-payload digest; metadata-only reads remain metadata-only.
Operational read failures do not authorize maintenance deletion. Xorb index
row cleanup now uses the descriptor-bound database owner below, but can still
wait for its busy timeout. Complete payload/database root correlation and
cancellation authority are not established by connection pinning alone.

Catalog eviction uses the same payload ownership and deletion boundary. Its
final lease/reservation check and row removal share an immediate SQLite writer
transaction. One pinned root covers the maintenance lock, metadata-only
inventory, and payload deletion. Inventory does not open SQLite files and aborts
reconciliation on unsafe entries. Dropping an owner does not create a missing
catalog.

Catalog reads, writes, and owner cleanup plus the local xorb index now share
a crate-private descriptor-bound SQLite owner on Linux/macOS. It checks the
cache-owned chain and existing file metadata without extra database opens,
rejects non-private files/links without permission repair, and creates new
databases as `0600`. A connection-specific, non-default VFS retains the parent
for main/journal/WAL/SHM/temp operations and unregisters only after close.
Namespace changes use short directory locks; database/WAL coordination uses
SQLite's standard byte ranges with open-file-description locks. Simultaneous
in-process connections must all use this owner; native SQLite interoperability
is tested across processes, not mixed owners on one inode in one process.

Maintenance acquires its writer before reading owner rows, avoiding SQLite's
non-waiting read-to-write upgrade race with reservation writers. Native macOS
tests cover root swaps, WAL mappings, cross-process writers, and killed-writer
recovery. Catalog maintenance, fill publication, and lease/reservation cleanup
now retain the original root and open SQLite relative to it;
maintenance scans and deletes through that same root. A replacement directory
does not receive old owner-row removal or inventory writes.

Reserved byte and file-backed fills use that captured root through temporary
creation, publication, registration, and release. They retain a shared payload
lease through the rename-to-registration interval; clean, object/range prune,
verify, and targeted object eviction skip the active payload. Directory
operations reopen independent descriptors so their namespace locks exclude
each other; payload descriptor clones intentionally retain one shared lease.

Private SQLite connections also retain the main descriptor and a shared lease
on a `-owner` file recording its device/inode. Opens, namespace cleanup, page
I/O, and new database/WAL locks reject a different main or owner inode. New
connections reject a mismatched binding while an owner is alive or any
journal/WAL/SHM exists. Only creating opens may bind quiescent state without
recovery files; read-only inspection never initializes the binding or reports
a missing binding as an empty catalog. Cleanup retains owner files.

Catalog reservations and SQL leases now retain that main/owner binding after
their creating connection closes. Temporary creation and publication validate
it; registration and cleanup reopen only the captured generation. Separate
connections still open independent descriptions for SQLite's byte-range locks.
SQLite close explicitly releases those locks even while an owner retains the
main descriptor. Accounting/eviction reuse the registration connection instead
of selecting a new catalog after releasing the reservation. The existing
catalog timeouts and WAL/NORMAL writer policy are unchanged.

This is a main-inode replacement checkpoint, not complete database-generation
qualification. Side-file-only replacement, identity reuse after all descriptors
close, resource/crash qualification of retained owners, remaining index callers,
complete temporary-byte accounting, bounded cancellation, and other
native-platform qualification remain open. Native SQLite does not
participate in Crab's generation lease; cross-process transaction locking is
qualified, but arbitrary external replacement or repair is not authorized.

Read-only catalog inspection now uses a read-only SQLite connection with
exclusive pager locking, SQLite's heap WAL index, and checkpoint-on-close
disabled. Existing WAL bytes are read, not bypassed; an absent WAL is represented
as empty only while the actual main-file EXCLUSIVE lock proves its absence.
The VFS rejects writes, truncation, deletion, temporary creation, and SHM
initialization. Catalog totals share one read transaction. Busy catalogs report
an error, and hot rollback journals require recovery by a writer, not inspection.

The main OS descriptor uses `O_RDWR` because OFD exclusive byte locks require
write permission; SQL and VFS data writes remain disabled. A filesystem that
cannot grant that descriptor reports unavailable rather than weakening locking.
Native macOS tests cover quiet and retained WAL state, contention, direct VFS
write denial, native writer exclusion, and inspection after writer death. This
is a library boundary, not full health-model/CLI integration, native Linux proof,
or a bound on heap WAL-index size and total inspection time.

Admission includes the incoming file and other active reservations when making
space, even below the current-usage high watermark. The final capacity check
and reservation insertion share a writer transaction. Object, file-backed
xorb, and decoded-range writers keep the reservation until the completed entry
is registered; active owners are not eviction candidates. An entry that cannot
fit is not cached. SQLite database/side-file access, complete accounting,
bounded reconciliation, and command/background lifecycle still need hardening;
this is not the complete budget/lifecycle contract.

The remote client is intentionally optional. The service contracts describe
auth modes, cache/dedup modes, limits, health, and known chunks without making
every local consumer depend on `reqwest`.

The `local-cache` feature also exposes directory lifecycle guards. Mutable
directory owners such as mirror reconciliation hold `CacheUseGuard`; both
`LocalCache::clean` and CLI cleanup hold `CacheCleanGuard`. Ownership is
exclusive across overlapping physical paths. Admission announces its lock
before probing existing owners, so a concurrent cleaner and user cannot both
proceed. Recursive cleanup retains sibling coordination files and their parent
directories, even when idle, to avoid splitting a lock between old and new
inodes. These are cooperative local locks, not distributed leases or protection
against uncooperative external deletion. Ordinary immutable cache fills do not
hold directory ownership.

Owners, cleaners and temporary admission probes explicitly unlock before
closing their file handles. On Unix, a descriptor inherited by a concurrent
fork must not prolong ownership until that unrelated child executes or exits.
Callers still must join their own cache workers before dropping the guard;
explicit unlock does not make a detached writer safe.

## Usage

Enable the local implementation and cache a verified chunk:

```toml
[dependencies]
crab-cache = { version = "1", features = ["local-cache"] }
```

```rust
use bytes::Bytes;
use crab_cache::{CacheKey, LocalCache};
use crab_xet::hash::compute_data_hash;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let cache = LocalCache::new(".cache/crab".into());
let payload = Bytes::from_static(b"cached chunk");
let hash = compute_data_hash(payload.as_ref());

let result = cache
    .get_or_fetch(&CacheKey::Chunk(hash), || async {
        Ok::<_, crab_cache::CacheError>(payload.clone())
    })
    .await?;
assert_eq!(result, payload);
# Ok(())
# }
```

For a cache service, enable `remote-client` and construct `CacheClient` with
the deployment's PSK, bearer, or mTLS settings. Call `is_healthy` and
`capabilities` before using service-specific features.

## Boundaries

- [`crab-cache-store`](../crab-cache-store/README.md) composes these cache
  primitives with an origin `Store` and owns fallback behavior.
- [`crab-storage`](../crab-storage/README.md) remains the source of truth;
  cache entries are disposable and must never weaken origin integrity checks.
- [`crab-read`](../crab-read/README.md) owns reconstruction and shard
  completeness, while this crate owns object reuse.
