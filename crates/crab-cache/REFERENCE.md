# Cache contracts and qualification limits

Start with [README.md](README.md) for features, usage, and source entry points.
This reference preserves the detailed persistence and lifecycle contracts and
their outstanding qualification limits.

- [Object identity and read-through behavior](#object-identity-and-read-through-behavior)
- [Cleanup and catalog retirement](#cleanup-and-catalog-retirement)
- [Read repair and file identity](#read-repair-and-file-identity)
- [Decoded ranges](#decoded-ranges)
- [Health inspection and accounting](#health-inspection-and-accounting)
- [Object maintenance and recency](#object-maintenance-and-recency)
- [Catalog eviction](#catalog-eviction)
- [Catalog checkpoint cadence](#catalog-checkpoint-cadence)
- [Private SQLite ownership](#private-sqlite-ownership)
- [Database generation binding](#database-generation-binding)
- [Read-only SQLite inspection](#read-only-sqlite-inspection)
- [Advisory shard hints](#advisory-shard-hints)
- [Capacity admission](#capacity-admission)
- [Remote client](#remote-client)
- [Directory lifecycle guards](#directory-lifecycle-guards)

## Object identity and read-through behavior

`CacheKey` distinguishes chunks, shards, xorbs, manifests, and stage entries.
`LocalCache::new` retains payloads without a disk cap. Optional budgets use
`None` for unlimited and `Some(bytes)` for bounded retention, including zero.
Unlimited catalog maintenance still reconciles accounting and stale owners
and recovers abandoned journaled fills, but does not evict for capacity.
Per-object validation/allocation limits,
explicit cleanup, and corrupt-entry repair are unchanged.
`LocalCache::get_or_fetch` coalesces fills, verifies content-addressed data
on read and write, and atomically renames completed files. Manifest entries
use ETags rather than a content hash.

Read-through fetches return validated origin bytes even if local reservation,
creation, or publication fails. Fetch errors and size/hash/xorb validation
failures still propagate; an optional cache write never causes a second fetch.
Explicit `put` and `put_bytes` calls remain fallible. Logical stage/manifest
content validation remains caller-owned, and separate manifest body/ETag
publication is not an atomic pair.

Native Git pack files have a separate `git-packs/aa/<plain-blake3>` identity,
not a Xet hash or a named-manifest key. File-backed fills use bounded buffers
and validate the exact byte length and hash. Hits verify that identity before
publishing an independent copy-on-write clone when supported, or a bounded
copy otherwise; cache and repository files are never hard-linked. A conflicting
caller length is a miss, not deletion authority; hash corruption invokes
descriptor-bound repair. Destination failures do not evict a healthy source.
Failed content validation never installs an entry. Complete published bytes can
remain if later catalog accounting fails. Git semantics and visibility remain
outside this byte cache. Concurrent fills may duplicate work; this path does
not yet provide per-key single-flight or remote cache-service pack retention.

Complete LFS files use native SHA-256 under `lfs/aa/<sha256>`, including identity
namespaces. A fill reserves its full declared length, streams bounded buffers,
checks size and SHA-256, then publishes through the common private journal.
Health, stats, prune, verification and cleanup include this family. A captured
`LfsCacheFile` supplies descriptors to the LFS integrity owner; callers keep it
alive through verification/delivery and release retained verification descriptors
before finishing. Only a failed captured source grants exact repair authority.
Cancellation and a conflicting caller length do not establish corruption.
These files remain eligible for eviction and explicit cleanup; no offline pin
or cache-service LFS route is implied.

## Cleanup and catalog retirement

`clean_cache` is the shared explicit cleanup boundary. It streams recognized
payload layouts through pinned private directories, retaining unknown subtrees,
SQLite databases and side files, profiles, and unrecorded temporaries. Active
read descriptors and concurrent publishers are skipped using nonblocking locks.
Reports count actual removals (or eligible files in a dry run) and separately
count retained, busy, and unsafe entries. It does not create missing roots.
Unix parent-directory locks coordinate payload publication and deletion.
Object/range prune and verify, clean, targeted eviction, and read-side corrupt
entry removal now share catalog row retirement. A healthy catalog's immediate
writer transaction excludes lease/reservation changes through deletion and
commit; declined or failed filesystem operations roll back the row deletion.
Dry runs use private read-only SQLite snapshots. Busy catalogs stop removal rather than bypass
owners. Missing/corrupt disposable indexes do not require repair before safe
payload cleanup; unavailable accounting emits a warning. Commit failure can
still leave an overcharged catalog and requires later reconciliation.
Standalone private range directories remain usable without a private parent;
only a captured private parent may supply a catalog. Packaged Windows qualification,
allocation-before-record and power-loss recovery, side-file ownership, and
complete byte accounting remain open.

Reserved object and decoded-range fills journal their relative temporary path,
reservation owner, volume/file ID and birth time before returning a writer.
Filesystem identity must be available; existing read-through cache write-failure
semantics still return validated origin bytes, while explicit puts remain
fallible. A completed rename or cancellation with proven absence retires its
journal. Failed cleanup retains the record until recovery observes absence or
conditionally removes the same object. Descriptor-bound live cleanup and
publication also reject a substituted temporary name.

Reservation teardown retains its payload lease through SQL owner retirement,
then closes that handle before checking temporary absence. Windows
[defers deletion until all file-object handles close](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntddk/ns-ntddk-_file_disposition_information);
checking earlier leaves a stale journal after successful cancellation.

Admissions, explicit cleanup and ordinary maintenance share that journal authority.
Before retaining another fill, admission recovers recorded dead fills and retires
stale SQL owners under the same writer used for its capacity decision. Recovery
commits even if root policy declines the new body; unlimited retention does not
wait for capacity pressure. A cheap caller-ceiling rejection can still avoid
initializing disk state. A
temporary-looking name or dead PID alone never authorizes deletion. Recovery
holds the captured directory, catalog writer and nonblocking file ownership
through identity comparison and unlink. Live fills, replaced files and unknown
neighbors survive. Previews use read-only snapshots and perform no journal or
owner repair; actual cleanup retires dead SQL owners, including after a process
dies following rename. A missing or unusable journal retains temporaries while
preserving the shipped payload-cleanup-without-a-usable-index contract.

Creation interrupted before identity registration can still leave empty,
unclaimed files; recovery does not guess their ownership. A packaged macOS
allocation-boundary probe observes one such file with zero logical bytes and
zero allocated payload blocks. Its reservation is retired by retry, exact
source reads succeed, and cleanup preserves the unrecorded inode. This is
bounded process-death evidence, not cross-platform or power-loss qualification.
HTTP response streams journal their exact scratch identity before writing and
retain that authority through final descriptor close. Unix unlinks the name
before writing; Windows deletion can remain pending while the stream is open. Packaged
process-death recovery has bounded macOS proof, including exact source bytes and
actual orphan removal. Focused native cache checks pass on Windows and Linux;
packaged cross-platform qualification remains required. This does not establish
power-loss or allocation-before-record recovery.

## Read repair and file identity

Object and decoded-range reads retain their original root and file descriptor
through validation and repair. A failed read releases its own finished read lease, then
compares the live descriptor's device/inode with the deletion candidate while
holding the candidate's exclusive lease and parent lock. A replacement file is
retained and its catalog row deletion rolls back. Other readers still exclude
removal; a root replacement cannot redirect cleanup into the new tree. The
reader duplicate shares only the seek cursor/lease, not a second independent
read. No new body copy or unbounded read is added. Failed opens and request-bound
rejections carry no file identity and authorize no deletion. A manifest-body
failure cannot delete an unobserved ETag. Outer xorb decoder failures reverify
the current file under the maintenance lock, retaining healthy refills. This
does not close all stale-read races or qualify arbitrary in-place mutation.

## Decoded ranges

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

Decoded-range handles are canonicalized by cache directory within one process.
Every live handle for that directory must use the same catalog owner and caller ceiling; a conflicting
caller receives a typed configuration error and uses its existing verified-origin
fallback instead of silently inheriting the first caller's limit. After the last
handle closes, the same persisted directory may reopen with a new budget.

## Health inspection and accounting

Product stats and doctor use `health::inspect_cache`: one pinned root, one
streaming filesystem walk, a read-only catalog transaction, and read-only
validation of the shard-hint database schema, row shape, row bound, and SQLite
`quick_check`. Shard-hint SQLite VM progress checks cancellation and a five-second
query deadline. This cooperative limit cannot interrupt blocked filesystem calls
and starts after database open; lock admission has a separate timeout.
Every linked file is counted by family, including databases,
side files, temporaries, hints, and retained/unknown state. Logical file lengths
and allocated 512-byte blocks are separate; directory allocation has its own
row. The walk retains at most 64 issue details, marks affected families
incomplete, and continues independent subtrees. Strict maintenance uses the
same walker but still aborts on an unsafe entry. Missing roots/catalogs/hint
databases are not initialized and unsafe or corrupt paths are not repaired.
Standalone object/range statistics retain their narrower contracts.

Catalog stats reject negative or non-integer entry/reservation sizes before
aggregation, and reject malformed maintenance timestamps instead of presenting
them as missing. These checks do not validate the complete catalog schema or
replace writer-side admission validation.

The live filesystem walk is not atomic, an integrity check, or unique-volume
allocation accounting (shared filesystem extents can be counted more than
once). It excludes unlinked open files and reports reservations separately.
An incomplete scan gives observed lower bounds and unknown over-budget status,
unless observed allocation already exceeds the budget. Catalog or shard-hint
validation failures do not invalidate independently measured file bytes. The
report does not establish all-family eviction or inspect other database/index
bodies.

## Object maintenance and recency

Object-cache stats, prune, targeted eviction, and verification use that same
private boundary. The three eviction loops are consolidated, and stats/verify
stream recognized objects instead of collecting an inventory first. Object
stats include chunks, shards, xorbs, ref transactions, native Git packs, stages,
and manifest counts; decoded ranges remain a separate report. Unknown filenames
no longer become corrupt
objects merely because they appear beneath a hash-prefix directory. Stages
and manifests retain their logical-key semantics and are not hash-verified.

Full-file xorb checks share one descriptor-owning worker implementation with
maintenance. They validate aggregate identity, compressed chunks, and the
footer's serialized-payload digest; metadata-only reads remain metadata-only.
Operational read failures do not authorize maintenance deletion. Xorb index
placement rows are no longer read, written, or cleaned up with payloads. The
v1.0.1 schema object remains dormant until an explicit migration can remove it
without discarding the live remote proof and parsed-index records beside it.
Complete payload/database root correlation and cancellation authority are not
established by connection pinning alone.

Bounded object, manifest/stage body, and xorb readers now share the decoded-range
reader's retained root/file identity through validation and conditional repair.
A failed read cannot delete a later publication at the same path. Body repair
does not delete a separately published manifest ETag. Cache-store decoding can
outlive the local handle, so its error path freshly verifies the current xorb
under the existing removal lock and retains healthy replacements. This reuses
streaming maintenance verification, without an additional full-body buffer.
Successful reads retain that same descriptor through their best-effort recency
update; a later publication at the pathname cannot receive the old read's mtime.
Object and decoded-range hits share this owner, without another open or body
copy. Request-bound misses, existence/size probes, and manifest/ETag reads keep
their existing no-touch behavior. This fixes recency identity, not all-family
LRU policy or batched catalog access accounting. Atomic manifest body/ETag
pairing and catalog-generation changes during a read remain separate work.

## Shared root retention

Identity-specific object and decoded-range payloads use
`namespaces/<64-hex>/<payload-layout>`. `LocalCache::namespaced` receives the
physical root catalog; range construction and maintenance receive that explicit
owner too. There is no child catalog or inferred owner. Traversal stays beneath
captured directories. Root object inventory and verification include namespace
payloads; decoded ranges reuse their existing streaming verifier when enabled.
Unknown namespace names, nested layouts and retained control state remain outside
payload deletion authority.

`CacheCatalog::set_retention_budget` saves the physical-root limit separately
from each caller ceiling. Missing policy retains the existing unlimited default;
malformed policy fails admission and is reported unavailable. Every reservation
reads the effective minimum under its SQLite writer, including handles opened
before the policy changed. A nonblocking maintenance lock serializes updates
with existing eviction passes; busy updates leave the old policy unchanged.
A zero root cap stops new retention; unlimited removes only that cap.

The setter does not run maintenance. CLI `cache budget` and Settings save policy,
run maintenance, then report actual usage. A later maintenance/inspection failure
does not undo the saved policy; Settings refreshes it on error. Retained state,
metadata and active payloads can keep disk usage over the cap. This is retention
policy, not a filesystem quota or an operation read limit. A missing catalog
initializes the default policy; deleting cache/catalog state resets this policy.
Offline pins and power-loss persistence remain unqualified.

## Catalog eviction

Catalog eviction uses the same payload ownership and deletion boundary. Its
final lease/reservation check and row removal share an immediate SQLite writer
transaction. One pinned root covers the maintenance lock, metadata-only
inventory, and payload deletion. Inventory does not open SQLite files and aborts
reconciliation on unsafe entries. Dropping an owner does not create a missing
catalog.

## Private SQLite ownership

Catalog reads, writes, and owner cleanup plus the remote xorb proof/index
database now share a crate-private descriptor-bound SQLite owner on
Linux/macOS. It checks the cache-owned chain and existing file metadata without
extra database opens, rejects non-private files/links without permission
repair, and creates new databases as `0600`. A connection-specific,
non-default VFS retains the parent for main/journal/WAL/SHM/temp operations and
unregisters only after close. Namespace changes use short directory locks;
database/WAL coordination uses SQLite's standard byte ranges with
open-file-description locks. Simultaneous in-process connections must all use
this owner; native SQLite interoperability is tested across processes, not
mixed owners on one inode in one process.

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

## Database generation binding

SQLite recovery upgrades from SHARED directly to EXCLUSIVE, without taking
PENDING. Only a RESERVED writer retains PENDING while waiting for readers;
otherwise a recovery reader can block the writer it is waiting to finish.

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

## Read-only SQLite inspection

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
is used by both stats spellings and doctor. Native Linux proof and bounds on
heap WAL-index size, SQL aggregation, and total inspection time remain open.

## Advisory shard hints

Shard hints now use `hints/shard-hints.sqlite` instead of the released global
`shard-hints.json` read-modify-write file. The primary key combines a digest of
the resolved provider/bucket/global-content prefix with the file hash, so two
managed views in one physical bucket cannot consume each other's hints.
SQLite transactions preserve unrelated concurrent updates. Missing, busy,
corrupt, or stale hints remain advisory misses and hydration falls back to the
authoritative file index. The old JSON cache is not migrated or read; cache
inventory still recognizes it as disposable retained state.

## Capacity admission

Admission includes the incoming file and other active reservations when making
space, even below the current-usage high watermark. The final capacity check
and reservation insertion share a writer transaction. Object, file-backed
xorb, native Git pack, and decoded-range writers keep the reservation until
the completed entry
is registered; active owners are not eviction candidates. An entry that cannot
fit is not cached. SQLite database/side-file access, complete accounting,
bounded reconciliation, and command/background lifecycle still need hardening;
this is not the complete budget/lifecycle contract.

## Remote client

The remote client is intentionally optional. The service contracts describe
auth modes, cache/dedup modes, limits, health, and known chunks without making
every local consumer depend on `reqwest`.

## Directory lifecycle guards

The persistence features also expose directory lifecycle guards. Mutable
directory owners such as mirror reconciliation hold `CacheUseGuard`; shared
`clean_cache` and CLI cleanup hold `CacheCleanGuard` during deletion. Ownership is
exclusive across overlapping physical paths. Admission announces its lock
before probing existing owners, so a concurrent cleaner and user cannot both
proceed. Payload cleanup retains mirror repositories, unknown state, and
coordination files, even when idle; admission does not authorize recursive
deletion. Previews do not create coordination markers. These are cooperative
local locks, not distributed leases or protection
against uncooperative external deletion. Ordinary immutable cache fills do not
hold directory ownership.

Owners, cleaners and temporary admission probes explicitly unlock before
closing their file handles. On Unix, a descriptor inherited by a concurrent
fork must not prolong ownership until that unrelated child executes or exits.
Callers still must join their own cache workers before dropping the guard;
explicit unlock does not make a detached writer safe.

## Catalog checkpoint cadence

Catalog writers retain their generation-bound WAL when short connections
close. Reservation creation, owner release and payload removal use the same
close policy; closing one SQL operation no longer requests a full checkpoint
of the catalog. SQLite's existing automatic checkpoint policy remains enabled
(the pinned dependency defaults to 1,000 pages). A long active reader can still defer an automatic checkpoint under
SQLite's existing WAL rules.

Disposable accounting uses WAL/NORMAL. Pending-file identity insertion and
explicit root-budget changes instead commit with FULL synchronization: recovery
authority must be flushed before exposing a payload writer, and a confirmed
user policy must outlive process exit. A failed identity commit prevents writer
admission. Journal retirement remains disposable; a stale record still requires
the same exact-file comparison. SQLite documents the
[WAL durability distinction](https://www.sqlite.org/pragma.html#pragma_synchronous).
Temporary allocation flushes its empty inode and parent before SQL captures
identity; payload bytes cannot precede that durable creation metadata.
File publication now flushes complete bytes before rename and its captured
parent afterward. New directories and unlink also flush the parent. A temporary
that is already absent cannot retire its journal until absence is flushed at the
nearest existing parent; a failed flush retains recovery authority. The shared
owner handles decoded ranges, objects, hints and copied Git packs. Async writers
drain before this publication worker. Preview/cleanup dry runs remain read-only.

Windows directory chains keep their existing deny-delete pins. A writable
handle reopens that pinned directory for `FlushFileBuffers`, whose API requires
[GENERIC_WRITE](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers).
The [owned NTFS probe](https://github.com/crabbuild/crab/actions/runs/43178147526)
confirms that the read-only handle fails while the writable handle flushes.
Deletion reaches the directory only after the final file-object clone closes.
HTTP completion therefore journals exact scratch identity in `responses/`
through that lifetime; cleanup recognizes this directory without treating raw
responses as immutable object payloads. Unrecorded/replaced files stay retained.

A packaged macOS trace reproduces the earlier gap: two decoded-range renames
had no corresponding parent flush after their journals were retired. The same
assertions pass on the rebuilt package: both captured parents flush before
journal retirement. Injected post-rename directory-sync failures retain pending
records, and Settings cleanup shows the I/O error without reporting success.
After fault removal, cleanup retires the records and an exact-content read
succeeds with no origin writes. Exact-source focused native checks pass on
[Linux and Windows](https://github.com/crabbuild/crab/actions/runs/43183402196);
Windows HTTP-body cases are gated out, while the shared scratch-lifetime case runs.
The later desktop build `f2f7e171511` repeats the directory-sync failure/repair
scenario with unchanged cache source: Settings retains a pending record on
failure, then cleanup retires it and exact reads succeed after repair.
These checks do not establish physical drive power-loss behavior.
The macOS sync-image probe replays the last successful flush and finds the exact
pending identity before the first payload write; its unchanged assertion fails
on the earlier package. Settings has bounded real sync-failure/retry proof.
An I/O error during commit does not prove rollback: callers refresh saved policy
before reporting its value or retrying maintenance.

Committed catalog state may remain in WAL after all catalog handles close.
Inspection reads that WAL through the existing read-only private VFS. Copying
the main database alone is not a coherent snapshot; fixture builders and repair
operations must use a complete SQLite snapshot or checkpoint under its locks.
Generation replacement still rejects stale owners instead of adopting recovery
files from a different main/owner identity. This changes checkpoint cadence,
not the schema or public cache configuration.

The pre-fix regression fails for repeated reservations and immediate release.
Post-fix qualification passes all 309 library tests and three credential
integration tests with local-cache, Xet chunk cache and remote-client enabled.
The corrupt-main fixture checkpoints before damage so committed WAL cannot
supply its supposedly corrupted header; its original assertions remain.
Strict combined-feature all-target Clippy also passes. Native-platform coverage
and the failed 1,024-file push rerun remain required.

The native Cellar 0.1.1 dependency pins rusqlite 0.40.2, which links bundled
SQLite 3.53.2 through libsqlite3-sys 0.38.2. The workspace disables rusqlite's
default features and explicitly retains statement caching and checked unsigned
integer conversions. Inspection propagates the fallible progress-handler setup
before starting its bounded read transaction.

SQLite 3.53.2 includes the upstream
[WAL-reset fix](https://www.sqlite.org/wal.html#walreset), removing the old
SQLite dependency blocker. Historical combined-suite evidence above does not
qualify the new dependency: matching cache, private-VFS, affected-consumer and
native workflow gates remain required before release. See the
[native runtime contract](../crab-server/CELLAR.md).
