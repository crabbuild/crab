# VFS hardening — active implementation

Objective: byte-correct, performant Crab v2 VFS through the real NFS and FUSE
product paths. Crab pack v1 backward compatibility must be removed; the user
explicitly added this requirement during the catalog-transition checkpoint. Linux native mounts use the isolated Colima environment and
RustFS; macOS qualification focuses on native NFS. This ledger preserves the
full objective across implementation checkpoints. No release qualification is
claimed by a partial checkpoint.

Baseline: `origin/crab-v2` at `33f8e6c360d922b3ab1e2c0e5115a2d200eccd97`.
Working branch: `codex/vfs-hardening`. PR:
https://github.com/crabbuild/crab/pull/630 (base `crab-v2`). Follow-up hardening
remains active. The PR is rebased onto `877b832b396`; final source and
installed-role evidence is recorded through checkpoint 62, with current Capsule
fixture coverage added at checkpoints 63/65. Checkpoints 59–65
retain separate source and qualification records below. Checkpoint 64 restores
NFS diagnostics after fresh CI exposed a regression. The human approved the
exact four-file CI/inventory patch; it is applied with these checkpoints.

Baseline evidence and reproductions:
`crab/docs/benchmarks/vfs-capsule-v2-audit-20261024.md` and its adjacent evidence
folder. The report describes the audited commit, not the current working tree.

## Requirements and acceptance gates

| Requirement | Required proof | Current status |
| --- | --- | --- |
| Preserve source state and honor selected revisions | Initial mount, local refresh/switch/adoption; ordinary, bare and linked repositories; staged index/HEAD preservation; native visible bytes | First, third and fourth checkpoints below; coordinator cached preparation and ownership migration remain open |
| Coherent Git/Capsule read generation | Startup, refresh, switch and commit adoption bind one captured catalog to the selected tree; old readers retain their view; GC ownership accounted for | Ninth checkpoint captures runtime readers; real RustFS NFS refresh/writeback now passes. Startup identity, remote retention and remaining modes remain open |
| Unpublished pointer content remains readable | Commit without push, overlay cleanup, immediate read, remount/recovery, later push and independent clone | Fifth checkpoint proves complete local recipes/remount; fourteenth and seventeenth prove native held-content retirement, retry and later push/clone. Twentieth proves unpublished base-pointer retirement and physical compaction, plus mounted reads and a fresh-cache eager clone on both Linux backends. Recovery, mixed recipes and namespace migration remain open |
| Stable open-file lifetime | Native FUSE and NFS rename, unlink, replacement, concurrent descriptors, writes/fsync and generation transitions | Thirteenth checkpoint repairs live commit handoff and Linux mapped writeback; seventeenth preserves failed-COW descriptors, twentieth pins unpublished base recipes. Reset/refresh, other product owners and broader failure/resource qualification remain open |
| Verified optional caches and bounded ownership | Same-length corruption, replacement races, missing/unsafe cache, ENOTDIR/disk-full, cancellation, explicit retention cap, active readers | Sixth and seventh checkpoints replace mutable VFS pointer/ODB byte caches with verified immutable retention; total admission/resource proof remains open. Shared persistent-cache policy is unchanged |
| Efficient range reads | Ordinary blobs and pointers: small random reads, sequential windows, cold/warm, concurrent readers; origin bytes, userspace amplification, throughput, p95/p99, RSS | Seventh checkpoint removes repeated whole-blob loading on warm ranges; cold/concurrent and native/provider performance remain open |
| Bounded dispatch and directories | Concurrent blocked hydration must not serialize unrelated FUSE operations; large-directory paging/inode lifetime; effective bounded prefetch | Linux blocked-read dispatch and entered-WRITE admission repaired at checkpoints 60/61; directory lifetime, prefetch and total memory remain open |
| Consistent lifecycle in every mode | Foreground/helper/coordinator/daemon; read-only/writable; automatic/manual refresh; reset/commit; stalled clients and unmount during reads | Open; managed engine-drain cancellation, foreground FUSE controls/refresh, macOS mapped writeback and Windows ownership remain gaps |
| Retire Crab pack v1 compatibility | Reject v1 stored repositories at all read/write/selection dispatch boundaries; delete fallback readers/writers and obsolete compatibility tests/docs; preserve empty-repository v2 initialization | Admission, protected receive, initialization, metadata owner/doctor, selected-replica validation and post-fetch warming require v2. Prepared repack retirement is qualified. Wire, GC/maintenance/replication and retained-history inventory remain open |
| Final qualification | Exact resulting sources, minimal/NFS/FUSE/combined feature checks, appropriate broad CI, native Linux NFS/FUSE + RustFS, macOS NFS; retained performance measurements | Scoped current-source checks are retained at each checkpoint; complete mode/platform/performance qualification remains open |

## First checkpoint: source state and local revision selection

Implementation:

- Pipeline resolves the requested commit rather than always selecting HEAD.
  The daemon startup sibling selects its configured branch too.
- Index replacement requires an explicit mount-owned index path. Local sources,
  including bare repositories, do not provide one. No ownership inference from
  Git's bare flag remains.
- Local manual refresh/switch resolve local refs without fetching or checking
  out the source. Runtime adoption receives the same pipeline ownership context.
- Snapshot mtimes use the selected commit, not an unrelated source HEAD.
- Automatic local polling uses `crab-git` ref resolution and follows the selected
  branch, packed refs, linked worktree refs, or a detached HEAD.
- Snapshot building and ordinary-blob reads resolve the common object directory
  for linked worktrees.

Evidence map:

| Surface | Caller / owner | Callee / dependency contract | Proof |
| --- | --- | --- | --- |
| Mount preparation | CLI/coordinator/NFS → `MountPipelineBuilder::execute` | Selected commit via Git rev-parse; explicit owned index via Git read-tree | Initial-ref and staged-index regressions |
| Manual transitions | Backend controls → `mount_runtime` | Same selector and index ownership; local source avoids fetch | Real Git worktree/bare/linked × read-only/writable transition table |
| Automatic refresh | Pipeline/daemon → `RefreshService` | `crab_git::ref_resolve::{resolve_head_symref_at,resolve_ref_at}` resolves worktree HEAD and common loose/packed refs | Refresh module tests, selected-branch/packing and linked/detached regressions |
| Object reads | Snapshot walker and `OdbReader` | `crab_git::discover::resolve_common_dir`, then gix ODB | Linked-worktree pipeline and visible ordinary-blob reads |
| Native adapters | Ignored `native_mount` tests in a dedicated privileged Linux container | Actual FUSE/NFS mounts, ordinary OS reads/writes, canonical shared hydrator with an in-memory origin and shard hint | Current run: NFS read-only/writable and FUSE read-only pass; writable FUSE fails after rename |

The transition table first failed for linked worktrees because snapshot building
looked for a private `worktrees/linked/objects` directory. After fixing both ODB
owners it passes all six cases. Source index and HEAD bytes are compared before
preparation and after refresh/switch. The original staged-index probe also failed
before its repair and passes afterward.

The native fixture now requests a branch different from source HEAD while the
source has staged changes. It checks selected-branch bytes through the OS mount
and source index/HEAD preservation after teardown in all four modes. Writable
FUSE reaches the source-state assertion but still fails its later descriptor
operation, so the complete mode remains unqualified. This fixture does not prove
Capsule catalog transitions or hosted object-store performance.

Known continuation work within this checkpoint:

- Cached remote clones must fetch/select a newly requested ref, preserving
  unpublished commits. The current clone-cache fetch still ignores that request.
- Qualify tags, detached commit selectors, source branch advances during startup,
  automatic local refresh end to end, daemon cached branch changes, and adoption
  with staged source data.
- Synthetic `.git` still references source Git metadata for local mounts. Inspect
  its intended Git-command semantics before claiming complete metadata isolation.
- The diagnostic unpublished-pointer regression remains failing and must be fixed,
  not treated as an expected passing case or hidden by a baseline update.

## Second checkpoint: read-window cancellation ownership

`ReadWindowClaim` now owns each foreground/prefetch request's inflight entry
through both mutex admission and reconstruction. Drop releases the claim on
success, error or cancellation. The map retains one lock while any claimant
still uses it, preventing a new caller from creating a competing lock when a
previous request finishes ahead of queued readers. Claim counts change under
the DashMap entry lock; Arc strong counts are not used to decide retirement,
because concurrent destructors can still hold Arcs after unlocking the map.

The real shared-hydrator cancellation regression first failed with a retained
entry after foreground cancellation plus service shutdown, then passed after the
repair. Existing admitted-prefetch/shutdown/foreground-read tests also pass.
This repairs abandoned-entry retention; it does not establish a total memory
admission limit or close the separate mutable-cache integrity failures.

Current retained checks (adjacent audit evidence folder, `hardening-*` files;
`hardening-checkpoint.json` records source hashes and results):

- macOS: pipeline 11, runtime 6 and refresh 25 tests pass. Runtime excludes the
  deliberately failing unpublished-pointer diagnostic. Hydration 36 tests pass,
  including the new cancellation regression. Combined-feature strict Clippy,
  minimal no-default-features and FUSE-only checks pass.
- Linux dedicated environment: the full VFS suite reports 586 passed, the known
  unpublished-pointer diagnostic failed, four privileged native tests ignored,
  and zero filtered tests. The earlier scoped run explicitly filtered that
  diagnostic; the final checkpoint run includes it. Separately executed native tests: three pass;
  writable FUSE still fails the open descriptor after rename.
- CLI consumer: no-default-features plus NFS/FUSE compile check passes; it emits
  existing warnings in untouched CLI code.
- Linux strict Clippy found existing `/proc/mounts` style lints in
  `daemon.rs` and `nfs_mount.rs`. The same expressions were verified on fetched
  `origin/main`; no baseline/suppression was changed to hide them. The final checkpoint repeats those two Linux lint failures. No
  Linux lint pass is claimed.

Next implementation boundary: cached remote ref selection and the shared
Git/Capsule generation owner, then unpublished-content and stable descriptor
ownership. Cache correctness/range performance and the full native/provider/
performance matrix remain required, not deferred acceptance criteria.

## Third checkpoint: shared Git preparation and clone-failure safety

`repository.rs` now owns cloning, default selection, ref fetching/reconciliation,
commit peeling and owned-index replacement. Its tests live in
`repository_tests.rs`; `clone_cache` retains cache naming and lock ownership.
Pipeline and daemon clone implementations delegate to this owner. Standalone
cached clones select refs missing from their original single-branch clone, and
daemon startup does the same before capturing its read context. Daemon force-fetch
uses reconciliation, preserves local ahead/diverged commits, and leaves index
replacement to snapshot publication. Its blocking Git operation is off the Tokio
worker thread.

Fetched values retain both the raw ref object and its peeled commit. Commit
peeling uses the captured object ID, so a concurrent tracking-ref update cannot
pair one tag's identity with another tag's commit. Missing branches are created
with Git's expected-absent CAS; annotated tags retain their raw object identity.
No persistent Capsule format or dependency changed.

The missing requested-branch regression first failed with `mount ref
refs/heads/other` not found, then passed. Additional real-Git coverage exercises
fresh/cached annotated tags, retagging, missing-ref failure and unpublished
commits. Default selection's main/master/sole-branch behavior exists in release
tag `v1.2.4` with NFS/FUSE enabled, so that shipped contract was retained and
moved with its tests rather than deleted as an apparent fallback.

A separate data-loss reproduction found clone failure recursively deleting a
pre-existing destination without HEAD, including a sentinel representing local
unpublished content. The regression failed with the sentinel missing. Cloning
now uses an owned temporary directory beside the target, admits only absent or
empty destinations, and renames completed metadata into place. Failure drops
only the preparation directory. Tests also cover an existing empty destination
and rejection without publishing partial metadata.

Clone and discovery clear repository-context environment variables while
retaining authentication/configuration. A direct bad-`GIT_DIR` regression covers
that boundary. The Linux bind mount separately exposes the checkout's `.git`
file with a host-only worktree path. The dedicated test environment sets
`GIT_CEILING_DIRECTORIES=/src` to stop implicit discovery before that file;
explicit fixture Git directories still work. No production cwd/discovery
workaround was added for this container limitation. Git documents these
boundaries in its [environment-variable contract](https://git-scm.com/docs/git);
the [clone contract](https://git-scm.com/docs/git-clone) permits an existing
destination only when empty.

Local source normalization is shared by the CLI and IPC client;
the registry records the already-normalized source rather than rechecking a
path that may have disappeared after a successful mount.

Current source-ownership gaps to resolve with the generation owner:

- Coordinator startup still bypasses cached-ref preparation when its clone
  already exists. Git preparation must precede capture of the matching catalog.
- Coordinator local mounts currently clone metadata; their automatic refresh
  and source classification differ from direct local mounts. CLI standalone
  remote caches use a bare root while coordinator caches use nested `.git`.
  Changing these locations requires preserving existing unpublished refs and
  staged content; do not silently switch namespaces or add fallback readers.
- Legacy foreground wiring uses a configured data remote as `PipelineConfig.source`
  while its Git directory is borrowed local metadata. That overloaded ownership
  input can defeat index protection. Separate metadata ownership from read-origin
  selection rather than inferring ownership from URL spelling or Git's bare flag.
  The fourth checkpoint repairs this mutation-policy gap explicitly.
- Snapshot/catalog capture, refresh tracking and publication need one shared
  owner. An in-process Arc alone does not satisfy the metadata dependency's
  requirement to retain captured roots against remote GC.

These gaps are part of the active goal. The third checkpoint does not close the
Capsule refresh, unpublished-pointer or native FUSE descriptor failures.

Retained third-checkpoint evidence: adjacent audit evidence folder,
`repository-*` files and `clone-preservation-before.txt`;
`repository-checkpoint.json` records source hashes. macOS repository tests
(25) and daemon tests (38) pass, as do combined-feature strict Clippy,
minimal/FUSE-only checks and the CLI NFS/FUSE consumer check. The dedicated
Linux full VFS run reports 593 passed, one unpublished-pointer failure, four
ignored native tests and zero filtered tests. Running the four native tests
separately yields three passes and the same writable-FUSE descriptor failure.
These native fixtures use an in-memory origin, not RustFS/Capsule publication.
The earlier Linux strict-Clippy baseline failures remain open; no new Linux
lint pass, final-source RustFS run or performance qualification is claimed.

The refactor moves shared production Git policy out of clone-cache, pipeline,
refresh and daemon modules into one owner. New non-test surface supports captured
raw/peeled ref identity, non-destructive clone installation, explicit index
ownership and cancellation-safe read-window claims. Tests add real-Git source
state, cached-ref, tag, clone-failure and native-mount regressions. No dependency
or serialized format changed.

## Fourth checkpoint: explicit Git metadata ownership

A new reproduction supplied a remote data origin with a borrowed source Git
directory, matching legacy foreground composition. Even a read-only pipeline
rewrote the source's staged blob. `metadata-ownership-before.txt` retains that
failure. Source URL classification was still controlling mutation authority;
being a local source and owning a local clone are distinct contracts.

`PipelineConfig.git_metadata` now declares `Borrowed` or `Mount`. Borrowed
metadata never gets a clone installed, refs fetched/reconciled, or index
replaced by preparation/refresh. Owned clones fetch refs and populate their
index even when their origin is a local path. The source URL remains an origin
input, not mutation authority. Every constructor states its ownership:

- CLI direct local and legacy foreground: borrowed; standalone remote: owned.
- Coordinator: owned, including its existing clone of a local source.
- Named daemon NFS controls: owned, matching daemon startup/refresh.
- Native borrowed-source fixtures and test-only runtimes: explicit as well.

Manual refresh/switch/adoption and automatic refresh use this declaration.
The transition table now covers ordinary/bare/linked repositories ×
read-only/writable × local/remote data origins. It checks selected bytes and
exact index/HEAD preservation through initialization, refresh, switch and
adoption. A separate automatic-refresh test verifies a source advance with a
remote data origin and staged local content. The owned-local-clone sibling
fetches a branch absent from its initial clone, observes a subsequent source-ref
advance through automatic polling, updates its own index and preserves the
source index. Missing borrowed metadata fails without creating a replacement.
Runtime tests moved into `mount_runtime_tests.rs` to keep the production module
small; the existing failing unpublished-pointer test remains enabled.

Dependency contract: Git's `read-tree` writes the selected index, and the
explicit `GIT_INDEX_FILE` in `repository::refresh_mount_index` controls that
destination. Ref fetching/reconciliation goes through the same shared Git owner
for owned clones. No cache location, wire/persistent format, dependency or
external configuration changed. This removes URL-based mutation policy; it
does not yet unify generation publication or migrate distinct cache Git roots.

Evidence: adjacent audit evidence folder, `metadata-ownership-*` logs and
`metadata-ownership-checkpoint.json`. macOS runtime tests pass eight tests
(the known unpublished-pointer diagnostic is excluded only in this scoped run),
pipeline tests pass six; strict combined-feature Clippy and the CLI consumer
check pass. Minimal, FUSE-only and NFS-only checks pass. The dedicated Linux
full run includes the diagnostic: 596 passed, one failed, four ignored and zero
filtered. Native Linux remains three passes and the writable-FUSE descriptor
failure. These are in-memory-origin native fixtures, not a new RustFS result.
The full goal remains active; cache integrity/range performance, coherent
Capsule generations, unpublished reads, stable descriptors and all-mode
lifecycle/provider/performance qualification remain required.

## Fifth checkpoint: readable retained local pointer content

The original unpublished-pointer diagnostic failed again before implementation.
VFS publication retained flushed local recipes, but its read service consulted
only remote metadata after overlay cleanup. The shared read crate now provides
`PointerReader` behind the opt-in `local-staging` feature. VFS and CLI opt in;
remote-only read consumers retain empty defaults. It selects captured published
content from the immutable remote catalog and otherwise opens retained staging.
An unavailable staging directory cannot shadow a captured published file. Local
absence permits the existing remote reader; selected local corruption remains
an error. No mutable-latest catalog repair was introduced.

Staging owns `StagedFileReader`, not a second VFS reconstruction implementation.
It captures an exact retained recipe and fences push retirement before reading.
Selected chunks pass the existing CRC/keyed-Xet-hash checks; complete files also
pass the pointer's plain BLAKE3 hash. Binary search locates bounded recipe pages;
coalesced reads keep at most one serialized prepared source and bounded decoded
batches. The prepared-source test checks one body open across many chunks.
Whole-file copy-on-write writes coalesced chunks by offset and verifies the final
output in order, retaining a bounded buffer instead of all decoded chunks.

Pure reads remove temporary snapshot journals even while sibling readers remain
active, leaving reclamation to push/exclusive maintenance. Reusing push cleanup
without this distinction would accumulate deferred read journals under overlap.
Shared-open filesystem/index admission now runs off the Tokio worker; the shared
lock lasts for an operation, not the mount lifetime. Cancellation maps to the
existing cancellation error; other staging failures retain their source.

Extending the regression to two commits revealed a second real failure: staging
publication superseded the earlier file version, so switching back to an unpushed
branch failed. Mounted batches now publish under distinct history owners, using
the existing staging tables and push-snapshot retirement contract. This preserves
the existing prohibition on rolling back published batches and rejects active
native/delegated handoffs. The initial draft test incorrectly assumed published
batches could be rolled back; the source keeps the dependency's prohibition.
Current tests instead prove earlier owners survive later history publication,
including identical-content publication.

Evidence map:

| Surface | Entry / owner | Dependency / proof |
| --- | --- | --- |
| Local publication | `publish::stage_crab_overlay_files` → staging flush/close → pointer index → history publication | Existing immutable recipe/payload ownership; new history owner uses the same transaction and retirement machinery |
| Shared reads | Pipeline/daemon → `HydrationService` → `crab_read::PointerReader` | Retained local recipes or the captured remote `SharedFileIndexLookup`; FUSE/NFS use the same owner |
| Copy-on-write | Engine promotion → hydration reconstruction → staged reader | Bounded coalesced source reads, output offset/size checks and whole-file hash |
| Lifecycle | Staged reader → retirement marker + exact recipe snapshot | Concurrent push cannot retire the reader's bytes; overlapping reader drop removes its temporary journal |
| Captured publication | Pinned catalog fixture → `PointerReader` with an unusable local staging path | First failed with ENOTDIR, then passed after captured-publication source selection |
| Native visible result | OS write → mounted commit without push → teardown → fresh service and read-only OS remount | New Linux FUSE and NFS tests verify an 8 MiB file hash without any remote read context |

The expanded runtime regression verifies immediate reads, remount without remote
context/warm windows, copy-on-write of committed bytes, a second local commit and
reading the older branch. The staging tests cover repeated occurrences across
page boundaries, prepared-body corruption after capture, coalesced output order,
one prepared-source open, cancellation, overlapping reader cleanup and deferred
push retirement. This is meaningful local-content proof, not complete v2 mount
qualification.

Current remaining work in this boundary:

- Verify later real Crab push and independent-clone hydration, including retirement
  after publication. Shared staging-root conventions for bare standalone versus
  nested coordinator clones still need explicit ownership/migration treatment.
- Local recipes that intentionally contain remote-only dedup terms need canonical
  mixed-source reconstruction when the new file is outside the captured catalog.
  Complete local VFS-created recipes are covered here; do not claim all staged
  recipe forms from these tests.
- Qualify interruption between staging handoff, Git commit/ref update and overlay
  cleanup; failure recovery and abandoned publication ownership remain gates.
- Coherent Git/Capsule generation publication, FUSE descriptor lifetime, mutable
  window/ODB cache integrity, efficient ordinary-blob ranges and the full resource/
  provider/mode/performance matrix remain open.

Manifests add only the optional workspace `crab-staging` dependency to `crab-read`,
enabled by VFS/CLI. `Cargo.lock` adds that one internal dependency edge; no external
version/checksum, dependency override or persistent format changed. Production
LOC grows for verified local source selection, bounded staging reads and lifetime
ownership; the new source implementations stay in the existing owner crates.

Retained fifth-checkpoint evidence: adjacent audit folder, `staged-read-*`
logs, `staged-reader-tests.txt` and `staged-read-checkpoint.json`. Latest dedicated
Linux VFS stress runs pass 597 tests each, five consecutive times; six privileged
native tests are ignored in each ordinary run. Executing those six native tests
separately yields five passes and the existing FUSE descriptor-after-rename
failure. Both new commit-without-push/remount tests pass. These fixtures do not
use RustFS. The read suite passes 238 tests. An earlier broad staging run passes
252 with one existing ignored manual RSS test; the latest eight reader tests
pass separately after subsequent source changes. No final broad staging rerun
or reader RSS qualification is implied.

One intervening broad run failed during fixture `git commit` with an invalid
object, before mount preparation. Its full output is retained. Investigation
found a test changing process-global `GIT_DIR` while other Git fixtures ran.
That injection now runs in an isolated child test process, and fixture command
failures include arguments/cwd/stderr. The five subsequent full VFS runs pass;
the exact earlier invalid-object failure remains unattributed. Rerun success
alone does not establish that this race caused that failure.

macOS focused reader, captured-catalog, local-history/metrics and child-process
tests pass, along with minimal/read/FUSE/NFS feature checks and the CLI consumer
check (13 existing unrelated warnings). Three-crate strict macOS Clippy passes.
Linux strict Clippy still reports the two previously verified origin/main style
errors; no suppression or passing Linux lint result is claimed.

## Sixth checkpoint: immutable verified pointer windows

The production warm-hit and unavailable-window-directory regressions were added
and run before the fix. Both failed: a same-length replacement returned `xxxx`
successfully; an ENOTDIR cache root rejected a healthy origin read. This was
separate from the older helper-only digest test, which did not reach the warm
production shortcut.

`HydrationService` now retains canonical verified reconstruction output as owned
immutable `Bytes`. Its window cache has a 64 MiB allocation budget and a 128-entry
metadata bound, evicts least recently used entries and never reopens mutable
window files. Admission copies only the selected window and charges the actual
Vec capacity. Failed optional allocation declines retention. Small foreground
replies copy only their requested range, so they do not retain an evicted window.
Persistent pointer-cache verification and repair remain in the shared read/cache
crates; VFS disk-window helpers, sidecars, verified-name sets and directory
configuration were deleted. Existing files are left untouched and carry no read
authority. No fallback reader, external option or persistent format was added.

Local staging without a configured remote hydrator keeps exact requested ranges
and skips remote-window prefetch. This preserves the prior efficient local path;
removing the directory option must not expand a small local read to 8 MiB. The
shared `PointerReader::has_remote` supplies that capability distinction. The
64 MiB bound describes retained VFS payload allocations, not all admitted
reconstructions, replies, lower-layer caches or whole-process RSS. Total memory
admission and foreground priority remain acceptance gates.

Both initial regressions pass. Cache tests prove byte/entry bounds and LRU reuse;
cancellation and overlapping shutdown tests remain enabled. The real in-memory
xorb fixture now uses 64 KiB chunks and canonical placement-to-shard helpers,
including repeated occurrences. This exposed a fixture assumption: fetching a
whole xorb does not mean every decoded chunk was retained. The reuse test primes
every chunk before blocking origin, then reads different ranges after dropping
VFS windows. The shared canonical cache serves them without another body read.
The earlier mistaken expectation and its failure are retained as fixture
diagnosis, not described as a production bug.

An intervening broad Linux run also hit the existing stale-Unix-socket fixture.
A deterministic OS probe demonstrates that a fork child can retain a listening
descriptor after the parent closes it, until child exit/exec. Git fixtures spawn
concurrently. The stale-socket fixture now creates its socket in an isolated
child test process and verifies successful exit plus the socket's existence
before probing. Production socket policy was not changed. The exact earlier
failure did not capture child descriptors; it remains an observed fixture
failure with a demonstrated unsafe assumption, not a traced attribution. A
separate 8,000-iteration socket loop found zero failures on both the shared
Workspace path and native container temporary storage; no filesystem workaround
or retry was added. Native test tasks now unmount before propagating worker
panics, preserving cleanup when an assertion fails.

Current verification and source hashes belong in the adjacent audit folder's
`verified-windows-*` evidence and `verified-windows-checkpoint.json`. This is
pointer-window hardening only: ODB warm-cache corruption, coherent Git/Capsule
generations and remote-root retention, stable FUSE descriptors, RustFS/native
macOS qualification and production performance remain open.

Final exact-source checks: macOS hydration tests 39 pass, local commit/remount/
history/metrics regression passes, isolated stale-socket fixture passes, strict
read/VFS Clippy and format checks pass, minimal read/VFS/FUSE/NFS checks pass,
and the CLI consumer compiles with the same 13 unrelated warnings. Dedicated
Linux broad tests pass 238 read and 600 VFS tests, zero failures/filtered, with
six privileged native tests ignored. Running those six explicitly yields five
passes and the same FUSE descriptor-after-rename failure; both local pointer
remount tests pass. Linux strict Clippy repeats exactly the two existing style
failures. Three earlier broad runs also passed read 238/VFS 600 before the final
local-only range-policy adjustment; the final exact source was qualified again.
No staging implementation changed in this checkpoint, so its earlier broad
suite was not repeated. The retained native timings use an unoptimized build
and the revised multi-chunk fixture; they are not a matched performance
comparison or proof of production throughput. A reusable Rust 1.97 Linux image
with FUSE/NFS prerequisites and Clippy is retained on the isolated VM's external
Workspace disk; its ID is in the checkpoint manifest.

Generation investigation confirms both manual `mount_runtime` publication and
automatic `RefreshService` publication leave hydration on its initial catalog.
The metadata `SharedFileIndexLookup::for_pointer_catalog` contract forbids widening
and requires caller-owned retention against GC. A future common transition owner
must bind snapshot/ref/time/read context, retain old reads and remote roots,
serialize competing control/automatic/adoption transitions, and update live
metadata used by status and tracking. Swapping only a mutable hydrator would
leave tree/read races and GC ownership unresolved; no such repair was introduced.

## Seventh checkpoint: verified Git blobs and promotion ownership

`blob.rs` owns ordinary Git reads. Native decoding now verifies the Git object
ID, kind and size before serving bytes. Missing promisor objects still use the
shipped native-Git retrieval path, with replacement refs disabled and output
verified against the requested OID. Immutable decoded retention replaces the
separate mutable VFS disk-blob cache. The common private `MemoryCache` supplies
constant-time LRU with separate byte/entry limits for pointer windows and blobs;
Git retention is 128 MiB and 4,096 entries. No persistent Capsule format changed.

Both warm-read and promotion probes first returned same-length corrupt bytes.
They pass after removing mutable blob-file authority. The misnamed loose-object
fixture originally failed during setup because Git object files are read-only;
that failure is retained and is not a product reproduction. Its corrected setup
confirms raw gix decoding accepts the misnamed bytes, then VFS checksum
verification rejects them. Replacement-ref, linked-worktree and new-pack
pressure tests cover the sibling native-Git path.

Async Git fills own their admission guard and tracker token inside the blocking
job. Abandoning the awaiting future cannot release admission while decoding or
fetching continues. Warm hits bypass an unrelated blocked fill. Engine shutdown
closes Git/hydration admission and drains both across foreground, coordinator,
daemon, IPC failure and NFS owners before cache locks are released. Native Git
fetches still lack cancellation/deadline support and can delay that drain.
Synchronous lazy classification/size/symlink reads still need latency review.

Git and pointer promotion now use unique owned temporary files. Git copies keep
the verified allocation through write/sync; successful promotion persists and
updates overlay metadata without an intervening await. Cancellation cannot
leave a fixed temporary pathname for the next attempt to reuse. A real pointer
reconstruction cancellation/retry test passes and verifies visible retry bytes.

Evidence map:

| Surface | Caller / owner | Callee / contract | Proof |
| --- | --- | --- | --- |
| Ordinary range reads | Engine, shared by NFS/FUSE | `OdbReader`, gix explicit checksum verification, native Git raw OID | Warm corruption, misnamed object, replacement refs, pack-pressure and range probe |
| Promotion | Engine writable operations | Verified allocation, unique `NamedTempFile`, persist ownership | Corrupt-cache promotion and pointer cancellation/retry tests; native Git COW on both adapters |
| Cancellation/teardown | Foreground/coordinator/daemon/NFS → engine | Explicit accepting lock plus Tokio tracker token owned by blocking job | Cancellation while native decode is blocked; shutdown closes cold admission; owner compile/tests |
| Retained byte reuse | Git blobs and pointer windows | Private bounded LRU with owned immutable payloads | Existing retention/eviction tests and broad read/VFS suite |

Pinned dependency proof: gix-object 0.60.0 exposes checksum verification
separately from decode; Tokio-util 0.7.18 tracker close alone does not prohibit
registration, requiring the accepting lock; tempfile 3.27.0 persist returns owned
output on failure and drop removes temporary paths. The LRU dependency was
already locked at 0.12.5; this checkpoint adds only its VFS dependency edge.
The full worktree lockfile also contains the earlier read→staging edge, with no
external package version/checksum changes.

An isolated unoptimized Linux probe reads 4 MiB in 1,024 small requests. Before,
it measured 4,294,967,401 userspace read bytes and failed its amplification
budget. The final range run measures 105 userspace read bytes. This measures
warm userspace read amplification, not physical I/O or native throughput. Cold
blobs still decode completely; separate cold objects serialize, and upstream
pack/metadata allocations are not globally bounded by the retained-cache cap.

Final qualification results and exact source hashes are recorded in
`verified-blobs-checkpoint.json` beside the baseline audit. Earlier raw failures,
including an obsolete removed-cache-directory expectation, remain retained.
That expectation now positively verifies a disposable cache sentinel cannot
block preparation; the daemon failure fixture instead exercises actual backend
setup failure after engine construction. No expected-failure list was edited.

Final exact-source checks: macOS engine 42 tests pass; minimal read/VFS,
FUSE-only and NFS-only checks, CLI consumer, format and strict read/VFS Clippy
pass. CLI retains 13 unrelated warnings. Dedicated Linux broad checks pass read
238 and VFS 606, zero failures/filtered; seven ignored tests are six native tests
and the isolated range probe, all run separately. Native checks pass five and
fail the same FUSE descriptor-after-rename case. Ordinary Git COW verifies in
both writable adapters; source index/HEAD/working bytes remain unchanged after
teardown. Both unpushed-pointer remount tests pass. Linux strict Clippy repeats
exactly the two existing origin/main style errors; no Linux lint pass is claimed.
The final range probe records 105 userspace read bytes and 2,121 microseconds;
the earlier repaired-source run recorded 1,520 microseconds. These timings do
not establish production throughput. Native fixtures use an in-memory origin;
no new RustFS or macOS native mount qualification is claimed by this checkpoint.

Code-size review includes new untracked modules, not just `git diff --numstat`.
Blob/cache extraction replaces the old engine disk-cache implementation and
pointer cache accounting. Added ownership surface is needed to retain verified
bytes and blocking work through cancellation; tests/helpers moved to their owner
rather than preserving a second implementation.

## Eighth checkpoint: retained local snapshot views

Two additional generation failures were reproduced before repair. A live resolver
on generation 1 lost its original file after newer publications pruned that tree.
Separately, late classification of a real generation-1 Git blob changed the size
metadata of generation 2's different blob. Both before logs are retained.

`FuseResolver` now owns an Arc snapshot view with one generation/timestamp pair
and an RAII pin. View capture is short and synchronous; no lock is held through
Git I/O or async reconstruction. Resolver lookup, attributes and directory paging
retain one view for the operation. Base read leases, promotion and base-directory
rename traversal retain the captured view, and classification writes only its
captured generation. Manual refresh, switch, adoption and automatic refresh all
use the single generation/timestamp installation method; separate timestamp
mutation was removed. Constructing/installing an unavailable generation errors
without replacing the live view.

Exact attribute resolution also moved into the engine. FUSE lookup/getattr/setattr
and NFS regular getattr no longer pair a first attribute lookup with a second
path lookup for size. NFS readdirplus captures one view but still omits unknown
blob attributes rather than hydrating directory entries eagerly. The concurrent
attribute regression exercises the engine with real Git blobs and unknown sizes,
replacing the narrower resolver-only swap test.

`SnapshotStore` serializes pin admission and pruning under the connection lock.
Pins retain current operations across arbitrarily many later publications;
pruning removes node and child rows in one transaction after the last pin exits,
on a later publication. Indexed generation ranges between pins avoid walking a
retained tree's rows during each prune. A shared generation decoder replaces
duplicated parsing; exhausted generation numbers return an error before mutation.
No SQLite schema, Capsule encoding or dependency version changed.

Evidence map:

| Surface | Entry / owner | Callee / contract | Proof |
| --- | --- | --- | --- |
| Active tree lifetime | Resolver capture and base engine lease | Store pin admission shares the SQLite connection boundary with pruning | Visible old file failed before; captured directory remains until final owner drops, then node/child rows are reclaimed |
| Late lazy classification | Read, attributes and promotion | Captured generation → `update_node_from_blob` | Real Git blob regression failed with newer size changed to 3; repaired path leaves newer metadata intact |
| Attributes | FUSE engine getattr and NFS attr owner | One captured view computes mode/size/type/time; NFS directory path stays lazy | Concurrent 5,000 exact attribute reads during 10,000 view swaps; existing NFS/FUSE adapter tests |
| Generation install | Pipeline/daemon construction; manual/automatic refresh and adoption | Pin before publishing one generation/time pair | Feature/consumer compile gates, runtime transitions and full VFS suite |

Pinned rusqlite 0.31.0 transaction source confirms its borrowed Connection and rollback
on unfinished transaction drop; the store's node and child tables have generation
as the leading primary-key column. Baseline `origin/main` still prunes all rows
older than the previous generation with no live-reader pin. Live NFS and
coordinator commit controls pass their mount's `SnapshotStore` into publication.

This checkpoint is a foundation for the joint Git/Capsule owner, not its
completion. Pins coordinate only users of the same store instance. Direct CLI
and daemon offline publication still have paths opening a separate store, so
routing/fencing those publishers remains required. Existing read-lease generation
rejection is unchanged; retaining SQLite rows does not yet make an old lease
readable after refresh. Capturing/replacing the catalog, retaining remote roots
against GC, serializing competing transitions and authoritative live status/ref
tracking remain open. No mutable-latest lookup was added to hide that gap.

Next integration boundary: `ShardHydrator` is Clone and its clone shares download
and buffer admission, cache and availability. A new captured lookup can therefore
bind a generation without rebuilding independent resource budgets. GC admission
must be audited separately: existing writer fences block sweep and require
storage mutations, whereas read-slot admission is only a capacity lease and does
not retain catalog roots. Neither can be silently treated as a read-only root
retention contract.

Final exact-source Linux VFS tests pass 611, with zero failed/filtered and
seven ignored (six native tests plus the isolated range probe, all run
separately). Native tests pass five and fail the same FUSE descriptor-after-rename
case; both unpushed-pointer remounts and both ordinary Git COW paths pass. The
warm-range probe remains 105 userspace read bytes for 4 MiB, at 1,388 microseconds
in this unoptimized isolated run. Linux Clippy retains exactly the two existing
origin/main style failures. macOS final format, minimal/FUSE/NFS checks, strict
Clippy and CLI consumer compile pass; CLI retains 13 unrelated warnings. Focused
engine44/NFS64/FUSE8 tests pass after attribute consolidation, before the final
explicit-overlay match lint cleanup; Linux broad proof covers the final Rust.
The unchanged lower read/staging implementations were not broadly retested.

Final exact-source results and source hashes are retained in
`generation-pins-checkpoint.json` beside the baseline audit. The initial broad
pass preceded the adapter attribute consolidation; final qualification repeats
that changed surface. The full hardening goal remains active and uncommitted. Code-size review
includes untracked modules from previous checkpoints. This step grows local
ownership code for pins and captured views, while deleting separate timestamp
mutation, duplicate state parsing and adapter size-resolution paths; the growth
pays for retained lifetimes and one exact-attribute owner.

## Ninth checkpoint: runtime catalog views and owned transitions

Runtime refresh, switch, automatic publication and commit adoption now capture a
new catalog before installing a resolver view. The view owns generation, selected
Git identity/ref policy, timestamp and pointer reader. Successive hydration
services share the mount's caches, inflight coordination, admission and shutdown.
The CLI capture provider authenticates a Capsule root and constructs an immutable
pointer-catalog lookup; a missing root no longer selects a mutable lookup.

Snapshot publication records each generation's Git identity in a local SQLite
header table, transactionally with its tree/state. Pin admission reads the exact
header under the pruning boundary. This is a disposable local cache schema,
unrelated to stored Crab pack v1; no Capsule encoding or external dependency
version changed. The VFS dev-dependency on metadata supports real catalog fixtures.

Transitions execute in an admitted, tracked mount-owned task. Abandoning a control
waiter cannot detach its blocking publication or drop its reset guard early.
Shutdown closes admission and drains these tasks before read resources. Commit
and reset tasks retain their invalidation target, avoiding a coordinator-lock
reacquisition during shutdown. Automatic polls compare their captured starting
view after admission, discarding obsolete results. Ref selection comes from the
live view, so an existing watcher follows a manually switched branch.

Three focused tests pass: a failed catalog capture preserves old visible bytes
before a successful retry; an existing automatic watcher follows a manually
switched ref; and an in-flight pointer read completes through its old catalog
while a refresh installs the next reader. The fixture uses authenticated object-
paged catalogs and encoded shards/xorbs, but a dummy Git pack descriptor; it does
not prove hosted-store Git/catalog publication or native mounts.

A new test gate initially blocked both shard and xorb admission and hung its
second request. The retained original run was terminated; a one-shot admission
gate fixes the fixture and all three tests pass. Separately, an introduced NFS
control regression blocked a Tokio worker on the runtime's synchronous mutex.
The unchanged cancellation test reproduced it; cloning runtime state now happens
in `spawn_blocking`, and the test passes. These are distinct failures.

Current retained Linux proof: 614 tests pass, zero failed/filtered and seven
ignored, before addition of the final in-flight test. Native tests pass five and
fail the same FUSE descriptor-after-rename case. Strict Linux Clippy retains the
two known existing style failures. The combined-feature CLI builds successfully,
with 13 unrelated warnings. macOS scoped runtime11/refresh25/engine45/coordinator14,
NFScontrol19/snapshot17/resolver29 and hydration39 pass at the scopes recorded in
raw logs. One earlier NFS socket-startup test failed with ENOENT; isolated and
full unchanged reruns pass, but its cause remains untraced. No load/flakiness
explanation is claimed. Final minimal/NFS-only/FUSE-only checks and combined strict macOS Clippy pass.

The real RustFS native NFS CLI smoke now passes: 32 MiB seed, 8 MiB new
content, RO manual and RW automatic refresh, reset, overlapping writers, COW,
rename/deletion, sparse writes, export, failed-push state preservation, retry,
commit/push and independent eager-clone byte checks. The original newly pushed
file EIO no longer reproduces in this scenario. Artifacts and binary identity
are retained in `catalog-generation-rustfs-nfs/`; the checkpoint manifest binds
them to sources. Functional timings are unoptimized and are not performance
qualification. FUSE runs separately; native descriptor-after-rename still fails.

Still open: startup Git/catalog ordering and identity binding; remote GC retention
(required explicitly by `SharedFileIndexLookup::for_pointer_catalog`); initial
reader retention; direct/offline publishers bypassing the shared store; adoption
failure after commit/overlay cleanup; stale duplicated pipeline/daemon metadata;
global coordinator serialization; read-only/foreground refresh ownership; NFS
child-task joins; and the remaining correctness/performance/v1 gates below.
Runtime captured readers are progress toward coherent generations, not complete
proof of that requirement. All sources remain uncommitted.

## Tenth checkpoint: short-ref reconciliation and v1 admission

The native FUSE RustFS scenario passes RO manual and RW automatic refresh,
then fails writeback after remount with a non-fast-forward rejection. Retained
refs show `refs/remotes/origin/main` advanced while `refs/heads/main` stayed on
the manual-refresh commit. A focused real-Git reproduction confirms visible
refresh succeeds but remount returns old bytes. The initial view retained the
short user selector `main`; the fetcher normalized it internally, but branch
reconciliation intentionally updates only fully qualified refs and skipped it.

Pipeline view construction now normalizes explicit selectors through the shared
Git owner. The new real-Git regression passes, including local-branch identity
and remounted bytes; all four catalog-transition tests pass. Daemon startup
already supplies a resolved full ref; switch normalizes its new selection, and
manual/automatic refresh now inherit the canonical live selection. The rebuilt CLI now passes the entire native FUSE RustFS scenario, including
explicit remount bytes, commit/push retry and independent eager-clone checks.
The native NFS repetition also passes with that same binary. Both cover
RO manual refresh, RW automatic refresh, remount, reset, overlapping writes,
sparse/COW writes, export, failed-push preservation, retry, commit/push and
independent eager-clone byte verification. Functional timings remain unoptimized;
no throughput/RSS/latency qualification is claimed. The final Linux VFS suite passes
616 tests, zero failed/filtered and seven ignored. This repair is additional to
the separate stable-descriptor FUSE failure, which remains open.

The v1 admission regression first fails: a canonical v1-only fixture is accepted
with no Capsule root. Direct and managed repository resolution now require the
same mandatory root type; the optional admission wrapper, remote-helper format
flag and legacy list branch are deleted. Test ref-advertisement helpers now use
the production payload-free Capsule reader rather than retaining a second full-
view decoder. Doctor and selected-replica validation also require the v2 root.
The obsolete legacy-capabilities test is removed; real v1 rejection and modern
root-without-descriptor tests replace the admission compatibility contract.

Scoped macOS proof passes: admission3, helper list9, doctor authority3 and
replica validation2; capabilities3, selected-replica listing1 and a real Git
push/list/fetch session1 also pass. Strict VFS Clippy passes after ref normalization; script
syntax passes. The native smoke now explicitly hashes the auto-refreshed file
after remount, before writeback. This is not complete protocol retirement:
legacy replica-readiness, protected receive plans, SDK initialization and old
publication/maintenance dispatch still require deletion or canonical replacement.
Lower codec versions and Git wire/helper commands are distinct contracts; they
must not be retired by searching the word `v1` alone. No dependency changed.

## Eleventh checkpoint: captured FUSE inode content and publication admission

FUSE open handles now pin one shared content owner per inode. The engine's
`engine/open_file.rs` captures the base snapshot/catalog or retains native overlay
descriptors. Rename updates its binding; unlink/replacement detaches it. Reads
do not reopen former names, and detached writes cannot recreate them. All opens
share COW promotion. Read-only descriptors on writable mounts use direct I/O,
avoiding stale kernel pages after another descriptor promotes the inode.
Release/forget retain in-flight operation pins, and final close releases native
descriptors without waiting for the kernel's lookup cache to forget the inode.
Getattr/setattr/read/write/flush/fsync validate handle/inode ownership. Sync errors
propagate from the retained descriptor rather than a best-effort pathname reopen.

The first new write/reconciliation regression fails: `edited` becomes `before`
because descriptor writes retained disposable promotion lineage. The overlay
owner now records content edits with source/base lineage cleared, matching the
existing path-write contract. Truncate-to-zero records local ownership in its
empty promotion. Both descriptor and path truncation survive reconciliation.

A native exchange-rename probe also fails before repair: RENAME_EXCHANGE returns
success after an ordinary rename, losing the destination. Exchange/whiteout now
return EINVAL without mutation. No-replace destination admission runs inside the
shared engine's publication/path locks. A native no-replace probe already passed
before this change; no overwrite reproduction is claimed for that case.
Parent paths for namespace mutations now resolve under the adapter namespace
lock. Metadata operations pin existing content through concurrent final close;
path-based setattr serializes its open-inode decision with new opens.

Further review finds descriptor I/O mutating bytes before publication admission.
The focused freeze probe returns an error but changes `before` to `WRONG!`.
`OverlayWriter::mutate_content` replaces the metadata-only size updater: its
shared publication lease covers file I/O and SQL recording together. Native
descriptor writes/truncation and the NFS/path truncate sibling use that owner.
The final regression freezes through a separately opened store connection and
checks all three operations reject before changing bytes. Existing fs4 shared/
exclusive file-lock ownership is retained; no dependency changed. Detached
native inode matching is checked on Unix; Windows descriptor identity remains
outside this checkpoint's proof. The removed updater's metadata-only test is
replaced by actual descriptor mutation/size/mtime behavior.

Current proof after publication-admission repair: dedicated Linux VFS suite
627 passed, zero failed/filtered, seven ignored; all six privileged native
NFS/FUSE tests pass separately. The expanded matrix covers descriptor rename,
unlink/recreate, replacement, multiple opens through COW, and first write to an
unlinked pointer. macOS strict combined-feature Clippy, minimal/NFS-only/FUSE-only
checks and the CLI combined-feature consumer check pass. The CLI emits its 13
existing warnings. No new Linux strict-Clippy pass is claimed. Both real RustFS
product repetitions pass with the same rebuilt CLI: RO manual/RW automatic
refresh, remount hash, reset, concurrent/sparse/COW writes, export, failed push,
retry, commit/push and independent eager clone. These unoptimized functional runs do not prove
throughput, tail latency, memory budgets or all product modes.

Still open: active descriptor ownership through commit cleanup/adoption and
reset/refresh; offline publishers bypassing the live engine; full foreground/
helper/coordinator/daemon behavior; total resource budgets and performance.
The additional real RustFS/FUSE probe now reproduces cleanup detaching a held
descriptor despite its surviving logical pathname: commit succeeds, a subsequent
write/fsync succeeds and the held file returns `AFTER!`, but the pathname returns
`before` and the overlay diff is clean. It also finds shared read-only mmap on a
writable FUSE mount fails with ENODEV; private mmap succeeds. Linux 6.8.0-117 is
the test kernel. The kernel's direct-I/O mmap negotiation contract must be
applied and coherent mapped reads/writes verified. These two failures remain
required repairs; passing standard scenarios do not establish complete descriptor
lifecycle. `open-file-checkpoint.json` binds 54 source hashes and retained logs,
including the failing probe. No full-goal completion is claimed.

## Twelfth checkpoint in progress: mmap and publication adoption

Linux's `FUSE_DIRECT_IO_ALLOW_MMAP` negotiation is now requested when advertised.
The kernel's [I/O contract](https://kernel.org/doc/html/latest/filesystems/fuse/fuse-io.html)
requires it for shared mmap of direct-I/O files. The locked fuser 0.17 exposes
that flag. Linux v6.8 `fs/fuse/file.c` rejects unnegotiated shared maps and flushes
mapped writeback/invalidate ranges around direct I/O. No dependency, format or
configuration option changed.

The six native tests pass with the new capability. Writable NFS/FUSE fixtures
now check a shared mapped reader sees writes through another descriptor, and
mapped writes followed by msync are visible through the descriptor. A mapping
owner unmaps on every test exit. The standard RustFS CLI scenarios and failed
held-descriptor probe above use the immediately preceding binary; current native
proof covers the negotiation repair. Shared mmap without explicit msync through
commit/reset still needs proof of kernel dirty-page draining, and native macOS
NFS remains blocked by local sudo requiring a password.

Next ownership boundary: publication currently clears overlay backings before
`adopt_published_snapshot` captures the next reader and installs the live view.
The successful post-commit-write reproduction must be repaired at that boundary.
Live commit needs one owner for client dirty-page draining, publication/freeze,
authenticated view adoption, active inode identity and cleanup. Cleanup must not
discard recoverable state before adoption can succeed. Retained descriptors must
be matched to the exact committed backing incarnation, not a reused path, byte
length or a global generation counter. Controls must not take inode content
locks while holding the engine publication write gate: writers take those locks
in the opposite order. Success, failure/retry, multiple commits without a new
open, rename/replacement, reset and unmount all require proof. Direct/offline
publication routing remains part of this same ownership requirement.

The new runtime regression `commit_keeps_open_inode_linked_for_later_writes`
also fails through the real local Git publisher and live adoption path, matching
the provider probe: held bytes are `AFTER!`, pathname bytes are `before`.
It is an ordinary required test, not ignored or recorded as an expected failure.
The earlier 627-pass broad run precedes its addition; the current tree has this
known failing regression until publication ownership is repaired. The latest
six-test native mmap run and strict macOS Clippy pass before that test addition.

## Thirteenth checkpoint: publication owns live adoption and inode handoff

The runtime regression and original RustFS/FUSE held-descriptor probe now pass.
`CommitView` gives the canonical publisher an explicit installation boundary:
live NFS/coordinator commits authenticate and install their captured view before
the frozen overlay can be cleared. The old snapshot-only commit entry point is
deleted. Live controls delegate to `commit_mount_runtime`; offline publication
keeps its distinct cache owner. Request payload is no longer duplicated in the
coordinator's captured resource target. Tree/index installation shares one
blocking implementation, avoiding nested blocking tree jobs during publication.

The engine keeps weak open-file bindings. Before cleanup the live owner matches
each committed backing device/inode to its exact captured new base source.
Subsequent COW resumes through verified reads; a same-named replacement does not
gain that authority. No controller takes an inode content mutex while holding
the engine publication write gate. Three commits with the same descriptor pass.
An injected authenticated-catalog failure preserves the old view and visible
overlay bytes; retry adopts the original Git commit rather than making another.
Retry also checks recorded transactions when snapshot state already advanced.

Two new privileged native probes first fail: shared mapped edits without msync
are omitted, and commit returns no changes. Linux `begin_mount_update` now drains
the client filesystem with syncfs before acquiring the engine gate, allowing
WRITE callbacks to complete. It opens the synthetic `.git` file instead of an
eager root-directory stream. Inactive/unmounted helper fixtures have no local
FUSE/NFS client pages to drain. NFS and coordinator commit/reset controls use this
boundary. Both native mapped-commit probes pass. macOS client draining and
automatic-refresh ownership still require separate proof; no global sync or
platform fallback was introduced.

Broader proof catches an introduced offline-owner error: a read-only snapshot
connection is used for installation, failing eleven publisher tests. The offline
owner now reads existing state without recovery and opens its writable store
only during installation, matching the prior lifecycle. A subsequent broad run
exposes a path-read admission race during automatic refresh. The focused
concurrent-invalidation regression reproduces it. `read(path)` now reacquires a
lease invalidated before I/O, yielding between attempts; explicit `read_at`
callers keep their stale-lease contract. The unchanged owned-clone refresh test
and the new path-read regression pass in the final broad run.

A strengthened recovery test finds another data-loss case: newer same-size
backing bytes with restored mtime are mistaken for an old recorded publication
and deleted. Retry fingerprints now stream BLAKE3 over file content under the
freeze lease. Directory records carry namespace only; apply_records materializes
tracked children separately and each child fingerprints its own bytes. The
directory sibling initially fails under a regular-file-only admission rule and
passes after that contract is applied. The locked hasher uses a 64 KiB read
buffer. This adds a local hashing pass; optimized publication cost remains to be
measured. No external dependency, Capsule encoding or lockfile version changed.

Final-source dedicated Linux proof: 630 passed, zero failed/filtered, nine ignored;
all eight privileged native tests pass separately. The ninth ignored test is
the existing isolated /proc/self/io blob measurement, not a hidden failure.
Strict macOS combined-feature Clippy, feature slices and CLI consumer checks pass
at their retained scopes. No new Linux strict-Clippy pass is claimed.

The rebuilt CLI passes both complete RustFS NFS/FUSE scenarios, including explicit
remount hashes, reset, concurrent/sparse/COW writes, failed-push preservation,
retry, commit/push and independent eager clone. The focused CLI probe now reports
held=`AFTER!`, path=`AFTER!`; shared/private mmap both admit. A dirty mapped commit
stores `BA` in the actual Git blob without application msync, and its next mapped
write remains visible as `BC`. A separate 32 MiB pointer probe holds one writable
descriptor through two no-push commits, then verifies its next write through
both the descriptor and pathname. These are functional, unoptimized proofs.

The full objective remains open: startup/catalog/remote-GC ownership; active
readers through all refresh/reset and clean-push/staging-retirement transitions;
other foreground/daemon/offline publication owners; crash/mixed-recipe/later-push
durability; total resource budgets, dispatch/directory/prefetch performance;
complete Crab pack v1 retirement; exact final all-mode and macOS native/performance
qualification. In particular, metadata-only root changes and retirement of local
staging while descriptors retain an older captured reader need explicit proof.

## Fourteenth checkpoint: committed descriptors cross staging retirement

The thirteenth checkpoint is now retained in
`publication-adoption-checkpoint.json`: 54 source hashes, 225 retained evidence
files and the installed CLI/helper hashes. Its qualification is historical at
that capture, not a claim about later edits.

A required runtime regression reproduces a post-push failure. A writable native
descriptor survives a local pointer commit; confirmed publication retires its
staging recipe and installs a catalog containing the same file. The pathname
reads correctly, but a subsequent descriptor write fails because its publication
binding resumes through the old catalog, which never contained that file.
The regression uses real encoded catalog/xorb data, real overlay publication and
`StagingPushSnapshot` retirement; it verifies the published recipe is absent.

`engine/open_file.rs` now retains only committed content identity in each weak
binding. When the current base content matches, it resumes through the current
captured lease and derives source lineage from that view. This removes the
binding's unnecessary old snapshot/catalog lifetime. Changed content or a
replacement backing still cannot authorize relinking. The existing base-Git
replacement, inode reuse, rename/unlink and COW sibling tests remain in the
passing suite. NFS does not use this FUSE open-file owner; its unchanged shared
publication/retirement path is exercised separately.

Final-source dedicated Linux library proof: 631 passed, zero failed, nine ignored;
all eight native mount tests pass. Strict macOS combined-feature Clippy and
minimal/NFS-only/FUSE-only checks pass. The rebuilt CLI uses the required Make
installer with the external target. No dependency or persistent format changed;
this adds 14 production lines to the existing owner rather than another reader.

The real CLI probe reproduces EIO with the preceding binary after clean push
reduces one staging path lease and 67 payload leases to zero. With the new binary,
FUSE and NFS both retain identical complete 4 MiB descriptor/path/expected hashes;
another write, later real push and independent eager clone preserve that hash.
These probes mount a local source and explicitly set the mount cache's origin
to its Crab URL for publication. They isolate the catalog handoff; they do not
qualify the default origin of a local-source coordinator clone.

Two initial probe results are retained honestly. A local-source clone's default
origin targets its checked-out seed, so Git refuses its push; that is a fixture
routing error, not proof of the descriptor bug. A fresh remote-source mount
fails earlier, while lazily fetching a policy blob during publication's linked
worktree checkout. The latter failure reproduces with the new CLI too:
`git checkout-index policy files` reports a bad revision and the promisor fetch
does not install the needed object. It is a new P0 qualification failure, not
fixed by the descriptor change. The next investigation must trace linked
worktree discovery and all Capsule fetch installation paths against Git's
common-object-directory contract before choosing a fix.

Still open: descriptors opened on an unpublished base pointer rather than its
retained native backing, old-reader/local-recipe lifetime through retirement,
failed re-promotion preserving a usable native descriptor, refresh/reset and
other owners. The current fix proves the reproduced committed-native-descriptor
case; it does not close every active-reader/staging transition. Startup/remote
GC, crash/mixed-recipe durability, full pack v1 retirement and optimized/all-mode
performance qualification remain required.

## Fifteenth checkpoint: fetch targets shared Git storage

The fresh-remote policy-file failure is reproduced with a real Capsule helper
fetch from a linked worktree. The helper reports success, then native Git cannot
read the promised blob. Its generated pack was installed beneath the private
worktree Git directory rather than the common object database.
`linked-fetch-before-source.json` captures the failing test's source hashes.

Git's repository-layout contract places objects, shared configuration and the
shallow boundary under the common directory. The three helper fetch entry
points and the cold-clone emptiness probe now discover that directory through
the existing canonical discovery API. Complete, constrained, incremental and
promisor installers consequently share the correct destination and lock path.
No public API, dependency, format or second installation path is added. The
protocol and VFS docs describe the shared-storage boundary.

A table-driven regression exercises real Capsule promisor, complete, filtered,
shallow and incremental fetches. It checks native Git object bytes from both
worktrees and shared shallow state; private directories must not gain an object
database or shallow file. The macOS regression and its Linux module occurrence
pass. The dedicated Linux helper module reports 146 passed and one failed.

The remaining failure is an existing checkpoint-format expectation: the push
and hydration test expects 5 after repack, while the encoder, root builder and
decoder require 9. Both conflicting values are present on the unchanged latest
`origin/crab-v2` tip, `33f8e6c360d922b3ab1e2c0e5115a2d200eccd97`; the isolated
test fails too. The production edits are in fetch-only paths, while that test
fails during push/repack before exercising its clone phase. No reader/writer
version is changed. A one-line assertion correction is prepared at
`plans/capsule-checkpoint-assertion.patch`; the user exception to AGENTS.md's
test-edit restriction is pending. No broad green helper-suite claim is made.
macOS CLI/lib Clippy completes with 671 repository warnings; this is not a strict
Clippy pass. Existing VFS source and its fourteenth-checkpoint proof are unchanged.

The Make-installed CLI now passes the exact fresh remote-source RustFS/FUSE
probe: lazy policy fetch, local commit, clean push retiring local recipe leases,
full 4 MiB held/path/expected hash equality, later push and independent eager
clone. No local-source or origin-routing adjustment is used in this probe.
NFS now passes the formerly failing local commit, but exposes a later failure:
clean push reports no staging entries for the new pointer. Its helper can resolve
push staging and state through an unrelated working-directory repository before
consulting explicit GIT_DIR; this is the leading next hypothesis, not yet a
repaired or fully proven path. The seed hash differs from the missing new hash,
so this is not a request to stage already-published seed content.

`linked-fetch-checkpoint.json` binds 54 source hashes, the installed binaries and
41 retained evidence files. Remaining requirements include the NFS staging/root
failure, pending format assertion decision, all earlier lifecycle/durability
gates, complete Crab pack v1 retirement and optimized all-mode qualification.

## Sixteenth checkpoint: explicit Git metadata owns both push roots

The required `explicit_git_dir_selects_push_roots_over_unrelated_cwd` regression
confirms the NFS clean-push hypothesis: with GIT_DIR set and GIT_WORK_TREE unset,
both staging and incremental push state select the unrelated cwd repository.
The previous generic WorktreeContext path requires both environment variables
before honoring the override, so cwd-first fallback chains select the wrong owner.
The seed pointer is already published, while the missing hash is the new local
version; selecting seed staging hides that version's local recipe from push.

One canonical `push_repo_root` now resolves and canonicalizes the selected common
Git directory. Session push state and staging both use that root. The old three
root helpers, cwd-first alternatives and empty-path default are deleted;
discovery/canonicalization errors propagate through the existing Result boundary.
The generic WorktreeContext API and its other callers are unchanged. A duplicate
nested-cwd test for the removed staging wrapper is deleted; its canonical-root
property remains covered alongside the new normal/bare/linked matrix. No public
API, dependency, serialized format or config/env option is introduced.

The first refactor selects the correct physical directory but returns its /var
alias on macOS, failing the canonical-path assertion. Canonicalizing selected
metadata repairs that introduced spelling mismatch. The final three-layout
regression passes, eight root/attribute sibling tests pass, and all five linked
fetch modes from the fifteenth checkpoint still pass. The minimal CLI consumer
check passes with its 18 repository warnings. CLI/lib Clippy completes with 671
warnings; no strict lint pass is claimed.

Both unchanged fresh remote-source RustFS probes now pass using the rebuilt,
Make-installed CLI: initialize/push seed, mount, local commit, clean push, native
descriptor reuse, another push and independent eager clone. Staging leases drop
from one path/67 payload leases to zero at clean push; complete 4 MiB held,
pathname and expected hashes agree. No origin-routing adjustment is applied.
This closes the reproduced fresh NFS staging-root error and rechecks FUSE's
sibling path. Linux helper/VFS, publication, read and reconstruction owners
remain source-qualified only at their explicitly retained scopes.

`explicit-push-root-checkpoint.json` binds 54 source hashes, installed binary
hashes and 50 retained evidence files. The broader helper suite's existing
checkpoint expectation 5 versus required format 9 remains unresolved: its
one-line correction still awaits the user's AGENTS.md exception and has not
been applied. The sixteenth-checkpoint root refactor has narrow/sibling/real
provider proof; the fifteenth-checkpoint 146-pass/one-fail helper run is not
presented as a green or final-source broad run for this later refactor.

The full objective remains active: coherent startup/remote-GC ownership,
unpublished base-pointer readers through staging retirement, failed promotion
retaining usable native content, all product-mode/refresh/reset owners,
crash/mixed-recipe/namespace migration durability, complete Crab pack v1 removal,
total admission/resource budgets and optimized/native-macOS qualification.

## Seventeenth checkpoint: failed COW retains native read and retry ownership

A new real-catalog/staging regression reproduces the remaining failed-promotion
hole: commit a local pointer while retaining its native descriptor, retire its
recipe through `StagingPushSnapshot`, then attempt COW before a catalog containing
that version is installed. Reconstruction fails as expected, but the old code
also makes the existing descriptor unreadable. `resume_published` had discarded
both the native backing and its publication binding before reconstruction.
The retained baseline log covers the initial direct-write case; the final
regression additionally covers an access upgrade on a retained read-only file.

`materialize_open_file` now prepares a candidate published lease without changing
the open file. It retains original descriptors/binding until reconstruction,
backing installation and overlay registration succeed. Descriptor cloning and
binding registration occur before publishing the overlay entry, leaving no
fallible descriptor setup after that publication. The eager `resume_published`
helper is deleted. Access upgrades await the same promotion path under the
existing inode-content/engine-gate ordering, then invalidate the path cache.
A failed new open therefore cannot revoke an existing descriptor's readable
content. The previous replaced-inode regression only changes to await the API;
its behavioral assertion remains unchanged.

The final regression proves both direct-write and access-upgrade failure retain
bytes and recover after the correct captured catalog is installed. Seven focused
macOS catalog tests pass; strict macOS VFS all-target Clippy and minimal/NFS/FUSE
feature checks pass. Dedicated Linux final-source VFS verification reports
632 passed, zero failed, nine ignored; all eight explicitly invoked native
NFS/FUSE tests pass. NFS's path-based promotion does not use `VfsOpenFile` and
is unchanged; its native and real-provider siblings still pass.

Both Make-installed fresh remote-source RustFS probes pass an additional access
upgrade flow: modify and close the original writer, retain a read-only native
descriptor, commit, clean push, open a writer, edit, compare complete 4 MiB held
and pathname hashes, push and compare an independent eager clone. Path leases
fall from one to zero and payload leases from 67 to zero. Both backend full-file
SHA-256 values are `a64e9469aa0cc63dbab1445b8843706921c4303bf293a0745a8a45b83fdb210e`.
The mounted probes cover successful promotion; injected reconstruction failure
and recovery are proved at the shared engine/captured-catalog seam.

Dependency contracts checked: Rust `File::try_clone` shares the underlying
handle and may fail, so clone before publication; Unix `FileExt::read_at` is
independent of the shared cursor and allows short reads. Canonical pointer
reconstruction and overlay registration preserve their existing error sources.
No dependency, public format or configuration change is introduced.

`cow-lifetime-checkpoint.json` binds the current sources, installed binaries and
retained logs. This is correctness progress, not a throughput improvement:
post-cleanup access upgrades may materialize a new backing before writing.
Avoiding repeated verified COW copies remains a performance opportunity.
Unpublished base-pointer reader lifetime, startup/remote-GC ownership,
crash/mixed-recipe durability, all product owners, full pack-v1 retirement,
total resource admission and optimized/native-macOS qualification remain open.
The existing checkpoint-format assertion exception remains pending and unapplied.

## Eighteenth checkpoint: remove manifest-only replica readiness

The new primary-root regression first reproduces `ready: true` for a manifest-only
repository. `replica_readiness` now propagates the canonical Capsule root load
instead of selecting the old manifest/index path. Its shared exported legacy
checker and private object walkers are deleted. Whole-workspace source search
finds no remaining caller/export of that API. Existing selected-replica root
validation stays in place; initialized empty Capsule views remain supported.

Routing fixtures now publish Capsule catalog sections and real encoded xorbs.
The missing-payload test still proves primary fallback and missing-object events;
the provider-failure test now faults the canonical xorb GET instead of a retired
pack HEAD. Named-replica policy, ready selection and per-ref cache semantics stay
covered. These are fixture migrations for the removed format, not weakened
assertions. Protocol and shared-read docs now state mandatory Capsule readiness.

macOS and dedicated Linux each pass 105 replication tests. Dedicated Linux passes
all 238 shared-read tests with local staging. Strict macOS shared-read all-target
Clippy and the minimal CLI consumer check pass; the latter retains 18 repository
warnings. This is scoped proof, not a full CLI/helper-suite green claim.

The rebuilt fresh RustFS NFS/FUSE probes both reach local commit but fail clean
push with `duplicate writer fence holder`. The retained bucket-wide fence body
contains the same holder twice. No readiness code touched that owner. The mounted
failure is retained, and the next checkpoint fixes the independently reproduced
GC acquisition defect before repeating successful E2E qualification. Full Crab
pack-v1 retirement remains open at the located maintenance, SDK initialization
and protected-publication boundaries listed below.

## Nineteenth checkpoint: GC writer retries retain one admission

A focused coordination regression reproduces the mounted failure mechanism:
a writer's conditional acquisition is already committed, then another CAS
iteration uses the same request identity. The previous code appended that holder
again and could persist a malformed fence. A lost conditional-write reply and a
cross-process identity collision are not conclusively distinguished by the live
logs; the correction addresses both contracts.

Active writer retries renew the existing holder through the same CAS path without
advancing writer history. Existing admission may renew at capacity; new writers
still obey both bounds. Expired or quarantined request identities return the typed
lost-fence error without writing state. Holder generation uses the already-enabled
UUID-v7 dependency plus a diagnostic PID, replacing PID/wall-clock/process-counter
identity, which has no distributed uniqueness guarantee. Consumers treat holder
strings as opaque. No lease schema, public API, dependency or configuration changes.
Malformed already-stored fence state remains an error; no deduplication repair or
fallback is added.

macOS passes 14 focused fence tests and strict coordination all-target Clippy.
Dedicated Linux passes all 123 coordination tests and strict all-target Clippy.
The actual damaged RustFS record and both failed mount probes remain preserved.
A new task-owned RustFS instance isolates fresh validation from that malformed
record. The Make-installed final CLI passes both fresh remote-source mounted
NFS/FUSE publication probes concurrently: seed push, local commit, clean push,
staging retirement, retained reader/writer upgrade, later push and independent
eager clone. Complete 4 MiB held/path/clone hashes match. The concurrent fence
monitor records 102 snapshots, observes two active writers and finds no duplicate
holders. The final fence has no writers, sweep owner, quarantines or block marker.
This proves fresh state remains valid; it does not repair or reinterpret the
retained malformed record.

`replica-fence-checkpoint.json` binds the final source/binary hashes and retained
results. Replication/shared-read suites predate the GC edits and retain their
explicit source scopes; the final mounted flows and coordination suite cover the
GC correction. The full CLI/helper suite and all-mode/performance signoff remain
open. No commit or push of the working patch has been made.

## Twentieth checkpoint: unpublished base-pointer recipe lifetime

A distinct open-file regression commits before opening the pointer, then retires
staging through the real push snapshot and reads the retained file before any
pathname read. The prior implementation fails with missing `file_index` from its
captured old catalog. The new shared-engine regression passes after retaining an
unpublished recipe pin. The unchanged real-provider probe against nineteenth-
checkpoint Make-installed binaries reproduces FUSE EIO immediately after clean
push; its NFS sibling passes the full retained-reader/later-push/eager-clone flow.
NFS has no open RPC and does not retain `VfsOpenFile` ownership.

`StagedFileReader::into_pin` transfers its captured recipe to a metadata-only
`StagedFilePin`: a SQLite connection and separately flocked ready marker, with
no global shared staging lock or reader pool. Read snapshots capture verified
retained recipes without path leases or publication authority. Push retirement
can therefore remove the published path while the open inode retains readable
bytes. Later byte reads reacquire ordinary shared staging admission. FUSE releases
the pin only after successful native promotion or the last open/inflight owner;
failed promotion preserves it. Remote root retention is a separate unresolved
contract and is not supplied by this local pin.

The operation marker becomes a ready pin by atomic rename after durable recipe
capture. Maintenance checks ready-marker OS locks, not diagnostic PIDs. A vanished
old marker from a directory scan cannot authorize journal removal. Exclusive
cleanup preserves active pins; pin drop removes metadata and leaves physical
reclamation with maintenance. Per-attempt staging/publication identities retain
64-hex shape and use the existing workspace UUID dependency for entropy. The
lockfile change adds only the internal staging-to-UUID dependency edge; no external
version or checksum changes. The earlier shared-read/staging and VFS dependency
edges remain separately accounted for.

The staging regression covers prepared xorbs and segment recipes, confirmed push
retirement, exclusive open/clean/compaction, a foreign diagnostic PID, complete
verified reads and final marker/journal removal. The segment variant now requires
nonzero physically moved chunks, proving locator transfer rather than a no-op
compaction. Its focused macOS run and all nine final-source reader tests pass.
Final-source macOS catalog tests (eight), strict all-target Clippy and minimal,
NFS-only and FUSE-only checks pass. Dedicated Linux passes shared-read 238,
staging 254 (one ignored), VFS 633 (nine ignored), guard five and native eight.
Strict Linux Clippy then found two unchanged baseline mount-probe expressions
in daemon/NFS lifecycle code. Both now use equivalent `Result::is_ok_and`, without
changing error/absence behavior. Strict Clippy passes on both platforms after
those two Linux-only edits. The resulting VFS sources pass another 633-test run
(nine ignored) and all eight explicitly invoked native tests. Shared-read,
staging and guard sources are unchanged by the lint edits. Make installation
passes for the final CLI/helpers/cache server; the minimal CLI retains 13
warnings, so this is not a strict whole-CLI Clippy claim.

Both final RustFS probes pass: local commit before open, clean push retirement,
full retained read before pathname warming, writer upgrade/edit, later push and
eager clone. Before the retained read, `POSIX_FADV_DONTNEED` requests clean client-page
eviction. A further NFS probe measures residency with a non-faulting mapping and
`mincore`: zero of 1024 pages reside before the read, and all 1024 reside afterward.
Its full publication/edit/clone flow also passes, closing the client-cache concern.
The clone uses a separate empty `CRAB_CACHE_DIR`;
the baseline nineteenth-checkpoint script used an independent checkout sharing
its client cache, so that earlier result alone did not prove cold origin reads.
Complete 4 MiB held/path/clone SHA-256 values match on both backends.

FUSE path leases fall from one to zero while 67 recipe payload records and one
read snapshot remain during the retained file's lifetime. After native promotion
and final close, snapshot journals and ready markers are both zero. The 67 old
payload records remain for maintenance, as designed; physical post-close
reclamation is not established by these mounted probes. NFS has zero pin journals
and retires its path and payload records to zero. The staging regression separately
runs exclusive cleanup after pin release and confirms no pin journals/markers.

Dependency contracts checked: fs4 0.13.1 maps nonblocking flock contention to
WouldBlock; SQLite WAL connections serialize metadata transactions, and the pin
holds no long-lived transaction. Released Crab v1.2.4 rejects staging layout v3,
so ready-pin interpretation does not introduce a mixed released-writer reader.
This is local staging ownership, distinct from retired Crab pack-v1 support.

Consumer review found a diagnostic follow-up: lifecycle health currently counts
all open snapshot journals as `open_push_snapshots`, so doctor can label a live
read pin as an unfinished push and recommend cleanup that correctly preserves it.
CLI staging stats only display `list_inflight`; they do not block maintenance.
Keep byte/publication lifetime proof separate from this remaining diagnostic
classification, and update the public marker-list rustdoc with that correction.
Large-scale pin overhead remains open: each held unpublished inode retains a
SQLite connection and marker FD, and operation admission currently scans ready
markers during stale-marker pruning. These deterministic payloads can reuse
bucket-global xorbs; fresh-cache clones prove origin availability/identity, while
new-upload amplification still needs a salted-content workload. The Make probes
use combined NFS/FUSE features for both helper selections; default split-helper
packaging and optimized release behavior remain separate qualification gates. Measure many-pin read/admission scaling
before choosing shared connection ownership or separating ready-marker scans from
the hot path. No throughput or total-resource bound is claimed here.

`base-pointer-checkpoint.json` binds the current source hashes, both old and new
installed binary hashes and retained evidence. The full goal remains active;
no commit or push was made. The existing checkpoint-format assertion exception
remains pending and its patch remains unapplied.

Dependency review also found bundled SQLite 3.45.0 via rusqlite 0.31.0 and
libsqlite3-sys 0.28.0. SQLite documents a rare WAL-reset corruption race for
concurrent writes/checkpoints, fixed in 3.51.3 and later (plus named backports):
https://www.sqlite.org/wal.html#the_wal_reset_bug . Existing staging shared
connections already use WAL, and new long-lived pins expand that concurrency.
Passing ordinary tests cannot exclude the upstream race. Qualify a normal
workspace dependency upgrade separately; do not patch, override or vendor the
SQLite dependency. Rusqlite 0.39.0 bundles fixed SQLite 3.51.3; 0.40.2 bundles
3.53.2. Upstream release notes and bundled headers are the dependency evidence.

## Twenty-first checkpoint: mapped writeback precedes every live transition

The controlled native probe now uses a separate Python client process. Pausing a
server WRITE while keeping client descriptors/mappings in that same process made
Git children block in close-on-exec: retained kernel stacks show `filp_close` /
`do_close_on_exec`. The test harness, not a product control operation, was stuck.
Only the owned test processes, three matching FUSE connections and its NFS client
were terminated/drained; the VM and both existing RustFS services stayed running.

With that harness corrected, the previous transition ordering fails all six
manual refresh, switch and automatic-refresh probes. A post-transition client
fsync acknowledges the pending WRITE while its mapping remains open. FUSE still
exposes the new `Z` base instead of the unpublished `B` edit in all three cases.
NFS eventually exposes the complete edited bytes after fsync, but publishes the
new generation before the previously pending WRITE completes. This distinguishes
persistent FUSE ownership loss from NFS writeback ordering; the first probe's
immediate read alone did not prove persistent NFS data loss.

Manual refresh/switch and `RefreshService` now use the existing
`begin_mount_update` boundary: drain the Linux client filesystem before acquiring
the mutation gate. The automatic service receives the actual mountpoint from the
pipeline, coordinator IPC and daemon owners. There is no second reconstruction
path or new configuration option. Commit/reset keep the same canonical boundary;
macOS client draining remains unqualified. Native tests hold the WRITE, require
the generation to stay unchanged, release it, acknowledge client fsync and compare
all bytes before unmapping. The WRITE/control scheduling seams compile only into
combined-feature Linux tests.

The old-ordering comparison uses two read-only source bind overrides in the
owned test container; the worktree source is never reverted. Cargo then reused
that older diagnostic executable after the overrides were removed because its
input timestamps were newer. That purported after-run is explicitly discarded.
Only this worktree's external `crab-vfs` build artifacts were invalidated (660
files, 1.7 GiB), and current sources were rebuilt. The clean final run passes
633 VFS tests (15 ignored), all 14 explicitly selected native tests and strict
all-target Linux Clippy. macOS passes strict combined-feature Clippy, 18
mount-runtime tests, 25 refresh tests, and minimal/NFS-only/FUSE-only checks.
Only the Linux-only ignored probe changed after those macOS checks.

Make installation now exercises distinct minimal feature selections: the FUSE
helper carries `nfs,fuse`; the main CLI/NFS helper carries `nfs`. Artifact lookups
use the external debug target explicitly, and GCC selects the toolchain's bundled
LLD. This proves the helper role split, not default release feature breadth or
optimized performance. The CLI still reports its existing feature-slice warnings.

Real RustFS/Capsule probes use fresh salted 4 MiB content, a separate mapped
client, a diverging published base, manual refresh/switch or automatic refresh,
then mount commit/push and independent eager clones with separate caches. All
three full-file clone hashes match on NFS through the main CLI. All three also
match through the installed FUSE helper, including the real coordinator's
scheduled refresh. These are functional/durability scenarios, not throughput or
whole-resource qualification.

The role-split probe exposes a separate required repair: the NFS-only main CLI
successfully launches FUSE, but `crab mount refresh` reports no live mount.
`mount_control` compiles its FUSE client branches only with `fuse`, and the IPC
client imports request/response types from the FUSE server module. Main-CLI FUSE
status/refresh/switch/commit/reset/shutdown need one canonical Unix client
transport independent of linking the FUSE server. Preserve the real failure;
helper-only success does not qualify the default user entry point. Audit all
request/response consumers, bootstrap/executable ownership and Windows/minimal
feature gates before choosing the bounded refactor. Do not add recursive CLI
fallbacks or solve only refresh. Source leads: `mount_control.rs`, `ipc_client.rs`,
`ipc_server.rs`, `lib.rs` under `crates/crab-vfs/src/`, plus CLI consumers.

Dependency investigation confirms the SQLite upgrade gate is external to the VFS
repair: Cellar 0.1.0 requires rusqlite 0.31/libsqlite3-sys 0.28. A normal rusqlite
0.41 resolution conflicts with its `links = "sqlite3"` contract. The trial root
manifest was restored and Cargo.lock was not changed by that trial. No dependency
patch, override or vendored copy was added. A matched Cellar release remains
required before increasing metadata concurrency. The checkpoint-format assertion
exception is still pending; its proposed expected-value patch stays unapplied.

Evidence is bound in `mapped-transition-checkpoint.json` beside the retained audit
logs. All prior remaining gates and full Crab pack-v1 retirement scope remain.
No commit or push was made; the full goal is active.

## Twenty-second checkpoint: Unix controls independent of the FUSE server

The installed role split needs the NFS-only main CLI to control the separately
installed FUSE helper. Unix client transport and unchanged wire DTOs now compile
with either backend; coordinator spawning and the server still require FUSE.
Shared status/list/refresh/switch/commit/reset/shutdown route through one client.
List identifies the owning coordinator before a pathname mutation. Missing or
refused sockets mean no coordinator; other transport/protocol errors propagate.
An uncertain mutation is never replayed through an offline path. List reuses one
connection for its per-mount status requests. CLI live overlay context/diff/reset
no longer requires linking FUSE, and duplicate live reset handling is removed.
All request/response consumers, including coordinator integration/property tests,
now import the protocol module. Transport errors preserve their source chain.

The fresh Linux NFS-only Unix peer regression passes all 11 control tests,
including a rejected publication. Fresh macOS NFS-only compilation and formatting
pass. Make completes the distinct-feature macOS CLI/helper/cache-server install
using the external debug target and task-specific prefix. The CLI retains its
13 feature-slice warnings; the macOS linker also warns about the large debug
unwind section. No optimized performance claim follows from this build. Exact installed-binary/provider qualification of this checkpoint is
pending; checkpoint-21 helper-only proof does not qualify it. Historical logs
remain retained. At this checkpoint the external build directory was absent;
fresh Linux/macOS build directories were created on the mounted Workspace volume.
The task-owned VM and RustFS services were retained. No prior binary was reused.
Two broad Linux attempts pass 633 tests and fail two Git fixtures because Cargo
starts their executable below the source mount's host-only linked-worktree
pointer. Changing Cargo's own working directory does not change test cwd. A
final rerun executes the exact newly built test binary from the isolated ext4
fixture directory: 635 tests and all 14 native probes pass. Strict combined-feature
all-target Linux Clippy passes. Source/tests are unchanged; both failed harness
runs remain retained.

`origin/crab-v2` advanced from the audited base to `9cc0fdab60a` (SDK/Capsule PR
#629). Its read-feature and metadata contracts overlap this patch. The first PR
is a draft checkpoint against `crab-v2`; integrating that change and rerunning
consumer/native/provider checks is the immediate next priority. Do not interpret
this checkpoint or historical passing logs as latest-base release qualification.
The checkpoint-format expected-value patch remains unapplied. SQLite/Cellar,
remaining stored-format retirement, all-mode and performance gates stay open.

## Twenty-third checkpoint: integrate native SDK/Capsule authority

Rebased onto `origin/crab-v2` at `9cc0fdab60a` (SDK/Capsule #629). Five conflicts
were resolved across CLI/VFS read dependencies, read feature declarations,
exports and scoped guidance. `local-staging` explicitly implies `hydration`;
CLI/VFS select that one capability. Upstream metadata-only/SDK read gates remain
intact. No compatibility alias, dependency override, version change or second
hydration path was introduced. Fresh macOS NFS-only VFS and minimal `crab-read`
checks pass. All ten SDK dependency profiles pass their existing boundary checker.
The new base passes 635 VFS tests, all 14 native NFS/FUSE probes, strict combined
all-target VFS Clippy, and minimal/NFS-only/FUSE-only VFS checks. Minimal read and
SDK checks pass. The final VFS rerun retains 635 passes after normalizing the new
Unix peer fixture's macOS temp alias to the canonical path advertised by the CLI;
all 11 macOS NFS-only control tests pass without changing their assertions.
Native production/harness sources were unaffected by that fixture correction.

Make installs distinct Linux roles from the external debug target: NFS-only main
CLI/NFS helper, combined-feature FUSE helper and cache server. Main-CLI RustFS
probes pass status/list, writable diff/reset, mapped manual refresh/switch and
real automatic refresh, then commit/push and independent fresh-cache eager clone.
All six salted 4 MiB full-file hashes match across the two backends. Read-only
main-CLI probes also verify status/list, real reads, denied writes/commits,
refresh/switch hashes and unmount. The read-only overlay-diff diagnostic is a
separate retained failure/parity finding below; it is not hidden by the narrower
supported read-only-control probe.

Both CLI coordinator consumers pass: nine lifecycle tests and eight property
tests. Their initial parallel GNU linker attempt was killed; the retained kernel
log confirms an OOM victim `ld` with about 2.2 GiB resident memory. Retrying with
the already bundled Rust LLVM linker and one build job passes all 17 tests. No
dependency patch/version/override was added. The current CLI feature slice has
14 warnings, including an upstream unused fsck field; strict whole-CLI Clippy
and full CLI tests are not claimed. Debug functional probes are not optimized
performance qualification. Source and binary bindings are in
`integration-checkpoint.json` beside completed logs and probe inputs.

The new read-only diagnostic exposes the next confirmed parity issue: FUSE
`mount diff` rejects a read-only mount through the coordinator's writable-overlay
check, while NFS succeeds through local overlay-path inspection. The CLI help
specifies writable mounts. Sources: `crab/src/cmd/mount.rs::overlay_diff_once`,
`crates/crab-vfs/src/coordinator.rs::inspect_mount_overlay`, and the mount diff
help in `crab/src/main.rs`. Live NFS inspection/export and read-only policy need
one ownership review; do not repair only the error message. Export already takes
OverlayStore's process-spanning freeze lease, so it must not be described as
unguarded. Prove generation/control ownership and concurrent-operation behavior
before choosing its next bounded refactor.

Upstream SDK `Client::initialize_remote` now delegates directly to
`crab_write::capsule_protocol::initialize`; its prior manifest-only initialization
lead is closed by that upstream change. Workspace search finds the old shared
`initialize_repository` helper still exported, with remaining calls in legacy
fixtures. Audit/remove that obsolete writer together with gateway admission and
fixtures; do not implement another SDK initializer or label upstream work as a
new change in this PR.

The rebuilt Linux target lives in this task's VM ext4 namespace. The Docker bind
using the host HOME symlink resolves inside the VM rather than to the host proof
directory. The VM disks remain on Workspace and the target is worktree-specific.
Qualification now binds the literal `/Volumes/Workspace` proof directory separately
for scripts and installed binaries. Two script launches failed before any Cargo
operation because the former bind did not expose the host script; source and
previous library/native results are unaffected. The explicit proof bind is
verified before restarting qualification. No other VM, target or repository was
cleaned or reused.

## Twenty-fourth checkpoint: owner-routed overlay inspection

A real writable → read-only → writable RustFS mode cycle proves the earlier
parity diagnostic is a correctness bug: the read-only NFS view does not contain
`unpublished-owner.txt`, but `mount diff` reports it and `mount export` copies its
saved writable bytes. A final writable remount recovers those exact bytes. FUSE's
coordinator rejects read-only inspection. The mode cycle distinguishes hidden
saved overlay state from the current mounted namespace; it is stronger than an
empty read-only cache probe. Initial host-bind executable startup attempts timed
out before mounting and one helper startup failed; those attempts are not product
mode-cycle proof. Completed before runs use writable artifact binds and retain
helper logs on failures.

NFS now has a live `diff_overlay` request. Both adapters hand the live overlay to
one owned runtime inspection operation. Linux client writeback finishes before
the engine's exclusive mutation gate; a process-spanning freeze then protects the
metadata read. A non-mutating quiesce guard preserves existing reader epochs and
leases. FUSE releases the coordinator lock before waiting and no longer performs
a WAL checkpoint just to inspect. The CLI removes repeated summary polling and
uses actual owner errors for read-only inspection/export preflight. Commit/reset
already own their drain boundary and no longer perform a separate diff preflight.

A second failing CLI regression proves that removing an NFS registry entry
between context capture and inspection could select private offline state. The
context now marks both registered backends as known owners and refuses that
redirection. Legacy/unregistered foreground and offline recovery ownership still
need separate qualification; this change does not claim to wire those owners.
All three offline CLI call sites share the same filesystem writeback helper;
CLI path validation stays above the helper's inactive-client skip. macOS retains
its previous global sync request in the live owner before the gate. A waiting
per-mount native NFS barrier is still unqualified. Apple's SDK declares
`fsync_volume_np` and `SYNC_VOLUME_WAIT`, but upstream implementation/error and NFS
contracts require proof before selecting it; no dependency binding override or
new product knob was introduced.

Current checks pass 637 Linux VFS tests and all 16 native probes, including two
new held-WRITE mapped inspection cases. The read-lease regression would fail if
inspection reused the mutating reset guard. macOS passes both new VFS inspection
tests and both CLI context tests; the lost-owner regression fails before its fix.
Strict combined-feature VFS Clippy passes on both platforms. All six real RustFS
4 MiB mapped refresh/switch/automatic → commit/push → independent eager clone
hash flows pass on the final installed roles. Final installed-role
RustFS mode cycles reject read-only diff/export on both adapters and recover the
saved writable bytes. A separate test precondition waits for the cache lease
between modes: NFS needed 111 ms after unmount had already returned. The original
immediate-remount NFS run is retained as a failure, with the new helper reporting
cache-already-in-use. This checkpoint does not repair that lifecycle gap.

Paired warm CLI diff runs compare the previous and new CLI against the same live
helper and identical structured diff payload: NFS median 201 → 61 ms, FUSE
154 → 40 ms (10 retained samples each after warmup). These debug results qualify
removal of summary polling only; they do not measure read throughput or establish
optimized production performance. Evidence, scripts and source/binary hashes live
under `crab/docs/benchmarks/vfs-capsule-v2-audit-20261024/overlay-owner-*`.

NFS unmount/cache-lease handoff is the next confirmed correctness repair. Wait
for the authoritative live cache owner to finish shutdown; do not insert a
mount retry or consider a missing kernel mount sufficient. Preserve the sibling
FUSE/coordinator and daemon ordering, which drains the engine before dropping
cache ownership.

Export remains the next inspection ownership boundary. Its existing freeze spans processes,
but metadata-only base renames are copied through the mounted path. Reproduce
interaction with blocked writes and refresh before a bounded refactor. A direct
canonical engine reader must keep local recipe pins for the whole exported file,
avoid kernel re-entry while holding the gate/freeze, preserve symlink/directory
semantics, and reject outputs overlapping the live mount or its private cache.
No second pointer reconstruction implementation or offline retry is acceptable.

## Twenty-fifth checkpoint: NFS cache ownership handoff

The original immediate-remount RustFS failure remains the reproduction: NFS
unmount succeeds while its helper still holds the mutable cache lease. A new
CLI regression with an explicitly held live cache fails before this repair.
The helper now returns its actual cache identity on accepted shutdown. Completion
requires kernel detachment plus acquiring that cache's existing exclusive lease;
the CLI holds it through owner-conditional registry removal. PID exit is not a
substitute: a zombie may still answer the liveness probe. Replacement PID/start
identity/control endpoint is preserved, while a ref switch does not change the
owner. No mount retry, arbitrary post-unmount delay, cache-path reconstruction,
error-string parsing, dependency override or new configuration was added.

Caller review also found single and batch unmount swallowing live-control errors
and selecting PID termination. The retained installed-binary fault probe proves
both paths terminate the test owner after an explicit shutdown rejection, masking
the original error. Both callers now propagate live errors. Only an NFS helper
confirmed dead before an exchange may select stale cleanup; accepted/uncertain
shutdown cannot authorize force termination. Helpers do not write FUSE PID files,
so live NFS cleanup no longer removes them. Typed backend shutdown resources
replace the previous optional-field combinations.

Sibling ownership: FUSE coordinator unmount already awaits the removed session,
then drains the engine before dropping its cache guard. Daemon teardown awaits
its backend and engine before releasing its separately owned runtime cache lease.
This repair concerns registered helper controls; direct daemon/private-endpoint
and native NFS child-task completion still require separate all-mode proof.

Linux passes 638 VFS library tests, strict combined all-target VFS Clippy,
and a FUSE-only check on the final typed control shape. The earlier pass also
runs all 16 native probes; engine/native lifetime code is unchanged by the final
enum boxing. Both Linux and macOS pass all 16 CLI unmount tests on final sources.
macOS additionally passes 12 control, 14 cache and 20 registry tests plus strict
combined-feature VFS Clippy. One new macOS fixture initially used a temporary
path alias instead of the canonical registered path; that failed attempt is
retained and is not product proof.

Make installs final NFS-only main/NFS-helper and combined FUSE-helper roles. Each
backend completes ten immediate writable → read-only cycles against RustFS,
with retained writable bytes, read-only inspection rejection and twenty immediate
nonblocking cache-lease assertions: forty mounted transitions and forty lease
checks total. The probe has no lease-ready wait or retry. The installed new CLI
also preserves the owner PID and registry after rejected single/batch controls,
and exposes the original error. The previous installed CLI terminates that test
owner and masks the error. No performance-throughput claim is made.

The initial GNU-linker install was deliberately stopped after a final production
cleanup edit invalidated its source snapshot; it is not qualification. The final
Make install uses the already-qualified bundled linker, without a dependency
patch or target cleanup. External targets remain per-worktree/platform. The
main CLI slice retains 14 existing warnings; full CLI/strict whole-CLI claims
remain excluded. Final source, installed binaries, probes and outputs are bound
by `unmount-cache-checkpoint.json` and adjacent retained evidence.

Non-test growth owns the live cache receipt, typed shutdown resources and atomic
owner-conditional registry cleanup. It removes guessed completion, invalid
optional shutdown states and force fallback after live errors. The existing
flock nonblocking contract is retained: contention is typed absence, other I/O
failures propagate. The SDK's macOS flock man page and actual Linux/macOS lock
regressions supply dependency-contract proof.

A new sibling diagnostic proves `unmount --all` omits coordinator-only FUSE
mounts: the installed pre-repair main CLI returns success with "No active mounts"
while the kernel view remains mounted; explicit single unmount then cleans it.
The batch routine enumerates only `mounts_registry`, despite the shared control
list already merging coordinator owners. Retain this failing gate. Canonical
all-owner batch discovery and disappearance/error semantics are the next repair,
before export ownership; they are not fixed by cache-lease completion.

## Twenty-sixth checkpoint: strict batch owner discovery

The prior FUSE-only diagnostic returns success/No active mounts while the native
mount remains attached. A new mixed RustFS reproduction on the checkpoint-25
installed roles returns Unmounted 1 mount(s): NFS is gone, FUSE remains mounted.
Both before probes clean their remaining mounts explicitly. All first five CLI
regressions fail before the fix; direct VFS listing also converts rejected
coordinator discovery into an empty successful list before this repair.

`mount_control::control_targets` now merges registered helpers with the live
coordinator inventory. One shared coordinator List validator serves discovery
and individual FUSE control admission. Rejected and incomplete responses propagate;
only missing/refused connections represent absence. User-facing list status
metadata is enriched after discovery. Batch unmount performs no extra NFS status
queries. A lost known live owner cannot select stale/PID cleanup. Backend
conflicts, changed recorded identity and live processes without control fail
closed; only the same recorded dead owner may select stale cleanup.

Both Linux and macOS pass all 24 unmount tests, including eight new batch cases.
They cover coordinator-only discovery, rejected/malformed inventory, owner loss,
rejected unmount, conflicting backend records, a live unowned PID, and registry
replacement during discovery. Linux passes 639 VFS tests, strict combined
all-target VFS Clippy and a FUSE-only check. macOS passes 13 VFS control tests
and strict combined VFS Clippy. No baseline/expected-format patch was applied.

Final Make-installed NFS-only main/NFS helper and combined FUSE helper pass the
FUSE-only empty-registry case and three mixed RustFS rounds: writable mounts,
writable remounts retaining exact bytes, and read-only mounts hiding the saved
writable overlay. Each mixed round unmounts both adapters, reports 2 mounts,
clears registered owners and permits immediate nonblocking acquisition of both
cache leases. All six cache handoff assertions pass without waits or retries.
Installed rejected-control probes still preserve the owner PID/registry and
original error for single and batch unmount.

A separate paired warm live-list check compares the previous and final main CLI
against the same two actual mounts. All structured fields except uptime match.
After three warmup pairs, ten retained samples per client give medians 8.32 ms
(previous) and 8.59 ms (current), with alternating execution order. These debug
CLI measurements do not establish data-plane throughput or tail performance.

The task-owned Colima profile was authoritatively stopped when final Linux
qualification resumed. Restart reused that same profile and its existing RustFS
containers. The failed launch happened before compilation and is excluded from
passing proof. No other VM, target or external repository was reset or cleaned.
Source/binary bindings and retained inputs/results are in
`batch-owner-checkpoint.json` and adjacent evidence.

Non-test growth owns the strict inventory/display split and the batch safety
checks; duplicate coordinator List validation is removed. Individual controls
still revalidate membership, so large-batch request/registry amplification needs
measurement before adding an owner-side batch operation. This checkpoint does
not qualify daemon/private mounts, stale native cleanup racing new admission,
or concurrent coordinator teardown/cancellation. `take_mount` removes a handle
before asynchronous cleanup; reservation/idle-shutdown ownership needs its own
controlled native concurrency probe before a verdict.

## Twenty-seventh checkpoint: exports cannot overwrite their source

Real RustFS-backed NFS and FUSE mounts reproduced source destruction with the
installed checkpoint-26 roles: exporting to the overlay upper directory returned
success while truncating 32 unpublished bytes to zero. The same regression fails
on macOS against the shared export function. The cause is source/destination
aliasing, consistent with Rust's documented
[`fs::copy` contract](https://doc.rust-lang.org/std/fs/fn.copy.html).

Both export entry points share destination admission before any overlay freeze
or output write. The destination must be a new directory outside the cache and,
when available, mounted view; its parent must already exist. Canonical parent
resolution rejects symlink aliases. `create_dir` rejects existing trees, avoiding
pre-existing child symlinks that could overwrite source bytes. This deliberately
replaces merging into an arbitrary existing directory. CLI help and mount docs
state the new rule. One private helper owns this policy for both entry points;
no protocol fields, dependencies or configuration were added.

Proof: 31 Linux publish tests, eight macOS export tests, strict VFS Clippy on both
platforms, and Make-installed native NFS/FUSE runs pass. Each installed backend
rejects the backing directory, cache descendant, mounted view, cache alias and
existing output while preserving source bytes. A normal export verifies regular
files, nested directories, an overlay rename and symlink target. Linux source
inputs match the pre-build manifest across 1,517 Rust sources/manifests. Local
`export-safety/checkpoint.json` binds inputs, binary hashes and before/after
results; raw evidence remains outside the PR.

This closes destination aliasing, not export's full generation/control lifetime.
Metadata-only base renames still read through the mounted view under the private
export freeze. Move materialization into the live owner with canonical read and
whole-file staging pins; qualify concurrent writes, refresh, retirement and
cancellation before treating live export ownership as complete. Optimized
performance, native macOS NFS and full stored pack-v1 retirement remain open.

## Twenty-eighth checkpoint: live export owns materialization

The controlled-view regression fails against the previous materializer: a
metadata-only rename copies changed mounted-path bytes rather than the engine's
captured base. The new regression covers a captured file, directory, symlink and
unchanged pre-existing read lease. It does not claim a native corruption race was
observed; the native baseline establishes the extra kernel reads described below.

`run_mount_export` routes to the live NFS/FUSE owner for the complete operation.
Both transports capture the actual overlay/cache and reject read-only state.
The shared runtime tracks export through caller cancellation, performs the
platform writeback step before mutation exclusion, and retains that exclusion
plus the process-spanning freeze through blocking materialization. Metadata-only
renames use direct engine leases, with bounded 1 MiB reads and a whole-file local
staging pin. FUSE descriptor opens share that pin selection unchanged. The old
mounted-view copier and its public entry point are removed.

Saved backing-file export remains a released CLI path, reachable in
[`v1.2.4` mount export](https://github.com/crabbuild/crab/blob/v1.2.4/crab/src/cmd/mount.rs#L3496).
It now acquires exclusive cache admission before inspecting/copying, excluding a
starting helper; metadata-only renames require an active engine. A known owner's
failure never selects saved state. This is local overlay recovery, not a stored
pack-format compatibility reader. New destination admission remains common to
both paths. No dependency, runtime setting or format fallback was added.

Proof on the final source:
- Linux: 645 VFS tests, all 16 dedicated native lifetime/mapped probes, strict
  combined-feature all-target VFS Clippy, and a FUSE-only check pass.
- macOS: 11 export tests, 13 control tests, strict NFS VFS Clippy and an NFS-only
  CLI library check pass. The CLI retains its 14 existing feature-slice warnings.
- Controlled origin admission proves a cancelled waiter cannot release export's
  mutation exclusion. A 3 MiB unpublished-pointer export works without an origin
  and leaves no inflight staging markers. Offline cache exclusion is tested.
- Make-installed NFS-only main/combined FUSE helper runs each export a 4 MiB
  remote pointer and distinct 4 MiB unpublished local pointer after metadata-only
  renames. Both rename records have no backing file; independent hashes, ordinary
  Git bytes, nested files, symlink targets and the deletion manifest match.
- Both native backends reject actual read-only owners despite stale registry
  flags. New clients against previous owners preserve the original unsupported
  operation error, leave no output and preserve source bytes. Destination/cache/
  symlink/existing-tree rejection probes pass through the new RPC path too.
- For the same warm 8 MiB NFS fixture, export's kernel READ count falls from 32
  to zero. These debug structural counts do not establish optimized throughput,
  tail latency or origin amplification.

The shared pin contract is `PointerReader::pin_file`/`StagedFilePin`, unchanged
below the new lease helper. Tokio 1.53.1's `Handle::block_on` runtime contract is
satisfied on an owned blocking worker while the runtime drives IO; shutdown waits
for the tracked control before shutting down hydration. Coordinator/runtime
mutexes are released before waiting for export. The extra production code is
mainly the two transport operations and the owned copy path replacing kernel
re-entry; there is no second reconstruction implementation.

Local `export-owner/checkpoint.json` binds 1,518 Rust sources/manifests, installed
binary hashes and retained probes. Raw evidence stays outside the PR. A slow
helper build was observed through the same live process/container handles and
completed without a restart. macOS still uses its existing non-waiting sync
request: native macOS NFS and a waiting barrier remain a separate required gate.
Full stored pack-v1 retirement and release performance qualification remain open.

## Twenty-ninth checkpoint: Capsule-only shard compaction

Source boundary: both CLI spellings call `run_compact_command`, then
`run_compact_with_cancel` in `crab/src/cmd/compact.rs`. It loads one required
Capsule root before maintenance admission and passes that snapshot to the read
view. Missing-root apply and dry-run no longer read or publish shard lists.
The previous implementation accepted the rootless fixture and performed 15 PUTs
plus one multipart upload; the regression now proves zero writes, zero multipart
uploads and no shard-list read.

Compaction selects only authenticated file identities. The locked Xet 1.6.0
`merge_shards` contract may move private inputs, so temporary directories and GC
admission remain owned until its blocking job finishes. Bloom trailers are
verified as part of the stored hash and stripped only from private native merge
inputs. Filtering rebuilds exact file/xorb dependencies, and replacement catalog
construction rejects missing or repeated files. Restoring the old empty-output
shortcut reproduced false success with two claimed files absent from all shards;
the regression now rejects corruption without changing the root.

Cancellation is checked before checkpoint publication. The owner awaits CAS
resolution and cleanup rather than dropping the publication future. A provider
hook cancelling immediately after root PUT reproduced `Cancelled` after a real
commit; the new regression preserves that known success after both fences have
released. Other release errors remain errors; ambiguous provider outcomes and
all sibling maintenance commands are not newly qualified by this change.

Tests live in flat `crab/src/cmd/compact_tests.rs`. Fourteen compaction tests and
the complete remote-helper staged-pointer/incremental/repack/remote-reuse case
pass on macOS. The latter requires checkpoint format 9 and real published
placement proofs; its default-stack dispatch shape matches the production
heap-pinned entry point. One FUSE-only CLI test now awaits the writeback API
instead of trying to unwrap its future. The background-registration test now
calls the same source normalizer as CLI admission and verifies that later source
disappearance cannot invalidate registry publication. Linux passes the same 14
compaction tests and remote-reuse case plus 103 selected mount tests; macOS passes
87 selected mount tests. NFS-only CLI library Clippy completes with 656 warnings
across the existing broad surface; no warning points at the changed compaction
module. Strict whole-CLI lint cleanliness is not claimed.

Make installs distinct NFS-only main/helper and combined FUSE-helper roles. A
real task-owned RustFS fixture publishes separate 4 MiB and 6 MiB files in two
pushes, yielding two source shards. The old binary accepts its rootless shard-list
fixture in dry-run. Both current compaction CLI spellings reject rootless apply
and dry-run without changing any provider key/ETag; both rooted dry-runs preserve
the root. Apply publishes checkpoint format 9 and retains its input shards.
Temporarily withholding only those two task-created inputs proves independent
fresh clones and cold-cache native NFS/FUSE mounted reads resolve the replacement
catalog and reproduce every byte by independent SHA-256. Inputs are restored
and re-read exactly afterward. This is compaction/read qualification, not a
production GC grace-period proof or a complete legacy-repository fixture.

Local `compact-v2/source-manifest.json` binds 1,530 Rust sources/manifests to the
Make-installed artifacts and retained provider/test logs. Sources remain byte-
identical through qualification. No dependency/lockfile changes or overrides;
raw evidence stays outside the PR. The implementation removes the alternate
publisher rather than adding a fallback or migration reader. Other pack-v1
owners, macOS waiting writeback and optimized data-plane performance remain open.

## Thirtieth checkpoint: Capsule-only Xorb optimization

The located vertical slice is `OptimizeCmd::Xorbs` → CLI `run` / `run_apply` →
executor → reconciliation → exact-view checkpoint publication. The configured
repository store builder already requires a v2 root. Previously apply opened the
SQLite journal before calling that builder, and resume opened it before looking
for an active run. Real RustFS baseline probes confirm both rootless operations
created a journal before rejection; dry-run already rejected without one.

The internal source enumerator still had a missing-root global inventory path,
and reconciliation still had a manifest publisher. Both are removed together.
Planning now consumes the root returned by authenticated store admission instead
of rereading mutable latest. Apply/resume establish that admission before journal
creation and retain the repository maintenance lease, both GC writer fences and
owned journal through completion. Execution and finalization require actual
store/layout references; the no-store path that marked rows done is removed.
Finalization requires the root even with no completed mappings, keeps only
selected catalog files, verifies replacement dependencies and publishes only the
Capsule checkpoint. A completed run retains success when fence release reports
late cancellation. Non-cancellation cleanup errors still propagate.

The removed manifest publisher was the sole caller of candidate shard-index
publication. Its newly orphaned result type and two functions are deleted from
`crab/src/cmd/metadb.rs`; shared rebuild/receipt helpers still used by other
located owners remain for their separate retirement. Local journal schema-v1
partial-page repair is the [released v1.2.4 local durability
contract](https://github.com/crabbuild/crab/blob/v1.2.4/crab/src/optimize/xorbs/journal.rs#L43),
separate from stored pack-v1 authority, and remains behind mandatory rooted
admission.

Tests are flat siblings `xorbs_tests.rs` and `reconcile_tests.rs`. Before removal,
the missing-root enumerator regression did not receive a root error and empty
reconciliation reported success. The narrowed enumerator now requires an admitted
root argument; its canonical empty-view regression checks no latest-root reread
or foreign inventory access. Reconciliation tests both empty/completed mappings
and cancellation immediately after the committed root PUT. All 70 optimization
unit/integration tests pass on macOS NFS and Linux NFS/FUSE. NFS-only CLI library
Clippy completes with 651 warnings across the broad surface; strict whole-CLI
lint cleanliness is not claimed. Formatting and the diff whitespace check pass.

Make installs the final NFS-only main/helper and combined FUSE-helper roles.
Real RustFS checks reject rootless dry-run/apply/resume without a journal or any
provider key/ETag mutation; offline profile/abort/confirmed-drop controls remain
local. The rooted plan selects exactly its two Xorbs. A task-only proxy forwards
the checkpoint PUT to RustFS, receives success, withholds the response, sends
SIGINT and waits for the CLI signal acknowledgement before delivering the PUT
response. The command exits successfully, with both source rows done, and the
format-9 checkpoint is visible.

Clearing only the task journal's completion timestamp models the post-commit
local completion window. Resume completes it without writing payload bytes or
changing the committed root. This is a simulated journal window, not a physical
crash/power-loss qualification. The first probe incorrectly retained a Python
SQLite connection and hit the expected exclusive-lock rejection. After closing
that harness-owned connection according to the [Python connection context-manager
contract](https://docs.python.org/3.11/library/sqlite3.html#how-to-use-the-connection-context-manager),
verification continued from the already committed
state; no root rollback or source change was used to make it pass.

With both original task-created Xorbs temporarily withheld, independent fresh
clones and cold-cache native NFS/FUSE mounts reproduce all 10 MiB by SHA-256 and
pass native Git fsck. Both source bodies/metadata are restored and reread exactly.
The repository has no legacy manifest, shard-list inventory or generation
receipt. Local `xorbs-v2/source-manifest.json` binds all 1,532 Rust source/manifest
files, unchanged through qualification, to the Make-installed artifacts and
retained logs. Raw evidence remains outside the PR. This debug run does not
qualify optimized throughput, tail latency or all mount modes.

## Thirty-first checkpoint: native macOS writeback and owner discovery

This checkpoint diagnoses two current-head failures; it does not repair them.
Make installs the NFS-only CLI/helper, combined-feature FUSE helper and cache
server from `dbf040fd024`. A retained manifest verifies 1,522 Rust source,
manifest and Makefile hashes remained unchanged through installation and probes.
The native client is macOS 26.5.2 / Darwin 25.5.0. Fixtures are ordinary local Git
repositories on the mounted Workspace volume; this is not RustFS or performance
qualification. Raw logs, scripts and artifact hashes remain outside the PR.

The registered-helper probe explicitly requires successful CLI live NFS status
before dirtying an 8 KiB file through a separate process's shared mapping. That
process holds its mapping without msync or close. The live `mount diff` returns
zero changes, and a read-only query of the actual helper overlay finds no backing
file. Darwin `fsync_volume_np(fd, SYNC_VOLUME_WAIT)` returns zero in about 14 us
without changing that result. Per-file fsync also returns without draining it.
A second process maps the same file read-only and calls libc `msync(MS_SYNC)`
without reading its pages; the backing file then contains the changed byte and
live diff reports the modification. After the client dirties a second byte,
client msync makes that byte visible too. A real no-push mounted commit preserves
all 8 KiB byte-for-byte in Git, Git fsck passes, and ordinary helper unmount
completes without forced cleanup.

The first external read-only mapping experiment accidentally used Python's
`mmap.flush`, which does not issue msync for an ACCESS_READ mapping. That invalid
negative was replaced with the explicit libc call above. Initial CLI parsing
also assumed one JSON shape for status and diff; the final probe follows their
actual distinct contracts. Those harness failures are retained, not product
regressions or evidence against msync.

Dependency proof explains why a volume-sync substitution is insufficient.
[Apple Libc's wrapper](https://github.com/apple-oss-distributions/Libc/blob/main/gen/sync_volume_np.c)
returns an error number directly and restores errno; a native invalid-fd probe
returns EBADF while errno remains zero. [Apple NFS's sync callback](https://github.com/apple-oss-distributions/NFS/blob/main/kext/nfs_vfsops.c)
skips nodes with no dirty NFS buffers, busy writers or an active flush. Its
[per-file fsync](https://github.com/apple-oss-distributions/NFS/blob/main/kext/nfs_vnops.c)
calls nfs_flush. [XNU's volume-sync owner](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_syscalls.c)
does not propagate the VFS sync result. These public upstream sources are not a
claim that every line matches this installed Darwin build; the native probe
establishes the observed failure on this host.

A separate current-head foreground probe proves native reads and a direct NFS
status response from the exact foreground PID. CLI `mount status --live-only`
still exits 6, and CLI diff selects the source's saved `.crab` directory. The
entry-point/owner trace is `run_mount_with_new_cli` → NFS mount/control server,
while `mount_control::nfs_context` requires a registry entry. Only
`spawn_nfs_background` registers the product mount. The foreground helper then
exits zero and detaches normally. No direct/offline commit was attempted.

Next repair foreground NFS discovery at the product ownership boundary, including
registry admission failure, startup races, owner-conditional removal and cache
lease lifetime. Do not add socket-guessing fallback. Check daemon and foreground
FUSE siblings separately: a live server alone does not prove CLI controls.
The macOS writeback repair additionally needs stable identities for client-read
files, rename/unlink/replacement coverage, error propagation and cancellation
ownership before the mutation gate. A repository-wide walk would undermine lazy
mounting; the targeted msync experiment is a mechanism probe, not proof of a
complete production design. Linux's qualified syncfs path is unchanged.

## Thirty-second checkpoint: serving-process NFS registration

The previous native probe found a mounted foreground NFS server whose direct
status socket worked while CLI discovery returned no owner. Registration now
belongs to `run_mount_with_new_cli`'s serving process, shared by foreground and
background-helper invocations. `NfsMountedSession::control_endpoint` supplies the
actual endpoint; generating another endpoint would change the Windows TCP token.
The launcher waits for the serving record and conditionally attaches log metadata.
It no longer creates or replaces that record. Registry cleanup and log attachment
use the same mountpoint/PID/start-time/endpoint identity under the Unix file lock;
a ref/view update does not replace the owner.

Registration failure cancels and drains the mounted session. On normal teardown,
the CLI removes only its own record after native detachment and engine drain,
while retaining cache admission. The launcher runs as an owned Tokio task,
observes cancellation and retains its child through startup/error cleanup. Unix
cleanup first sends SIGINT to its unreaped child and waits for cooperative exit.
The pre-existing eventual forced-termination policy remains after the grace
period; stuck cleanup is not qualified by these cooperative probes and remains
an explicit lifecycle gate. The [standard-library Child contract](https://doc.rust-lang.org/std/process/struct.Child.html)
requires explicit exit/reaping; dropping Child does not terminate it. Locked
Tokio 1.53.1 `runtime/task/join.rs` confirms that dropping a JoinHandle detaches
its task, so losing the startup waiter cannot drop its child handle early.

Proof on the unchanged Rust sources:

- Linux passes all 136 CLI mount tests; macOS NFS-only passes 120. Both pass all
  21 registry tests and strict all-target VFS Clippy. The six CLI owner tests
  cover canonical source preservation, remote source, waiting for registration,
  conditional log attachment, replacement rejection and graceful child reaping.
  Extracted tests are the flat sibling `mount_nfs_registration_tests.rs`.
- Make installs distinct NFS-only CLI/NFS helper and combined FUSE helper roles
  on both platforms. Native macOS NFS passes foreground RW, foreground RO and
  background RW with real Git bytes. Each live status identifies the serving
  PID; foreground appears in list, concurrent cache reuse is rejected, and
  write/fsync → diff → export → no-push commit preserves bytes. Read-only
  rejects native writes and overlay diff. Both remount modes retain the
  unpublished committed note; control unmount and SIGINT remove only the owned
  registry record without forced cleanup.
- Dedicated Linux repeats that matrix against the existing Capsule/RustFS
  repository, verifying its original 10 MiB by SHA-256 plus the new local note.
  A fresh independent eager clone verifies the original 10 MiB, has no local
  note and passes Git fsck. The no-push note remains local.
- A task-only native-mount wrapper captures the real Linux mount table before
  injecting registry admission failure. The CLI rejects startup, detaches and
  permits immediate reuse of the same cache. Another wrapper holds the native
  mount command after attachment; SIGINT to the launcher cancels startup, drains
  the helper and permits the same recovery. Neither uses forced cleanup.
- Initial Linux harness mountpoints were under a fixture volume containing a
  Git root, so the existing nesting guard correctly rejected them before mount.
  Moving mountpoints under `/mnt` makes the intended probes reachable without
  a product change or an allow-nested exception. NFS-only CLI library Clippy
  completes with 651 broad repository warnings, including pre-existing large
  futures in mount preparation; strict whole-CLI cleanliness is not claimed.

Local `nfs-owner/source-manifest.json` binds 1,523 Rust source/manifest/Makefile
hashes to the installed artifacts and retained evidence; raw evidence remains
outside the PR. No dependency, lockfile, serialized format or runtime setting is
added. Production LOC grows to own registration/error cleanup in the serving
process and startup child lifetime in the launcher, plus the atomic log update;
this replaces parent-only publication rather than adding discovery fallbacks.

Sibling boundaries remain explicit: daemon NFS persists a private endpoint and
uses its own control/runtime owner; coordinator FUSE uses coordinator inventory.
Neither calls the CLI registrar. Foreground FUSE still needs its own discovery
and control proof. This checkpoint does not close macOS mapped writeback,
stalled shutdown, daemon/private admission, full generation/GC retention,
stored pack-v1 retirement or optimized data-plane performance.

## Thirty-third checkpoint: owned NFS control teardown

The previous installed helper left a Unix socket pathname after successful
native unmount: its PID was dead and connect returned ECONNREFUSED. Regressions
against `a3614ba492a` reproduce three failures: ignored control bind failure,
socket cleanup skipped on cancellation, and detached blocking runtime snapshots.
The baseline overlays only test declarations and fixture visibility onto the
previous production sources; it exits with all three regressions failing.

`run_until_cancelled` now supervises the control owner and returns its failure
after teardown. It closes engine control admission, cancels future work, joins
refresh and control tasks, then proceeds to native unmount and read-resource
shutdown. Unix and authenticated TCP handlers share one implementation and an
owned JoinSet. Idle reads stop on cancellation; admitted dispatch and final
replies finish. A ten-second write/flush deadline prevents a non-reading client
from retaining a completed handler indefinitely. Control jobs and native Git
operations themselves still have no completion deadline.

Unix preflight and serving use the same per-endpoint exclusive lease. Preflight
cleans missing/refused stale sockets; other connect failures propagate. Serving
binds without unlinking. The lease survives accepted-handler drain and cleanup
of only the bound socket's device/inode. Its zero-byte `.sock.lock` file remains
on purpose: deleting it would let a replacement bypass an old open descriptor.
The locked fs4 0.13.1 source returns `Ok(false)` for contention; the owner maps
that result to WouldBlock. Locked Tokio 1.53.1 documents cancel-safe `join_next`
and aborts remaining tasks when a JoinSet drops, which requires explicit drain.
[Rust 1.97's Unix file owner](https://github.com/rust-lang/rust/blob/1.97.0/library/std/src/sys/fs/unix.rs)
combines custom open flags with O_CLOEXEC, so executed Git children do not
inherit the endpoint lease.

Proof on the final Rust sources:

- Dedicated Linux passes 654 VFS tests, all 16 ignored native FUSE/NFS probes,
  all 136 CLI mount tests and strict combined-feature all-target VFS Clippy.
  Native probes exercise writable/read-only mounts, descriptor lifetime,
  mapped transitions and publication against their in-memory origin.
- macOS passes 103 selected NFS tests and strict NFS all-target VFS Clippy.
  Eight new regressions cover the three reproduced failures, duplicate bind,
  replacement preservation, stable lease identity, TCP idle-handler drain and
  bounded stalled replies. Extracted tests use flat sibling files.
- Make-installed roles on both platforms repeat native foreground RW,
  foreground RO and background RW. Live status identifies the actual owner;
  write/fsync, diff, export, no-push commit, cache exclusion, RO rejection and
  retained unpublished bytes after remount pass. macOS reads 8 KiB plus a
  29-byte note; Linux verifies the existing 10 MiB Capsule/RustFS fixture plus
  that note. These are correctness probes, not optimized performance results.
- Each native case keeps an idle Unix control client open.
  Control unmount and SIGINT close that client, remove the owned socket,
  release the endpoint lease and clear the owned registry record without
  forced cleanup. A separate OS lock attempt confirms exclusivity while live
  and availability after teardown. The new doctor removes the known stale
  socket from the previous native checkpoint after dead-PID/refused-connect
  checks; the persistent lease remains.

Local `nfs-control/source-manifest.json` and installed-artifact hashes bind
these probes to 1,525 final Rust/manifest/Makefile sources; raw proof stays outside
the PR. No dependencies, lockfiles, wire formats or runtime settings change.
Production LOC grows for endpoint exclusion, connection ownership and mount
supervision while deleting the duplicate Unix/TCP handler loops. No second
control or discovery path is added.

Daemon NFS calls the same mount/control owner, but its private discovery and
stalled teardown still need native qualification. Coordinator FUSE has a
different lock boundary and detached IPC handlers; blindly joining those under
its coordinator lock could deadlock. Its lifecycle remains explicit follow-up.
The locked nfs3_server 0.11.0 data listener separately spawns detached connection,
message and transaction-cleaner tasks. Joining the listener does not join those
children. Busy native-unmount failure also still stops the data listener; normal
process-exit probes do not prove safe in-process ownership in that failure path.
This checkpoint closes control-plane ownership, not those data-plane gates or
the macOS dirty-mapping writeback failure.

CI at the preceding `a3614ba492a` head passes NFS evidence and large-repository
RustFS qualification. Its architecture job rejects two dependency-policy edges
and two replication layout sites; the partial-clone workflow fails fresh mirror
refs and compatibility smoke; gateway qualification fails its sustained-write
throughput/report gate. These are open PR gates, not attributed to an unrelated
base without reproduction. No inventory, threshold or expected-failure file is
changed to bypass them. The draft is not ready to merge.

## Thirty-fourth checkpoint: reproduced NFS data-owner failures

The preceding control repair does not establish data-plane teardown. A temporary
diagnostic module at `55647f5f771` uses the real nfs3_server TCP listener and
answers an NFS NULL RPC. After `run_until_cancelled` returns and joins its
listener, the accepted idle connection remains open and retains the engine.
Closing that client releases the engine. The diagnostic module declaration was
removed after capture; no expected-failure test or dependency override is added.

A Make-installed Linux helper mounts the existing Capsule/RustFS fixture
read-only. A separate process holds its current working directory in the mount.
SIGINT makes ordinary unmount fail as busy; the helper exits with code 9 while
the kernel mount remains attached and the cache lease is already available.
Releasing the client does not detach the filesystem. The diagnostic needs lazy
detachment of only its disposable read-only task mount afterward; that is failed
product cleanup, not a passing teardown result. Retained proof lives under local
`nfs-data/` and `linux/nfs-data/baseline/` outside the PR.

The dependency trace is listener → accepted connection → message parser → RPC
task. Locked nfs3_server 0.11.0 detaches all three task layers and exposes no
public child-task drain. Its transaction cleaner also uses a detached task and
Notify waiter. Blocking-RPC/disconnect behavior still needs a dedicated probe;
idle retention and busy native unmount are now reproduced, not source leads.

A busy-unmount retry alone is insufficient. Pipeline and daemon production
owners give hydration their shutdown-request token; cold pointer reads reject
that token after cancellation. Installed data service lifetime must instead
extend through native detachment, with retained control discovery and cache
admission, followed by complete RPC/engine drain. FUSE, daemon and CLI callers
must preserve the same ownership ordering.

A four-file dependency patch is prepared outside the checkout for review only.
It proposes a graceful listener API, owned connections/parser/RPC tasks and
listener-owned transaction cleanup, without changing the NFS wire format or
adding dependencies. It is rustfmt parsed but unapplied and uncompiled.
Root `AGENTS.md` requires explicit approval for dependency patches/overrides/vendor
changes. The user approved this scoped nfs3_server 0.11.0 patch, vendoring with
its license and the corresponding Cargo manifest/lockfile changes. No other
dependency override is approved. It remains unapplied at this checkpoint;
implementation and full native regression qualification follow the LFS change.

## Thirty-fifth checkpoint: Capsule-only LFS pre-push history

`run_lfs_pre_push_batch` resolves the primary store before collecting published
ref tips. Its resolver already requires a Capsule root through
`StoreResolver::write_store` → `build_repository_url_store_with_root` →
`open_repository_root`. The later ref read nevertheless fell back to a manifest,
or an empty exclusion set, if its root was missing. That could reinterpret
storage authority after admission. Current `origin/main` at `cc2e4e25f46` reads
only manifest tips; the previous crab-v2 implementation prefers Capsule but
retains this fallback.

The pre-push reader now requires a root and reads its transaction-consistent
visible refs through `crab_read`. Missing/corrupt roots propagate before payload
transfers. The manifest reader, empty-root substitute and redundant CLI storage
adapters are removed. Standalone push and object-ID push already use the same
rooted primary admission; managed LFS writes remain rejected by their existing
protected-push contract. Fresh remotes still use canonical Capsule initialization.

Two regressions fail against the previous implementation: an empty missing-root
context and a canonically initialized pack-v1 repository both returned success.
The latter uses the real legacy initializer, not an incomplete shard-list-only
fixture. The new behavior passes both, while empty valid roots and corrupt-root
errors retain their canonical behavior. All 25 LFS push tests and five repository
admission tests pass on macOS NFS-only and dedicated Linux NFS/FUSE. Tests move
to flat siblings `push_tests.rs` and `push_all_tests.rs`.

Make-installed Linux roles initialize a new RustFS prefix, then real Git pushes
run the installed LFS pre-push hook for first and incremental publication. Each
hook reports uploading its one new 64 KiB payload. Visible Capsule refs match
both commits. A fresh eager clone reproduces the current bytes; fetch-all then
verifies both historical LFS payload hashes, and strict Git fsck passes. The
initial harness used `--skip-repo`, which skipped hook installation: its successful
Git push/clone was insufficient proof of this caller. That run is retained
separately, and the corrected run asserts actual hook upload output.

Local `lfs-capsule/source-manifest.json` binds 1,525 Rust source/manifest/Makefile
hashes to these probes and the actual installed VM artifact hashes. macOS proof
is compiled CLI unit coverage; no new native macOS LFS or optimized performance
claim is made. Production LOC shrinks. No dependencies, lockfiles, protocol
encodings or runtime settings change in this checkpoint. The approved NFS
dependency work remains separate. Stored pack-v1 retirement is still incomplete
at protected receive, gateway, legacy initialization and maintenance/publication.

## Thirty-sixth checkpoint: graceful NFS data ownership

The approved nfs3_server 0.11.0 patch is now integrated through one graceful
serving path. The listener owns accepted connections; each owns its parser and
admitted RPC JoinSet. Listener-owned transaction cleanup replaces the detached
cleaner. After native detachment, the mount owner signals transport shutdown and
awaits all descendants rather than aborting only the listener. Incomplete input
and blocked replies stop; admitted filesystem operations finish even after their
client disconnects. RFC 9293 section 3.6.1 permits TCP reset when unread partial
bytes remain; complete idle RPC exchanges must still close with EOF.

Ordinary native unmount now runs in an awaited blocking task and retries while
busy. Cache admission, the data listener and control status remain owned until
the kernel client detaches. Engine control admission closes and admitted controls
drain while WRITE callbacks still work. Requesting shutdown no longer closes the
control transport immediately; repeated shutdown/status reach the same owner.
Pipeline and daemon hydration use an independent data lifetime, so request
cancellation cannot reject a busy client's cold pointer read. Background shutdown
still uses the existing explicit service drain contract. Already-cancelled
pipeline preparation returns before clone/startup, with a final check before
worker admission.

MOUNT/UMOUNT handlers await sends on a bounded notification channel. Teardown
closes its receiver before joining RPCs; mount failure drops it too. Otherwise a
second admitted notification could wait forever after the lifecycle loop exits.
Synthetic servers in ownership tests now stop cooperatively through their token,
rather than depending on the production abort being tested.

A late-write regression against the candidate's original pre-unmount journal
flush returns with one unstable write still pending. Its owned callback finishes
a real NFS adapter/overlay write only after listener shutdown starts. The final
flush now follows every RPC and control join, before engine shutdown. The test
requires a cleared journal, successful local sync and exact retained bytes.

The complete locked package, BSD-3-Clause license and provenance are retained at
`vendor/nfs3_server`. All retained upstream files match the registry package except
four patched Rust modules and README trailing-whitespace normalization. Cargo workspace membership stays unchanged. Cargo.lock only
removes this package's registry source/checksum; versions and dependency edges do
not change. The user explicitly approved this dependency patch and vendor scope;
no Cellar/SQLite or other override is authorized.

Final-source qualification:

- Dedicated Linux passes 663 VFS tests and all 16 native FUSE/NFS probes;
  macOS passes 139 selected NFS/control/pipeline/hydration tests. Strict all-target
  VFS Clippy passes both feature/platform slices; all 136 Linux CLI mount tests
  also pass. Linux minimal and FUSE-only
  checks and the dependency's test-reexport feature through crab-vfs also pass.
- Make-installed roles on each platform pass foreground RW/RO and background RW
  NFS flows: live owner identity, cache exclusion, write/fsync, diff/export,
  no-push commit, retained bytes on remount, RO rejection, idle-control EOF,
  socket removal, endpoint-lease release and registry cleanup. Linux verifies
  the existing 10 MiB Capsule/RustFS fixture; macOS normal flows use real local
  Git bytes. No forced cleanup is used.
- A separate native busy-client process retains its mount cwd. On both Linux
  and macOS, SIGINT/RO and CLI-control/RW shutdown retain the serving owner,
  kernel mount, cache exclusion and live status. A new VFS read after shutdown
  returns the exact 6 MiB RustFS pointer; counters prove new hydration requests
  and window misses. Lower cache warmth is not origin-performance proof.
  RW additionally completes native write/fsync after shutdown was requested.
  Releasing the client produces ordinary detachment, owner exit zero, cache
  availability and clean registry; the control CLI also exits zero. All four
  cases require no emergency/forced cleanup.
- All five Docker stages that explicitly copy workspace Cargo inputs now include
  this vendor directory, and the root context whitelist includes only that new
  vendor subtree. An ordinary directory-context build with the old ignore rules
  fails to find it; the corrected source context builds and offline Cargo
  dependency resolution selects `/build/vendor/nfs3_server`. Eleven auth-helper
  packaging checks pass. Full service-image/runtime proof remains CI-owned.
  The release builders copy the whole context; the unpublished VFS crate is
  absent from the SDK's published package closure.

Local `nfs-data/source-manifest.json` binds 1,556 Rust/manifest/build-context
inputs, with actual installed binary hashes on both platforms; raw evidence
stays outside the PR. The initial Linux install exhausted the VM root disk.
Only this task's build/artifact directory was moved to the VM's existing data
filesystem, preserving its paths and contents; final installs and native checks
then pass. No unrelated VM, target or repository was cleaned. Production code
adds explicit ownership rather than a second serving path; the complete vendored
package accounts for most of the LOC increase, while the upstream delta is four
modules. Test additions use flat siblings.

The separate startup launcher still eventually force-kills after ten seconds;
this shared-owner repair does not qualify that path or daemon/private modes.
Mount-presence query failures still collapse to a boolean in the inherited probe;
error-aware ownership decisions need separate reproduction and repair. macOS
mapped-write draining, complete generation/remote-GC retention, stored pack-v1
retirement, Windows locks and optimized total-resource/performance gates remain.

## Thirty-seventh checkpoint: cancelled NFS startup retains its child

The prior installed launcher has three reproduced failure modes. A helper still
serving a busy native mount is killed after the ten-second startup-cleanup grace
period, leaving the kernel mount attached with cache admission released. A
terminal process-group interrupt reaches both launcher and helper; forwarding
then becomes the helper's second signal and forces its exit. Finally, a delayed
successful readiness reply admits the helper after launcher cancellation and
returns success. The actual Linux/RustFS reproductions retain their failed
results separately from diagnostic cleanup; none counts as successful teardown.
A real child regression also fails at 10.02 seconds before this repair.

`crab/src/cmd/mount.rs` keeps one owned launch/cleanup path. Pre-cancelled startup
never resolves or spawns a helper. Cancellation now covers the entire awaited
readiness exchange, with cancellation checked first when both branches are
ready. Unix helpers get their own process group, so the launcher forwards one
shutdown signal. Failed startup warns after ten seconds and continues waiting
for the retained child to exit and be reaped; elapsed time cannot authorize
termination or release its cache ownership. Non-Unix cleanup retries the
PID-checked control request until shutdown is acknowledged, then waits for exit;
Windows native qualification remains open.

The Rust 1.97 standard-library contracts were read directly: dropping `Child`
neither kills nor waits, `kill` is SIGKILL on Unix, `try_wait` reaps an exited Unix
child, and `CommandExt::process_group(0)` assigns the child's own process group.
Current `origin/main` instead inherits the group and kills after its sixty-second
admission timeout; the ten-second graceful-then-kill behavior belongs to this
branch's previous checkpoint. FUSE background startup follows coordinator
standalone preparation before Tokio/signal initialization and calls `setsid`
during daemonization, so it does not share this NFS helper signal path. Its
other cleanup and foreground-discovery gates remain separate.

Proof on Make-installed binaries:

- Dedicated Linux: all 138 CLI mount tests pass with NFS/FUSE enabled. macOS:
  all eight NFS registration/startup tests pass, including the real child held
  beyond ten seconds. The new tests remain in the existing flat sibling file.
- Native NFS on both Linux and macOS: individually signal the launcher, signal
  its owned process group, and deliver a real successful readiness reply after
  cancellation. Each case holds another process's cwd in the mount for twelve
  seconds. Launcher and helper remain alive, cache admission stays exclusive,
  live control responds, and a new read returns exact 6 MiB Capsule/RustFS bytes.
  Releasing the client gives ordinary detach, cancelled launcher exit, child
  completion/reaping, free cache, clean registry and removed control socket.
  All six corrected cases require no emergency cleanup.
- The Linux late reply finds its peer closed, as does macOS; neither can admit
  the cancelled helper. Both helpers' process groups equal their own PIDs.
- Normal foreground RW/RO and background RW installed-role flows are repeated
  separately; this fault-injection proof does not replace normal operation.

Raw harnesses, source/artifact hashes and results remain in external
`nfs-launcher` evidence. The macOS harness initially raced a transient `ps` child
and assumed its log directory contained only one file; both harness errors were
corrected before counting results. No runtime test knob, new setting, dependency
change or force-detach fallback was added. Production LOC grows modestly to make
cancellation and retained ownership explicit; the old hard-kill path is removed.
The stale README description of the already-repaired detached dependency owner
is removed, and the user guide explains waiting for busy clients during startup
cleanup. This closes the reproduced serving-stage launcher failures, not every
early-preparation interruption or uncertain mount-presence query.

## Thirty-eighth checkpoint: Capsule-only S3 gateway authority

The gateway's explicit initializer already creates Capsule roots, but its reads,
publication, ref revalidation, completion receipts, listing acceleration,
attribute checkpoint ordering and both maintenance schedulers still selected
stored pack-v1 authority when the root was absent. A new regression on the
previous source accepts both read and write against a valid manifest-only
repository. Its failing baseline is retained; the first compile attempt exposed
an incorrect test-fixture `Store::get` call, corrected to the actual
`get_with_etag` API before recording that baseline.

The gateway now requires the canonical root for Git reads and publication.
The protocol/artifact enums, manifest/journal publisher, fallback reader,
periodic v1 catalog scheduler, its unused owner fields and legacy-specific
maintenance tests are removed. Read views keep only the authenticated Capsule
state digest and a definite frontier map. Publication retains the same ref
lease, global/repository GC fences, expected-parent retry, Capsule intent/receipt
and confirmed-publication boundary. Completion recovery resolves Capsule
receipts, so an overwritten completed upload still recovers its historical
commit without publishing again. Attribute/listing paths no longer consult a
v1 manifest. Idle maintenance calls the shared root-pinned Capsule owner
directly; foreground count/byte admission remains canonical.

All positive gateway fixtures now initialize Capsule authority. The two real
v1 initialization calls left in its repository tests construct negative cases;
the receipt-negative assertion proves no v1 receipt is created. S3 attribute
schema versions, S3 SigV2/ListObjects wire APIs, Git pack/index formats and Git
LFS `spec/v1` are separate contracts, not stored Crab pack-v1 authority.
The shared exported legacy initializer and CLI/receive/metadb dispatch still
need removal; this is gateway retirement, not complete protocol retirement.

Proof:

- Dedicated Linux: all 203 gateway library tests and the binary logging/filter
  test pass; strict all-target Clippy and an executable build pass. Existing
  lease-loss, concurrent writers, batching/history, conditions, cross-pack
  deltas, native roll-up, frozen multipart recovery and attribute tests run on
  the canonical fixtures. A strengthened assertion initially forgot the
  coordinator's source-preserving `Batch(Arc<Error>)` wrapper; the final assertion
  checks the actual nested missing-root error. No source error was flattened.
- Real RustFS, unchanged AWS CLI 2.17.14 with signed requests: explicit and
  repeated initialization; exact ordinary and 6 MiB LFS GET, range GET, HEAD,
  COPY, complete listing, two-part upload, completion and exact readback. An
  overwrite followed by identical completion retry preserves the newer bytes.
  The listing harness initially omitted the generated `.gitattributes` file;
  corrected expectations preserve the intended Git LFS tracking contract.
- A valid v1-only remote fixture uses the real unchanged metadata constructors
  for its manifest and layout. Explicit initialization refuses it; signed GET,
  PUT and listing fail, readiness reports 503, and both original objects remain
  byte-identical. Rejected PUT still enters the existing GC coordination
  boundary: its persisted state/clock remain, with no writer, sweep or quarantine
  holders after release. A repeated harness assertion expecting only two keys
  was invalid after that probe; original data identity and fence release are
  checked directly instead. Current `origin/main` still has manifest authority; the
  checkpoint removes the gateway's optional-root switch rather than changing
  S3 request/auth defaults.
- The serving gateway handles one SIGINT and exits zero after drain. The running
  legacy-rejection process and final build hash to the same executable. Raw
  evidence, source/build inputs and actual executable hashes stay external.

The native cross-interface probe also found two existing VFS gaps. Linux and
macOS NFS expose the exact 132-byte committed Git LFS pointer for the gateway's
6 MiB object; S3 returns the correct full object. Ordinary file, copy and later
multipart-overwrite bytes match. Snapshot/late-blob classification recognizes
only Crab pointers, confirming the missing LFS hydration path. This is not
stale Capsule state and must not be disguised as successful full-content
interoperability. The original expected-byte failures are retained.
Linux foreground FUSE also reads those ordinary/pointer bytes, but the first
SIGINT leaves its native mount attached for thirty seconds. The external
harness mistakenly sends another SIGINT during failed cleanup; the existing
second-signal handler forces exit. That diagnostic is a failure, not native
teardown proof. Fix the harness to avoid duplicate cancellation and repair
foreground FUSE owner cancellation next; then add canonical VFS LFS hydration
with range/integrity/COW/generation and resource qualification.

This checkpoint deletes substantially more non-test code than it adds. No new
settings, dependency changes, wire aliases, compatibility readers or test-file
layout patterns are introduced. Documentation follows the single Capsule path;
raw benchmark files remain excluded. All-mode correctness and performance
signoff remain open.

## Checkpoint 39 — FUSE PID and native detach ownership

The installed core CLI intentionally omits FUSE. Its blocking child `.status()`
kept PID-directed cancellation in the wrapper; the actual serving helper stayed
mounted. A direct helper also received cancellation without completing native
detachment: fuser 0.17's pure AutoUnmount path closes its socket and returns,
which is not completion proof. Both baselines are retained; an ordinary native
unmount released each diagnostic without a second shutdown signal.

Unix delegation now replaces the process before cache/mount admission. The
caller's PID becomes the serving helper. Shared FUSE cleanup retains its session,
engine and caller-held cache admission while ordinary unmount is busy or native
state is unknown, then joins callbacks only after confirmed detachment. The old
timeout, force/lazy fallback and detached join thread are removed. Occupied
mount targets are refused rather than force-cleared. Daemon sessions carry their
canonical target through failed installation and teardown, eliminating the
optional-runtime-path bypass. Managed async-worker starvation/cancellation and
synchronous Drop engine drain remain explicit qualification gates.

NFS and FUSE now share a fallible exact-byte native mount-table query. Linux
splits kernel fields at ASCII spaces and decodes octal escapes without lossy
UTF-8 conversion; vertical-tab/form-feed bytes remain part of the path. A real
bind-mount table probe confirms that distinction. macOS uses bounded
`getfsstat(MNT_NOWAIT)` snapshots with checked buffer lengths; query errors or
repeated table truncation remain errors. NFS startup, control and teardown no
longer interpret a failed query as an absent mount. Windows query failures also
propagate; native Windows proof remains open.

A real busy-client regression fails with the old cleanup at 12.78 seconds,
when it disconnects/releases the owner. Final rebased sources pass all 662 VFS
unit tests, all 16 native NFS/FUSE tests, all 138 CLI mount tests, strict combined
VFS Clippy and minimal/FUSE-only checks. macOS passes strict NFS Clippy and
112 selected daemon/NFS/control/pipeline/hydration tests. Rebase onto
`877b832b396` brings the gateway tree-append optimization; all 208 gateway tests
and strict all-target Clippy pass after integration.

Make-installed final binaries pass twelve real Linux FUSE cases: PID SIGINT and
SIGTERM, process-group interrupt, external ordinary detach, busy read-only and
writable clients, busy group/TERM cancellation, unusual path bytes, an occupied
target, native query failure, and direct helper invocation. PID identity is the
actual helper; admitted and running executable hashes match. Busy cases retain
the mount/cache for twelve seconds, read exact 6 MiB after cancellation and, for
RW, complete write/fsync. Client release gives ordinary detach, callback join,
exit zero and cache availability, without force or a second signal. Hiding only
the container's `/proc` proves query errors retain serving/cache ownership; NFS
passes the same fault and completes after the table is restored. macOS mount
doctor recognizes the already-mounted APFS root without trying to mount it.

Both platforms repeat normal native NFS foreground RW/RO and helper RW flows on
fresh local Git fixtures: cache exclusion, write/fsync, diff/export/no-push
commit, remount, read-only refusal, idle-control EOF and socket/lease/registry
cleanup. Separate real Capsule/RustFS busy RO/SIGINT and RW/control cases retain
data/status/cache, deliver exact 6 MiB through new VFS hydration after shutdown
request, and finish with normal detach and exit zero. RW also finishes a new
write/fsync. Installed artifact hashes remain unchanged through qualification.

The native mapped-inspection harness initially paused writeback before its mmap
prefault completed, deadlocking readiness. Kernel stacks identify that test
ordering fault; prefault/setup now completes before arming the pause, retaining
the later dirty-write/controller invariant. The failed diagnostic and exact
task-PID termination are retained, not counted as successful cleanup. An early
external harness used a nonexistent fixture filename; the corrected probe reads
the fixture's actual 4 MiB file. Reused remote overlays correctly preserved notes
from earlier busy-write tests, violating the normal script's clean-fixture
precondition. Those failures are retained; normal flows use fresh local inputs,
while separate Capsule probes verify remote hydration. No persisted notes were
discarded or assertions weakened to hide that state.

The best bounded repair uses one fallible mount table, one native detach/join
path and direct PID ownership, deleting the timer/force/detached-wait alternatives.
Managed async-worker starvation/cancellation, synchronous Drop engine drain,
identity-bound replacement-mount races, foreground FUSE control/discovery,
macOS FUSE, native Windows and optimized performance remain explicit gates.
The upstream fuser auto-helper reaping behavior is also an investigation lead;
no additional dependency override is authorized.

Evidence binds 1,558 source/manifest/build inputs and both installed role sets;
all inputs remain unchanged through final qualification. The rebase replayed
historical tracked audit additions and their later exclusion, removing the local
ignored copies. All 2,156 files were restored from the original Git tree and
verified against their blob IDs; zero are tracked or in the PR diff. Raw evidence
stays external and added tests keep the flat sibling layout. This checkpoint
removes roughly fifty net Rust lines, including its new shared query module and
regressions; no dependency, lockfile, setting or threshold changes are added.

## Checkpoint 40 — managed FUSE worker handoff

The next native probe reproduces a separate managed-owner availability failure.
Five independent read-only mounts share one four-worker coordinator. One busy
unmount leaves control responsive at roughly 0.4 ms; four concurrent busy
unmounts stall an unrelated control request beyond its two-second observation
window. Releasing clients immediately restores control. Native thread snapshots
show all four runtime workers in the synchronous unmount retry sleep, proving
worker starvation rather than a coordinator mutex or cache/endpoint collision.
All diagnostic mounts detach normally; no forced cleanup is used.

The shared native FUSE cleanup now uses Tokio `block_in_place`, handing off
runtime work while preserving session/cache ownership on the caller's stack.
Tokio 1.53.1's pinned source documents that this scope cannot be cancelled and
that unrelated tasks can continue. FUSE admission requires a multi-thread
runtime before native setup; all product foreground/coordinator/daemon builders
already supply one. Current-thread runtimes are rejected rather than reaching
Tokio's unsupported blocking context. The cleanup helper is crate-private; all
its callers and synchronous Drop siblings use the same path. No detached task,
new queue, compatibility mode or runtime setting is introduced. Cancellation
during later asynchronous engine drain remains a separate gate.

The native regression previously hid this path behind its own `spawn_blocking`.
It now calls the same synchronous boundary as managed owners. Its real FUSE
read-only case uses one runtime worker and an independent timer task; an OS-side
observer always releases the client after twelve seconds, so starvation fails
without deadlocking cleanup. The regression fails on checkpoint-39 source at
12.30 seconds with `busy FUSE teardown starved the runtime`, then passes after
the repair. The retained busy-owner assertion still covers premature detach.

Final-source proof:

- Linux: 662 VFS unit tests, all 16 native tests, all 138 CLI mount tests, strict
  combined VFS Clippy and minimal/FUSE-only checks pass. The modified read-only
  case proves independent progress even with a sole runtime worker.
- macOS: strict all-target VFS Clippy with both backends, 112 selected NFS/daemon/
  control/pipeline/hydration tests and Make install pass. Native FUSE remains
  unavailable on this host; native NFS normal foreground RW/RO/helper RW and real
  Capsule busy RO/SIGINT and RW/control are repeated on its installed roles.
- The real Linux five-mount scenario now returns the unrelated control response
  in about 0.48 ms while all four clients remain busy and cache admission remains
  held. Closing the four unmount request sockets preserves native/cache owners;
  releasing the clients completes detach and engine/cache release. The unrelated
  mount still serves exact bytes, and all task mounts then detach normally.
- The twelve foreground FUSE cases, Linux NFS mount-query failure and both-platform
  normal/busy NFS cases are repeated against the new Make-installed roles. Running
  foreground and installed executable hashes agree, and installed hashes remain
  unchanged through proof. These are debug qualification observations, not an
  optimized throughput or tail-latency benchmark.

Only two Rust source inputs change from checkpoint 39, out of 1,558 captured
source/build inputs. Net production Rust grows fourteen lines to make the
ownership-preserving worker handoff and runtime admission explicit; the enhanced
regression replaces the masking test adapter. No dependency/lockfile/settings
changes are made, and the 2,156 excluded raw audit files remain preserved.
Target identity, asynchronous engine-drain cancellation, synchronous Drop drain,
macOS mapped writes and the other all-mode/resource gates remain open.

## Checkpoint 41 — canonical LFS payload reads

The failing native regression establishes the user-visible gap on both Linux
adapters: stat returns 132 pointer bytes instead of the declared 6 MiB payload.
Current `origin/main` (`cc2e4e25f46`) and `origin/crab-v2` (`877b832b396`) both
classify Crab pointers only in snapshot/late-blob paths. Git LFS is a current
content type; its pointer version does not select Crab pack-v1 storage.

VFS snapshots and late classification now share typed Crab/LFS content identity.
The read-window key includes the hash family, preventing identical 32-byte
addresses from crossing SHA-256/BLAKE3 authority. Snapshot serialization retains
pointer text and reconstructs the variant through the canonical classifier.
Existing Git source OIDs remain the lineage authority; pointer hashes are no
longer invented as substitute Git OIDs in fixture-only missing-OID nodes.

`crab-read::PointerReader` accepts resolved local LFS storage. It selects the
captured remote runtime only when opening the local payload reports absence.
Local corruption, malformed configuration and I/O failures remain errors.
`crab-lfs::local` verifies exact size and full SHA-256 in one bounded-memory pass,
retaining only the requested range. Its worker owns the existing process-wide
verification permit and registers with the shared read session. Remote LFS
uses canonical verified streams and existing reconstruction buffer/download
admission; EOF verification precedes immutable window insertion. LFS COW streams
those same windows into owned backing storage. No second cloud object reader,
new setting, dependency version, or additional override is introduced.

The first repaired FUSE run exposed an actual read-path deadlock: spawning Git
configuration resolution from the callback left its child waiting on FUSE while
the read owner waited for the child. Configuration is now resolved during
preparation/capture, outside callbacks. The deadlocked diagnostic required an
exact-child SIGKILL and abort of its identified task FUSE connection; it remains
a failed run and is excluded from normal-cleanup claims. The final NFS/FUSE
regressions pass size, tail/full SHA-256, COW write/fsync and held-descriptor
rename/unlink checks with ordinary teardown. LFS extensions are explicitly
unsupported because their stored hash/size describe transformed bytes requiring
smudge; exposing those bytes as normal file contents would be incorrect.

Evidence map:

| Surface | Caller / owner | Callee / dependency contract | Proof |
| --- | --- | --- | --- |
| Snapshot and late blob | Pipeline/daemon and engine attributes/open | `FilePointer`, canonical `crab_git::classify`, snapshot encoding | Real Git native stat plus LFS serialization/extension tests; Crab regressions unchanged |
| Canonical range | Both native adapters → engine lease → HydrationService | `PointerReader` → local verified range or captured ShardHydrator → `LfsObjectStore::get_stream_with_session` | Native exact ranges/COW, 64 LFS tests and 237 read tests |
| Integrity/cache | Shared HydrationState across captured readers | Typed cache identity; full stream EOF and immutable cache ownership | Warm LFS followed by a Crab pointer with identical hash bytes must fail; corrupt local source cannot select valid remote; same-length/out-of-range corruption and size mismatches fail |
| Storage settings | Pipeline and daemon preparation; runtime capture sibling | Git config includes, existing environment precedence, common Git directory | Real included config and linked-worktree fixture; reads spawn no Git |
| Cleanup | Engine/hydration shutdown | Shared LfsReadSession tracks blocking SHA jobs and drains them before cache release | Existing detached-verification/session test; all native owners detach normally |
| Publication | Overlay publisher → Git add | Configured Git filter driver | New native diagnostic fails without LFS clean configuration; follow-up required |

Final-source proof:

- Dedicated Linux: 64 LFS, 237 read, 666 VFS and 138 CLI mount tests pass.
  All 18 native adapter tests pass. Strict all-target Clippy covers all three
  changed libraries with local staging and both backends; minimal and FUSE-only
  checks pass. macOS passes the same strict library Clippy and 85 selected
  snapshot/late-classification/hydration/pipeline/runtime tests.
- Fresh Make-installed Linux NFS/FUSE and macOS NFS pass the original real
  Capsule/RustFS gateway fixture: exact ordinary and 6 MiB LFS hashes, copy and
  multipart-overwrite bytes, read-only refusal, exit zero and kernel detachment.
  No forced cleanup is used in these final probes.
- Both platforms repeat native NFS foreground RW/RO and helper RW flows using
  fresh local Git inputs: write/fsync, diff/export/no-push commit, remount,
  cache exclusion, idle-control closure, registry/socket/lease cleanup.
- Evidence binds 1,558 source/build inputs and both installed role sets, unchanged
  through final qualification. Only an existing workspace `crab-lfs` dependency
  edge is added to VFS; Cargo.lock has no version/source/override changes.
  New production surface supplies typed identity, local verification and canonical
  read admission; fixture conversions and regressions account for remaining
  growth. Tests remain inline or flat siblings; all 2,156 raw audit files stay
  preserved and excluded from the PR.

The next native publication probe is an explicit failure, not an accepted
baseline: a fresh mount clone without an LFS clean driver preserves COW/commit
adoption bytes but commits a 6 MiB Git blob instead of an LFS pointer. The probe
does not push and detaches normally. Fix driver/publication ownership and prove
no-push/remount/later-push/independent clone before claiming writable LFS support.
Cold local windows rehash the whole payload; remote partial reads may do the
same without a validator receipt. Bounded verification reuse, large-file origin
amplification, LFS generation changes and payload retirement remain required.
The full PR and all-mode performance goal remain unqualified.

## Checkpoint 42 — shared LFS local writer and mount publication

The native follow-up from checkpoint-41 and a new unit regression both fail
before the repair: overlay publication commits full LFS payloads as Git blobs
when the mount-owned clone has no configured clean driver. The unit observes a
65,536-byte blob rather than a parseable pointer; the real NFS probe observes
6 MiB. [Git's filter contract](https://git-scm.com/docs/gitattributes) explicitly
treats a missing optional driver as passthrough, so configuring more retries or
trusting `git add` cannot establish pointer publication.

The existing CLI atomic local writer, staged object and file verifier now live
in `crab-lfs::local`. CLI clean/filter-process, extension transforms, migration,
cache installation and mounted publication call that same implementation; the
old CLI implementation is deleted without a re-export/shim. Its errors remain
typed through VFS and CLI conversions. The existing `tempfile` workspace
reference moves from dev to production dependencies; Cargo.lock is unchanged,
with no additional version, patch or vendor override.

Publication queries Git's cached attributes for both Crab and LFS candidates,
including nested rules and overlay policy edits. Both pointer types skip the
redundant publish-worktree payload copy and external filter invocation. LFS
streams backing bytes through the canonical writer, installs the verified local
object first, then writes the small pointer to the prepared Git index. The
normal push pipeline publishes introduced LFS dependencies under its writer
admission before making refs visible. Configured LFS extensions fail without
changing the source ref or clearing overlay bytes; CLI extension behavior remains.

Evidence map:

| Changed surface | Caller / owner | Callee / dependency contract | Proof |
| --- | --- | --- | --- |
| Local writer ownership | CLI standalone/filter-process, extension, migrate and VFS publisher | `crab-lfs::local::{ObjectWriter,StagedObject,verify_file}`; pinned tempfile 3.27.0 atomic persist | CLI cache vectors, extension/clean and migration conversion tests; 64 LFS tests |
| Attribute routing | Live/offline `commit_overlay_with_view` | `git check-attr --cached`; shared Crab/LFS candidate routing | Missing-driver regression, nested mixed Crab/LFS/ordinary regression, existing Crab staging tests |
| Index/ref order | Publisher under overlay freeze → prepared index → live CommitView | Install payload before pointer/index/ref, adopt before backing cleanup | Native held descriptor, no-push immediate read/remount, two commits before later push |
| Remote durability | Both mount controls → ordinary push pipeline | `lfs::publication::publish_reachable` joined before refs | Independent eager clone and all three historical payload hashes on real RustFS |
| Extension failure | VFS publisher → Git extension configuration | Stored transformed bytes require smudge, unsupported by VFS | Source ref and overlay records remain after failure |
| Current main/base | `origin/main` cc2e4e25f46 and `origin/crab-v2` 877b832b396 | Neither VFS publisher stages LFS directly; previous native failure preserved | Canonical pointer publication is new; unrelated minimal-profile glob failures separately reproduced |

Final-source proof:

- Dedicated Linux: all 669 VFS tests, all 18 native NFS/FUSE tests, 64 LFS tests,
  strict all-target library Clippy and minimal/FUSE-only checks pass.
- CLI slices pass 293 tests: cache 7, extensions 6, migration 78, standalone 2,
  clean 57, LFS publication 5 and mount controls 138. Two migration glob tests
  fail in the no-default-feature profile. Exact unchanged matcher/test extracts
  from `origin/crab-v2` reproduce both; `origin/main` reproduces its existing
  exact-root-path failure. The multi-pattern matcher/test is present on v2 but
  absent on main. No test assertions, thresholds or inventories were changed.
- macOS passes strict all-target library Clippy and 94 selected publication,
  hydration and snapshot tests. Both platforms use fresh Make-installed roles.
- Linux foreground NFS and managed FUSE, plus macOS foreground NFS, pass real
  Capsule/RustFS seed → 6 MiB edit/fsync → no-push commit as a 132-byte pointer →
  held-descriptor read → detach/remount → second edit/commit → later push →
  independent eager clone. All three historical LFS payloads verify and strict
  Git fsck passes. Each mount clone explicitly has no LFS clean driver.
- Final Linux FUSE proof binds the coordinator process incarnation through last
  unmount, confirms exit and state-file removal; NFS owners exit zero. No forced
  cleanup occurs. An earlier driver completed both data flows but tried to send
  redundant shutdown after the coordinator correctly auto-exited; that harness
  failure remains recorded and is superseded by the final normal-cleanup proof.
- All 1,558 captured source/build inputs and both installed role hashes remain
  unchanged through qualification. New production growth supplies shared writer
  ownership and typed publication routing; most moved code replaces its CLI
  owner, while new regressions cover behavior rather than duplicate helpers.
  All 2,156 raw audit files remain excluded/preserved; tests use inline/flat layout.

Atomic `tempfile::persist` does not synchronize directories. The writer retains
its existing explicit temporary-file sync and post-install verification, without
claiming power-loss recovery. Directory durability, crash/restart, old-reader
payload retirement and efficient full-object verification reuse remain gates.

The final ignore-policy probe discovers another correctness failure: an ignored
ordinary file disappears after commit/remount, while an ignored LFS file is
unexpectedly published because direct pointer staging bypasses `git add`'s ignore
selection. Native macOS NFS proves both against the current installed source;
normal cleanup succeeds. The existing ignore test checks tree membership only,
so it does not protect retained bytes. This is a blocking follow-up before full
publication signoff: plan one committed/retained scope for ordinary, Crab and LFS
records; retain excluded backing bytes through adoption, recovery and remount.
Do not repair only LFS filtering and thereby turn unintended publication into
silent byte deletion. The full PR remains unqualified and the goal remains active.

## Checkpoint 43 — retain content outside Git publication scope

The checkpoint-42 native failure and a new inline publisher regression fail
before this repair. Ordinary ignored bytes are discarded by blanket cleanup;
direct Crab/LFS pointer staging bypasses Git ignore selection. Filtering pointer
staging alone would convert unintended publication into silent byte loss.

Publication now prepares effective `.gitignore` and `.gitattributes` changes,
then plans one selected record set before any content staging. Git's
[`check-ignore` contract](https://git-scm.com/docs/git-check-ignore) excludes
tracked files from ignore results, preserving tracked edits even when patterns
match. Nested rules, negation and repository-local excludes remain Git-owned.
Empty directories and directories containing only ignored children stay local.
The existing disposable AppleDouble policy remains unchanged.

Only selected records enter staging, live adoption, returned commit diff and
cleanup. `OverlayStore::clear_paths` retires selected rows transactionally,
checks remaining backing references and removes directories nonrecursively;
retained child bytes cannot be swept with a committed parent. The publisher
uses its original freeze-owning store. `overlay_cleaned` remains false while
ignored or empty-directory rows remain. An ignored-only commit returns no new
commit and retains bytes; a later overlay policy edit can include and publish them.
Explicit reset remains the separate full-discard operation.

Recovery journals record the committed path scope before commit creation.
The content fingerprint includes that scope and the whole frozen mutation set.
An existing retry regression caught an initial implementation that fingerprinted
only selected records: it could acknowledge an older commit despite added writes.
The source now preserves the existing rejection; its assertion is unchanged.
New regressions preserve ignored bytes after failed adoption/retry and refuse
missing or redirected cleanup scope. Recovery does not infer scope from old
journals or add a compatibility reader.

Evidence map and best-fix assessment:

| Changed surface | Entry / owner | Caller / callee and siblings | Proof |
| --- | --- | --- | --- |
| Selection and policy | `commit_overlay_with_view` under publish freeze | Live `commit_mount_runtime` and offline `commit_overlay`; Git policy preparation → shared ordinary/Crab/LFS routing | Inline table covers all three content types; tracked edits, nested negation, repository excludes and ignored-only no-op |
| Adoption and cleanup | Publisher → `CommitView` → `OverlayStore::clear_paths` | Live open-file capture/adoption receives selected records; snapshot install precedes row/backing retirement | Failed adoption/retry and scope corruption regressions; native held ignored descriptors remain readable/writable |
| Namespace retention | Shared overlay store, both adapters | Deepest-first nonrecursive cleanup; remaining backing references retain ownership | Empty directory, symlink and ignored child under committed directory survive commit/remount |
| Current main/base | `origin/main` cc2e4e25f46 and `origin/crab-v2` 877b832b396 | Both filter only `run_git_add` and clear the full overlay after publication | Original native failure and failing regression preserved; main/base source inspected |

This is the best bounded fix because one publisher-owned selection governs all
content types and all live/offline callers. Adapter-specific filtering or a
pointer-only patch would leave the shared cleanup/adoption defect reachable.
Production LOC grows to supply selective transactional retirement and recorded
recovery ownership; five inline regressions account for most added lines.
No dependency/configuration surface is added.

Final-source proof:

- Dedicated Linux: 674 VFS tests, all 18 native NFS/FUSE tests and all 138 CLI
  mount-control tests pass. All 39 publisher tests pass, including five additions.
- macOS: 147 selected publisher/overlay/hydration/snapshot tests pass. Both
  platforms pass strict combined-feature all-target VFS Clippy, minimal and
  FUSE-only checks, formatting and fresh Make installation.
- Real Capsule/RustFS on Linux foreground NFS/managed FUSE and macOS foreground
  NFS: ordinary/Crab/LFS selected versus ignored data, held ignored descriptor
  read/write/fsync, retained empty directory/symlink/mixed-directory child,
  no-push remount, ignored-only no-op, overlay `.gitignore` edit, later push,
  independent eager clone and strict Git fsck all pass. Small committed pointer
  types/sizes and exact LFS pointer SHA-256 are checked separately from hydrated
  bytes. Every owner detaches normally; FUSE last-unmount coordinator exit and
  state cleanup pass. No forced cleanup occurs.
- Eager clone emits an existing warning when its LFS pass sees already hydrated
  payload bytes; committed pointers, cloned bytes and Git integrity still verify.
  Two initial macOS harness starts failed on fixture-directory setup before any
  repository or mount operation. The final typed proof uses fresh output/input.
- All 1,558 captured source/build inputs and both four-role installed artifact
  sets remain unchanged through qualification. All 2,156 raw audit files remain
  preserved/excluded; no nested `feature/tests.rs` layout is introduced.
- Preceding head 02c8f451df0 CI now reports NFS Mount Evidence, GC qualification,
  Replica Enterprise Gates, HTTP server container and large-repository RustFS
  success. Other runs remain queued/in progress; new-head CI is still required.

The next-priority diagnostic measures local LFS amplification on these same
installed Linux roles. A fresh 32 MiB no-push LFS payload is committed, normally
unmounted/remounted, then read sequentially with 1 MiB client reads. A temporary
external libc-read interposer counts successful reads of local `lfs/objects`
files only during the client intervals. Both foreground NFS and managed FUSE
read 128 MiB in 2,048 calls for 32 MiB returned: 4× local-file amplification.
The source agrees: four cold 8 MiB windows each verify the entire 32 MiB object.
The warm client pass performs zero additional local object reads. Client caching
is enabled; this does not isolate userspace cache hits or physical disk I/O.
Instrumentation and debug builds perturb timing, so no optimized throughput or
latency claim follows. Both mounts detach normally; the interposer and raw logs
remain outside the repository. Bounded verified ownership, rather than trusting
mutable file metadata, is the immediate optimization target.

Ignored-content loss/unintended publication is repaired in these qualified
flows. Full crash recovery, other mount owners and optimized performance remain
open. Next: bound LFS verification reuse and measure local/origin amplification
without trusting mutable cache metadata; retain source/session ownership through
cancellation and payload retirement. The full hardening goal remains active.

## Checkpoint 44 — bounded local LFS block verification reuse

Checkpoint-43's matched native diagnostic measured 128 MiB of local object reads
for a 32 MiB sequential payload on both Linux backends. Four cold 8 MiB windows
each repeated whole-object SHA-256 verification. Metadata/mtime caching cannot
safely remove those reads because a local content-addressed pathname is mutable.
A new source-incarnation regression fails before this repair: after one verified
read, replacing the pathname redirects the next range to different bytes.

`LfsReadSession::read_local_range` now owns canonical local reads. Its first
selection verifies complete SHA-256 and exact size while deriving BLAKE3 proofs
for 1 MiB blocks. It retains the opened file and those proofs; later ranges read
and verify every covered block before returning owned bytes. The per-source
mutex protects that descriptor's seek position and serializes initial
verification. Replacement/unlink cannot redirect a retained source, and changed
size or covered-block content fails. No timestamp or pathname is byte authority.

The per-session cache retains at most 128 sources and 8 MiB of block hashes.
Admission accounts for a proof before allocation; active claims cannot be
evicted into duplicate verifiers. Reads whose proofs cannot fit still perform
full SHA-256 verification without retaining them. Four process-wide verification
permits bound active blocking readers; jobs retain admission after future
cancellation. Close rejects new jobs, clears retained sources and drains admitted
work. A failing range clears its proof for a fresh later repair; it does not
select remote in that failing call. This bounded cache is not a durable LFS
GC-root or an open-file lifetime pin across eviction.

The former free `local::read_range` implementation is deleted. Its only
production caller, `PointerReader`, uses the session-owned API directly. The
free API was introduced on this untagged branch at checkpoint-41; no release tag
contains that commit. There is no compatibility wrapper or second local reader.
SDK remote LFS streams continue using the same tracked session lifecycle, with
no new local policy or dependency edge.

Evidence map and best-fix assessment:

| Surface | Entry / owner | Caller / callee / sibling contract | Proof |
| --- | --- | --- | --- |
| Local byte authority | VFS hydration → PointerReader → LfsReadSession | Full SHA-256 authenticates block proofs; pinned BLAKE3 1.8.7 Hash is 32 bytes; private File/Seek position stays under its entry mutex | Incarnation replacement, changed covered block outside requested bytes, boundary/tail repair and existing complete-source rejection tests |
| Retention/admission | Session state/cache and existing global verification permits | Active claims survive cache pressure; close serializes with tracked-job admission | Proof-byte capacity/live-claim test, 129-source eviction/unlink/close test, existing detached-job drain test |
| Remote distinction | PointerReader local selection and shared hydrator | Only open-time absence selects captured remote; selected corruption remains failure | New VFS regression verifies one local window, corrupts a later block and rejects its new-window read despite valid remote bytes; fresh repair succeeds |
| Both native adapters | Shared hydration/session owner | Foreground NFS and managed FUSE share canonical local verification; view changes share the mount state | Exact 32 MiB multiwindow reads plus retained descriptor/publication/remount/clone flows on real Capsule/RustFS |
| Main/base | origin/main cc2e4e25f46; origin/crab-v2 877b832b396 | Neither has current VFS LFS payload integration; preceding d5cd0de66d9 repeats full local hashes | Matched preceding-head byte-count trace and failing new source-incarnation regression retained |

This is the best bounded fix because canonical LFS ownership supplies reusable
proofs to all VFS adapters without teaching VFS a second integrity algorithm or
copying whole payloads into another disk cache. New production LOC supplies that
proof index, source retention and bounded admission; the old scan is removed and
existing verification assertions remain intact. Requested output buffers and
other sessions are separate resources; no whole-process RSS ceiling is claimed.

Final-source proof:

- Linux: 69 LFS tests, 237 read-library tests, 675 VFS tests and all 18 native
  NFS/FUSE tests pass. All 42 hydration tests pass, including the new corrupt
  later-block/valid-remote rejection. Both platforms pass strict all-target
  Clippy for LFS/read/VFS, minimal read/VFS checks, FUSE-only VFS checks and format.
- macOS: all 69 LFS tests plus 85 selected VFS publication/hydration/catalog tests
  pass. An initial existing linked-worktree assertion compares symlink aliases
  under default `/var` TMPDIR; canonical Workspace TMPDIR passes unchanged.
  No assertion/baseline was weakened. Review also caught local output allocation
  before open-time absence: a missing object with a large range returned OOM
  rather than absence. Its new regression fails first; allocation now follows
  source selection, so remote-only reads do not allocate/discard local output.
- Both platforms use fresh Make-installed four-role artifacts. Native Linux
  NFS/FUSE and macOS NFS pass 32 MiB local no-push commit/remount/multiwindow hashes,
  the preceding 6 MiB two-commit/later-push/independent-clone flow with all three
  historical LFS payloads, and isolated ignored-content retention/policy/clone
  flows. Pointer types/sizes/LFS SHA-256, strict Git fsck and normal detach pass.
- Matched Linux read tracing measures **56 MiB** for 32 MiB returned on each
  backend, versus 128 MiB before: **4× → 1.75×**, a 56.25% reduction in local
  bytes. The first full verification returns its requested range, then three
  remaining windows read their proven blocks. Warm client reads add zero local
  object reads. Instrumented debug timings are about 1.68/1.63 seconds versus
  4.36/4.41 seconds before; these single samples with client caching enabled do
  not qualify optimized throughput, physical disk I/O, tail latency or RSS.
- Final binding checks cover 1,558 source/build inputs and unchanged installed
  role sets. An initial manifest preceded rustfmt by two seconds for one file;
  its final hash is corrected, and fresh FUSE/NFS builds on both platforms confirm
  all affected libraries/binaries are current without recompilation. No
  dependency/version/configuration change,
  raw benchmark inclusion or nested feature/tests.rs layout is introduced.
  One macOS replay started before the final Make install completed; it is excluded
  from final-source native evidence and repeated after installation/hashing.

The sequential native driver exposes a separate reproducible correctness gate:
managed FUSE's later push creates `/.crab/perf-state.json`, its lock and
`audit/events.jsonl`. A subsequent mount then correctly rejects its target as
inside a repository because the root now has a `.crab` marker. Both publication
flows themselves finish with correct clones and normal detach; the next startup
fails. An isolated retention container passes both backends. A second sequence
reproduces the root marker, and per-command snapshots localize its first
appearance to managed FUSE's later-push control. These failures remain recorded;
a passing isolated fixture does not close the combined-flow gate.

Next investigate remote-helper telemetry defaults/configuration and Git-directory
propagation from the daemon's cwd. `Config::perf_path` defaults to a relative
`.crab/perf-state.json`; remote-helper persistence consumes it unchanged. Its
audit append uses the cwd-relative `default_log_path()` directly. Repository
configuration discovery alone does not rebase these paths. Do not bypass nested-mount validation or delete the marker to
make the sequence pass. Remote LFS verification reuse, actual payload-retirement
pins and the full lifecycle/format/performance matrix remain open. The goal is
active, not achieved.

## Checkpoint 45 — repository-owned helper configuration and telemetry

The checkpoint-44 sequential native failure is repaired. The old installed
helper completes LFS publication, then its managed FUSE later-push control creates
`/.crab/perf-state.json`, its lock and `audit/events.jsonl`. The next mount
correctly refuses a nested repository. Reproduction survived restarting the
same dedicated Colima instance; no marker deletion or admission bypass is used
in final qualification.

Two focused regressions fail before repair: actual Capsule push writes metrics
and audit state into an unrelated cwd instead of the selected Git owner; malformed
`crab.toml` in that unrelated cwd also prevents configuration resolution despite
valid selected-repository settings. Final Linux qualification additionally catches
an existing atomic rejection test opening staging outside Git before reporting a
known parse-time failure. Its assertion is unchanged; source admission is fixed.

- Reuse the captured shared Git state root in `RemoteHelperContext` for metrics,
  audit and existing push state. Relative performance paths join that owner;
  explicit absolute paths remain absolute. The installed Rust standard-library
  `Path::join` contract explicitly replaces the prefix for an absolute argument.
  Existing metric deltas and best-effort telemetry error policy remain intact.
- Resolve configuration once through the canonical selected-Git-directory
  resolver. Delete the duplicate cwd project overlay and swallowed discovery
  alternative. The canonical resolver already merges repository/worktree project
  settings; unrelated cwd files cannot fail or override that merge.
- Admission rejects actual pushes without a captured local owner. Parse-only
  rejection batches need no local telemetry. An atomic batch with a rejected
  sibling now avoids store, staging and push-auth preparation before reporting
  its known rejection; admitted sessions retain audit behavior. Existing
  protected-push atomic enforcement and dry-run outcomes remain intact.

Evidence map: native mounted publication supplies explicit Git metadata to the
normal remote helper; `run_remote_helper` captures the owner,
`run_remote_helper_loop` carries it, and `dispatch_batch` uses the existing
`record_push_audit_event` / `SessionCache::persist_pending_metrics` boundaries.
Canonical configuration, Git discovery/common-directory resolution, metric
persistence, audit append, CLI push, release and replica callers were inspected.
Sibling CLI push/release/replica paths already anchor audit state explicitly.
Both current `origin/main` and `origin/crab-v2` retain the relative helper paths
and duplicate cwd overlay. Five new inline tests cover selected configuration,
relative/absolute/disabled metrics with accumulated deltas, parse-only/atomic
rejection, missing owner and real Capsule publication from unrelated cwd.

Is this the best bounded fix? Ownership already exists at helper admission, so
passing that owner to telemetry and using canonical configuration removes the
second discovery path. Changing daemon cwd, bypassing nested-mount validation or
adding another config/env option would leave ownership ambiguous. Production
Rust grows by five lines; test fixtures stop passing an unused push-state borrow.
No dependency, serialized format, config option, alias or fallback is added.

Final-source qualification:

- Dedicated Linux: **450 CLI tests** (152 helper, 137 config, 22 metrics,
  138 mount and one audit), **675 VFS tests** and all **18 native NFS/FUSE tests**
  pass. macOS: all **152 helper tests** pass. Minimal CLI checks pass on both.
- Regular combined NFS/FUSE CLI Clippy completes on both platforms with warnings
  (656 macOS / 657 Linux). All 29 helper warnings point to unchanged primary
  source lines against checkpoint-44; that mapping is not a baseline build or
  strict whole-CLI cleanliness proof. No lint suppression or baseline changes.
- Make installs finish before native proof. In one fresh Linux container the
  original, unmodified publication flow runs on NFS/FUSE, then the original
  ignored-content flow runs on NFS/FUSE with no cleanup between flows. No root
  marker is created; ordinary detach, coordinator exit/state cleanup and
  independent eager-clone/hash checks pass.
- Enhanced Linux NFS/FUSE and macOS NFS publication additionally select valid
  Git metadata from a cwd with malformed project config, verify relative/custom
  metrics and audit at the selected owner, and remount after later push in that
  same working directory. All three historical 6 MiB payloads, held descriptors,
  132-byte committed pointer, no-push remount and strict Git fsck pass.
  Ignored-content retention passes on macOS NFS; the 32 MiB local multiwindow
  read/remount flow passes again on both Linux adapters. No forced detach.
- Hash binding verifies **1,558 source/build inputs** unchanged and all four
  installed roles unchanged before/after qualification on each platform.
  External telemetry qualification reports retain the source manifest, failing
  reproductions, final test logs and native result JSON. No raw evidence enters
  the PR.

The dedicated VM disk was increased from 40 to 80 GiB after reaching 2.4 GiB
free. Only this task's RustFS containers and Colima instance were stopped and
restarted; storage data, network and endpoint were retained. The unrelated VM
was untouched. All 2,156 excluded raw benchmark files remain outside Git;
tests stay inline/flat, with no feature module plus nested tests directory.

Next: remove remaining stored pack-v1 authority, starting with shared legacy
initialization and its CLI publication consumers, then protected receive and
maintenance/GC/replication dispatch. Search real callers before removing exports;
valid v1 refusal fixtures must keep proving authoritative rejection. Current Git
LFS and Crab pointer spec/v1 formats are separate contracts and remain supported.
Remote LFS proof reuse, actual payload-retirement pins, all-mode lifecycle/crash
qualification and optimized performance remain open. The goal stays active.

## Checkpoint 46 — retire the shared manifest-only initializer

Delete `crab_write::initialize::initialize_repository` and its `InitialHead`
error. The shared library no longer exposes that layout-plus-generation-zero-
manifest repository creator. No alias, adapter, migration or fallback replaces
it. CLI, SDK, gateway and view-service initialization already call
`capsule_protocol::initialize`; those production paths remain canonical.

The four remaining initializer calls were negative fixtures: CLI read admission,
LFS remote-ref discovery and two gateway authority tests. They now seed the same
valid canonical layout and `Manifest::default_for_repo` through the metadata
codecs directly. `ensure_canonical_layout` validates persisted descriptor bytes;
`create_manifest` validates the complete manifest before conditional creation.
The fixtures still prove refusal of valid old authority, including corrupt
Capsule-over-v1 precedence and unchanged legacy bytes after refused writes.
Their behavioral assertions are unchanged. Five tests exclusively preserving the
removed initializer and its layout-race behavior are deleted with that API.

Evidence map:

| Surface | Proof |
| --- | --- |
| Removed owner | Full `initialize.rs` and every `initialize_repository` / `InitialHead` consumer searched; CLI and SDK error matches updated |
| Current entry points | CLI `initialize_remote_repository_store`, SDK `Client::initialize_remote`, gateway `initialize_repositories` and auth view publication already call Capsule initialization |
| Current callee | `RepositoryRoot::initial` validates before I/O; Capsule initialization conditionally creates/adopts a verified root and rejects nonempty rootless prefixes |
| Siblings | Gateway and CLI legacy-admission fixtures keep valid old descriptor/manifest bytes; actual Capsule creation/opening remains covered by CLI/SDK and native flows |
| Main/base | Both current `origin/main` and `origin/crab-v2` still export the deleted initializer; current v2 production call sites are not relabeled as new work |
| Best fix | Delete the obsolete public producer; wrapping Capsule initialization under its old signature would retain a second API and invent repository-ID policy already owned by current callers |

Strict write-only SDK lint exposed a content-only test helper compiled without
its callers. Both `prepared_or_panic` callers are under `feature = "content"`;
the helper now has the same gate. This is a feature boundary correction, with no
lint suppression or changed assertion. The failed lint logs are retained. Full
SDK tests then pass with both `write` and `write,content` on Linux and macOS.
This test-only follow-up is explicitly recorded in the source manifest; both
Make installs and native proof are repeated after final sources are frozen.

Qualification:

- Linux and macOS: **212 CLI tests** each (30 initialization, five repository
  admission, 25 LFS push, 152 helper), **36 shared write tests** and all six
  focused SDK initialization cases pass. Write has one ignored test.
- Linux: all **208 gateway library tests** pass, including both valid-v1
  admission fixtures. Full SDK profiles pass on both platforms: **49 write**
  and **52 write/content** tests, with one ignored in each profile. The six
  initialization cases are included in those SDK totals; child-process output
  is not counted as additional parent tests.
- Strict all-target write/SDK Clippy passes with each SDK profile on both
  platforms; the Linux invocations also include gateway. Minimal CLI/SDK checks
  pass on both. Existing whole-CLI warnings remain; strict whole-CLI/workspace
  cleanliness is not claimed. Format and diff checks pass.
- Fresh Make-installed Linux foreground NFS/managed FUSE and macOS foreground
  NFS pass Capsule/RustFS initialization, 6 MiB payload publication, 132-byte
  pointer commit, held-descriptor reads, no-push remount, later push and
  post-push remount in the same cwd. Independent eager clone verifies all three
  historical payloads; strict Git fsck and ordinary teardown pass. Unrelated
  malformed project config and repository-owned telemetry checks pass again.
- Final hash binding covers **1,558 source/build inputs** and unchanged four-role
  installed sets on both platforms. The first Linux launch selected a nonexistent
  qualification-directory binary path and stopped during hashing, before any
  mount. Final proof uses the actual Make install prefix in this task's VM;
  no binaries are manually copied and no admission check is bypassed.

Rust source shrinks by 242 lines, including removed obsolete tests. No dependency,
lockfile, config surface or serialized format changes. No new test files or
nested test layout are introduced. Raw benchmark evidence remains excluded;
all 2,156 files are preserved with zero tracked/diff paths in their directory.
The scoped guide and README point to current Capsule initialization; the SDK
qualification document marks the old initializer discussion as historical.

Remaining stored-format work is explicit: `initialize::publish_initial_manifest`,
the legacy CLI push pipeline/native entry points and CLI fixture initializer,
protected receive, metadb/GC, maintenance and replication authority still need
retirement. Current receive `validate_layout` and prepare-record creation fall
back to v1 metadata; plan schemas 1/2 route into manifest verification/commit.
Retire those linked paths together, preserving Capsule prepare/evidence records
and shared payload verification that current Capsule publication still needs.
The broader lifecycle, crash/pin ownership and optimized performance gates remain
open. This checkpoint is progress, not completion of the goal.

## Checkpoint 47 — Capsule-only protected receive

Protected prepare, verify and commit require a valid Capsule root. Delete
manifest fallback, plan schemas 1/2, prepare-record schema 1, manifest verification
evidence and the manifest finalizer. Receive now has one typed Capsule plan path;
current plan schema 3 and prepare/evidence schema 2 stay unchanged. Admission
validates the plan and active-active options before acquiring writer fences.
The commit operation still drains before releasing either fence; cleanup failure
after known publication remains repair work rather than a false rejection.

Four regressions establish the boundary. Before repair, valid layout/manifest-only
repositories reached prepare, verify and commit, with respectively 1, 0 and 8
observed writes. Schemas 1/2 parsed, and a manifest-only prepare record authorized
recovery. After initial path removal, commit still acquired fences for schemas
1/2: 8 observed writes each. Final tests reject all these inputs, with zero writes
for rootless entry points and retired-plan commit admission. Existing unknown-
field, request-limit, evidence-replacement and staged-byte checks target current
Capsule inputs rather than preserving removed manifest internals.

Delete the legacy Git receive workspace, stored-pack intake and metadata
publisher. Surviving graph verification and source-commit synthesis are free
functions shared by current scoped/unscoped candidates. Filtered visibility is
proved before source packs become available; physical pack presence still does
not grant read authority. Current intake remains the bounded shared Capsule
installer, using authenticated descriptors and content-addressed filenames.

Evidence map and best-fix check:

| Surface | Proof |
| --- | --- |
| Entry and owner | Real `crab-auth-receive` process → public prepare/verify/commit → `ReceiveContext`; full workflow/session/Git-workspace paths read |
| Caller | CLI `finalize_capsule_push` and SDK mutation → shared `CapsuleArtifacts::stage_protected_plan` already produce plan schema 3; removed exports have no remaining workspace consumers |
| Callee | `load_root` verifies Capsule authority; captured ref view binds prepare identity; canonical pack installer verifies bounded incoming/base packs; promotion rereads and hashes staged bytes before immutable writes |
| Siblings | Standalone auth view still selects/publishes manifest authority and uses retained locator/visibility mechanics. Its retirement is explicit next work; no duplicate replacement was extracted |
| Independent formats | Physical shard GC closures remain consumed by CLI GC repair; their schema 1 and the Xet SH01 trailer are current payload contracts. Git LFS/pointer spec/v1 is independent of stored pack-v1 authority |
| Main/base | Freshly fetched `origin/main` `cc2e4e25f46` still uses legacy layout admission; `origin/crab-v2` `877b832b396` selects native root with a manifest fallback and accepts plan schemas 1/2 |
| Is this the best fix? | Yes for receive: remove the obsolete orchestrator/producer and retain the captured Capsule path. Additional flags or rejection wrappers would preserve dead authority. View, CLI/shared remote producers and maintenance must be retired at their actual owners |

Final qualification:

- Linux and macOS: all **77 auth-helper library tests** pass, none ignored.
  Both actual helper binaries build; strict `--all-targets -- -D warnings`
  Clippy passes on both platforms. All 11 helper packaging checks pass.
- Actual prepare → verify → commit processes publish ordinary Git, **6 MiB
  Crab** and **6 MiB LFS** content to the task's RustFS bucket. Immutable receipt
  exists and no legacy repository authority is created. Actual processes reject
  valid manifest-only repositories, legacy plans and legacy prepare records;
  refusal preserves the legacy fixture's bytes, ETags and object count.
- In each final platform run the fresh helper publication feeds the unchanged
  Make-installed client roles: native Linux foreground NFS and FUSE, and macOS
  foreground NFS. All three files return exact sizes, complete hashes and random
  ranges. Writes are refused; normal detach and owner exit zero follow. NFS
  status remains live. No force cleanup or admission bypass is used.
- Hash binding verifies **1,557 source/build inputs** and identical seven-artifact
  before/after sets per platform: four installed client roles, both fresh helper
  binaries and the external qualification producer. Client sources do not depend
  on auth-server and are unchanged from checkpoint 46. Helpers run directly from
  their Cargo build outputs; no product binary is copied or manually installed.
- One initial harness run published successfully, then incorrectly reused a
  push ID for two negative plans. Immutable-plan enforcement refused replacement.
  Failure evidence is preserved; unique session IDs make the full replay pass.
  This is a harness correction, not a product-policy bypass.

Evidence stays on the external workspace volume under
`protected-capsule-only/` and `linux/protected-capsule-only/`: before failures,
source hashes, build/test/lint logs, actual process logs, artifact bindings and
native results. These are debug correctness proofs, not optimized throughput,
p95/p99, RSS or full HTTP-service qualification. Post-cleanup binary retry,
active-active infrastructure and all-mode cancellation still need dedicated proof.

Rust source shrinks by 4,520 lines including obsolete tests; source before the
inline test modules shrinks by 3,272 lines. No
dependency, lockfile or config change; no new/nested test file. The existing
scoped guide and README document the native contract and explicit view gap.
All 2,156 excluded raw evidence files remain preserved and untracked.

Immediate priority: standalone auth view's manifest fallback and publisher, then
remaining shared remote/CLI producers, metadb/GC and maintenance/replication
authority. The lifecycle, crash/pin and optimized performance gates below remain
open. The goal remains active.

## Checkpoint 48 — captured Capsule views and native cache hits

Retire standalone view's manifest source fallback, protocol enum and publisher.
Delete its segmented inventories, metadb/chunk receipts, stored-pack sidecars,
locator/visibility publisher and generation-receipt helpers. These had no remaining
current receive consumer after checkpoint 47. Keep native pointer catalogs,
view-local shard/xorb storage, the physical GC registry and shared Git peeling.

Two before proofs establish the failures. A valid descriptor/manifest-only source
was accepted by source-view admission; the new negative regression fails on that
acceptance. The actual macOS helper built a rooted Capsule view, then its second
materialization failed on the nonexistent legacy `layout` object. Native view
publication intentionally creates no such descriptor. Cache hits now verify the
native readiness/root and view-local dependencies, without selecting old metadata.

Git state and pointer recipes now use one captured `CapsuleRepositoryView`.
The workspace installs authenticated packs through the shared store installer,
then installs only captured refs and HEAD. It no longer clones mutable latest
history. The selected origin layout also reaches hydration unchanged. The unused
`ViewS3Credentials` and alternate credential-taking Rust entry point are removed;
every current consumer uses the subprocess helper or caller-owned store. The
strict seven-field JSON output stays unchanged, including the current field name
`source_manifest_hash` carrying the Capsule state digest.

The public build/cache-hit test exposed literal `allowed.txt` failing bare Git
export from an unrelated cwd. Direct Git reproductions confirm that failure and
that explicit `:(top)` paths work. Export now uses root-relative include/exclude
pathspecs; wildcard semantics stay intact. This follows Git's documented
[pathspec contract](https://git-scm.com/docs/gitglossary#def_pathspec) and
[fast-export interface](https://git-scm.com/docs/git-fast-export). The original
literal-path assertions remain unchanged after the source repair.

Evidence map and best-fix check:

| Surface | Proof |
| --- | --- |
| Entry/caller | Real `crab-auth-view materialize`; Python `SubprocessViewHelper.materialize` parses the same output and supplies authorized include/deny rules. Full helper, caller, service scope validation and wire-field definitions read |
| Owner | `materialize_view_with_store` admits the native capture before publication; lookup cleanup is awaited on build, cache-hit and error paths |
| Callee | Shared `install_git_packs_from_store` verifies origin-backed packs, external-delta order and bounded intake; 2 GiB view intake is documented. Captured catalog lookup never opens mutable latest metadata |
| Siblings | Current receive already uses captured native Git verification and does not call the deleted locator/metadb producers. SDK mutation stages Capsule plan schema 3; shared remote/CLI legacy producers remain separate next work |
| Formats | Native view readiness schema 1, GC closures, Xet shard encodings and pointer/LFS spec/v1 are current independent contracts; they are retained |
| Main/base | Fresh `origin/main` `cc2e4e25f46` uses manifest-only view authority; `origin/crab-v2` `877b832b396` retains native/manifest dispatch and the incorrect cached-view layout requirement |
| Is this the best fix? | Yes for view authority: remove the old producer and use the existing captured store path for Git and content. Adding an empty descriptor, clone retry or manifest fallback would preserve the authority split and leave cache identity races |

Tests preserve the filtered content/history assertions using native publication
and reconstruction. Removing the source root after capture still permits exact
filtered Git and Crab reconstruction, proving those reads do not reopen latest
authority. Forced shard partition tests retain complete file/xorb catalog coverage.
Three obsolete manifest-only tests disappear with their producers; one native
admission regression is added. The unused process-global workspace override
harness is deleted after strict Clippy identifies its last caller as removed.

Final qualification:

- **75 current library tests** pass on Linux and macOS, none ignored. Both actual
  helper binaries build; strict all-target Clippy passes on both platforms. The
  macOS test build/run is repeated after removing the unused test harness. All
  11 packaging checks, formatting and diff checks pass.
- Real helper processes on both platforms publish to RustFS, then materialize
  wildcard, literal-filename and deny-path views. Each first build and second
  cache hit succeeds. Canonical captured-store readers reconstruct exact ordinary
  bytes and **6 MiB Crab / 6 MiB LFS** payloads; denied history/content is absent
  from the filtered Git object database. No legacy layout/manifest is created.
- The actual Python `SubprocessViewHelper` caller passes initial build/cache-hit
  with literal include and deny rules on both platforms. The exact macOS request
  that originally failed on `layout` now reuses that existing native view.
- A separate actual-helper error probe removes one qualification-owned view LFS
  object. Cache-hit verification errors and leaves it absent, despite the source
  payload remaining available. Explicit restoration of the fixture payload makes
  the cache hit pass again. This qualifies refusal, not an automatic repair path.
- Each final platform sequence binds real receive → RustFS → view reads plus
  native source mounts: Linux foreground NFS/FUSE and macOS foreground NFS return
  exact complete/range reads, reject writes, detach normally and exit zero.
  Valid legacy-source refusal preserves fixture bytes, ETags and object counts.
  No force cleanup or admission bypass is used.
- Hash binding covers **1,557 source/build inputs** and identical seven-artifact
  before/after sets per platform. Actual Python-caller hashes match the qualified
  helpers. Four client roles remain Make-installed checkpoint-46 artifacts;
  client sources have no auth-server dependency and are unchanged. Both helpers
  run directly from fresh Cargo outputs. No product binary is manually copied.

Evidence is external under `view-capsule-only/` and `linux/view-capsule-only/`.
An initial external probe compile lacked an LFS-pointer error conversion and
stopped before runtime; the corrected probe uses source-preserving conversion.
No full HTTP authorization service, scoped native-view mount, GC retention or
optimized performance signoff is implied by canonical store reconstruction of
generated views. Initial view publication still has separate LFS publication and
post-publication copy/verification calls; measure that amplification before
consolidating ownership or claiming performance improvement.

Rust source shrinks by **1,735 lines**; source before inline test modules shrinks
by **1,458 lines**. No dependency, lockfile, config option, wire schema or new test
file. The raw benchmark directory stays excluded. Immediate format priority moves
to shared remote/CLI legacy producers, metadb/GC and maintenance/replication.
All-mode lifecycle, payload/crash ownership and optimized performance remain open.

## Checkpoint 49 — one shared Capsule preparation path

Remove the unused shared manifest `prepare`, `Prepared::upload`, `Artifacts`
journal commit and protected-plan staging API. Fold the existing Capsule entry
point into its implementation without changing its caller signature. Delete the
catalog/materialized visibility switch and ordinal branch; keep the former
materialized visibility behavior as the sole path. Captured ref closure bounds,
visibility replacement when borrowing another ref, verified thin-base reads,
normalized pack preparation, cancellation observation and awaited reader closure
remain intact. Indexed Git kinds are optional under `GraphSource`; absent kinds
still require a verified visible-body read, never locator-only authorization.

Current SDK callers already use `prepare_capsule` → `upload_capsule` →
`CapsuleArtifacts`, including protected schema-3 staging. No current workspace
caller uses the removed APIs. Keep `ProtectedPushPlan` temporarily at its actual
owner boundary: the older CLI publisher still constructs that DTO and calls the
separate journal helpers. This checkpoint does not claim to retire that producer.
Normal remote-helper push already selects `capsule_push::run`; its current chunk
planner still lives in `push.rs`, so deleting that module wholesale would break
the native path.

Evidence map and best-fix check:

| Surface | Proof |
| --- | --- |
| Entry/caller | Full shared preparation module, public exports and all workspace consumers searched/read; SDK mutation and its tests select captured Capsule preparation |
| Owner | Shared remote orchestration owns quarantine/graph/visibility preparation. Caller retains the authenticated capture, scratch, ref leases and GC fences through awaited completion |
| Callee | Capsule upload validates reader/capture identity and dependencies before publication; blocking preparation retains `operation.finish` error composition and drains its cancellation watcher. `GraphSource` optional-kind contract and remote reader placement identity read |
| Siblings | Direct SDK write/write-content, auth helpers, CLI/gateway/server dependency edges checked. Only the old CLI still needs the manifest-plan DTO/journal helpers. Current Capsule chunk planning is explicitly retained |
| Main/base | Fresh `origin/main` `cc2e4e25f46` and `origin/crab-v2` `877b832b396` both expose the removed manifest APIs; the native callers on this branch already use Capsule |
| Is this the best fix? | Yes for the unused shared producer: delete its entire API and alternate proof instead of a runtime flag, error wrapper or renamed compatibility path. Actual old CLI orchestration and initializer remain explicit next work |

Qualification on both Linux and macOS:

- **107 remote tests** pass: 32 library, 13 publication, 14 preparation and 48
  checkpoint. The two normally ignored live preparation tests are also run
  explicitly against the dedicated RustFS bucket: native content reconstruction
  and ref visibility after source ref/local source deletion both pass. This is
  S3-compatible qualification, not live AWS proof.
- SDK write has **49 passes / one ignored**; write-content has **52 passes / one
  ignored**. Auth has **75 passes**, none ignored. Strict all-target remote and
  both SDK-profile Clippy pass; minimal remote/SDK checks and SDK all-feature,
  all-target checks pass. No test assertion or baseline is weakened.
- An external API probe compiles before removal (exit zero) and fails afterward
  on the removed `prepare`/`Artifacts` exports (exit one). This proves API removal,
  not runtime refusal of an old repository. No synthetic repo test is added.
- Four fresh client/helper roles per platform are installed through `make
  install`, with every Cargo target and Make artifact lookup on the per-worktree
  Workspace volume. Formatting passes. Regular CLI builds have warnings; strict
  whole-CLI/workspace cleanliness is not claimed.
- An external producer exercises the actual shared API with ref/GC leases,
  captured Git readers, real Git pack input, content attachment, upload and
  proven commit outcome. Fresh publication is followed by an incremental commit
  against nonempty captured visibility; both return distinct committed
  transactions. The second input requests `--thin`; external delta presence is
  not separately asserted. Runtime shutdown and lease cleanup are awaited.
- That published source mounts on **Linux foreground NFS/FUSE and macOS
  foreground NFS**. Ordinary Git, Crab and LFS files are each **6 MiB**; complete
  hashes and boundary/middle ranges match. Read-only writes return EROFS, NFS live
  status reports read-only, ordinary SIGINT detaches and owner exit is zero.
  No legacy layout/manifest is created; no force cleanup or admission bypass.
- All **1,557 source/build hashes** still match the tested source. Five artifacts
  per platform are identical before/after native qualification: four fresh
  Make-installed roles plus the external producer. No auth helper subprocess is
  claimed by this shared-API qualification; preceding receive/view proof remains
  recorded at its own checkpoint.

The first external fixture compile accidentally included an unused legacy helper;
that harness-only compile failure is retained. The first native attempts stopped
before publication because the harness passed Capsule generation zero as the
reader placement generation. The dependency rejects zero placement; the corrected
fixture uses placement one for its single immutable placement, separately from
Capsule generation. Both corrected full runs pass; production validation is
unchanged. Raw failures and final evidence remain external under
`remote-capsule-only/` and `linux/remote-capsule-only/`.

Rust source shrinks by **559 lines**. No tests are added/removed, no dependency or
lockfile changes, no new config or test files. All 2,156 raw benchmark files remain
preserved and excluded from Git/PR. Debug correctness evidence does not establish
optimized throughput, tail latency, RSS or all-mode lifecycle behavior. Next:
retire the actual old CLI orchestration/manifest initializer, then metadb/GC and
maintenance/replication authority. The full hardening goal remains active.

## Checkpoint 50 — Capsule-only chunk candidate authority

Remove `AddChunkIndex` and add-time layout/manifest selection. The production
classifier opens only the bounded global `CapsuleChunkIndex`; an absent root
stays session-scoped and does not manufacture persisted per-chunk misses. A
miss from an available index can still be cached. Delete the legacy committed-
receipt lookup wrapper and its source-manifest validation branch.

Staged placement revalidation now accepts only exact Capsule records and their
current source catalogs/physical origin assertions. Delete the unresolved-ID
manifest-receipt fallback and native publication's receipt-only payload import.
Verified Capsule xorb descriptors already expand all outgoing requested chunks;
there is no second importer. Missing proof requires actual local recipe authority
or preparation fails before refs move. Add and native preparation no longer
construct/open a legacy MetaDb session. The former classifier close entry point
now reports cache statistics synchronously; there is no MetaDb session to drain.
The advisory SQLite cache follows the classifier’s Arc lifetime.
The older `PushPipeline` remains the current proof implementation owner, not a
new `NativeChunkPlanner` type. Its legacy publisher/fixture callers remain work.

Three before failures establish the removed behavior:

- Add records legacy layout/receipt reads despite an otherwise empty lookup.
- Unresolved staged IDs probe the legacy receipt database.
- A seeded old candidate cache returns opaque old IDs to fresh adds even after
  the two reader branches are deleted. Persisted entries have no format tag.

All three regressions pass after removal. A separate Capsule cache-key domain
prevents old positives from suppressing local payload preparation; old entries
are not migrated. Publisher negative-cache invalidation uses the same shared path
function. The unreachable path helper's unscoped fallback is deleted. SQL table
schema version 1 remains an independent local cache contract, not stored pack-v1.

Heap-pin the publication future after Clippy identifies its 22,616-byte state.
The same awaited scope still orders content, GC admission, ref publication,
hint maintenance, staging adoption and lease release. No task is detached.

Evidence map and best-fix check:

| Surface | Proof |
| --- | --- |
| Entry/caller | Real `crab add` → streaming `ExistingChunkLookup`; `crab push`/remote helper → `capsule_push::run` → `xet_publication::prepare_delta`; full changed functions, callers and cache consumers read |
| Owner | Shared classifier and staged-proof verifier own candidate interpretation. Staging retains recipe authority; native publication owns captured destination dependencies and GC admission |
| Callee | `CapsuleChunkIndex` bounds/validates roots, pages, placement IDs and source anchors; current source catalogs plus fresh provider identity certify reuse. Its hint/root version 1 is independent of retired repository formats |
| Siblings | Primary/duplicate add tasks share the classifier. Native CLI/protected payload preparation shares the same delta path. Current filtered clean uses local staging/builder policy; legacy public pipeline and its integration harnesses are explicitly inventoried, not assumed absent |
| Main/base | Fresh `origin/main` `cc2e4e25f46` retains manifest candidate discovery; `origin/crab-v2` `877b832b396` retains both the add dispatch and unresolved-ID receipt fallback. No new base commits at final fetch |
| Is this the best fix? | Yes for current candidate authority: remove both readers and the receipt importer, isolate ambiguous advisory cache entries, and retain the existing Capsule membership/origin proof. Keeping zero-valued legacy counters or a renamed importer would preserve obsolete concepts |

Final-source verification on both platforms:

- **595 relevant Rust passes / one ignored**: 311 push, 17 Capsule push, ten Xet
  publication, 64 add, 152 helper, 14 candidate-cache and 27 cache-path/filter
  matches. The complete selected slices are repeated after heap-pinning and
  moving the two touched nested test modules inline. Existing native full/partial
  hint reuse, exact-ID forgery, missing source root, replaced origin and conflicting
  origin assertions remain intact. They also retain zero full-xorb GET and zero
  duplicate-payload assertions for current proof reuse.
- **Six Python proof-oracle tests** pass on both platforms. The real macOS run
  first committed successfully and revalidated all 90 staged chunks, but its old
  oracle required removed receipt-only counters. The oracle now requires complete
  current Capsule counts, exact types and one ordered event pair, and rejects
  retired counter fields even when zero. Its payload, adoption and inventory
  thresholds are unchanged. Full fresh CLI replays then pass.
- Regular combined-feature library Clippy completes on both platforms with
  647/648 warnings; no primary warning span intersects a changed Rust hunk.
  The large-future warning on the touched publication scope is removed. Minimal
  library builds, formatting and diff checks pass. Strict whole-CLI/workspace
  cleanliness and a current-main warning baseline are not claimed.
- Four roles per platform are freshly `make install`ed from the frozen source,
  with external Cargo targets and Make artifact lookups. No product executable
  is manually copied. **1,555 compiled source/build inputs** match, and checked-in
  Python oracles plus external driver hashes are bound separately.
- The checked-in RustFS cross-repository CLI smoke passes **all 25 checks** in
  each final platform/backend sequence at **6 MiB**. Real source add/commit/push
  produces hints; consumer add retains remote recipe authority with no local
  segment or prepared payload, consumer push revalidates every exact proof and
  creates no new xorb, and a separate cold clone/hydrate returns exact bytes.
- Native **Linux and macOS foreground NFS RO/RW** pass complete and boundary/
  middle range reads, RO write refusal, RW write/fsync → commit/push → independent
  eager clone and strict Git fsck. **Linux foreground FUSE RO** also returns exact
  complete/range reads and rejects writes. All qualified owners detach normally
  and exit zero, with four identical before/after role hashes per platform.
  These are debug correctness/reuse proofs, not optimized throughput/RSS results.

The attempted Linux foreground FUSE RW flow exposes an open mode failure:
`mount commit` cannot resolve the mounted Git directory. It reads exact bytes and
accepts write/fsync first, then falls into offline routing because foreground FUSE
has no discoverable live control owner. The same actual failure reproduces with
unchanged checkpoint-49 binaries whose four hashes match their prior qualified
set. After the failed command, the baseline owner still serves the edited bytes;
ordinary SIGINT detaches with exit zero. This is not attributed to the new chunk
proof change, nor claimed repaired. Next priority: install and advertise the live
foreground FUSE control owner, and route publication through that owner; guessing
a cache Git path would bypass engine mutation/writeback ordering.

Initial Linux harness attempts also preserve missing-AWS-tool and unshared Git-
metadata preflight failures, both before publication/mount. AWS CLI 2.17.14 is
installed only in the dedicated Workspace qualification environment. A read-only
snapshot of actual Git metadata matches host HEAD and complete worktree status;
it permits the existing provenance checks without changing them. No assertions,
baselines, inventories or thresholds are weakened.

Evidence stays external under `capsule-chunk-only/` and
`linux/capsule-chunk-only/`. Runtime source before test modules shrinks by
**296 lines**; all Rust shrinks by **155 lines**, including three new regressions
and inlining the two existing test modules without new test files. No dependency,
lockfile, serialized payload format, config option or override changes. The
protocol document now identifies the actual `CRBCKP09` writer/reader contract;
older measurements retain their historical scope. All 2,156 raw benchmark files
remain excluded. Foreground FUSE controls, remaining legacy CLI/metadb/GC owners,
source-root retention, crash/resource ownership and optimized performance stay
open; the full hardening goal remains active.

## Checkpoint 51 — foreground FUSE live owner and ordered control drain

The checkpoint-50 replay established an actual mode failure: foreground FUSE
served writes/fsync but `mount commit --push` fell into offline handling and
failed to resolve mounted Git metadata. Unchanged checkpoint-49 Make roles
reproduced exit 6 with readable edited bytes retained. This is independent of
checkpoint-50 chunk-candidate changes.

`mount::run_foreground` now retains the complete prepared pipeline in a private
coordinator, sharing the existing hydration chunk cache. It binds the unchanged
FUSE IPC protocol before the serving CLI registers its actual PID/socket and
prints readiness. Registration failure drains the installed session. Signals,
external detach and live unmount converge on accepted-handler completion,
engine-control drain, native detach and read-resource shutdown. The caller keeps
exclusive cache admission through owner-conditional registry removal.

`mount_control` selects a registered FUSE socket before default coordinator
discovery. Rejected, missing or malformed known-owner exchanges cannot select
saved overlay state or stale/PID cleanup. The IPC server tracks connections in
a JoinSet, stops idle reads on cancellation, joins accepted handlers and retains
their final replies. Response writes have a bounded I/O deadline; accepted
mutations are not cancelled by that deadline. Coordinator removal and graceful
shutdown close control admission and drain controls while native WRITE callbacks
remain available. NFS retains its separate existing transport/runtime owner.

The exported legacy `run_mount` helper also consumed the removed synchronous
session loop and now shares the canonical foreground owner. Exhaustive Rust
caller search finds no production caller of that helper: positional mounting is
not exposed by the current CLI. Its preparation tests compile and pass, but it
has no claimed positional CLI E2E proof. Local-source native proof uses the
actual `--repo` entry point. Removing that pre-existing exported dead surface is
separate from stored pack-v1 retirement.

Two regressions fail before the repair and pass after it: private-owner controls
return no owner, and IPC shutdown returns while an accepted resolver/handler
still owns work. The routing regression covers status/list, refresh/switch,
commit/rejection, diff/export/reset and live unmount for default/private sockets;
it also requires a hard error after the registered endpoint disappears. These
are transport tests, not native qualification of every control.

| Evidence cell | Checked boundary |
| --- | --- |
| Changed surface | Foreground session owner, registry routing, coordinator startup/cache injection and IPC connection drain |
| Product entry | `crab/src/main.rs` Mount → `run_mount_with_new_cli` → `run_foreground_fuse`; only registration-after-bind reports readiness |
| Owner | Foreground PID retains coordinator, background callback session, pipeline and outer cache lease through cleanup |
| Caller/callee | `mount_control::commit` → shared IPC commit → `VfsEngine::run_control` → `commit_mount_runtime` → canonical publisher/adoption/invalidation |
| Siblings | Background FUSE shares IPC controls/removal ordering; NFS keeps its established pre-detach control drain and passes native replay |
| Tests | Two before/after regressions; backend/control/client/coordinator slices; CLI mount/coordinator and integration lifecycle/shutdown consumers |
| Main/base | Fetched main and crab-v2 retain the uncontrolled foreground session shape; actual failure reproduced on unchanged checkpoint-49 roles. No native main baseline is claimed |
| Dependency | Cached fuser 0.17 Session::spawn/BackgroundSession::join and Tokio JoinSet join/cancellation contracts read before changing ownership. No dependency, lockfile or wire-format changes |

Focused proof:

- Linux/macOS: 120 VFS tests each. CLI mount/coordinator plus coordinator
  lifecycle/shutdown: 160 Linux / 161 macOS passes.
- Linux minimal and FUSE-only VFS checks pass; NFS-only Unix control routing
  passes 13 tests without linking the FUSE server. Actual normal NFS-only CLI
  roles also control the real foreground FUSE auxiliary role.
- Combined-feature VFS library Clippy: zero warnings on both platforms.
  Regular combined-feature CLI library Clippy completes with 636 macOS / 635
  Linux warnings, zero primary spans on changed hunks. This is not a strict
  whole-CLI or current-main lint baseline claim.
- Both final four-role installations use Make with explicit external artifact
  lookups. Exact final installed roles complete the checked-in RustFS CLI smoke,
  all 25 checks per platform. Native Linux FUSE/NFS and macOS NFS, RO/RW,
  return exact 6 MiB/full/range bytes. RO writes return EROFS; writable cases
  fsync, live commit/push and verify an independent eager clone plus Git fsck.
  Status PID equals the foreground serving process; ordinary SIGINT detaches
  the kernel and returns exit 0. Four role hashes stay unchanged per replay.
- Managed background FUSE also fsyncs, live commits/pushes, verifies an
  independent eager clone and completes live unmount; its coordinator PID becomes
  absent and its socket disappears. Its final role set matches the foreground
  replay. The exact-role independent lifecycle replay also passes busy signal/
  IPC unmount, cache exclusion, fail-closed controls during drain, local-source
  fsync/live commit, registry-refusal cleanup and private socket/registry removal.
  No forced detach was used.
- The combined same-container managed-FUSE → foreground replay fails before
  mounting: foreground preparation rejects the managed cache as a nonempty clone
  destination. This is an open mode-interoperability gap, not a passing teardown
  claim. Separate fresh-session successes do not close it; preserve the cache
  and compare owner/preparation contracts and unchanged-base behavior next.
  Unchanged checkpoint-49 Make roles reproduce the same exit-5/nonempty-clone
  failure after a successful managed mount/read/unmount. Their four role hashes
  match retained checkpoint-49 evidence, and the failing repository cache is
  copied to external proof storage for investigation. No cache deletion or
  initialization bypass was used to force that replay green.

The user's test-layout preference also applies to the remaining PR-modified SDK
`client/mutation/tests.rs`: it is moved byte-for-byte to the flat sibling
`client/mutation_tests.rs`, with its module path explicit. Logical module/test
names and assertions are unchanged. Its write+content mutation slice passes
15 tests, one existing RustFS case ignored, on both platforms. No newly modified
`feature/tests.rs` pair remains in the PR. Existing unmodified module layouts
are not a repository-wide refactor.

Is this the best fix? For the reproduced ownership gap, reusing the existing
FUSE coordinator/controls keeps one publication/invalidation implementation,
retains the foreground PID and preserves its warm cache. It adds no config,
protocol mode, offline fallback or dependency. Runtime-prefix LOC grows by 157:
the growth pays for the missing private owner, readiness/error cleanup and
accepted-handler drain; control policies themselves are reused.

CI on checkpoint 50 exposes earlier checkpoint follow-ups: architecture policy
rejects three current dependency edges, still requires retired pack helper calls
and rejects Git LFS's documented storage environment override. Its integration
gate selects a removed staged-pack test instead of the existing Capsule receive
regression. A concrete guard-update patch is prepared outside the repository;
root AGENTS.md requires explicit approval before inventory/baseline changes.
That request is pending; no checks, inventories or thresholds were edited.
Its released-shape partial-clone job also fails fresh-destination ref discovery
(exit 128 / CRAB-E0070 ENOENT); all four Git compatibility jobs fail their
lifecycle report. The downloaded mirror artifact records a successful mirror
command followed by failed remote ref discovery. Root cause and main/base
comparison are unproven: investigate that producer/read path next, without
classifying the failure as an obsolete oracle. Checkpoint-50 SDK, GC, NFS,
provider, large-repository, replica, cache and HTTP workflows pass; gateway
was still running at inspection.
External `foreground-fuse-owner/qualification.json` binds the final native
results, tests, before/after role hashes and 1,555 source/build inventory hashes;
`evidence-inputs.json` separately hashes the drivers and checked-in smoke oracle.
Final Make rebuilds and native replay follow the legacy caller's heap-pin fix.
Failed harness attempts are retained: wrong unmount/positional CLI syntax,
expected-byte races between two probes sharing a remote, an early PID-exit sample,
and the actual managed-to-foreground cache failure. The Linux consumer compile
was killed under concurrent memory pressure; a one-job retry passes. A shorter
symlink to the same external macOS fixture directory avoids the Unix socket path
limit without changing assertions or production fallback behavior.

All-mode correctness, optimized performance and remaining pack-v1 removal are
still open. This is a qualified checkpoint, not goal completion.

## Checkpoint 52 — preserve the mount cache's Git owner across modes

Foreground remote preparation passed the cache root to `ensure_blobless_clone`,
while managed FUSE passed `cache/.git`. After a managed mount, foreground startup
therefore rejected its nonempty cache before mounting. Reversing that handoff
could create a second clone beside existing bare metadata, losing access to local
refs and the staging namespace derived from their original Git directory.

`clone_cache::git_dir` now selects one metadata owner for foreground FUSE/NFS,
managed FUSE and saved controls. Fresh caches use `.git`. Existing bare metadata
retains its actual directory, unpublished refs and staging ownership. Mixed
root/`.git` owners fail without replacing either history. Both mount entry paths
hold cache admission before selection; IPC failures release their reservation.
This consolidates the prior saved-control selection rather than moving metadata
or adding a second reconstruction path. These are current Git directory shapes,
not stored pack-v1 readers. Native proof below demonstrates the unpublished-data
contract that requires retaining the actual directory.

| Evidence cell | Checked boundary |
| --- | --- |
| Changed surface | Remote cache metadata selection in CLI preparation, saved controls and managed FUSE IPC |
| Entry/owner | Normal CLI `--repo` foreground FUSE/NFS and managed mount request; exclusive cache lease precedes owner selection |
| Caller/callee | CLI/IPC → `clone_cache::git_dir` → existing pipeline/`ensure_blobless_clone` → selected-ref fetch/reconciliation; publication and hydration derive staging from that same Git directory |
| Siblings | NFS foreground/helper share CLI preparation; saved controls share selection. Daemon uses its separate named cache and explicit `.git`, with existing clone preparation; borrowed local sources are unchanged |
| Tests | Real Git root/nested cache handoff retains unpublished commit and overlay; mixed owners reject without touching either HEAD; adjacent repository/pipeline/control/CLI consumers pass |
| Main/base | Fetched main and crab-v2 retain root-only foreground versus nested managed preparation. Unchanged checkpoint-51 roles reproduce exit 5 against the exact preserved checkpoint-49 managed cache; candidate succeeds without resetting it |
| Dependency | Existing shared Git clone/fetch/reconciliation and staging contracts retained; Git bare metadata remains the same directory/inode. No dependency, lockfile, format or protocol changes |

Focused proof on the frozen four-file Rust change:

- Linux and macOS: 93 VFS passes each across clone cache, repository, pipeline,
  IPC server and mount control. CLI mount/coordinator and coordinator lifecycle/
  shutdown consumers pass 160 Linux / 161 macOS tests.
- Combined-feature VFS library Clippy has zero warnings on both platforms.
  Regular CLI library Clippy completes with 635 Linux / 636 macOS warnings,
  zero primary diagnostic spans on changed hunks. This is not a strict whole-CLI
  lint or main baseline claim. Minimal/FUSE-only and NFS-only checks are recorded
  separately in the external qualification.
- Both Make-installed four-role sets pass the checked-in RustFS CLI smoke's 25
  checks, including 6 MiB proof-bearing chunk reuse. Native Linux foreground
  FUSE/NFS and macOS foreground NFS, RO/RW, verify full/range bytes, EROFS,
  write/fsync → live commit/push → independent eager clone and Git fsck.
  Status identifies the real serving PID; ordinary SIGINT detaches and exits 0.
- Linux restores the exact preserved failing managed cache to its original
  location. Unchanged checkpoint-51 roles reproduce the nonempty-clone failure;
  candidate foreground FUSE reuses the original Git directory/inode, commits
  without push, retains uncommitted overlay bytes across managed remount, then
  pushes and independently verifies latest and historical content.
- A second Linux case uses unchanged checkpoint-49 foreground NFS to create a
  bare-root cache and real unpublished Crab/LFS versions. Candidate managed FUSE
  reads both, performs COW edits and pushes; candidate foreground FUSE remounts
  exact bytes. No second `.git` owner appears. Independent eager clone and
  historical checkout/hydration verify both Crab and LFS versions.
- macOS foreground NFS → background NFS helper retains unpublished 6 MiB
  Crab/LFS versions, edits and pushes both, then verifies independent latest and
  historical bytes. Normal live unmount succeeds. All native cases retain role
  hashes; no forced detach, cache deletion or initialization bypass is used.

Is this the best fix? A shared owner selection covers both handoff directions
and saved controls while preserving the actual unpublished-data owner. Changing
only the foreground clone destination would leave the opposite handoff capable
of orphaning refs and recipes. Runtime-prefix LOC grows by 29; that pays for
error-aware selection, mixed-owner rejection and IPC reservation cleanup. The
remaining additions are behavior regressions and contract documentation.

A separate real product failure is now reproduced from an empty working directory
with repository-context environment cleared: `git ls-remote --refs <valid remote>`
exits 128 / E0070 ENOENT, and default-ref foreground mounting exits 9 / E0099.
The remote helper eagerly resolves push state/root before dispatching a read-only
list request. This is the next investigation boundary; complete caller/callee
and Git dependency-contract proof must precede a fix. Successful native cases
above explicitly select `--ref main`; they do not qualify default discovery.
CI mirror/ref and compatibility failures remain open. The separately proposed
CI guard/inventory patch still awaits its own approval; the NFS dependency-patch
approval does not authorize those edits.

External `mount-cache-owner/qualification.json` binds source/build inventory,
installed role hashes, focused tests, native handoffs and the preserved failing
baseline. Drivers and the checked-in smoke oracle have separate hashes. Raw
benchmark files remain outside the PR, and no nested feature/tests file is added.
All-mode correctness, optimized performance and remaining pack-v1 removal remain
open; this checkpoint does not complete the goal.

## Checkpoint 53 — remote discovery without local push state

The checkpoint-52 installed CLI reproduces `git ls-remote --refs` failing with
E0070/ENOENT from an empty cwd, and default-ref foreground mounting failing with
E0099 before native setup. The helper unconditionally canonicalized a local Git
owner before dispatching reads. It also loaded/saved `.crab/push-state` despite
its current Capsule publisher never consuming or updating that frontier. A
focused resolved-session regression confirms read-only list creates that state
before repair; actual entry-point qualification remains separate from that seam.

Remove the obsolete helper state, its context fields and fixture-only roots.
Capture the shared local state owner only for an actual non-preview push batch,
before opening staging or preparing push authorization. Read list, write-authority
list, protocol-v2 serving and fallback no longer resolve that push-only owner.
Actual pushes retain strict Git ownership, selected-repository staging/telemetry,
parse-rejection admission and canonical Capsule publication. No missing-metadata
fallback, fake cwd owner, new env option, compatibility reader or wire change is
added. The old CLI publisher still has separate PushState consumers; its remaining
pack-v1 authority is not removed by deleting this unused helper state.

| Evidence cell | Checked boundary |
| --- | --- |
| Changed surface | Production helper setup/context and shared protocol loop; push-only staging/telemetry admission |
| Entry/owner | Git remote-helper entry, `git ls-remote`, remote mount default-ref discovery; actual push captures canonical common Git metadata before staging |
| Caller/callee | `run_remote_helper` → protocol loop → read advertisement/upload-pack or `dispatch_batch` → Capsule publisher; publisher takes its frontier from the captured Capsule view |
| Siblings | Classic/local-object fetch retains its Git installation owner; protocol-v2 serves the same captured runtime; dry-run/parse-only requests avoid local push state; actual push and unrelated-cwd telemetry tests remain strict |
| Tests | Read-only local-state regression fails before repair; helper, Capsule push and discovery slices; dedicated native/default-ref and mirror checks recorded below |
| Main/base | Current main/base still use native push state and their earlier root policy. Strict helper root capture entered this branch in `3fe268e82da`; the current failure is reproduced on unchanged checkpoint-52 installed roles, not claimed against a native main baseline |
| Dependency | Git v2.51.0 `git.c` registers ls-remote with gentle setup; `transport-helper.c` exports GIT_DIR only if `have_git_dir()`. Read its source/manual before changing admission; no dependencies/lockfile changes |

Is this the best fix? Removing unused persistence and moving strict local-owner
capture to its actual consumer repairs read discovery without weakening push
ownership or restoring the earlier cwd fallback. The old helper frontier was not
a Capsule authority and cannot justify retaining read-time metadata mutation.
Runtime-prefix LOC shrinks by 16; obsolete fixture arguments and state-only
temporaries also disappear.


Focused final-source proof:

- Linux/macOS each pass 173 tests: 153 helper, 17 Capsule push and three Git
  discovery tests. The no-push-owner rejection, selected-repository telemetry,
  damaged-staging reads, classic/promisor/linked-worktree fetch and runtime-drain
  cases remain covered. The read-only local-state regression fails before repair.
- Final regular combined-feature CLI library Clippy completes with 635 macOS /
  634 Linux warnings and no primary diagnostics on changed hunks. A new
  single-match warning after removing state saves was fixed before final Make
  rebuilds, tests and native replay; strict whole-CLI cleanliness is not claimed.
- Both final Make-installed four-role sets pass all 25 checked-in RustFS CLI
  smoke checks. With repository-context variables cleared and no local Git,
  `git ls-remote --refs` succeeds, while an actual ownerless push returns E0070,
  creates no local push state and leaves remote refs unchanged.
- Every final native mount omits `--ref`: Linux foreground FUSE/NFS RO/RW,
  managed FUSE RW, macOS foreground NFS RO/RW and NFS helper RW. Exact 6 MiB
  full/range bytes, EROFS, real serving-PID status, write/fsync → live commit/push
  → independent eager clone/fsck, ordinary detach and foreground exit zero pass.
  All four installed role hashes remain unchanged; no forced cleanup is used.
- The checked-in Linux mirror namespace suite passes 20 checks / 56 commands,
  including the previously failing fresh-destination ref discovery. The isolated full
  partial-clone suite passes 92 checks / 324 commands on Git 2.39.5, including
  shallow/filter/lazy recovery, everyday workflows, visibility refusals,
  disconnect cleanup and smaller filtered transfers. Other CI versions are not
  inferred from one local Git version.

An additional same-remote/same-environment entry replay binds the retained
checkpoint-52 four-role hashes: baseline `git ls-remote` exits 128 / E0070,
candidate returns success, and neither local Git nor push state is created.
Both role sets stay unchanged. This distinguishes local-owner admission from
configuration/auth or remote Capsule corruption.

The native first attempt stopped on a RustFS health timeout while compilation
was active; its failed report is retained. A first mirror attempt lacked git-lfs
in the disposable qualification image; installing Debian git-lfs 3.3.0 in the
transient runner permits the unchanged oracle to pass. The first broad
partial-clone attempt passed unborn discovery but RustFS timed out listing the
shared bucket for the dry-run inventory oracle. Its replay uses a separate fresh
qualification bucket, retaining the original data and assertions. No production
fallback, cache reset, admission bypass or threshold change was used.

The gateway CI failures are real performance-gate failures, not stale report
schemas. Runs 40314421527 and 40323576147 retain verified integrity but terminal
small-write soak throughput drops to 0.766 and 0.750 of early windows; p95 ratios
are 1.555 and 1.501. Existing limits require throughput at least 0.8 and p95 at
most 1.5. Neither run measures a direct baseline, so these prove repeated
within-run degradation, not a regression against main. Profile native small-write
preparation and state growth next; keep these gates and establish a matched
baseline before a causal verdict.

Checkpoint-52 CI passes SDK, GC, NFS, provider, large-repository, replica, cache
and HTTP; architecture, partial-clone and gateway fail. Architecture's separate
concrete guard/inventory patch remains unapplied pending explicit approval.
The NFS dependency-patch approval is scoped to that fork. Other stored pack-v1
publishers/authority dispatch, lifecycle/retention proof, the matched Cellar/SQLite
gate and optimized end-to-end throughput/tail latency/RSS remain open.

External `read-only-helper/qualification.json` binds final source/build inputs,
role hashes, tests, before/after entry points, native/default-ref proof and
checked-in mirror/protocol results; drivers/oracles have separate hashes. Raw
benchmark files remain excluded and tests stay inline/flat. Goal remains active.

## Checkpoint 54 — independent Capsule chunk-proof ownership

A fresh exhaustive caller search corrects the earlier remaining-work wording:
normal `cmd::push` and the Git helper already invoke the Capsule publisher.
`run_native_push` is called by legacy test fixtures, not a current workspace
product entry. Its public API, publisher and fixtures still need retirement;
this is not a claim about unknown external library consumers or a native main
baseline. Current add classification and Xet staged-recipe adoption did still
instantiate `PushPipeline` solely to reach its strict chunk-proof methods.

`git::chunk_reuse` now owns those current operations. Its verifier retains the
origin/cache, metadata concurrency and cancellation token without constructing
a legacy push config, orchestrator, manifest state or metadb lifetime. Add's
classifier keeps its existing advisory cache/absence policy. Exact staged IDs
still select immutable candidate records, match every planned placement/proof
field and revalidate current source roots/shard membership. Conflicting physical
assertions remain errors; absent/stale authority cannot supply a reuse hit.

`git::xorb_proof` owns the existing bounded footer/metadata decoder and exact
provider/cache identity mechanics. The dedicated current verifier always checks
a fresh origin HEAD and the rooted origin assertion before admitting selected
chunks; generic legacy memory-cache shortcuts cannot certify it. The generic
legacy method's strict-mode boolean is removed: no remaining production caller
requests that mode. Current strict tests now exercise their actual verifier.
The stored data, cache encodings, two-minute proof budget and default metadata
concurrency of 64 are unchanged; the shared config constant replaces the
previous duplicate default. No config/env/dependency or lockfile surface is added.

| Evidence cell | Checked boundary |
| --- | --- |
| Changed owner | Add classifier, exact staged-ID revalidation, source-root/catalog checks and provider identity verification in `chunk_reuse`; shared format/cache mechanics in `xorb_proof` |
| Entry/caller | `cmd::add::open_add_remote_classifier` and Capsule `xet_publication::prepare_delta`; legacy fixture pipeline delegates its retained revalidation call to the same verifier |
| Callee | Metadata `CapsuleChunkIndex`/anchor validation/current source closure → canonical Store HEAD and bounded xorb index → exact chunk/hash/size/provider binding |
| Siblings | CLI/helper/import use Capsule publication; NFS/FUSE publication reaches the same Xet path. Git LFS has its separate SHA-256 store/session contract, unaffected by the moved Xet format mechanics |
| Cache/dependency | Read complete index-cache validation and Store HEAD adapters: caches require identity tokens, contiguous rows and recomputed xorb hash. Cache validity cannot replace fresh source authority/provider binding |
| Tests | Existing rooted reuse, exact/forged IDs, replacement/conflicting identity, corruption, scoped misses and cancellation assertions retained; fixture changes only select the new owner |
| Main/base | Main/base retain earlier manifest-era organization. Against the retained checkpoint-53 source, this is an ownership extraction and strict-mode specialization; no native main equivalence or performance baseline is claimed |

Is this the best fix? It removes the current proof consumers' dependency on the
legacy orchestrator while preserving one current Capsule proof path. It does not
keep an alias or format fallback. Two test-only injection/access seams retain the
existing corruption/cancellation probes without exposing mutable production
classifier fields. Runtime-prefix LOC grows by 127 (117 total Rust lines): the
new immutable owner and dedicated strict provider check replace the old class
coupling, while the non-current generic publisher remains explicit next removal.
The new modules immediately serve both current callers and the retained legacy
fixture caller; this growth does not constitute completed publisher retirement.

Final qualification:

- Linux dedicated runner: 555 passes, one existing ignored case, across push
  (311), add (64), Xet publication (10), Capsule push (17) and helper (153).
  macOS focused proof: 41 passes across rooted candidate reuse, advisory cache,
  cancellation, generic physical proof and current publication. Test assertions
  are retained; no test is removed or weakened to accommodate the extraction.
- Final combined-feature regular CLI library Clippy completes with 635 macOS /
  634 Linux warnings, zero primary diagnostics on changed hunks. An extra
  import left after moving the parser was identified separately and gated to
  its remaining test consumers before final builds. Strict whole-CLI cleanliness
  is not claimed. The final macOS Clippy run takes 16m18s; it is observed live
  and allowed to complete, not restarted on an observation timeout.
- Six moved physical/cache helper bodies compare byte-for-byte with checkpoint
  53 after their opening brace. Their visibility/imports change, not data or
  validation mechanics. The old source declares 65 production fields and two test-only fields;
  the current verifier has five. These are structural observations, not RSS/throughput measurements.
- Both final Make-installed four-role sets pass the checked-in 6 MiB reuse
  smoke's 25 checks: current source membership, exact physical proofs, no new
  duplicate xorb and independent cold reconstruction stay mandatory.
  Linux foreground FUSE/NFS RO/RW and managed FUSE RW, plus macOS foreground
  NFS RO/RW and helper NFS RW, pass full/range bytes, EROFS, actual serving-PID
  inspection, write/fsync → live commit/push → independent eager clone/fsck,
  normal detach and foreground exit zero. All omit `--ref`, and ownerless push
  still fails without changing remote refs or local push state.
- Final installed Linux CLI also passes checked-in mirror namespace proof
  (20 checks / 57 commands) and full isolated partial-clone proof
  (92 checks / 324 commands, Git 2.39.5). This is not all-version CI evidence.
  Protocol runners install the required Git LFS tool inside their disposable
  container and use separate qualification buckets. Four native role hashes
  remain identical before/after; no cache reset, force cleanup or bypass is used.
- `chunk-proof-owner/source-manifest.json` binds 1,557 source/build files;
  `qualification.json` records tests, final roles, helper-body equivalence,
  ownership evidence and native/protocol results. Drivers/oracles have separate
  hashes. The raw benchmark directory remains excluded and tests stay inline/flat.

While awaiting the macOS build, retained gateway evidence narrows the performance
investigation. For 16-commit build samples, early→late log-order p95 grows
2.215→8.393 ms in checkpoint-51 CI and 2.532→9.949 ms in checkpoint-52 CI.
The emitting owner is gateway `mutation::build_batch`; its timer includes
`MutableTree` preparation, individual commit construction and attribute checkpoint
selection. These samples lack timestamps/request alignment, so they nominate
that build path for profiling but do not establish the cause of the SLO failure
or a regression against main. No gateway code, metric or threshold is changed.

Next remove the obsolete public native-push entry/publisher and legacy fixtures,
then remaining metadb/GC/maintenance/replication authority. Current product push
routing is already Capsule; retain current chunk proof tests at their real owner
while removing the old producer. Complete lifecycle/retention, matched
Cellar/SQLite and optimized end-to-end performance gates remain open. The separate
CI inventory/guard patch still awaits its own explicit approval. Goal remains
active.



## Checkpoint 55 — direct Capsule configuration in the helper

The helper still constructed `NativePushConfig` after routing publication to
Capsule, then consumed only its inner `PushConfig`. Its incremental/progress,
color, summary, JSONL wrapper and Git-only discovery flags never reached the
current publisher. The helper now configures `PushConfig` directly. Atomic mode,
compare-and-swap expectations and the canonical mirror plan identity are retained;
managed/protected preparation still strengthens that same config before admission.
The final JSONL result remains on the existing stderr stream after the authoritative
stdout reply. No new option, environment variable, dependency or fallback is added.

| Evidence cell | Checked boundary |
| --- | --- |
| Changed surface | `remote_helper::dispatch_batch` and its configuration helper; 39 net Rust lines removed |
| Entry and caller | Real Git helper batch dispatch after captured local-owner admission and staging setup |
| Callee | Current `capsule_push::run(&PushConfig, ...)`; coordinator and protected preparation retain their ordering and overrides |
| Siblings | Direct CLI already builds `PushConfig`; mirror plan identity uses the same canonical `crab_remote::local::PUBLICATION_PLAN_ID_ENV` that the old alias named |
| Contract | Read the whole dispatch and old constructor: only the `push` field was projected; removed wrapper fields were never consumed by Capsule. No Git/S3 wire or upstream API changes |
| Tests | Existing ref-lease parsing/conflicting-repeat and atomic configuration checks use the current helper. Assertions for removed, unconsumed native-wrapper defaults are retired; current publication assertions are retained |
| Main/base | Current main still invokes `run_native_push`; checkpoint 54 already invokes Capsule. This cleanup targets the v2 caller after that routing change, not equivalence with main's old producer |

Is this the best fix? It removes an unnecessary configuration owner instead of
maintaining a wrapper around current publication. The helper has zero references
to the old native module. Its separate public entry and legacy fixture producers
remain next retirement work; this checkpoint does not claim complete removal.

Final qualification:

- Both platforms pass 170 current helper/Capsule tests: 153 helper and 17
  Capsule publisher cases. Initial no-run builds caught two references to the
  removed private test seam; tests select the current helper instead of
  restoring the wrapper. Final builds and existing publication assertions pass.
- Regular combined-feature CLI library Clippy completes on macOS/Linux with
  635/634 warnings. Warning level/code/primary-file counts match checkpoint 54.
  One inherited large-future span overlaps the push block; its measured compiler
  size decreases from 17,640 to 17,616 bytes. This is not zero-overlap lint proof
  or a runtime performance measurement. No lint suppression changes.
- Both final Make-installed role sets pass the checked-in 6 MiB reuse smoke's
  25 checks. Fresh Linux foreground FUSE/NFS RW and managed FUSE RW, and macOS
  foreground/helper NFS RW, pass full/range bytes, fsync → commit/push →
  independent eager clone/fsck, actual owner inspection, ordinary detach and
  successful foreground exit. All omit `--ref`; four role hashes stay unchanged
  before/after. Checkpoint 54 retains the RO/EROFS proofs; they are not rerun here.
- The final installed Linux CLI passes mirror namespace qualification
  (20 checks / 57 commands) and isolated full partial clone
  (92 checks / 324 commands, Git 2.39.5). With JSONL enabled, 15 valid final push
  result events appear on stderr and none on stdout. Mirror plan receipts,
  revalidated leases, namespace preservation and Git protocol oracles stay intact.
- `helper-capsule-config/source-manifest.json` binds 1,557 source/build files;
  `qualification.json` binds tests, lint comparison, final roles, native and
  protocol reports, JSONL and caller evidence. Driver/oracle hashes are separate.
  No dependency, lockfile, CI guard/inventory or threshold change. Raw benchmark
  evidence stays excluded; no new test file or nested test module path.

The checkpoint-53 gateway CI report independently repeats the sustained-write
failure: all 95,100 acknowledged writes remain present with no failed requests,
but terminal throughput is 0.721520 of early windows and p95 is 1.568081×.
For 5,943 sixteen-commit build samples, early→late log-order p95 is
2.133→8.116 ms. A direct baseline and timestamp/request alignment remain absent;
this is a profiling target, not a cause or an optimized-performance claim.


## Checkpoint 56 — typed fetch failures and current worktree fixtures

The unchanged full RustFS add/push harness stopped on a corrupt pack: checkout
and local HEAD correctly failed, but the helper discarded the typed failure and
reported `CRAB-E0060`. Fetch planning/admission/generation now use one conversion
through the existing `ReadFailure` owner. Its diagnostics borrow typed integrity,
origin and metadata facts while retaining the original error, including shared
reads and close-after-failure wrappers. Corrupt Git pack/index data reports the
existing `CRAB-E0020`; storage/authentication failures retain their category.
No enum variant, format, configuration, fallback or dependency is added.

Fatal pack-sideband reporting retains the original failure if Git disconnects.
Pre-response ERR packets and pack-sideband errors keep their distinct wire phases.
The payload future is polled once under read admission; this change adds no stream
replay. Upstream `gix-pack 0.70.0` distinguishes index I/O from corrupt/unsupported
index bytes, and transparent source wrappers require direct typed-field traversal.

| Evidence cell | Checked boundary |
| --- | --- |
| Changed surface | Core read/write RemoteGit conversions, borrowed read diagnostics, upload-pack wire and classic helper fetch adapters |
| Entry / caller | Real Git clone/fetch through helper batch and stateless protocol-v2 dispatch |
| Callee / owner | Shared upload-pack planner and remote Git repository generation return typed read/remote errors; product diagnostics belong in `ReadFailure` |
| Siblings | Normal/cached protocol v2, classic incremental selection/admission, shallow/filter and promisor generation use the same conversion |
| Tests / contract | Typed corruption/source retention, nested provider diagnostic parity, exact fatal packet framing and disconnected-client reporting; current Capsule worktree publication/hydration |
| Main / previous behavior | Source inspection of current main retains the stringifying wire/read adapters and write-to-I/O conversion. The same-origin live comparison uses final checkpoint-55 roles, not main; no live main baseline is claimed |

Is this the best fix? Retain the source once and reuse the existing diagnostic
owner instead of assigning every remote failure a protocol string or introducing
a new error hierarchy. Runtime Rust grows by 78 lines to classify borrowed typed
causes and preserve fatal-reporting ownership. Total Rust decreases by 900 lines.

The old linked-worktree integration target reproduced a missing file-index failure
on checkpoint 55 because its native producer still wrote obsolete manifest state.
Four useful behaviors move to the private flat `capsule_worktree_tests.rs` module:
xorbs before Capsule visibility; hydrate/edit/push/reclone exact bytes; concurrent
sibling staging; namespace contention retaining a recipe through successful retry.
They use the real current Capsule publisher and canonical hydrator. The old
1,454-line integration file and 510-line prepopulated-walk target are retired;
obsolete push-state/trace assertions are not current stored-format contracts.
The cohesive 893-line sibling fixture avoids a new public test API or nested
`feature/tests.rs` path. The native publisher API and its remaining fixtures still
require removal; current product CLI/helper publication already uses Capsule.

Final qualification:

- Both final-source platform suites pass 381 tests each: errors 74, wire 44,
  helper 153, Capsule publisher 17, current worktrees 4 and hydration 89.
  The new corruption diagnostic test fails before repair. Original failed attempts
  and the obsolete fixture failure remain outside the checkout.
- Regular combined-feature CLI library Clippy completes with 635 macOS / 634
  Linux warnings. Diagnostic level/code/file/message inventories exactly match
  checkpoint 55. Two inherited style spans overlap edited fetch admission; this
  is not zero-overlap proof. No lint policy or suppression is relaxed.
- Optional broader test lint remains red: 58 binary-test assertion errors in
  unchanged checkpoint-55 source (all source fragments also occur on current
  main); library-unit lint reports 7,526 errors / 1,557 warnings. The frozen-source
  attempt matches the earlier diagnostic inventory. Its only edited primary
  spans are the same two inherited style warnings. No whole-test lint signoff.
- The same retained damaged Capsule (pack magic `QACK`) is read with separate
  empty caches by checkpoint 55 and final roles. Both clones fail with exit 128,
  no HEAD and no payload. Diagnostics change E0060 → E0020; raw root/ref and
  damaged Capsule bytes stay identical. Four final installed roles remain fixed
  through native qualification.
- The unchanged full checked-in add/commit/push harness passes 195 checks /
  374 commands, including corrupt pack/index refusal, concurrent CAS, committed
  restaging, partial overlap and cross-repository reuse. Mirror namespace passes
  20/57; partial clone passes 92/324 with Git 2.39.5. Fifteen final push-result
  JSONL events appear on stderr and none on stdout.
- Both final Make-installed role sets pass the checked-in 6 MiB reuse smoke's
  25 checks. Fresh Linux foreground FUSE/NFS RO/RW and managed FUSE RW, plus
  macOS foreground NFS RO/RW and helper NFS RW, pass full/range bytes, RO/EROFS,
  actual owner inspection, fsync → live commit/push → independent eager clone/fsck,
  ordinary detach and foreground exit zero. All omit `--ref`; all role hashes
  stay unchanged. No forced cleanup, cache reset or admission bypass.
- `native-publisher-retirement/source-manifest.json` binds 1,556 source/build
  files and both deleted fixture paths. Final qualification and separate driver/
  oracle hashes bind the tests, exact-source roles, same-origin fault replay and
  reports. Raw benchmark artifacts remain excluded. No dependency, lockfile,
  checked-in guard/inventory or performance-threshold change.

Checkpoint-55 CI passes SDK, GC, cache, large-repository, provider, replica and
HTTP workflows. Architecture and gateway remain red; partial clone was queued at
the last snapshot. NFS fails its feature-gate suite (587 pass, one failure, one
ignored) at the post-shutdown endpoint-lease reacquisition, before native jobs.
A Linux/macOS kernel probe confirms that implicit close retains flock ownership
while a forked child retains the same open-file description; existing cache guards
already explicitly unlock for this reason. A deterministic endpoint regression
and scoped lease guard are prepared outside the checkout, not applied here.
This is the next correctness repair; the CI cause is not declared proven by the
kernel probe alone. The full hardening goal remains active.

## Checkpoint 57 — explicit NFS endpoint lease release

Checkpoint-55 CI failed its post-shutdown endpoint reacquisition. The control
owner already joined handlers and performed inode-conditional socket cleanup,
but closed its lease descriptor without explicitly unlocking. A concurrent fork
can retain the same open-file description until exec, extending flock ownership
after the serving owner has returned. Both Linux and macOS kernel probes reproduce
that condition; Rust 1.97 documents that `File::try_clone` shares the underlying
handle, and locked fs4 0.13.1 uses rustix flock on Unix.

A private `UnixEndpointLease` now explicitly unlocks on drop. The existing
acquisition, shutdown order, socket identity checks and persistent lock-file inode
stay intact. Preflight and server errors receive the same release policy without
repeating cleanup at each return. The runtime change adds 15 Rust lines; no public
API, dependency, configuration, wire format, delay or retry policy is added.

| Evidence cell | Checked boundary |
| --- | --- |
| Changed owner | `nfs_control` private lease guard, consumed by Unix preflight and serving |
| Entry / callers | CLI foreground/helper NFS and daemon NFS through `nfs_mount::run_until_cancelled`; daemon uses the same control owner |
| Callee / dependency | fs4 exclusive flock/unlock; Rust `File::try_clone`; actual duplicated-descriptor and fork kernel behavior |
| Release boundary | Existing connection drain → conditional socket cleanup → listener drop → lease drop; early bind/preflight errors also drop the guard |
| Siblings | Cache lifecycle and mount-cache guards already explicitly unlock. FUSE IPC has separate coordinator/cache ownership. TCP uses socket binding, not this flock guard; Unix TCP-override port lifetime under fork remains separate follow-up. Changed code is excluded on Windows |
| Existing tests | Duplicate bind, persistent inode, replacement-path preservation, blocked snapshot drain, idle connections and reply deadlines remain in the full NFS-feature suites |
| Main comparison | Current main has no persistent endpoint lease and uses socket bind/unlink admission. The direct lease baseline is checkpoint 56, not a claimed equivalent main test |

Is this the best fix? Put release responsibility in the private owner so normal
completion and every early error share it. Reusing the cache directory guard
would import different path/cleanup policy; unlocking only two success branches
would miss errors. No generic lock abstraction or new test API is needed.

The regression keeps a duplicate descriptor alive, proves exclusion before owner
drop, then requires immediate next-owner acquisition. Dropping the old duplicate
must not release the new owner's lease. Its source is byte-identical before/after
the fix. A single-field wrapper first preserves the previous implicit File-close
behavior; that baseline fails with `WouldBlock` at next-owner acquisition on both
platforms. Adding only the release implementation makes the same test pass. This
reproduces the release mechanism, not the exact nondeterministic CI schedule.

Final qualification:

- Linux NFS/gix-facade library suite: 589 passes, one existing ignored case.
  macOS: 591 passes, none ignored. The package has only its library target;
  its enabled doctest slices complete with zero tests on both platforms.
- Scoped VFS Clippy completes with zero diagnostics on both platforms. Empty,
  FUSE-only and NFS-only library checks pass. No lint suppression changes.
- Both final Make-installed role sets pass the checked-in 6 MiB reuse smoke's
  25 checks. Eight fresh native cases omit `--ref`: Linux foreground FUSE/NFS
  RO/RW and managed FUSE RW; macOS foreground NFS RO/RW and helper NFS RW.
  Full/range bytes, EROFS, serving PID, fsync → live commit/push → independent
  eager clone/fsck, normal detach and successful foreground exit pass. All four
  role hashes stay fixed on each OS; no forced cleanup or cache reset.
- The unchanged-source Linux feature baseline passes 588 tests with one ignored.
  Its first generic runner lacked the real read-only Git metadata and failed two
  ambient-context fixtures; the same binary passes after attaching that metadata.
  Both original logs are retained. No test or oracle is changed for that recovery.
- `endpoint-lease/source-manifest.json` binds 1,557 source/build/doc files,
  including the touched README. Qualification binds immutable checkpoint-56
  originals, failing baselines, unchanged regression, upstream contracts, feature/
  lint results, exact-source roles and native reports. Separate driver hashes are
  retained outside the checkout. No lockfile, guard/inventory or threshold change.

Checkpoint-56 full Git qualification (195-check add/push, 20-check mirror and
92-check partial clone) is retained; those whole Git suites are not rerun for this
private NFS release change. Its workflows remain queued at the last snapshot.
Return to the obsolete native publisher/fixtures and remaining stored pack-v1
dispatch. Daemon/private native proof, TCP overrides, native target identity,
Windows cache locking and all-mode performance/durability gates remain open.

## Checkpoint 58 — publish physical Git history

Current Capsule publication now uses the same physical Git object graph for
packing, visibility, object kinds and fast-forward admission. Native Git's local
shallow markers, `info/grafts` and replacement refs previously changed those
queries independently. An incomplete first push could return success and expose
a tip whose eager clone failed `git fsck`. A graft/replacement could also make a
divergent update appear fast-forward; a forced cross-kind replacement changed the
kind reported for an immutable blob OID.

`crab_git::raw_object_command` owns the shared mechanical interpretation:
`--no-replace-objects`, an explicit Git directory, an empty alternate shallow file
and an empty `GIT_GRAFT_FILE`. Callers retain transport restrictions, lazy-fetch
policy, subprocess lifetime and receive policy. No client history file is edited.
Incomplete first pushes fail before refs change; incremental shallow pushes can
reuse ancestors already published at the excluded remote tip. Fetch repacking
keeps its deliberately selected shallow frontier.

Current publisher tests also cover HEAD policy, atomic/non-atomic changing refs,
delete/create batches, missing objects, canonical v2 plan intent/replay and lease
release. An empty batch needs no remote Root, but still enters durable plan
admission and checks cancellation. Final review caught an unpublished shortcut
outside admission; the extended replay test fails before the ordering repair and
passes unchanged afterward, for both empty and nonempty batches.

Tests use flat `capsule_push_tests.rs`, `capsule_push_policy_tests.rs` and
`batch_tests.rs` siblings. The existing Capsule CI filter runs all 32 tests;
the batch test move is byte-identical. No Native producer is deleted in this
checkpoint: porting its useful coverage exposed these integrity defects first.

Evidence map:

| Changed surface | Entry / caller | Owner / callee | Sibling and baseline proof |
| --- | --- | --- | --- |
| Pack and visibility graph | `crab push`, helper push, mounted commit/push → Capsule publisher | CLI policy → `crab-git` mechanics → native `pack-objects` / `rev-list` | Both push commands, both OSes; eager/filtered clones and unchanged local history controls |
| Fast-forward admission | Capsule `is_ancestor` / `has_direct_parent` | Same physical graph → `merge-base`, `cat-file`, parent walk | Graft and replacement rewrite denials; four unchanged regressions fail before repair on each OS |
| Immutable object queries | Shared kind/blob consumers, auth receive/view, verified read/fsck | `object_kinds_from_git_dir`, `verify_git_dir_blobs` → native `cat-file` | Cross-kind regression fails before repair; shared crate minimal/facade and auth consumer tests pass |
| Plan admission | CLI/helper → `run_capsule_push` → `with_capsule_plan` → `run_inner` | Durable intent/replay owner encloses no-op completion | Linux empty-batch replay baseline fails; identical extended test passes after ordering repair |
| Fetch sibling | Remote-helper shallow/filter paths | `repack_shallow_fetch` retains its explicit client frontier | Current-source 92-check partial-clone suite passes; physical gix OID/pointer reads already ignore local rewrites |
| Current main / old producer | Main native commands lack one physical-history policy | Static main source retained; behavior reproduced against checkpoint 57 and the qualified shallow-only candidate | No full main E2E claim; obsolete Native entry and legacy publication remain explicit retirement work |

Dependency contract proof: Git's [replacement documentation](https://git-scm.com/docs/git-replace),
and supported-floor Git 2.39.5's
[global option parser](https://github.com/git/git/blob/v2.39.5/git.c),
[shallow reader](https://github.com/git/git/blob/v2.39.5/shallow.c) and
[graft environment handling](https://github.com/git/git/blob/v2.39.5/environment.c).
The empty alternate shallow file disables the local boundary; empty graft input
prevents local graft ancestry. Linux runs Git 2.39.5; macOS runs Git 2.50.x.

Final-source qualification, external `history-controls/qualification.json`:

- 281 CLI tests pass each OS: Capsule 32, current worktrees 4, helper 153,
  command push 46, pack 35, incremental walk 11. Shared Git: 288 minimal and
  289 facade passes each, with one existing ignored test. Auth-server consumers:
  75 Linux passes. Shared Git Clippy has zero diagnostics on both OSes.
- Final Make installs all four roles on each OS. Fresh native runs omit `--ref`:
  Linux foreground FUSE/NFS RO/RW plus managed FUSE RW; macOS foreground NFS RO/RW
  plus helper NFS RW. Eight cases pass full/range reads, RO refusal, serving-owner
  checks and writable fsync → commit/push → independent clone/fsck. Ordinary
  detach succeeds, foreground owners exit zero, no forced cleanup. Checked-in
  6 MiB reuse smoke passes 25 checks each OS; all four role hashes stay fixed.
- Each OS passes six shallow-history cases and ten local-history/type/policy
  cases, using both CLI and helper. Positive graft/replacement/cross-kind cases
  include full and real blob-filtered clones, ancestor reads and `fsck`. Negative
  cases preserve refs and history-control bytes. Final binary hashes match the
  native roles. The fixture's initial receive-policy path was corrected from
  project `crab.toml` to supported `.crab/local.toml`; its failed report is kept,
  and no product parser or denial oracle was weakened.
- Final installed Linux roles pass checked-in full add/commit/push at 16 MiB:
  195 checks/374 commands; mirror 20/57; isolated partial clone 92/324. Fresh
  `history58` run IDs/buckets avoid earlier-run authority. Reports record actual
  parent `e6a3497fcf1` plus dirty source, bound to 1,561 source/build/doc hashes;
  they do not pretend to be compiled from the later commit.
- Final CLI SHA-256: Linux
  `1072e19ef98cadabf62433cd6a42070cff30a8df11c62934db80d27c4fdb8035`;
  macOS `6cee17c5962060437debd6f07fe47e4aeba319f7b2a118f1008d693e79b51a86`.
- Empty-feature CLI checks pass. Normal library Clippy retains identical
  checkpoint-56/phase-one inventories: 635 macOS/634 Linux warnings, no errors.
  Optional whole-library test lint remains failing at 7,526 coded errors and
  1,557 warnings. Its normalized diagnostic inventory matches checkpoint 56,
  including moved source spans; no primary diagnostic targets the new policy
  file. The metadata-only audit uses actual unit compiler arguments and verifies
  final Cargo externs, features and build-script environment. It is not a green
  whole-test lint gate. Format/diff checks pass; no suppression changes.

Is this the best fix? One low-level command constructor makes immutable identity,
graph coverage and ancestry checks agree without deleting intentional client
history controls, adding settings or retaining another producer. Runtime grows
five net lines across six touched modules; the shared constructor replaces
duplicated command setup. New test surface protects current publication policy
before retiring old fixtures. No dependency, lockfile, vendor, CI guard or
inventory change is included. All 2,156 original raw benchmark files remain
untracked and excluded. Full hardening and optimized performance remain open.

Next: finish obsolete Native/manifest publisher and fixture retirement, then
metadb/GC/maintenance/replication authority. Preserve useful multibatch, overlap,
shallow and follow-tag coverage on current Capsule paths. The sustained-write
profile, bounded admission and remaining lifecycle/Windows gates below remain
required. The separate concrete guard/inventory candidate is still unapplied
pending explicit approval; the NFS dependency approval does not authorize it.

## Checkpoint 59 — Native entry retirement

Status: source and final installed roles qualified locally/dedicated Linux.
The separate CI guard/workflow candidate was subsequently approved by the human
and applied at checkpoint 62.

The obsolete `git::push_native` module, public Native configuration/input API,
push-state discovery, old manifest producer entry and its fixtures are removed.
The current CLI's only live use of that module was follow-tag selection. That
private helper now belongs to `cmd::push`, using shared strict tag peeling and
physical commit reachability. Worktree HEAD still comes from the selected private
Git directory; objects and shared tags come from its common directory. Bare and
frozen-OID sources retain their meaning. Remote missing-tag filtering stays in the
existing captured Capsule ref-view owner. `--no-incremental` remains active.

Mirror no longer writes the unconsumed Git-only environment knob. Its publication
plan identity uses `crab_remote::local::PUBLICATION_PLAN_ID_ENV` directly; the
current helper already reads that canonical constant. No alias or replacement
Native wrapper is retained.

Clippy identified newly orphaned handoffs after the entry removal. The cleanup
deletes precomputed-walk injection, exact-ref state/branches, pre-acquired-lock
entry/DTO/setters and the committed-release wrapper. The remaining generic
publisher acquires its own leases. Shared `crab-remote` committed-release behavior
has current Capsule callers and is preserved. Five tests for removed internal
modes are deleted; one retained generic journal test installs its fixture lease
state directly with its publication/recovery oracle unchanged.

Six obsolete Native/manifest fixture groups leave `remote_helper_transcript`.
Retained refname, fetch reachability, peeled-tag, hidden-ref and protocol-edge
modules compare byte-identical. All seven remaining generic add/commit tests are
also byte-identical, including the explicit 65-chunk multibatch assertion,
overlap and staging-recipe coverage. Their publisher remains retirement work;
this checkpoint does not pretend those fixtures exercise current Capsule.
The removed Native follow-tag integration duplicate is covered by current
command tests and the checked-in real CLI smoke.

Evidence map:

| Surface | Entry / caller | Owner / dependency | Proof and sibling boundary |
| --- | --- | --- | --- |
| Follow-tag selection | `cmd::push::run_push_once` | Private command helper → `crab-git` ref/OID resolution and strict annotated-tag peeling → incremental commit walk | Four flat sibling tests: main/linked/bare HEAD, lightweight/unreachable/explicit exclusion, missing tag objects, frozen tag OIDs; actual CLI smoke verifies publication |
| Mirror subprocess | Legacy mirror and reconciliation apply → Git helper | Canonical plan identity constant → current Capsule admission | 75 command/mirror tests, real 20-check mirror suite; removed Git-only env has no current reader |
| Orphaned publisher handoffs | Only the removed Native entry supplied them | Generic publisher keeps its ordinary lease and coherent-snapshot path | Constructor/caller search proves removed states had no remaining production setter; generic admission, refresh, CAS retry, compaction and lease tests pass |
| Current publication sibling | CLI/helper/mounted commit/push | Existing Capsule publisher, Xet preparation and shared committed release | Current publisher/helper/worktree tests plus eight fresh native cases and full Git suites |
| Main comparison | `origin/main` retains Native CLI calls and the old module | Static main source snapshot retained | Behavioral baseline is checkpoint 58; no main live equivalence claim |

Final-source proof in external `native-owner-removal/`:

- 401 checks pass each OS: focused CLI 360 (including four new command follow-tag
  tests, Capsule 32, worktrees 4, helper 153, command push 46, mirror 75, pack 35,
  incremental walk 11), transcript 25 and generic publisher 16.
- Empty-feature checks pass. Production library Clippy has no errors or added
  diagnostics: 629 macOS/628 Linux warnings, six fewer each. Optional whole-unit
  metadata-only Clippy remains failing: 7,484 coded errors/1,536 warnings. No newly
  diagnosed primary spans or diagnostics in the new follow-tag test file; two
  inherited large-future warnings change measured size at unchanged call sites.
  No lint suppression is added to production.
- Four roles are installed through Make on each OS. Checked-in 6 MiB reuse smoke
  passes 25 checks each. Eight fresh cases omit `--ref`: Linux foreground FUSE/NFS
  RO/RW and managed FUSE RW; macOS foreground NFS RO/RW and helper NFS RW. Visible
  full/range bytes, RO refusal, serving owner, fsync → commit/push → independent
  clone/fsck, ordinary detach and foreground exit zero pass. No forced cleanup;
  all four role hashes remain fixed on each OS.
- Final installed Linux roles pass full 16 MiB add/commit/push (195 checks/374
  commands), mirror (20/57) and isolated partial clone (92/324). Fresh `native59`
  namespaces bind reports to actual parent `74d0da3b22c` plus dirty sources and
  1,567 source/build/doc hashes. Linux CLI SHA-256:
  `68107ada67eafd089329c1cbf9fb2582a024e58c5251dc84a04b1d4465fd9307`;
  macOS `cd1b5d8d64af6723491b11de0059c457c012a91bbba0b3fc833edfe199ce7892`.
- The optional ref-edit feature check passes. Direct minimal SDK-profile proof
  passes 32 current Capsule tests and four follow-tag tests; the exact replay
  selector separately passes one test. Candidate Python/YAML and touched SVG syntax pass; its
  strict tag-owner and pack-layout delegations pass a scoped external trial.

Is this the best fix? Delete the producer and all newly orphaned modes; keep the
one useful command policy at its caller and reusable Git mechanics below it.
This removes 2,021 runtime lines without adding compatibility shims or new
settings. The generic manifest publisher, initializer and remaining authority
dispatch are explicit next work, rather than being relabeled as complete v1
retirement. The checkpoint-58 runtime delta is corrected to five net lines: the
earlier calculation stopped `push.rs` at a leading test-only import and missed
two deleted command-setup lines; behavioral evidence is unchanged.

The concrete 150-line, four-file CI candidate was prepared outside the checkout
and subsequently approved/applied at checkpoint 62. It moves
strict tag delegation to the command owner, follows current SDK paths/replay
selection, and replaces both obsolete partial-clone Native selectors with current
follow-tag coverage (plus Capsule coverage where missing). It retains the earlier
pending actual-dependency/Capsule-delegation/Git-LFS override/integration-selector
corrections. Forbidden raw peeling/pack patterns and performance thresholds stay
intact. Approval covers exactly those four guard/workflow files. No dependency,
lockfile or vendor change is included; the earlier approved NFS fork is separate.

## Checkpoint 60 — measured read dispatch and sustained-write costs

Status: qualified intermediate source, superseded by checkpoints 61/62. The
separate protected CI patch is approved/applied at checkpoint 62. Checkpoint-59's
exact 1,567-file source is retained
in an external hashed archive before these VFS edits; its evidence is not rebound
to changed sources. `origin/crab-v2` remains `877b832b396` and `origin/main` remains
`cc2e4e25f46` after a fresh fetch.

A real Linux kernel data-read probe reproduces FUSE head-of-line blocking. Open a
known-size ordinary Git file, remove only the fixture-created loose object, then
hold its lazy Git fetch. An unrelated directory lookup under a different parent
cannot finish before release with the old single FUSE dispatcher. NFS finishes
that lookup in RO and RW modes. Exact bytes and ordinary detach pass. The first
probe stalled classification during pathname lookup instead; both protocols
blocked there. That weaker probe is retained, not used as dispatcher proof.

`mount::mount_config` now selects two to eight fuser workers, scaled to available
CPUs, for read-only Linux mounts. No setting, dependency override or detached
reply-task path is added. Foreground, coordinator and daemon share that owner;
fuser joins callback workers through the existing session teardown. Its locked
0.17.0 source documents a default of one worker and supports multiple workers
only on Linux. Current `origin/main` also leaves that default selected.

Writable mounts deliberately retain one dispatcher. The first unrestricted
candidate passed the blocked-read probe in both modes but failed four existing
native mapped refresh/switch/automatic-refresh/inspection tests: a paused pending
WRITE was overtaken by a control and the visible bytes became the diverging base.
All four pass against the preserved single-dispatcher source. No test or oracle
was changed. Stronger server callback/writeback admission is required before
writable concurrency; changing thread count alone is insufficient.

Evidence map:

| Surface | Entry / owner / callee | Sibling and proof |
| --- | --- | --- |
| Linux RO FUSE dispatch | Foreground/helper, coordinator and daemon → shared `mount_config` → fuser `Session` worker dispatch/join | Actual installed helper: four workers on four CPUs; unrelated lookup completes during held data read; exact 1 MiB bytes and ordinary detach |
| Writable FUSE | Same mount owner; no concurrency enabled | Actual installed helper retains one worker; blocked lookup remains an explicit performance gap; mapped-transition baseline/candidate comparison protects correctness |
| NFS | Same engine, independent NFS transport owner | RO/RW unrelated lookup remains responsive; full bytes and ordinary detach |
| Lifetime and shared reads | Existing engine/open-file, hydration, overlay and mount controls | Existing privileged native suite covers mappings, commit/remount, LFS, refresh/switch, open-file replacement and detach; no new callback lifetime abstraction |

The optimized gateway diagnostic uses unchanged product/client sources frozen
before the VFS change, 300 seconds per endpoint, 30-second windows, 16 writers
and 4 KiB objects. The task-owned Colima data disk was increased from 80 to
160 GiB through Colima, preserving all other configuration and existing data;
a five-second capacity probe showed the original free space was insufficient.
No acceptance threshold was changed.

- Gateway: 81,278 ACKs, zero failed requests, exactly 81,278 listed keys and 48
  post-timing sample GET byte checks. Independent native Git clone and strict
  fsck pass: 81,279 commits/files including the seed; every committed blob
  matches the original payload, totaling 332,914,709 bytes. The owner exits zero
  without forced cleanup.
- The CI terminal slowdown is not reproduced here: terminal/early throughput
  1.006369, p95 ratio 1.173877. This is no CI repair or full performance signoff.
- Direct baseline: 709,883 ACKs and zero PUT errors, but the original listing
  check fails. A separate full-list audit fails with HTTP 500 on page 512.
  The original paired report remains failed, including integrity failure;
  successful gateway proof does not turn that report green.
- Existing timestamped phase logs align with actual client windows: clock-offset
  drift 522 ns, maximum anchor-read skew 1,583 ns. Across 5,079 batches, build
  p95 grows from 0.993 to 3.352 ms while encoding, upload and ref-CAS remain much
  steadier. Maximum sampled RSS is 1,330,472 KiB. These are aggregate associations,
  not proof of an individual-request cause or a live-allocation bound.

Final read-only candidate proof: 679 VFS tests pass (19 privileged tests ignored
in that run); crate library Clippy reports zero warnings/errors; all 18 dedicated
native tests pass, including the four mapped-transition regressions. Four actual
installed CLI cases verify FUSE/NFS RO/RW. FUSE uses four workers in RO and one in
RW on this VM. All role hashes stay fixed through testing; only the FUSE helper
changes from checkpoint 59. Core CLI, NFS helper and cache server hashes remain
identical. New Linux FUSE helper SHA-256:
`37d042e0984ff509344b58ff7ce6651bc58a2f6e941c853afaf5e9a1d483ffc7`.
Format and diff checks pass. No macOS native rerun is claimed for this Linux-only
runtime change; checkpoint-59 macOS proof retains its original frozen source.

External `gateway-profile/` retains the failed paired report, timestamped service
log, clocks, metrics, process samples, independent all-byte Git audit and the
separate baseline failure. `vfs-dispatch-profile/` retains the stronger native
probe, initial unrestricted failure, restored-source baseline, final candidate
and source/role hashes. No raw evidence enters the PR.

Is this the best fix? For read-only dispatch, use the dependency's intended
concurrency at the single shared mount boundary and retain its joined lifetime.
Writable concurrency needs a real callback/writeback fence first; its four native
regressions are not waived. Growing tree-build cost and RSS need allocation/CPU
proof before a gateway optimization; a local run without the CI symptom cannot
justify a speculative fix.

Immediate priorities: pending-WRITE/control fencing for writable FUSE; managed
read-only dispatch qualification and global in-flight read memory; gateway
allocation/tree-build diagnosis and an equivalent CI slowdown reproduction;
remaining generic manifest publisher, metadb/GC and replication authority removal.

## Checkpoint 61 — entered WRITE admission and writable dispatch

Status: qualified writeback change; the demonstrated startup failure is repaired
at checkpoint 62. The separate CI inventory/guard patch is approved/applied there.
The objective remains open. Checkpoint-60 source is
retained in a hashed external archive before this change; checkpoint 61 has its
own 1,567-file source manifest/archive and platform-specific proof.

The shared engine now admits native WRITE callbacks through a separate read
lock before FUSE waits for namespace/inode ownership or NFS resolves its target.
Quiescence takes that gate exclusively before the existing mutation gate and
retains both through reset/adoption, inspection or export. An entered WRITE can
therefore reach its mutation gate and finish before the control freezes it.
Inode-lock ordering remains unchanged. Owned guards preserve cancellation and
shutdown behavior; no detached reply tasks, retry setting or dependency patch is
introduced.

Linux FUSE dispatch now uses its bounded worker pool in both RO and RW modes.
The four mapped refresh/switch/automatic-refresh/inspection tests that failed
under unrestricted dispatch without admission now pass unchanged. A new engine
unit test proves a queued quiescence still lets an admitted write reach actual
overlay storage; two native mapped-export tests prove FUSE/NFS exported bytes
include the pending edit. No prior oracle is weakened.

Evidence map:

| Changed surface | Entry / owner / callee | Sibling and contract proof |
| --- | --- | --- |
| Native admission | FUSE `write` and NFS `write` → engine native-writeback read guard → existing mutation/COW/storage path | Both callbacks retain admission through their result/reply boundary; ordinary engine writes retain their original mutation policy |
| Control freeze | `begin_mount_update`, live inspection and live export → shared `quiesce_overlay` → native drain then mutation freeze | Refresh, switch, commit/reset and automatic refresh use that boundary; offline CLI paths have no live engine/accepted callbacks |
| Lock/lifetime ordering | Inode ownership remains before engine mutation admission; controllers acquire neither inode lock nor mounted-path reads | Tokio 1.53.1 owned RW guards provide FIFO writer preference and RAII cancellation; native admission drains before blocking the writes it awaits |
| Native dependency | Shared FUSE mount config → locked fuser 0.17.0 worker pool/session join | `origin/main` leaves one dispatcher selected; current Linux uses four on this VM, bounded to 2–8 by available CPUs |

The VM runs Ubuntu kernel `6.8.0-117-generic`. The upstream
[6.8 FUSE sync handler](https://github.com/torvalds/linux/blob/v6.8/fs/fuse/inode.c)
skips the backend roundtrip when `sync_fs` is disabled. Upstream
[fuser's contract notes](https://github.com/cberner/fuser/blob/master/CHANGELOG.md)
explain that ordinary fuser mounts do not receive that roundtrip. A dependency
upgrade or assumed `syncfs` callback is therefore not the owner fix. These sources
explain the observed admission gap; they do not establish an arbitrary client
mapping/unqueued-write guarantee or close macOS mapped draining.

Dedicated Linux proof:

- 680 unit tests pass; 21 privileged/performance tests are ignored in that run.
  All 20 native mount tests pass separately, including the four old regressions
  and two new FUSE/NFS mapped exports. VFS library Clippy has zero warnings/errors.
- Four roles installed through Make. The actual installed FUSE/NFS RO/RW helpers
  all complete an unrelated directory lookup during a deliberately held data
  read, return exact 1 MiB bytes, detach normally and exit zero. FUSE has four
  workers in RO and RW. Core CLI SHA-256:
  `6aa2a18a461598f3da8a6495f930d1418c6bdbfc96e72b140b5be928fca5dd0f`;
  FUSE helper `8bb254d0fc92039f936e703172a9a5558df16fc69a281b085af1e2124e0b97d1`.
- The first provider harness stops at preflight because its runner omitted the
  installed AWS CLI directory from PATH. That failure is retained. With PATH
  restored, the unchanged checked-in 6 MiB reuse smoke passes all 25 checks.
  Foreground FUSE RO/RW then pass through RW fsync/commit/push, independent clone
  and strict fsck. The full native harness stops later at foreground NFS RO
  live-status failure, despite correct file bytes. NFS RW and the managed case
  are not reached in that run; it is not reported as a full Linux E2E pass.

macOS proof: 88 focused checks pass (native admission 1, mount runtime 22,
cancelled control 1 and NFS adapter 64); VFS Clippy has zero warnings/errors.
Four roles installed through Make. Fresh 6 MiB reuse passes 25 checks, then
foreground NFS RO/RW and helper NFS RW pass exact full/range reads, RO refusal,
live identity, write/fsync/commit/push, independent clone/fsck and ordinary
detach. All role hashes stay fixed, without forced cleanup. The existing large
`__eh_frame` linker warning is also present in checkpoint-59 logs; it is not a
new diagnostic or a performance qualification.

The Linux control failure is retained and minimized, not retried until green.
Six fresh ordinary NFS RO diagnostics return live status successfully, but a
task-only LD_PRELOAD bind-delay probe reproduces the ordering gap deterministically:
preflight binds twice while unmounted, the serving control bind happens only
after native visibility and the printed "Mounted" message. While that third
bind is held, 6 MiB reads succeed and live-only status exits 6. Releasing it makes
status exit zero; the owner then detaches normally and exits zero. Product code,
protocol expectations and acceptance thresholds are unchanged by the fault probe.

Source confirms `nfs_mount::mount` creates the kernel mount first; the CLI then
registers/announces it, and only `run_until_cancelled` spawns asynchronous control
binding. The next fix must prepare and own the control listener before exposing
readiness, drain it on startup failure, and cover Unix/TCP plus foreground,
helper and daemon owners. Adding status retries or treating readable data as
control readiness would leave this ownership gap intact.

Is this the best fix? The writeback drain belongs at the shared engine boundary,
with adapters admitting callbacks before waits. This preserves both protocol
surfaces and allows writable dispatch without the four demonstrated data-loss
regressions. Native startup/control availability is a different owner boundary
and remains an explicit failing priority rather than being hidden by these
passing tests. Global in-flight memory, remaining native lifecycle modes,
performance reproduction and stored pack-v1 authority retirement remain open.

## Checkpoint 62 — control readiness before native visibility

The human approved the exact 150-line, four-file CI/inventory candidate. It is
applied without changing raw-peeling/pack prohibitions, performance thresholds,
dependencies or lockfiles. All 75 architecture checks pass. The updated current
Capsule/follow-tag/minimal SDK selectors have direct checkpoint-59 proof; the
full final-integration script passes all seven checks in the dedicated Linux
environment, including protected Capsule receive/retry and both helper doctors.
Earlier checkpoint descriptions retain the historical pending-approval state.

`nfs_control::bind_server` now returns an owned `BoundControlListener`. Public
mount setup validates the actual data address, then binds control before starting
the data server or invoking the native mount command. The session retains that
same listener until serving starts; no second bind or readiness polling is used.
The obsolete late-binding entry is removed and all callers migrate. A Unix lease
retains the socket identity through handler drain and startup rollback, removes
only its own socket, and explicitly unlocks its persistent lock file. TCP holds
the actual bound listener through the same prepare/serve lifecycle.

Evidence map:

| Surface | Entry / caller | Owner / callee / dependency | Proof / sibling boundary |
| --- | --- | --- | --- |
| Native setup | CLI foreground, helper and daemon preparation → `nfs_mount::mount` | Actual-address preflight → prepared control binding → NFS data task → OS mount | Linux bind-delay probe: old installed binary exposes kernel/printed readiness before control; new Unix and TCP paths remain invisible until binding succeeds |
| Serving | `run_until_cancelled` | Session consumes its prepared listener; Tokio bound listener queues connections until accept | First status succeeds without test polling; Unix/TCP lifecycle tests, existing handler cancellation/drain tests |
| Failed startup | Native mount helper failure → session setup unwind | Bound Unix listener/lease Drop → inode-aware cleanup → explicit fs4 unlock; TCP listener Drop closes port | Injected native failure observes prepared socket, no mounted/ready state, released lease; replacement-file bytes preserved; subsequent real mount succeeds |
| Writable sibling | FUSE/NFS WRITE callbacks → shared engine admission | Native-writeback gate drains before mutation quiescence | Existing 20 Linux native tests retain mapped transition/export oracles; both writable protocol cases pass provider publication/clone proof |
| Product siblings | Native foreground and managed owners | Same library mount factory, existing lifecycle controls | Five Linux and three macOS installed-role cases; other daemon/Windows and macOS mapped-writeback qualification remains open |
| Main / baseline | `origin/main` versus selected `origin/crab-v2` | Main is not a live-equivalence target; behavioral baseline is checkpoint 61 | Exact old-binary fault probe retained; no claim of cross-platform production completion |

Final source is frozen across 1,567 files in external `nfs-control-readiness/`;
archive SHA-256 `1ebcb0eedd76aac8e217567a0d91625432eadf4d2a1035fef63d13c8ad7b1641`.
The approved CI files have a separate patch hash and are outside that Rust source
closure. Two TCP test pattern matches receive a test-only portability correction;
all 12 final connection tests pass again. No production code changes after the
full unit/native run. Initial three fixture failures are retained: synthetic
sessions lacked the newly required bound listener; their setup migrated while
the original drain/error assertions stayed intact.

- Linux: 685 unit tests pass (21 ignored), all 20 native mount tests pass, CLI
  mount/unmount 15 + 17 pass. VFS Clippy has no warnings/errors. Whole CLI
  production-library Clippy has 628 inherited warnings, no errors, and exactly
  checkpoint-59 warning signatures; no suppression is added.
- macOS: 173 focused checks pass (admission, runtime, cancellation, NFS adapter,
  control, session lifecycle and CLI unmount); VFS Clippy has no warnings/errors.
  Four final roles install through Make on each OS.
- Linux RustFS reuse passes 25 checks; foreground FUSE/NFS RO/RW and managed FUSE
  RW pass exact full/range bytes, RO refusal, live owner/mode, writable fsync →
  commit/push → independent clone/fsck and normal detach. Core CLI SHA-256:
  `950dc689424f9e202a4e00e9106260738e63af837b833d6ca15d458fb0e38544`.
- macOS RustFS reuse passes 25 checks; foreground NFS RO/RW and helper NFS RW
  pass the same applicable byte/publication/clone/lifecycle checks. All role
  hashes remain fixed on each OS; no forced cleanup.
- Deterministic Linux Unix/TCP bind-delay probes and both startup-failure cleanup
  variants pass. The original checkpoint-61 NFS status failure and failing
  old-binary regression probe remain preserved; no oracle/threshold is relaxed.
- Windows cross/native proof is unavailable: the dedicated image has only the
  aarch64 Linux Rust target and no Windows linker. TCP proof here is Linux;
  no Windows qualification is claimed. Existing whole-unit CLI lint debt and
  the failed paired gateway performance/integrity experiment remain open.

Is this the best fix? Bind ownership belongs before native visibility and must
survive into serving. One prepared listener removes the asynchronous bind race
for every mount caller, while preserving socket identity, handler drain and
failure rollback. Retries would only hide that ordering defect. The extra
production ownership surface replaces the former late-bind path; no compatibility
wrapper, dependency patch, environment option or test-directory pattern is added.
The raw benchmark directory remains excluded. Next retire the generic manifest
publisher and its seven useful fixtures, then metadb/GC/maintenance/replication
fallback authority; keep global memory and remaining lifecycle/performance gates
explicitly open.

## Checkpoint 63 — current Capsule add/commit fixtures

The human explicitly approved the prepared one-file fixture migration, including
the obsolete recipe-owner assertion correction. The exact candidate patch hash is
`dab722f29cdd23a9a3ab485699d86dcf640fd09b08998341dc705632601ff78e`.
Only `crab/tests/e2e_add_commit_push.rs` changes in the source closure; production
code, exports, dependencies, configuration, CI inventories and thresholds are
identical to checkpoint 62. Native/provider proof remains checkpoint-62 proof.

Seven useful integration fixtures now use the existing public publication stage
(`import::publish::run_publish`), which delegates to private Capsule orchestration.
No internal push API is exported for tests. Each selected branch publishes through
that current owner; these fixtures do not establish atomic multi-ref behavior.
They initialize through the real publication owner, open the resulting Capsule
view, compare exact ref OIDs, require the authenticated pointer catalog, retain
xorb/shard existence checks and reconstruct published pointer bytes independently.

All seven test bodies compare unchanged except migrated helper names/arguments.
The original minimum-65-chunk multibatch assertion, partial overlap, cross-branch
recipe lifetime, filter/add behavior and byte-identical reconstruction remain.
Shallow first push now exercises the current error contract and additionally
checks an unchanged root, empty refs and unchanged local shallow-marker bytes.
The retired generic per-ref error wrapper and manifest generation/index assertions
are removed with the old publisher dependency.

The first direct migration fails identically on both OSes: six passes, one failure
before publication at the old published-recipe lookup. That failure and a focused
Linux reproduction are retained. Source/dependency-contract review establishes
that `origin/crab-v2` already delegates verified clean-filter recipes; the filter
cannot attest the outer Git writer's index success. Shared staging documents and
queries distinguish published native transactions from delegated external leases.
`push_staging::recipes_for_selected_pointers` selects delegated leases only after
Git pointer discovery. Changing production to satisfy the old lookup would break
that ownership boundary. The approved fixture instead requires no native published
transaction and an exact delegated recipe for the selected file identity, while
retaining no-segment-rewrite, local reconstruction and later remote hydration.
There is no published/delegated fallback in the fixture. An earlier attempted
private-module call fails compilation and is preserved; the public entry avoids
expanding the product API.

Evidence map:

| Surface | Caller / owner | Callee / contract | Proof / sibling boundary |
| --- | --- | --- | --- |
| Add/filter staging | Actual `crab add`, Git add/filter subprocesses | Prepared payload flush → verified recipe → external handoff; Git index owns pointer visibility | Seven unchanged fixture bodies; skip-index, ≥65 chunks, overlap and cross-branch retention assertions |
| Current publication | External fixture → public import publication stage | Same private Capsule publisher used by CLI/helper → shared initializer/write admission | Exact Capsule refs/catalog, uploaded objects and independent byte reconstruction; no production API change |
| Recipe admission | Git-selected pointer identity → staging lookup | Exact delegated lease; native published state remains absent | Failing old lookup retained on both OSes; strict new owner assertion passes, without changing product ownership |
| Shallow refusal | Actual shallow clone → current publication stage | Physical history validation before ref visibility | Failure plus unchanged root, empty refs and preserved local marker |
| Sibling / main comparison | CLI/helper keep the same private Capsule owner | Current v2 delegates; static main retains the older publication owner behavior | Product hashes identical to checkpoint 62; main is not asserted live-equivalent; atomic/ref-policy proof stays in existing Capsule tests |

Final approved sources pass all seven fixtures on each OS, with zero ignored or
skipped cases; success output is retained and checked for skip messages. Linux
qualification first mounts the candidate as one read-only source overlay, then
all 1,567 resulting source hashes match the approved checkout. macOS runs the
applied source directly. Exact archive SHA-256:
`827bf7e41d2a437dd5851f2cf87b6b9acd6199c91d9ee1cfa18e90e6555bd29a`.
Format/diff checks pass. No raw benchmark path is added.

Is this the best fix? Preserve the useful data/recipe assertions on a current
public caller, rather than exposing internal orchestration or retaining an old
producer solely for tests. The explicit owner correction follows the existing
v2 contract and strengthens the absence of a false native publication claim.
Production LOC delta is zero; the test-only growth pays for current view/ref and
rollback assertions. A prior external note incorrectly inferred seven old fixture
passes from preserved bodies; it is corrected: checkpoint 59 proved 16 generic
publisher unit checks, while these seven integration bodies were retained there.
No earlier pass evidence is invented or rebound.

The refined census finds four actual generic-publisher calls across three suites:
`e2e_incomplete_shard_fail_loud` (two), `e2e_pack_size_limit` (one) and
`prop_push_reconstruction_coverage` (one). `parallel_metadata_upload` has historical
comments only. No production caller outside the generic module remains. Migrate
those behavior/property contracts before deleting the generic pipeline, initializer
and newly orphaned helpers; preserve shared policy/types still used by Capsule.
Metadb/GC/maintenance/replication authority, total memory and remaining platform/
lifecycle/performance proof stay open.

## Checkpoint 64 — restore prepared-control failure context

Fresh checkpoint-63 NFS CI fails before native execution: run `40430901673`,
feature-gate job `121948021245`, release-contract check for redacted endpoint
failure context. The checkpoint-62 listener refactor removed that warning field.
Both `origin/crab-v2` and static `origin/main` retain it. No guard change is needed.

The bound listener now retains its already-redacted diagnostic identity through
serving. Authentication stays in the listener variant; diagnostics do not retain
another raw TCP token. The shared failure boundary includes the endpoint through
the existing redaction helper and preserves the original error. Unix/TCP bind,
readiness, serving, connection drain and lease ordering are unchanged.

A new Unix regression uses real sockets/filesystem operations: ping the prepared
owner, move its private socket directory, replace the original parent with a file,
then cancel. Socket cleanup returns `NotADirectory`; the same warning line must
identify the serving endpoint. The old production source fails that unchanged
assertion. Existing TCP malformed-token and authenticated serving/shutdown tests
remain intact; no simulated TCP accept failure is claimed.

Evidence map:

| Surface | Entry / owner | Callee / contract | Proof / sibling boundary |
| --- | --- | --- | --- |
| Prepared control identity | `nfs_mount::mount` → `bind_server` | One bound listener owns redacted diagnostic identity | Actual binding/startup API unchanged; release guard restored without edits |
| Failure reporting | Session `run_until_cancelled` → bound listener `spawn` | Unix/TCP serving result → shared warning; original error returned | New real Unix cleanup-error regression; same assertion fails against checkpoint-63 source |
| Redaction/authentication | Shared endpoint display helper versus TCP listener token | Diagnostics retain a redacted string; request verification keeps the original token | Existing token-redaction and TCP authentication/shutdown cases pass |
| Lifecycle sibling | Unix serving owns lease through handler drain and socket cleanup | Identity-aware unlink and explicit fs4 unlock | Existing connection, replacement-file, fork-descriptor and cancellation tests retained |
| Main / base | Static `origin/main` and `origin/crab-v2` | Both log redacted endpoint context | This repairs the earlier listener refactor regression; no live-main equivalence claim |

Exact 1,567-file source archive SHA-256:
`c7e1b690b6c100d01a889b7a379d1a192fd84e45c2c6f0f04de9db85665b888a`.
Only NFS control production/tests and its README differ from checkpoint 63 in that
closure. macOS has 32 control checks passing and zero VFS Clippy diagnostics.
The rebuilt full Linux NFS feature gate passes: 596 VFS tests, one existing
ignored, two coordinator response checks, core CLI NFS/gix-all check and all report/
release self-tests. Linux VFS Clippy has zero diagnostics too. The new same-warning-
line regression passes unchanged; no guard or acceptance threshold is relaxed.

The first Linux before/after attempt reused the intentionally failing baseline
unit artifact after restoring older-mtime source from a read-only overlay. Its
full-gate failure is retained. Source timestamps and Cargo's lack of a VFS compile
confirm the cache problem; only this worktree's VFS package artifacts were
invalidated (387 files/2.4 GiB), with its explicit external target directory.
Final qualification rebuilds the package; the regression and all guards stay
unchanged. No other repository target, installed evidence binary or task service
is removed.

Is this the best fix? Retain diagnostic identity at the owner that crosses the
prepare/serve boundary, then report it at the shared failure boundary. This
restores useful context for both protocols and keeps tokens out of diagnostic
state. Relaxing a source guard would leave the observed regression. Runtime grows
ten net lines; no configuration, dependency, lockfile or protected inventory
changes. Fresh CI/native qualification remains separate from local feature-gate
proof. The broader hardening, performance and stored pack-v1 removal goal remains
open; the current Capsule bounded-pack replacement is retained as a separate
prepared candidate while this CI regression is addressed.

## Checkpoint 65 — bounded multi-pack publication on Capsule

The remaining pack-size integration suite no longer invokes the generic manifest
publisher. Its aggregate case is replaced by a current Capsule test under the
existing private policy fixture, in flat `capsule_push_data_tests.rs`. No product
API is exported. All six retained generation/installation test bodies compare
byte-identical, covering over-limit rejection/timing, typed/protocol rejection,
exact-limit acceptance, zero/unlimited semantics and local installation refusal.

The new test commits four distinct incompressible 700 KiB blobs, configures a
1 MiB per-pack limit, and publishes through current Capsule orchestration. It
requires the exact ref OID, one visible Capsule transaction, multiple authenticated
pack descriptors and every pack at or below the bound. It handles both inline
and origin-backed descriptor types. The selected origin installs the full view
into an independent bare repository with validated sidecars, strict full fsck
and exact bytes for every blob.

Evidence map:

| Surface | Entry / owner | Callee / contract | Proof / sibling boundary |
| --- | --- | --- | --- |
| Aggregate publication | Existing policy fixture → private Capsule `run` | Bounded plural pack generation → one Capsule/ref publication | Complete ref/transaction, per-pack bounds, full independent install/fsck and four exact blob reads |
| Pack admission | Existing public Git pack tests | Generation/intake size checks and shared rejection mapping | Six retained cases are byte-identical; no current Git pack contract is retired |
| Read/install | Captured Capsule view and selected origin | Inline body sections versus origin-backed member ranges; authenticated sidecar installation | Both descriptor shapes are included in the size inventory; installer/strict fsck proves complete object closure |
| Sibling policy | Full Capsule test group | Cancellation, direct/coordinated publication, atomic refs, history, leases and replay | All 33 cases pass on both OSes |
| Main / base | Static main/v2 older integration fixture | Old case asserts manifest generation, old pack layout and metadata keys | Replaced only after stronger current-format proof; these obsolete storage details are not current Capsule contracts |

Final source has 1,568 hashed files, archive SHA-256:
`8b3c981fd0a3c34e77ba8ed479ed877540e239e9507a74dc0cfc0b4919c8ca72`.
Both OSes pass 33 Capsule tests and all six retained pack-intake tests, zero
ignored/skipped. The first test draft mixed the two authenticated descriptor
Rust types and failed compilation; its logs are retained. Correct code follows
the reader's explicit descriptor contracts and installs origin-backed payloads
through the selected store. The test body first passes each OS before its flat
file extraction, then the entire final group passes again. Format/diff checks
pass. No dependency, config, guard, inventory, threshold or production code
changes; no fresh native/performance proof is claimed by this test-only update.

Is this the best fix? Preserve aggregate closure/bounds on the actual Capsule
owner and preserve shared Git intake behavior independently. One authenticated
transaction replaces the old manifest-generation assertion; verified sidecars,
strict fsck and exact blob reads replace old-layout metadata existence checks.
Test LOC shrinks overall. One obsolete publisher call and its format-specific
fixture leave; no shim or duplicate producer is introduced.

Three actual generic-publisher calls remain across two suites:
`e2e_incomplete_shard_fail_loud` (two) and
`prop_push_reconstruction_coverage` (one). Next preserve their real CDC, high-chunk,
multifile, negative-publication and reconstruction properties on Capsule before
removing the generic pipeline/initializer and orphaned helpers. Keep current
shared policy/types. The extreme cases currently hide behind environment-gated
returns; qualification must identify actual executed chunk counts and distinguish
skips. The owned Linux environment has 8 GiB RAM: stream fixture generation/staging
and expected-byte comparison rather than retaining multiple whole-file copies.
This is test resource ownership, not a production memory-admission fix. Metadb/GC/
maintenance/replication authority and full mode/performance qualification remain.

## Checkpoint 66 — transfer compact verified Git blob bodies

Cold native ODB reads no longer unconditionally copy the complete decoded blob
before returning it. After the existing type, size and Git object checksum
checks, the reader transfers the decoder buffer through `into_boxed_slice`.
Boxing discards spare capacity before retention. This matters for real deltas:
the fixture decodes a 262,144-byte body into a 524,307-byte allocation; its returned
verified body retains exactly 262,144 bytes. Direct Vec-to-Bytes transfer would
retain the larger allocation under bytes 1.11.1 and is deliberately not used.

The dependency proof reads the pinned gix-odb 0.80.0 cache, loose, packed and
ref-delta return paths: they return the complete body in the supplied buffer.
gix-pack 0.70.0 uses two result work buffers plus instructions and truncates the
final body without reducing capacity. The installed standard-library contract
for `Vec::into_boxed_slice` discards excess capacity; bytes' Box conversion owns
that exact slice. No dependency, configuration or concurrency changes.

Evidence map:

| Surface | Entry / owner | Callee / contract | Proof / sibling boundary |
| --- | --- | --- | --- |
| Ordinary reads | Engine captured BaseBlob → OdbReader | Native gix lookup, kind/size and whole-object checksum before ownership transfer | Both FUSE read_open_file and NFS read_at share this engine boundary |
| Decoder ownership | read_native → boxed verified body | Loose, packed, ref-delta and object-cache paths return the entire supplied buffer | Real loose/delta-packed bodies match; packed fixture removes loose files, verifies a delta, and measures spare capacity |
| Retention | Box-to-Bytes transfer | Discard decoder scratch capacity before retaining output | Returned capacity equals exact body length for both fixture bodies and storage representations |
| Cache and replies | read_blob_id → MemoryCache; read_blob_range | Cache still owns/charges its exact reservation; range replies copy only the selected bytes | No cache policy or reply-lifetime change; this is not a total peak-memory budget fix |
| Sibling fetch / COW | fetch_native and copy_blob | Git fetch already transfers its own verified Vec, without a second gix-body copy; COW retains tracked output ownership | Existing promisor, raw-OID, cancellation, shutdown and range tests pass; fetch allocation growth remains part of the broader budget audit |
| Main / base | Older OdbReader in engine.rs:159 on main and v2 | Cold body copied with data.data.to_vec into an uncapped disk cache | Earlier checkpoints migrated verification/cache/admission; this follow-up preserves those current contracts |

macOS passes all 16 blob tests, including the new compact-body regression,
production VFS Clippy with zero diagnostics, and `make install` for all four
binary roles. Fresh installed NFS read-only, writable and managed cases pass,
plus all 25 checked-in RustFS reuse checks. Proof includes full/range reads,
read-only refusal, write/fsync → commit/push → independent clone/strict fsck,
normal detach, exit zero and unchanged binary hashes; no forced cleanup.
Core binary SHA-256:
`3f9000ae75bc906d3cc8a0b282c6dbe41d261d24e0d60f414f69d6dcefd4d027`.

Runtime review source contains 1,568 hashed files, archive SHA-256:
`8f7492a5ff80c458ee6c10ff0e8f60082f73ded1fbab90406afa6d33c0fdf28f`.
The separately running reconstruction fixture is restored to HEAD in that review
archive. Compiler dependency files prove it is absent from both product and VFS
unit inputs. Its actual running-source archive is preserved separately; no proof
is rebound to a different fixture. The runtime diff adds three explanatory
comment lines and replaces one expression; the flat regression module grows.

Is this the best fix? Transfer the already verified decoder owner and trim spare
capacity at that same boundary. An unconditional copy is unnecessary for ordinary
bodies; a plain Vec transfer would retain delta workspace. Preserve the existing
cold-fill admission, checksum, cache accounting, reply copies and tracked teardown.
No helper, wrapper, compatibility path or wider concurrency is introduced.

Fresh Linux qualification remains pending while the dedicated VM finishes the
previous reconstruction corpus; no new Linux native or throughput claim is made.
The new default reconstruction group passes seven tests on macOS, with three
large cases explicitly ignored there and running separately on Linux. Linux has
already passed exact 2k/5k, deterministic/randomized 22k, and one five-file batch
at 100/2k/5k/22k/27k chunks. The replacement fixture remains uncommitted and both
old suites remain intact until its full corpus completes. Generic publisher
retirement, metadb/GC/maintenance authority, all-mode lifecycle, total memory,
cold-read concurrency, SQLite qualification and performance gates remain open.

## Checkpoint 67 — current Capsule reconstruction corpus

Both remaining external generic-publisher suites are replaced by current Capsule
coverage in the existing flat `capsule_push_data_tests.rs`. Their original files
and hashes remain in external evidence. All replacement cases pass before either
old file is removed; Cargo autodiscovery then confirms both old targets are gone.
Historical benchmark reports naming those targets retain their original evidence.
No production API or dependency changes. Test LOC shrinks by 1,425 lines.

The shared fixture generates an exact number of real Gear CDC chunks, ending at
a boundary, and writes expected bytes to private temporary files. It stages real
bytes in 512-chunk batches, flushes, publishes a verified recipe lease, closes and
reopens staging, then requires complete durable file-version membership. One
memory-heavy scenario owns admission until its staging/origin/file owners drop.
This bounds fixture scheduling; it is not a production memory-budget change.

Valid inputs must publish successfully, rather than accepting the old conditional
E0085 outcome. Current rooted refs, catalog file count, sizes and shard membership
must agree; the CLI hydrator writes every byte to disk, then full byte comparisons
and both EOFs must match. A deliberately inconsistent pointer/recipe size returns
typed StagingCorrupt, leaves the root digest unchanged and publishes no ref.
Current Capsule preparation explicitly permits safe immutable orphans on other
failure paths; old manifest/MetaDb-prefix absence assertions are not a current
storage contract. Complete term construction and missing-placement E0085 guards
remain in canonical crab-xet. No data-plane guard is removed or weakened. The five canonical term/coverage
guard tests pass on both OSes, including missing identity, excess coverage,
reversed/overflowed ranges and repeated chunk occurrences.

Evidence map:

| Surface | Entry / owner | Contract | Proof / sibling boundary |
| --- | --- | --- | --- |
| Real CDC membership | Private pointer fixture → StagingArea | Exact real chunks, bounded batches, verified recipe lease and durable reopen | Exact requested count after reopen; all payload bytes remain independently available for comparison |
| Current publication | Existing private Capsule run with explicit Git directory | One selected pointer set and captured staging ownership before ref publication | Exact HEAD ref and authenticated catalog coverage for every file; no new public test API or global Git-directory mutation |
| Reconstruction | Existing CLI hydration owner → verified streaming writer | Full content hash, exact size and complete output | Whole-file byte comparisons plus both EOFs for deterministic and randomized cases |
| Negative admission | Selected recipe/pointer size disagreement | Typed error before visibility | Unchanged root digest and no visible ref; valid corpus cannot hide behind a fail-loud alternative |
| Shared guards | xet_publication → file_info_from_placements → FileTermBuilder | Every recipe occurrence resolves or errors | Existing canonical missing/overflow/repeated-occurrence checks remain; no old storage layout is retained |
| Main / base | Both removed suites are byte-identical on fetched main and v2 | Three old generic calls; environment-gated returns and manifest/MetaDb assertions | Current-format replacement qualified before removal; original files/hashes and complete test-domain map preserved |

Linux's default Capsule group passes all 39 cases, with three explicitly ignored
large cases. Those three also pass in the dedicated Linux environment with zero
ignored/skipped: deterministic 22k/27k/50k, a single five-file publish call at
100/2k/5k/22k/27k, and four randomized large cases. In total they reconstruct 12
files, 226,100 chunk occurrences and 14,560,023,399 exact bytes. The 50k case alone
is 3,225,368,782 bytes. macOS passes all seven default data cases, including the
three properties with four samples each; its three large cases are explicitly
ignored and are not claimed as macOS large-corpus qualification. The original
sampling domains remain; the old hidden environment returns are gone.

The complete large run takes 3,589.27 seconds in debug mode, including generation,
staging and comparison. It is correctness evidence, not optimized throughput.
Its source archive is `f804576435cdfb1d95e9c98d5d8969d5c229ef8f118ea919c4d2c994d5d87e86`.
That binary predates checkpoint 66's VFS blob-copy change; the current publication
and reconstruction code is unchanged. The final Linux default group includes
the published blob change. No earlier proof is rebound to a different source.
Final migration source contains 1,566 hashed files; archive SHA-256:
`841d0322ef3c07d83fc9cf550f3938f7f3ec0f8746d5e13df425c61131154ae3`.
The new fixture is byte-identical to its qualified source; the only subsequent
change is deleting the two integration files absent from lib-test inputs.

Checkpoint 66's deferred Linux proof also completes: 687 regular VFS tests,
all 20 explicitly run native FUSE/NFS tests, one isolated warm-range test and
production VFS Clippy with zero diagnostics. Thus all 708 VFS cases execute
across those runs. Native coverage retains mapped transitions, refresh/switch,
inspection/export, rename/unlink/replacement, COW, LFS and committed remounts.
The isolated 4 MiB warm-read probe records 113 userspace-read bytes under the
unchanged bound. This is not a fresh installed Linux CLI or optimized benchmark.

Is this the best fix? Put reconstruction properties on the current private
Capsule owner, require success for valid files, and retain direct byte proof.
Exact executed chunk counts and explicit large-test disposition replace approximate
labels and hidden skips. Preserve current shared types and metadata consumers;
remove the obsolete producer instead of keeping a second test-only publisher.

No external generic-publisher calls remain. The 37,278-line push module still
contains the old producer and current shared policy/maintenance code. An unapplied
retirement candidate preserves 115 live production item bodies and the current
Capsule chunk-test module; it parses but still needs manual review and semantic
qualification. Stored pack-v1 maintenance/GC/replication authority, all-mode
lifecycle, total memory, cold-read concurrency, SQLite and performance gates
remain open.

## Checkpoint 68 — retire the generic manifest producer

The change removes the remaining generic `PushPipeline`, `run_push_batch`,
manifest-only initializer, and their private producer/test closure. Current CLI,
remote helper and import publication already call `git::capsule_push::run`; no
external generic calls remain after checkpoint 67. The push module falls from
37,278 to 7,156 lines: 14,118 fewer lines before the test module and
16,004 fewer inline-test lines. This is producer retirement, not complete removal
of stored pack-v1 authority. No compatibility wrapper, dependency or configuration is added.

An original-source Rust AST inventory identifies the live consumer closure;
manual consumer review and semantic compilation qualify the candidate rather
than treating identifier search as a proof of Rust name resolution. All 115
retained push-module production item bodies and the nested current Capsule chunk-test
module are byte-identical to checkpoint 67. Test-only imports remain in the existing
inline module. The unused optional HEAD-etag mode of the private range fixture
is removed. Current data-plane and term-construction guards remain intact.

Evidence map:

| Surface | Entry / owner | Caller / callee / sibling contract | Current proof |
| --- | --- | --- | --- |
| Current publication | CLI, remote helper, import → Capsule run | Shared PushConfig/outcomes/ref policy → crab-git and current Capsule publication | 70 retained push tests each OS; macOS 32 Capsule policy/transaction cases, excluding its heavy data group |
| Retained maintenance | metadb, repack, upload-pack → push metadata helpers | Existing manifest maintenance/reader repairs are still live; their owners require separate format retirement | 115 production bodies preserved exactly; both platform lib-test builds compile |
| Current chunk behavior | Nested Capsule chunk tests → add/staging/classifier | Current cross-repository ownership and receipt/proof behavior is retained | Whole nested module byte-identical; included in the 70 executed cases each OS |
| Shared Store adapter | Integration fan-out → Store PUT → futures 0.3.32 | FuturesUnordered owns admitted futures; try_collect returns the first error; provider completion after drop is not proved by an InMemory fixture | Both test bodies, concurrency 16, 250 ms limit and error bounds unchanged; two passes on each OS |
| Main / v2 comparison | Fetched main and crab-v2 → old generic producer | Current branch has already migrated external fixtures before producer removal | Exact old source, AST spans and consumer map preserved outside the checkout |
| Protected source guard | Architecture inventory → current push ownership | Old requirements still demand deleted local-HEAD and manifest tag helpers | Unchanged guard fails two expectations; exact human-approved two-file migration applied, 15 self-tests and 75 actual source checks per OS pass |

The guard candidate requires current-branch policy to use captured Capsule root
HEAD, forbids local-HEAD substitution on both push surfaces, requires current
crab-git tag delegation, retains all raw-peeling prohibitions across both shared
and current push plus command policy, and rejects missing owner files. The human approves this exact two-file patch after reviewing its concrete
candidate. Both working files equal the approved candidate bytes. All 15 guard
self-tests and all 75 actual source checks on each OS pass. Raw-peeling
prohibitions and performance thresholds remain intact.

The task Colima VM stopped during the first final-qualification attempt. Both
unified process handles subsequently disappeared; neither incomplete run is a
pass. Its Linux log records a container EOF. The same owned VM and RustFS
containers were restarted without resetting their data; new named qualification
runs pass all 70 retained cases on each OS. Product source has not changed during
the resumed runs. The producer-only archive contains 1,566 files, SHA-256
`649b4ef8ec44bce263cfc99335575b6d5f49583e53c165723455d538ea12ed67`.
Its Linux Capsule group passes all 39 cases with three explicitly ignored large
cases; its Store tests pass twice on each OS under the unchanged limits. Linux
production Clippy exits zero with 598 warnings; existing push-module diagnostics
include the pre-existing dangling HEAD-concurrency doc comment. The missing Linux
rustfmt component stops the orchestration before any installed build. The same
source passes host formatting. macOS Clippy is deliberately interrupted with its
verified task-only process group before installation to finish the producer's
newly orphaned buffered-write path. No native mount is active at that interruption.
Neither interruption is counted as a completed build or native proof.

The final working cleanup removes MetaDbGuard::commit_buffered, the session and
database buffered methods, both durability booleans, their unused increment API
and runtime atomic counter, and the two obsolete buffered-mode tests. Durable
session commits retain read-only refusal, empty-transaction lazy opening,
per-database parallelism and receipt metrics. Durable Db writes retain
await_durable:false followed by explicit flush, typed source errors and successful
flush counting. Pinned SlateDB 0.15.0 defaults await_durable to true and waits on
durable_seq; Crab disables timer flushes. Substituting default write options
would change that ordering and could wait for a flush this caller has not issued.
Existing takeover/reopen and current durable commit tests remain.

The cumulative perf-state counter is a shipped CLI contract: it is present in
release tags including v1.2.4, persists in stat output and has an existing roundtrip
fixture with eight historical writes. Keep those stored totals while current
operation deltas contribute zero; remove the unused runtime mutation API. A new
filesystem regression checks that a current operation adds its bytes and preserves
the prior count. CLI and public stat documentation describe the historical total.

The revised frozen archive contains 1,568 files, SHA-256
`e4bf19366c488f8edca336ec33ccd49bd05c6cc0d6c4f3b0779b8bf13af1adf3`.
This runtime revision passes 209 cases on each OS: 78 metadata, 23 metrics,
six stat, 70 retained push and 32 Capsule policy/transaction cases. This focused
Capsule run explicitly excludes its heavy data group; the producer-only revision's
39-case result retains its original binding. Production-library Clippy exits zero
on both OSes (593 macOS / 596 Linux warnings, no suppressions). Four roles install through Make on each OS. Fresh Linux foreground FUSE/NFS
RO/RW plus managed FUSE RW and macOS foreground NFS RO/RW plus managed NFS RW
pass: eight native cases and 25 RustFS reuse checks per OS. They prove exact
6 MiB full/range reads, RO refusal, live ownership/mode, applicable fsync →
commit/push → independent eager clone/fsck and ordinary detach. All four role
hashes remain fixed, with no forced cleanup. Linux core SHA-256 is
`00f8842b4ccfccaaad62d8056898c9c69eed6b789f2e8b603b0e377b113654da`;
macOS core is `ad5224dab92da543ae342a7d92c618ad091e09bfc89c0f094cb2da874e45d990`.
The initial Linux native preflight fails because the image lacks AWS CLI, before
any mount starts. Installing awscli only in the disposable task container permits
the rerun; all four installed role hashes match the failed attempt exactly. The
preflight failure remains in evidence. No source patch or binary rebuild is used
to bypass it. No prior proof is rebound
to the new metadata implementation.

The public documentation build, typecheck, lint and ten tests pass. The built
stat page contains the updated historical-counter description; the production
link crawl checks 398 HTML pages and 4,307 fragments. npm installation leaves the
lockfile byte-identical. The build reorders two tracked generated registries;
their import target sets are identical. Those task-generated changes are saved
outside the checkout and restored, then typecheck passes again.

An exhaustive scan of the 238 removed unit-test roots finds two explicit CI
selectors, while prefix review shows all nine filters in an old partial-clone
block now match no retained tests. A routine workflow-wiring migration is prepared:
the existing full Capsule and shared reader suites remain; current Capsule writer
contracts replace the empty legacy group. SDK fault recovery selects current CLI
staging retirement and current prepared-attempt/receipt recovery tests. No source inventory, expected-failure file, test assertion or threshold is changed
by this workflow wiring. Both OSes execute 32 writer cases (one existing live-S3
case ignored), two captured-catalog cases, one missing-binding case, one prepared
multi-ref reader case, the two exact writer recovery cases and one exact current
CLI case under the SDK gix-transport profile. Thus 39 passes per OS establish
nonzero selection before applying the routine two-workflow patch. macOS uses the
SDK MSRV compiler, Rust 1.97.0; Linux uses the dedicated image's Rust 1.97.1, so
exact Linux MSRV remains a CI responsibility. YAML parsing proves only two run
blocks change; all other workflow fields, permissions, actions and thresholds
are identical. The separate commit/push receipt binds the qualified bytes to the published head;
checkpoints 66/67 retain their original source-bound proof.

Final review source contains 1,572 hashed files: the 1,568 qualified runtime/doc
inputs plus the two approved guard and two routine workflow files. Archive SHA-256:
`46363463ade1717c29a5956bbe1611e9d3ca8eae714e4b3ff6d0be0e7d2b2af9`.
Runtime source remains byte-identical after final cargo fmt. No benchmark raw
paths or dependency/lockfile changes are included. No optimized performance or
all-mode signoff is claimed.

Is this the best fix? Delete the obsolete producer after moving its useful
properties to the current publication owner. Preserve live shared policy and
maintenance consumers while retiring their old authority at the appropriate
owner boundary; deleting the whole module would break those current callers.
Retire the newly orphaned buffered mode through its real guard/session/database
owner chain, keeping the one explicit durable path instead of suppressing the
unused-wrapper warning. Stored metadata/GC/maintenance/replication dispatch,
all-mode lifecycle, total memory and optimized performance remain open.

## Checkpoint 69 — require Capsule authority in metadata commands

Reproduced with checkpoint-68 installed macOS binaries against a real healthy
RustFS repository: `metadb diagnose --json` reports Capsule authority, while
`doctor --metadb --json` exits zero with both legacy databases uninitialized.
With poison bytes at the task-owned local cache path, doctor still exits zero
and reports `exists: false`, despite the file existing and retaining its bytes.
The baseline commands, binary hash and poison-byte receipt are retained.

Remove the legacy owner/diagnose/rebuild/recovery dispatch and doctor's legacy
MetaDb, acceleration and global shard-LIST report. Missing or corrupt Capsule
root is an error, never a selector for manifest-backed metadata. Direct metadb
and optimize-index entry points both drop `--db` and the obsolete `compact`
no-op. File, shard, xorb, visibility and Git metadata are one correctness unit.
Doctor shares the current Capsule diagnosis and propagates authority/cache
errors. Cache stats reuses `PersistentChunkIndex::inspect`: read-only schema
validation and SQL counts, with no whole-index Vec or writer-capable shared
handle. Filesystem errors and corruption do not become absent/zero caches.

Rebuild captures root inside the existing repository maintenance/GC writer
fence, authenticates the complete closure before either no-op success or exact
root CAS, and explicitly releases both leases on errors. The generation owner
requires valid authority before claiming its lease, reloads samples while that
lease renews, drains cooperative cancellation and releases explicitly. Preserve
the existing Capsule checkpoint/repack policy. Preserve typed
`CheckpointError::Storage`, including provider source and retry classification.

| Evidence-map cell | Current boundary and proof |
| --- | --- |
| Entry and callers | `main.rs` direct/optimize metadata dispatch; `cmd/doctor.rs` metadata report; `cmd/recover.rs` selected file/shard lookup |
| Owner | `cmd/metadb.rs` owns strict Capsule selection; `maintenance.rs` owns destructive maintenance admission/release; coordination owns renewable generation-owner lease |
| Callees | Captured root/view/activity, selected pointer catalog lookup, full reachable-dependency verifier and exact-root checkpoint publisher |
| Siblings | Both CLI selectors retired together; doctor/diagnose share Capsule health; rebuild retains full closure; recovery retains selected order/duplicates and 64-query batches |
| Existing contracts | Real large-repository qualifier requires owner locator-sweep telemetry before its Capsule branch; preserve truthful zero work fields without a legacy authority path. Rebuild's existing write counters remain zero |
| Dependencies | `while_renewing` drains after renewal cancellation and requires caller release. Maintenance acquires GC writers before its lock and releases both. Read-only SQLite inspect neither creates nor migrates. Storage `put` is create-only; deliberate existing-root corruption uses explicit overwrite |
| Current main/base | Fetched main `cc2e4e25f46` still reports legacy doctor state; crab-v2 `877b832b396` has Capsule-first metadata selection plus missing-root legacy dispatch. The installed parent reproduces both operator defects |
| Retention | 23 production items remain byte-identical. All existing external Capsule test bodies remain after removing the retired selector and moving them to flat sibling files. The module shrinks 5,676 → 1,442 lines; no dependency or lockfile changes |

The five new regressions cover missing/corrupt/successful owner admission and
release; rebuild's maintenance/GC release on all three authority states; no root
read before maintenance admission plus fresh authority afterward; cache
corruption without byte replacement; and throttle retryability/provider cause.
The 18 existing Capsule tests retain deep corruption/closure refusal, whole-source
byte admission, ordered selected lookups and the 100k-unrelated-catalog read
budget. Legacy-only tests retire with their deleted producer paths. Current
test modules are flat `metadb_capsule_tests.rs` and
`metadb_capsule_lookup_tests.rs`; no feature/tests directory is added.

Qualification against the final frozen source:

- Each OS: **109 passes** — 23 metadata, 50 doctor, 34 recovery and two actual
  CLI parser cases. Both parser filters execute one case. No ignores. Unchanged
  architecture guard: **15 self-tests + 75 source checks** each OS.
- Production library and CLI Clippy exit zero, **686 macOS / 689 Linux warnings**;
  no suppression. These include the CLI binary, unlike checkpoint-68's
  library-only count. Host formatting passes. Host Rust 1.97.0; Linux 1.97.1,
  leaving exact Linux MSRV to CI.
- Four Make-installed roles per OS. Linux FUSE/NFS RO/RW and managed FUSE; macOS
  NFS RO/RW and managed NFS: **eight native cases**. Exact 6 MiB full/range reads,
  RO refusal, live ownership/mode and applicable fsync → commit/push → independent
  eager clone/fsck pass. **25 reuse checks** each OS, normal detach, fixed role
  hashes and no forced cleanup.
- Installed metadata CLI: **31 commands per OS**. Healthy doctor matches Capsule
  diagnosis; corrupt local bytes fail doctor/cache stats without replacement;
  diagnosis leaves remote inventory unchanged; both CLI groups expose current
  syntax; deep closure, rebuild and owner succeed. Isolated canonical project
  config points at missing/corrupt roots: all four metadata entry points fail,
  no legacy authority is created, corrupt remote bytes remain unchanged, and
  the original native repository/configuration remains unchanged.
- Web production build, typecheck, lint and **10 tests** pass; built HTML contains
  current authority text and the existing shared path-diagram component. Link
  crawl: **398 HTML pages / 4,301 fragments**. Generated registry import multisets
  match; reordered output is retained externally and restored, followed by a
  second passing typecheck. Lockfiles are unchanged.

Failed qualification attempts remain explicit. The newly added fence test first
used create-only `put` to inject corruption into an existing root: both OS runs
retain **22 passes / one setup failure**. Only the fault setup changes to the
existing explicit overwrite API; assertions are unchanged. The final source
passes all 23 metadata cases. The first macOS CLI error probe changed only Git
remote; commands correctly retained committed `crab.toml` authority. A separate
project-config fixture supplies the intended missing/corrupt authority. Linux's
packaged AWS CLI ignored the endpoint environment setting; explicit
`--endpoint-url`, already used by native smoke, fixes only that harness setup.
Original group exits remain failed, and successful separate CLI retries are
bound to the same installed role hashes. No runtime change or threshold waiver
is used to bypass either failure.

Source archive: **2,054 files**, SHA-256
`ac090c9cb9b121968a711392d7a76507b7aa9a9651161dc546d278f0bad10687`.
All inputs remain identical after qualification. Core installed SHA-256:
macOS `8f150bb8a7b465d9b6c3a5779b2f957626288a4e0d9a4aed2ffcf57dd7155363`;
Linux `9b2ea6a6ce380a1b05780fe21e9675350a2c2b008c5eaf8e52c46138274821a8`.
Receipts, archives, AST/body retention proof, terminal group results and separate
successful CLI retry results live under the owned external
`metadata-authority-retirement/` qualification directory. No raw benchmark
files are added to the PR.

Is this the best fix? Require the canonical root and delete the legacy writer
closures, rather than repairing only doctor formatting. Reuse the established
read-only cache API and fenced checkpoint owner. This removes over 4k lines
from the command module while retaining the current dependency ordering and
useful tests. Stored GC/maintenance/replication authority, complete lifecycle
and optimized performance remain open.

Next: remove remaining GC/repack/replication manifest dispatch with sibling
reachability, reader grace/history and root-fence proof. Separately audit the
recovery planner/journal's existing `MetadataRepaired` status for verification-only
Capsule bindings and propagation of its caller's cancellation token. The unchanged
cache-clear command still unlinks the main SQLite file; operator docs now state
its stop-process restriction. Replace that lifecycle only after proving live
handles, sidecars, corrupt-cache handling and Windows ownership. Total memory,
cold/concurrent reads and sustained-write degradation remain performance work;
SQL counts alone are not an optimized throughput result.

## Checkpoint 70 — Capsule-only repack and checkpoint error ownership

Runtime/doc changes are qualified in the worktree, not yet committed or pushed.
The prepared two-file pack-layout guard migration remains unapplied pending
explicit human approval under AGENTS.md:222. PR #630 therefore still publishes
checkpoint 69 at `98cbaf091cb504f34544697454bfa5a7f5e66e40`.

Remove the remaining CLI manifest repack publisher, bounded-owner policy,
standalone pack/index/metadata repair and legacy-only tests. Production callers
already use captured Capsule roots. Keep that current publisher and its
`RepackOutcome`/`RepackSummary` bodies exactly, replacing the configuration that
only affected the deleted path with the needed dry-run boolean. The module
shrinks **2,124 → 149 lines**. Existing Capsule assertions move unchanged into
flat `repack_capsule_tests.rs`; no feature/tests directory is introduced.

A macOS red probe against the original repack adapter proves a retryable typed
storage reset becomes `Internal("checkpoint storage failed")` with no cause.
Replace all **five** product checkpoint adapters — repack, metadb, retained
history, shard compaction and xorb reconciliation — with one exhaustive
`From<CheckpointError>` at the CLI error boundary. Preserve storage/read/Git/
metadata/write/I/O causes, worker JoinError and existing leaf error messages.
The match has no future-variant catch-all. New regressions preserve provider
retry classification, GC fence identity/expiry with unchanged root, worker cause,
and uncertain checkpoint evidence without a blind retry. The existing metadb
throttle/delay/source assertion follows the canonical conversion unchanged.

Preview previously used the apply view, which retains a small frontier's Git
bodies. Select the existing complete metadata-only entry point for dry-run;
apply retains its prior pinned-view/consolidation implementation and limits.
One fixture has a **262,268-byte** frontier Git pack. Qualified read totals:

| OS | Former preview/apply-view path | New preview | Scope |
| --- | ---: | ---: | --- |
| macOS | 275,774 bytes | 10,739 bytes | Same fixture and captured authority |
| Linux | 275,781 bytes | 10,746 bytes | Same fixture and captured authority |

The initial focused run measures 275,785 → 10,750 bytes; small control metadata
varies between fixtures/runs. These measurements prove about **96.1% fewer
source bytes** and no full frontier pack download, not improved latency,
throughput or bounded total memory. Preview retains counts/bytes, zero body-work
counters and an unchanged root. Existing dry-run and selected-suffix/no-op
assertions remain. Update operator docs to current Capsule publication and
truthful terminal JSONL output, removing obsolete manifest/locator/receipt and
per-pack-progress claims.

Evidence map:

- Entry: main direct repack and grouped optimize packs → authenticated selected
  store/root → `run_repack_from_root`. Current Capsule push/helper fixtures call
  the same API, with unchanged verification assertions.
- Owner/callee: root-bound view → shared `crab_remote::checkpoint` policy →
  verified Git consolidation, immutable pack layers, catalogs/visibility and
  exact-root CAS. No speculative new setting, dependency or fallback.
- Siblings: all five CLI adapters use one typed conversion. Compaction retains
  preflight before writer leases: its existing missing-root test requires zero
  writes. Its captured root still protects publication by exact CAS; document
  that intentional admission difference instead of blindly changing its lease
  ordering. Metadb/history lease drain and release remain unchanged.
- Dependency contract: shared repack performs one selected suffix pass; a
  competing root replacement is an unpublished result, while a GC fence is a
  typed write conflict. Uncertain write errors retain verification evidence.
  Metadata opening does not need apply's retained body buffers.
- Main/base: main retains the manifest publisher; crab-v2 has both the old
  publisher and Capsule entry point. After checkpoint 69 the manifest publisher
  has no production caller. Whole-module and all-consumer reads precede removal.

Runtime qualification on each OS: **179 actual tests** — repack four, error
contracts 73, metadata 23, compaction 14, reconciliation 18, Capsule policy 32,
real helper pointer round-trip one, CLI parsers two, and corrected history 12.
The original history filter matches zero tests and is explicitly not counted;
`cmd::history_recovery::v2::` executes the real cases. No ignored cases in the
selected groups; heavy Capsule data tests remain outside this focused slice.
Production library/CLI Clippy exits zero, **684 macOS / 687 Linux warnings**, no
suppression. Host formatting passes.

Four Make-installed roles each OS pass **eight native cases**: Linux FUSE/NFS
RO/RW and managed FUSE; macOS NFS RO/RW and managed NFS. Exact 6 MiB full/range
reads, RO refusal, ownership/mode, applicable fsync → commit/push → independent
clone/fsck and normal detach pass. **25 reuse checks** each OS, fixed role
hashes and no forced cleanup. Corrected installed probes pass **13 repack + 31
metadata commands per OS**, including both CLI entries, refs unchanged,
independent exact 6 MiB reconstruction, strict fsck/deep closure, missing-root
refusal and previous cache/authority error checks.

The first new CLI probe assumes one suffix pass is followed by no-op. Actual
receipts show macOS **3 → 2 → 1** and Linux **4 → 2 → 1** packs; both passes
succeed. Shared policy explicitly permits this. Preserve those failed harness
exits/receipts and require bounded strict progress to zero body work in the
corrected probe, with a bound derived from initial fixture pack count. Separate
retries reuse the same role hashes and verify the original refs plus exact
content after the earlier successful passes. No runtime change or numerical
performance-gate waiver bypasses that failure.

Web build, typecheck, lint and **10 tests** pass. Built HTML verifies the current
repack/storage/compaction contracts. Link crawl: **398 pages / 4,301 fragments**.
Generated import multisets match; task reorder is retained externally/restored,
then typecheck passes again. Lockfiles and the 2,156 excluded raw files remain
unchanged.

Unchanged architecture self-tests pass **15** each OS; the actual source gate
fails only the repack inventory's three obsolete loose-pack delegation strings.
Prepared **109-line/two-file** migration requires captured root, scoped storage
layout, metadata preview and shared Capsule consolidation, retains raw-path
prohibitions, forbids manifest repack calls, and rejects missing pack-layout
owners. Against the unapplied candidate, **18 self-tests + 75 source checks**
pass each OS. This is candidate evidence, not a passing current source gate.
The exact patch is open for review and explicitly awaiting approval.

Runtime/doc source archive: **2,055 files**, SHA-256
`b976cd4cf243cd27a62f20aa901e5a2e0500850decc8c8632e9ed59c956b1d5c`.
All frozen inputs remain unchanged through qualification. Installed core SHA:
macOS `945d7da6dd09adefb0f30345e2911c58b3cbe47075c294fd8943423e5055e230`;
Linux `f073d0e40fb985c77de70c6a608de1f5697ff9d89733636b7292875bc0c63c5a`.
Receipts, failed attempts, final retry results, candidate guard hashes and body
retention proof live in the owned external `repack-authority-retirement/` tree.

Is this the best fix? Delete the orphaned format-specific publisher while
keeping one captured Capsule path. Put cross-product error conversion at its
existing CLI error owner so source/retry semantics cannot drift between five
copies. Preview uses an already supported metadata contract and has direct read
amplification proof. Remaining cold large-catalog/member costs, GC/reader pins,
replication, cache clear/recovery semantics, lifecycle/platform and optimized
performance still require work. The next audit records newly orphaned publisher
helpers and the legacy history/locator repair closure; synthesized current-view
Manifest DTOs and Xet shard format v1 must not be mistaken for Crab pack-v1
repository fallback.

## Checkpoint 71 — keep selected Capsule authority through helper reads

Qualified worktree changes; not committed or pushed. The separate 109-line,
two-file pack-layout guard candidate from checkpoint 70 remains unapplied.
The repeated HEAD/tag approval covers checkpoint 68's already published guard,
not this inventory migration. PR #630 still publishes checkpoint 69 at
`98cbaf091cb504f34544697454bfa5a7f5e66e40`.

The normal protocol-v2 fetch reproduction passes on both OSes after the fix.
Before the fix, replay one verified pre-checkpoint primary root response, then
allow selection to read matching current primary/replica state. Exhaustive
current-state replica verification passes. A replaced checkpoint is absent only
from the disposable replica. The helper combines its old primary root with the
selected replica store, requests that old checkpoint and fails with CRAB-E0020;
Git exits 128. Current-root controls succeed. The same old root works on primary;
fresh cache does not repair the replica failure. This is provider-response replay
of a capture window, not simultaneous live-writer stress or complete retained-
history replication proof.

Retain the selected root that `StoreResolver::read_store` previously validated
and discarded. Its product result now carries store, layout, source and mandatory
verified root. The shared generic policy-selection type stays unchanged. Primary
selection reuses the authenticated root; replica selection reuses its existing
validation read. No extra root validation request, config option or dependency.

The helper owns this read selection across wire and classic capability/list/
fetch batches. Capability probes and fetch views open its captured root. Classic
fetch opens one view after request validation under the existing reader admission,
then shares it with ordinary, constrained and raw-object paths. A cached cold-clone
view supplies its own advertised refs. Primary push ref views have their own cache;
ordinary read advertisements cannot seed push CAS admission. The optional cached
primary root is an I/O reuse decision: absence strictly opens Capsule authority,
never a manifest reader. Existing reader renewal/release and runtime drain remain.

The sibling classic-path reproduction also passes. On the old Make-installed
binary, a normal cold clone succeeds, but denying primary-root GETs after its
third successful root request causes Git exit 128 despite a readable selected
replica. The helper was selecting again between capability/list/fetch batches.
The new binary keeps its selected repository: both baseline and denial arms
succeed with exact checkout bytes and strict full fsck on macOS and Linux.
Primary-root GETs fall **7 → 3** in the baseline on both OSes. The denial arm
needs no fourth root request. This is a measured request-count reduction, not
an optimized throughput, latency, RSS or sustained-write result.

Evidence map: repository auth captures physical primary scope → product resolver
returns verified selected authority → helper session owns that selection → shared
Capsule view opener and Git runtime → provider/native paths. Push continues through
its primary ref snapshot and Capsule publisher. Other resolver consumers retain
the selected store/layout; they do not combine this read root with an independent
provider. Mount selection and clone shard sync retain direct selector coverage.
Current main still serves manifest authority; current crab-v2 and the published
PR head retain the reproduced independently passed Capsule root. Both fetched
base refs remain unchanged.

Focused qualification executes **262 distinct tests each OS**: 154 helper,
105 replica, two clone-selector and one mount-selector cases, with no ignores.
The pinning case additionally runs alone. It removes the mutable root after
capture and still reads the pinned replica snapshot; the selector runs once.
The classic test repeats fetch with a selector that would fail, proving reuse.
A new read/push sequence verifies replica read refs and independent primary push
refs. Existing assertions remain. The clone shard fixture now publishes a Capsule
catalog instead of a legacy manifest. Its shard-selection and unsafe-cache checks
stay unchanged. The marker-only mount fixture initializes its root before data.
No feature/tests directory is introduced.

Four Make-installed roles each OS pass **eight fresh native cases**: Linux
FUSE/NFS RO/RW plus managed FUSE; macOS NFS RO/RW plus managed NFS. Exact 6 MiB
full/range reads, RO refusal, live ownership/mode, applicable fsync → commit/push →
independent clone/fsck and normal detach pass. **25 reuse checks each OS**; no
forced cleanup. Fresh installed probes pass **15 repack + 31 metadata commands
each OS**, using unchanged installed role hashes through all later replays.

Preserve unsuccessful attempts. An initial fixture update used a create-only
ref edit for an existing head; the pinning case now tests mutable-root removal
instead of that unrelated publication. A mechanical selector fixture edit failed
compilation and was corrected without changing existing assertions. The first
macOS NFS-only mount filter executes zero tests and is not counted; both final
runs enable the required feature and execute the case. Linux catches the marker
fixture's initialization order; the rootless-nonempty-prefix refusal stays intact.

The first Linux installed group stops before native mounting: its global xorb
sets contain 188 objects after source push, 189 before consumer add and 189 after
add. The changed inventory predates that add; the simultaneously running macOS
harness shares this bucket-global namespace. Preserve the failed report and all
three lists. A serialized retry uses the exact same binaries and unchanged
harness/assertions and passes all five Linux native cases plus CLI probes. No
threshold or inventory assertion is waived. An initial Linux inline Python probe
receives no stdin through the noninteractive Docker runner and executes no code;
only the subsequent file-based, result-validated replay is counted.

Production library/CLI Clippy exits zero: **692 macOS / 695 Linux warnings**,
without suppression. Formatting passes. Web production build, typecheck, lint
and **10 tests** pass; built HTML contains the selected-root/session/push contract.
Link crawl: **398 pages / 4,301 fragments**. Generated import multisets match;
reordering is retained externally/restored and a second typecheck passes. Locks
and all 2,156 excluded raw files are unchanged.

Actual architecture self-tests pass 15; the actual source check has 74 passing
gates and only the three obsolete repack-delegation violations. Its first local
invocation lacked Cargo on PATH; the corrected invocation supplies the actual
failure evidence. The unchanged, unapplied guard candidate passes **18 self-tests
and 75 source checks each OS**. AGENTS.md:222 still requires explicit approval.

Published-head CI is now terminal: nine workflow successes, S3 gateway failure,
and the protocol workflow canceled. Native platform jobs skip on this PR event;
the dedicated runs above provide the stated native proof. S3 service, traffic,
ecosystem and native workload steps succeed, but retained-report verification
rejects the native soak's unchanged performance limits. Ten clean windows give
terminal throughput **0.764566×** early throughput and p95 **1.518136×** early
p95, outside 0.8 / 1.5 limits. The report's integrity flag passes with 94,862
acknowledgements; performance fails. No direct baseline is measured. Preserve
this as a current performance gap; the new read request-count result is not a
soak rerun. No workflow, threshold or expected-failure file is changed.

Current frozen source: **2,056 files**, archive SHA-256
`fa3bf72fb10d2bc803b8c88c12c1169b842e9c2215e57ca797f451a374671c2e`.
All frozen inputs remain exact. Installed core SHA:
macOS `bf285107e4ff392a6f243a0105b3862821de6f81d578c4bfcc18f2c20d74c2dc`;
Linux `d40a955fbeaa39e59038e069de70b3f8f52d7614f88930de511c18dffce71e9f`.
Intermediate archive/tests, failed attempts, role hashes, before/after traces and
`qualified-summary71.json` live in the owned external `selected-root-binding/`
trees. The older audit and checkpoint-70 archives remain separate bindings.

Is this the best fix? Capture physical read authority at selection and retain it
through the whole helper read session; do not reconstruct provider/root pairing
at terminal handoff or keep selecting beneath cached views. Reuse verified reads
and existing canonical view/admission APIs. The extra product carrier and session
state replace an invalid independent-root assumption and prevent cross-provider
push admission; non-test LOC growth pays for that ownership boundary.

Stored pack-v1 retirement is still incomplete. Wire's optional-root manifest/
visibility repair closure, post-fetch shard-sync fallback, GC/maintenance and
retained-history contracts remain. Reader/payload retention, total memory, cold
concurrency, lifecycle/platform proof and the unchanged failing optimized gates
also remain. This checkpoint is not all-mode or optimized-performance signoff.

## Checkpoint 72 — Capsule-only post-fetch shard warming

The prepared worktree removes the shard-sync manifest/journal fallback and its
repository-generation cursor. A valid manifest-only fixture returns a successful
zero-shard result before the fix; the same regression now requires typed missing
`v2/root` authority and proves no derived cache was opened. Existing corrupt-root
refusal is retained. Empty authenticated catalogs still produce an empty delta.

`load_published_shards` now loads the physical Capsule root, captures its complete
pointer catalog and returns its shard hashes. `PointerCatalogInspection` finishes
and closes scratch state on success/error. The shared publisher commits through
independently mutable per-ref heads, so root generation cannot key this catalog.
The new regression publishes another shard while root generation is unchanged;
the second sync downloads one new shard and skips its predecessor.

Delete `CachedShardListGen`, its JSON cursor, `with_repo_cache_dir`,
`sync_from_manifest`, generation persistence and the unconditional full-list
clone. `sync` accepts only the authenticated hash slice. Preserve bounded shard
GETs, content hashes, receipts, bloom/query behavior, private cache handling,
spill/install behavior and all `SyncStats` fields. The production section shrinks
1,202 → 829 lines. The main module is 834 lines; 707 lines of tests live in flat
`shard_sync_tests.rs`, with no feature/tests directory.

| Surface | Caller / owner | Contract / sibling proof | Current behavior |
| --- | --- | --- | --- |
| CLI warming | Opt-in clone and default fetch/pull → `run_post_fetch_shard_sync` | Selected physical store/layout; clone warns after admitted clone, fetch propagates authority failure | Same strict catalog reader for both paths |
| Authority | `load_published_shards` → shared Capsule root/catalog capture | Complete normalized catalog and explicit inspection finish; per-ref publication need not change root generation | No manifest or generation-cursor fallback |
| Derived reuse | `compute_delta` → persistent `has_shard`, verified shard cache and hash-bounded download | Existing receipt, corruption, bloom, exact-query and spill assertions retained | Skip installed receipts; download missing candidates |
| Main/base comparison | Current main warms legacy manifest state; crab-v2 admits missing-root manifest shards | Before-fix valid-manifest refusal fails with `Ok(SyncStats { ...0 })` | Current worktree returns typed missing Capsule authority |

All **19 retained test assertion sets remain identical after whitespace**.
Remove nine tests solely for the retired generation cursor and add three for
missing authority, unchanged-root publication and selective persistent receipts.
Final qualification executes **25 cases each OS**: 22 shard-sync, two clone
selector and one fetch-output test; no ignores. Production Clippy exits zero
with 692 macOS / 695 Linux warnings and no suppression. Format/diff checks pass.

Four roles are Make-installed each OS. New actual CLI qualification executes
**10 commands each OS** across lazy clone with warming and lazy clone followed
by fetch warming. macOS clone downloads three shards; Linux downloads four.
Cold fetch installs the missing two/three shards after canonical prefetch caches
one. Repeated fetches download zero and skip all installed shards. Every fetch
emits a real shard-sync phase and selects one 6 MiB file. SQLite receipts and
chunk rows plus cached shard bytes are checked, hydration is byte-exact, and
independent Git fsck is strict. Role hashes remain fixed. Checkpoint 71's eight
native mounts, 262 focused tests and 46 CLI commands per OS retain their separate
source binding; this slice does not claim a fresh native mount rerun.

A controlled private-cache profile uses the same 6 MiB fetch and adds 500,000
unrelated valid chunk rows. Eight samples per OS cover normal/large cache and
warming on/off, with two repeats each. All fetches pass. Large-cache peak RSS:
**macos 251.6–253.0 MiB with warming versus 73.5–74.6 MiB without; linux 228.1–228.4 MiB with warming versus 83.7–83.7 MiB without**. The bulk `PersistentChunkIndex::load_all` and temporary
in-memory mirror are still present. This is a measured next target, not an
applied memory improvement or sustained native-write proof. No threshold changes.

Preserve probe/setup failures. The initial macOS CLI probe expects text tracing
fields but the successful clone emits JSON; a structured-output retry with fresh
caches retains every assertion. The first macOS profile requests WAL checkpoint
inside its uncommitted fixture transaction; committing first fixes only setup.
Linux fixture insertion on the Docker host bind is interrupted after three
minutes of page spill. The retry gives only the fixture writer a 128 MiB page
cache; measured CLI connections use normal settings and the same 500,000 rows.

Docs production build, types, lint, 10 tests and the 398-page/4,301-fragment crawl
pass. Built HTML states mandatory Capsule warming and clone's warning boundary.
Generated registry import multisets match, are restored, and typecheck passes
again. No dependency/lockfile or inventory/threshold change. All 2,156 raw audit
files remain preserved and excluded; zero tracked paths.

Frozen source: **2,058 files**, archive SHA-256
`4c5dd72c86e544c93928d1d11c4281c46cb02e083cbc3e6f085ecdf3a3bccde3`.
All inputs remain exact. Core SHA:
macOS `4c764de04073d541d948db5a281e2240546382f1322f8182009a14053dc6f986`;
Linux `974a09da184a0c735982bb0c5dbabda8d8fc31780690cdec11e566edb0e39771`.
External `shard-sync-authority/` receipts retain sources, failed attempts,
commands, SQLite/cache checks and memory samples. Earlier snapshots stay separate.

Is this the best fix? Use one authenticated catalog path and exact installation
receipts. Delete the cursor rather than finding another root-generation key for
independently mutable ref catalogs. Existing typed authority, inspection cleanup
and persistent receipt APIs provide the required contracts without a new layer.

The unchanged 109-line/two-file pack-layout guard candidate passes 18 self-tests
and 75 source gates each OS. It remains unapplied pending the separate explicit
approval under AGENTS.md:222. Actual source gates retain the same three obsolete
repack requirements. Earlier HEAD/tag approval was already published. No push is
attempted with this known related gate failure. Next remove the measured bulk
cache load, retire wire's optional-root/visibility legacy closure, finish GC and
reader-retention authority, and repair the unchanged sustained native-write soak
failure. Whole-mode/platform/optimized-performance acceptance remains open.

## Checkpoint 73 — warming memory follows the candidate delta

The prepared worktree removes the temporary bulk persistent-index mirror from
post-fetch shard warming. Previously every fetch deserialized the bucket's
entire `chunks_v1` table into a Vec, copied it into a HashMap, loaded all shard
receipts, then discarded both collections after warming. The same path already
checks `PersistentChunkIndex::has_shard` for each authenticated candidate.

Keep that existing keyed path and delete the eager mirror. `EXPLAIN QUERY PLAN`
shows a covering-index search on the shard receipt's primary key on both OSes.
Receipt hits mark only the current ephemeral candidate set. Verified local bytes,
bounded/hash-checked downloads, full/query-scoped installation, persistent
transactions and spill behavior remain unchanged. No function signature, schema,
error or metric field/method changes. Both production warming callers pass no
metrics collector; the existing direct synchronizer metrics test still passes.
The production section shrinks another 27 lines. No tests or baselines change.

| Surface | Entry / owner | Dependency and sibling evidence | Proof |
| --- | --- | --- | --- |
| Post-fetch warming | Clone selector / fetch → `run_post_fetch_shard_sync` | Ephemeral ChunkIndex never escapes; persistent `has_shard` checks installation receipt | Full bulk Vec/map and all-receipt enumeration removed |
| Receipt hit | `compute_delta` → keyed `has_shard` | SQLite primary key; hash-verified catalog selects candidates | Existing selective-receipt and unchanged-root cases pass |
| Installation | `download_and_install` / verified local-cache install | Persistent shard transaction stores entries before receipt; current bloom/query/spill methods unchanged | All 22 shard-sync cases plus clone/fetch callers pass |
| Telemetry / other owners | Direct synchronizer metrics; persistent inspection/current SQLite API | Metrics methods unchanged; production callers supply None. Shared APIs and their property tests remain | Existing metrics assertion unchanged; no schema/API edit |
| Main / base | Both fetched refs and published head retain the bulk mirror | Source 72 process baseline captures actual scaling, rather than relying on code shape | Same source 72 profile fails the new external 64 MiB growth check |

Final source executes **25 tests each OS**, without ignores, and production
library/CLI Clippy exits zero (692 macOS / 695 Linux warnings, no suppression).
Four roles are Make-installed each OS. Ten actual CLI commands each OS prove
clone warming, fetch warming, repeat receipt reuse, exact 6 MiB hydration and
strict Git fsck. All existing test source hashes remain unchanged.

The memory comparison copies the retained source-72 private caches; streaming
row digests prove identical chunk mappings and receipts before/after each arm.
The large cache has exactly 500,000 unrelated additional valid chunk rows.
Eight fetch samples per OS cover warming on/off and normal/large caches, twice
per arm. Every sample succeeds, selects one 6 MiB file, and every warming sample
downloads zero while skipping all installed shards. Large-cache peak RSS:

| OS | Before peak RSS (MiB) | After peak RSS (MiB) | Shard-sync before / after (ms) |
| --- | --- | --- | --- |
| macos | 251.6–253.0 | 76.2 | 756–768 / 23–25 |
| linux | 228.1–228.4 | 81.6–81.7 | 1157–2645 / 112–133 |

The external regression check allows at most 64 MiB extra peak RSS from those
unrelated rows. Both source-72 baselines fail; both current binaries pass.
This is a private-cache, two-sample comparison. It does not bound all VFS reply,
COW, reconstruction, prefetch or captured-metadata memory, and it is not sustained
mounted-write throughput proof. No existing threshold or CI policy changes.

Frozen source: **2,058 files**, archive SHA-256
`52f708821dc1fd498c8e47666ca052ca261e845a499923923542dcbe89299270`.
Only shard_sync.rs and its architecture document differ from source 72. All
frozen inputs and installed roles remain exact. Core SHA: macos `6aa0b6829e5904a3b63a211a15a9f9a1a7cb049c637656f088125c16e10131b0`; linux `7bb0677a3295c6101c6ccea097be67d4a744a74105f9881afe1c2c9f0342915f`.
External `shard-sync-memory/` receipts retain the old failing memory budgets,
query plans, exact copied-cache row digests, command results and process samples.
No new unsuccessful product qualification attempt. One diagnostic `du` process
is stopped to avoid competing with the live compilers; it supplies no claimed
size evidence. All qualification jobs finish and are collected.

Source-72 web inputs remain byte-identical; that checkpoint's build/type/lint,
10 tests and 398-page/4,301-fragment proof retain their separate binding. This
slice changes only the Rust architecture document and reruns no web suite.
Checkpoint 71's eight native mount cases also retain their separate binding;
no fresh OS-mounted performance or correctness run is claimed here.

Correct the soak scope. The S3 gateway workflow's `native-soak-proof.json`
executes signed HTTP PUTs against a native Crab repository, with 16 writers,
4 KiB objects and ten 30-second windows. It does not exercise FUSE/NFS mounts.
The earlier native-write wording was too broad. Its observed throughput/p95
failure remains a gateway gate; sustained OS-mounted writes need their own
measurement. No report assertion or threshold is changed to conceal the failure.

Is this the best fix? Installation receipts already answer whether a candidate
needs warming. Remove the discarded global mirror instead of adding limits,
configuration or another cache. Preserve full persistent contents and canonical
read verification. The measured memory scaling disappears through the existing
ownership boundary, with a net deletion and no new production path.

Candidate guards pass 18 self-tests and 75 source checks each OS, unchanged and
unapplied. Publication still waits for the separate 109-line/two-file pack-layout
guard approval under AGENTS.md:222; the already-published HEAD/tag approval does
not cover it. All 2,156 original raw files remain preserved/excluded. Next finish
wire/GC authority and reader retention, measure sustained mounted writes, and
resolve the separate gateway degradation. The full goal remains active.

## Checkpoint 74 — retain advertised Capsule state through wire fetch

A concurrent push can advance ref heads B → C without changing the authenticated
Root digest. Source 73 advertises B, then reopens mutable heads for complete-view
promotion and denies its previously advertised want. The new negotiation test is
red on that runtime (three retained cases pass, the new case fails CRAB-E0060).
The repaired test validates the exact three-object pack/checksum with Git's strict
indexer, admits B, and proves C absent.

`crab-read::CapsuleRepositoryCapture` owns the physical layout, RootSnapshot,
validated ref heads and committed activations for this session. Discovery uses
only captured refs. Fetch opens verified metadata lazily, and complete promotion
reuses those same heads. Root is mandatory at the wire entry; no optional-root
manifest/visibility-repair path remains. The shared metadata owner retains its
existing bounded version/epoch/activation and ordered-frontier checks. No stored
format, policy default, dependency, configuration or threshold changes.

Preserve borrowed-layout one-shot APIs: ref-only and layered-control function
bodies remain exact, while complete metadata passes borrowed heads/activations to
the existing assembler. Only the multi-stage wire session owns the new capture.
`RepositoryRefs::from_maps` exposes the existing parser without a Manifest DTO;
its trait caller retains the same validation and ordering.

Remove the orphaned push locator/journal/visibility-repair closure and private
backoff, with six tests solely for those retired writers. The push module shrinks
7,156 → 5,747 lines. Historical recovery still uses the retained local-ODB
visibility publisher; pre-existing graph/materialization maintenance remains a
separate retirement inventory. The wire module is 1,850 lines, with flat sibling
test files of 668, 413 and 436 lines. Five obsolete legacy wire tests are removed;
empty/unborn, hidden-ref, peeled-tag and payload-free discovery assertions are
migrated to actual Capsule fixtures. Missing payload after successful discovery
produces the terminal error and no pack.

Final runtime qualification, each OS: **365 passes, zero failures/ignores** —
64 retained push, 32 current Capsule policy, 41 wire, 154 helper, 33 repository
parser and 41 shared read/capture cases. Minimal-feature read/remote checks pass.
Production library/CLI Clippy exits zero, with 687 macOS / 690 Linux warnings;
new unused imports are removed, no suppression. Format passes. Four roles are
Make-installed per OS, using the external per-worktree target.

**Eight fresh native cases pass**: Linux FUSE/NFS RO/RW plus managed FUSE; macOS
NFS RO/RW plus managed NFS. Each OS also passes 25 reuse checks, 15 installed
repack commands and 31 installed metadata commands. Exact 6 MiB full/range reads,
RO refusal, applicable fsync → commit/push → independent eager clone/strict fsck,
default refs and outside-Git discovery pass. Owners exit normally, kernels detach,
role hashes remain exact, and no forced cleanup is needed. Serialize the OS runs
so global xorb inventories cannot race each other.

An isolated RustFS fixture proves the regression through normal Git protocol-v2
shallow fetch on both OSes. Hold the ls-refs response, publish C, verify the Root
digest stays fixed, then resume Git. Old source-73 helpers exit **128**, explicitly
denying advertised B; final helpers exit **0**, install B as FETCH_HEAD, exclude C
and pass strict fsck. The first unconstrained probe stays on the tip-bound path
and succeeds with the old helper; retain it as a control, not a false red case.
Depth-one requests force the affected complete-view admission.

A separately bound intermediate run passes 39 default Capsule cases per OS,
including seven data cases: 3,236,572,256 reconstructed bytes on macOS and
3,955,888,660 on Linux. Three existing large-corpus ignores remain explicit.
An external parser incorrectly required zero ignores; Cargo passed, the wrapper
failed, and both outputs remain retained. Final focused runs execute the 32
non-data policy cases with no ignores; do not add the intermediate seven to the
final-source 365 count. The earlier explicit large-corpus proof remains separate.
Other retained setup failures include the test fixture borrow/stack corrections,
missing Cargo in a probe PATH, and a guard-manifest shape check rejected before
mutation. No product assertion or source baseline is relaxed for those retries.

The human explicitly approves the remaining **109-line/two-file pack-layout guard
migration**. Apply the exact before/candidate hashes. Actual repository checks now
pass **18 self-tests + 75 source checks per OS**. Required captured-root/shared
Capsule repack ownership, raw-path prohibitions, missing-owner refusal and
manifest-repack refusal all remain enforced. The earlier three stale repack checks
are superseded by the approved current contract; no further approval is pending.

Final source: **2,060 files**, approved archive SHA-256
`acc0722f664d157a0c3d732d43329393cd9b8f374ba8d95c9d8ca53c2d5920c8`.
Runtime archive `886277cba7da2a3cebcc1c5f6fd255b9427c7b538a214fb9ffa0a90813b23b23`
differs only in the two approved Python guard files; all Rust/runtime inputs stay
exact. Core SHA: macOS `e028163bdd4717fcc32b8e39f3420f76b5a0b12c5a657257616f3228779dc3d5`;
Linux `a6e44704754a367e2979655bd83fe78d9268b44487a9e60aaaf0d90d5520c78c`.
All 482 web inputs remain byte-identical to this checkpoint's production build,
types, lint, 10 tests and 398-page/4,301-fragment validation. Original 2,156 raw
benchmark files remain preserved/excluded; zero tracked paths. External
`wire-authority-retirement/` receipts retain every source binding and attempt.

Evidence map: helper selection → wire session → shared captured physical owner →
metadata stable-head/activation/frontier contracts → authenticated pack generation.
Current main still has legacy manifest snapshot/repair. Classic
`open_capsule_fetch_view` recaptures but explicitly refuses changed refs/peeled
refs/transactions; uncheckpointed multi-run `fetch_capsule_packs` can reopen Root.
Those classic paths need the same capture audit next. GC reader retention,
maintenance/replication/history authority, Windows/lifecycle qualification and
sustained mounted performance remain open. No all-mode or optimized-performance
signoff; the gateway PUT degradation remains a separate failing CI gate.

Is this the best fix? Retain the validated physical capture at its shared owner.
Root-digest comparison alone cannot detect this reproduced mutable-head race;
retrying against newer refs would change the advertised contract. Removing the
retired wire writer closure and keeping one-shot borrowing also reduces local
complexity without adding a fallback or unrelated async ownership.

## Checkpoint 75 — classic discovery owns its fetch capture

Three regressions fail on published head `bbfdbc9e7ab` with production code
unchanged: an uncheckpointed cold fetch rejects advertised B after B → C;
a checkpointed shallow promotion reports changed heads despite unchanged Root;
and promisor recovery admits a blob from the later C generation. All fixtures
use actual Git packs and publication, with exact Root identity checked.

Classic capability/list/fetch now borrow one `CapsuleRepositoryCapture` owned by
`SessionCache`. Payload-free list retains heads/activations; complete admission,
constrained and promisor fetches reuse them. The uncheckpointed multi-run path
opens captured metadata and feeds authenticated source ranges to the existing
installer, instead of reopening heads and eager complete Git bodies. Existing
one-run/layered direct installation stays intact. Router-owned stores keep
admission and reads on the selected physical origin. Primary push ref admission
remains separate.

Consolidate capability routing into the ordinary dispatcher using the existing
pinned selector; delete the separate loop branch. A newly created capability
capture survives only when its cached direct view will supply discovery. A
fourth control catches the first candidate freezing an empty probe before any
advertisement, and passes after the source cleanup. Existing descriptor
eligibility errors still propagate outside the optional probe handler; final
review restores that exact error boundary before publication. No new fallback,
shared-crate API, config, dependency, inventory or compatibility path.

The final focused suite passes **477 cases per OS**, no ignores: 158 helper
(including four new cases), 41 wire, 105 replica, two clone/one mount selector,
64 push, 32 Capsule policy, 33 repository and 41 shared read/capture. Minimal
feature checks pass. Production Clippy exits zero with **683 macOS / 686 Linux
warnings**, no unused item in the changed helper and no suppression. Actual
guards pass **18 self-tests + 75 source checks** each OS, byte-identical to the
approved migration. Format passes. The helper file has a net deletion; its
prefix before the central test module shrinks 3,925 → 3,863 lines. Four new
behavior tests live in a **314-line flat sibling file**; existing assertions stay
intact (one direct-call fixture supplies the now-mandatory captured Root).

**Eight fresh native cases pass** with final Make-installed roles: Linux FUSE/NFS
RO/RW and managed FUSE; macOS NFS RO/RW and managed NFS. Both OSes pass 25 reuse
checks, exact 6 MiB full/range reads, RO refusal, applicable fsync → commit/push →
independent clone/strict fsck and normal detach. No forced cleanup; fixed role
hashes. **15 repack + 31 metadata CLI commands** per OS also pass. OS runs are
serialized to protect the shared inventory fixture.

Real Git/RustFS races execute **three modes on both OSes**: protocol-0 classic
cold fetch, protocol-0 classic shallow fetch and the protocol-v2 wire sibling.
Hold the advertised response, perform a normal concurrent push, verify Root is
unchanged, then resume. Each old helper exits **128** at the reproduced policy
boundary; each current helper exits **0**, installs the advertised FETCH_HEAD,
excludes the concurrent commit and passes strict fsck. Classic comparisons use
head-74 binaries; the wire sibling retains its source-73 failing baseline. This
is functional/debug-role qualification, not sustained-throughput proof.

Frozen source: **2,061 files**, archive SHA-256
`ca39dfb69eb34601deb330e8d9dd83cba4e6d6e088ac7bd74a099c6f6fbd4905`.
Only the helper, flat regression module and Rust architecture document differ
from checkpoint 74. Core SHA: macOS
`8d824a2ca85dcb1e037b44454d8449f5ecb4bc72777acab6c6436d1812ad9b5b`;
Linux `2c7ea0a88f9f6e1e90dfbce54995e71d7649aa1e804de9bc6fe3de447bb89c1c`.
All source inputs and installed roles stay exact. The first qualified candidate,
red tests, empty-probe correction and final error-boundary correction retain
separate bindings in external `classic-capture/` receipts. All 482 web inputs
remain exact against checkpoint 74's production build/types/lint/10 tests and
398-page/4,301-fragment proof; no new web execution is claimed. Raw 2,156 files
remain preserved/excluded; zero tracked paths.

Evidence map: helper dispatcher → selected router/Root → retained read capture →
shared stable-head/activation/frontier validation → complete metadata/planner
or authenticated source installer. The wire sibling remains independently
captured and is exercised live after the routing consolidation. Replica/push,
hidden-ref, shallow-boundary and promised-object siblings retain their existing
tests. Git's helper contract ties fetch object IDs to list responses:
https://git-scm.com/docs/gitremote-helpers . Current main still opens a fresh
manifest/journal repository snapshot for classic fetch. No main code is merged
into this v2 follow-up.

Is this the best fix? Retain the validated physical capture at the read-session
owner; Root digest comparison cannot detect these ref-only updates, and retrying
against newer heads changes the advertised state. Reuse existing metadata-only
installation and delete recapture/routing duplication. Preserve typed failures
and direct cold-clone eligibility rather than adding another fallback.

Remaining evidence gaps: the generic layered range installer collects payload
windows before installation (`fetch_layered_payload_windows`), so total memory
still needs a measured bound. GC's bucket root collector still falls back from
missing Capsule roots to historical shard traversal; whole-call-graph reader
protection and retained reachability need proof. `ReadAdmissionTicket` bounds
concurrency; its interaction with GC is not established by that type alone.
No bucket-wide GC command runs. Finish stored pack-v1 retirement and retained
reader protection, then sustained mounted writes and the separate gateway PUT
performance failure. The full goal remains active.

## Checkpoint 76 — Capsule-only bucket maintenance roots

Four strict-authority regressions fail on published `5a1a963aa45` with production
code unchanged: missing-root loading, bucket root capture, closure-repair roots,
and registry certification each accept a valid manifest repository. Make the
existing Capsule root loader mandatory and remove both manifest-history shard
walkers and their mark/repair dispatch. A missing or corrupt root is terminal.
The complete captured map supplies marks; no second history read can replace
its authority. Existing snapshot identity, current registry roots, coordinator
proof, writer/sweep fences, journal reconciliation, grace and delete checks stay
intact.

Registry repair still discovers retired manifest markers so an unsupported
repository cannot silently disappear from the coverage scan. Every discovered
repository must authenticate its Capsule root/catalog before any registry
replacement starts. Mixed Capsule/manifest roots remain terminal. This does
not change replacement's existing behavior on a storage failure during writes;
root-validation failure is proven to leave registry objects unchanged.

Move the touched tests into flat sibling `bucket_tests.rs` and
`bucket_hint_tests.rs`. Remove two tests dedicated to retired v1 history/layout
success; normal coverage, writer-fence, corrupt-shard and missing-shard fixtures
now use the real shared v2 initializer and keep their original assertions.
Production code shrinks by 137 lines. No dependency, lockfile, inventory,
configuration, threshold, persistent format or public API changes.

Final source passes **218 focused tests per OS**, zero ignores: 136 GC,
12 history, 58 fsck and 12 source-retention cases. Both OSes pass **18 guard
self-tests + 75 source checks**, format and production library/CLI Clippy;
684 macOS / 685 Linux warnings remain, zero errors and no unused bucket import.
Four roles per OS are built and installed through Make with external targets.

Each final RustFS fixture passes **34 commands**: normal native-content
track/add/commit/push; valid registry repair; missing-root and corrupt-root
refusals with exact unchanged registry bytes and ETags; successful repair after
restoration; fresh eager clone and strict fsck. Each reconstructs **6,291,456
bytes** exactly. These are isolated owned buckets, with **no bucket sweep**.
No fresh OS-mount or optimized-throughput result is claimed for this GC slice.
The earlier checkpoint-75 native mount/race receipts retain their bindings.

Fresh operator-site build, typecheck, lint, all ten tests and the 398-page /
4,301-fragment link check pass. Generated registries are restored and rechecked.
Runtime inputs are frozen: **2,063 files**, archive SHA-256
`f9807a7d30c4b4d5ec681cf0cb311e0f64d41d8952b054923d76a8cd1900aec4`.
Core binaries: macOS
`bcf6286c265033aab81fe6650e8a780a01e7f4a088636f4f461aefc58f301545`;
Linux `d890470202a66ab15199f1c515d73350bcbf779c6e409ce7358e39e2e6aecbae`.
All **2,156** raw audit files stay preserved and excluded from the PR.

Publisher retention is independent of browse readiness: replaced ref-frontier
sources receive authenticated lifetime records before the replacement ref CAS.
Repository GC marks those records; explicit history pruning advances a durable
generation floor before retiring them. Pointer catalog deltas retain immutable
shard/xorb rows, and history restore merges current and historical catalogs to
retain newer-only data. These source contracts explain sibling behavior; they
do not prove arbitrary reader lifetime through explicit pruning. Fixed reader
admission slots are not treated as root leases.

Evidence map: CLI registry/closure-repair entry → bucket capture/mark/seal owner →
bounded Capsule root GET/decode → captured complete catalog inspection →
registry replacement only after complete validation. Repo GC already uses
strict Capsule authority. Current main still walks manifest history; no main
code is merged into v2. Is this the best fix? Make the existing owner mandatory,
delete fallback dispatch and duplicate traversal, and preserve discovery of
unsupported repositories instead of hiding them behind complete coverage.

Retained qualification attempts include the initial missing private argument
updates, the empty external history selector (corrected to the real module),
and complete pre-import-cleanup proof. No empty selector counts as validation;
no assertion or threshold is weakened. External evidence is under the dedicated
`gc-capsule-authority` directory on the workspace volume.

Next: finish retained reader/shared-shard closure and remaining stored-v1
maintenance/replication/history retirement. Audit repair policy when both root
and marker are absent, including explicit deregistration. Bound the generic
payload installer, then measure sustained OS-mounted FUSE/NFS writes and the
separate gateway throughput/p95 failure. This checkpoint does not establish
complete VFS correctness or optimized performance; the full goal remains active.

## Checkpoint 77 — repair retains registered repository identities

On published `145ca2adf79`, deleting only a repository's `v2/root` marker makes
registry repair omit that repository and remove its existing shard roots.
The new missing-marker regression fails with production unchanged; four
selected controls pass, including explicit deregistration and corrupt coverage
recovery. The initial coverage fixture incorrectly used immutable `Store::put`
to inject different bytes at an occupied key; the corrected fixture uses the
explicit overwrite API. That setup failure and the actual baseline are retained
separately.

Under the existing exclusive global sweep lease, repair now captures the union
of validated registered identities and discovered physical markers, then
requires an authenticated Capsule root/catalog for every identity before exact
replacement. A missing marker cannot imply deregistration. Explicit removal
remains available through the existing deregister operation. Retired-marker
and mixed-root refusal, shard replacement, coordinator/fence/grace/delete
contracts are unchanged.

The storage-gated metadata API returns only registered prefixes independently
of coverage and shard-root bodies. This lets repair rebuild a missing/corrupt
coverage marker. The API and normal summaries share the existing record reader
and identity validation; normal summaries retain coverage checks and workflow
normalization. Targeted push receipt reads remain unchanged. New production
surface grows by **39 lines** (six in bucket repair, 33 in metadata), paying for
independent identity recovery and shared schema ownership. No new dependency,
format, config, compatibility path, inventory or suppression.

The prior manifest-refusal fixture also registered an unrelated rootless
`org/current` peer. Initialize that intended healthy peer through the v2 writer
so its original missing-legacy-root and unchanged-registry assertions stay
focused on their actual fault. No assertion is weakened. Three new CLI cases
cover missing markers, explicit retirement and coverage recovery; two metadata
cases cover independent identity reading and invalid JSON/record identity.

Final frozen source passes **249 focused tests per OS**, no ignores: 139 GC,
12 history, 58 fsck, 28 registry and 12 source-retention cases. Both OSes pass
the minimum metadata feature check and **18 guard self-tests + 75 source checks**.
Metadata Clippy has **zero warnings/errors**; production library/CLI Clippy has
zero errors with 684 macOS / 685 Linux preexisting warnings. Format passes.
All four product roles are built and installed through Make for each OS.

The final isolated RustFS fixture passes **63 commands on each OS**. The exact
published-76 binary succeeds after marker loss while dropping known repository
roots; the current binary refuses the missing root and retains byte/ETag/size-
identical registry objects. Explicit deregistration, re-registration after root
restoration, missing/corrupt roots and corrupt coverage recovery all pass.
Fresh eager clones and strict Git fsck pass with **6,291,456 bytes reconstructed
exactly per OS**. No bucket sweep runs. New mount-throughput proof is not claimed.

Fresh operator-site build/types/lint, all ten tests and the **398-page /
4,301-fragment** link check pass; generated registries are restored/rechecked.
Runtime source: **2,063 files**, archive SHA-256
`8be84dd2bb2d4dfbd679e200c1d73077830e69ccdc93e3462a0f558b51979aea`.
Current core SHA: macOS
`dc1764eae7f8abd01706a69c0c2ee480dfb8fcb63fc1abfbe90ae27c8a06590e`;
Linux `1393142ed74589105f195db04c207448ac8d14b684a2d9f1532101a6c88ac2c3`.
Old comparison binaries retain source-76 archive/core bindings. All source and
role hashes remain exact; all **2,156** raw audit files remain excluded from the
PR. Evidence lives in the dedicated `registry-identity-retention` directories
on the workspace volume.

Evidence map: CLI repair → exclusive bucket sweep → registered identity reader
plus physical marker discovery → complete Capsule root/catalog authentication →
existing exact replacement. Bucket marking and closure repair already require
roots for registered repositories; repair now preserves that identity set.
Exact replacement still removes aborted candidate shards for healthy repos.
Current main shares list-only discovery/omission; no main code is merged here.

Is this the best fix? Keep identity discovery at the metadata owner and union
known identities at the repair owner. Requiring a healthy coverage summary
would block coverage recovery, while preserving every stale shard would defeat
exact repair. The narrow API gives repair the required identities without adding
steady-state VFS or targeted-push reads.

Next: retained-reader/shared-shard closure through explicit pruning and remaining
stored-v1 maintenance/replication/history retirement. Bound generic payload-window
memory and measure sustained actual FUSE/NFS writes. This root-loss repair does
not establish reader lifetime through pruning or optimized mount performance.
The full goal stays active.

## Remaining priority order

Checkpoint 73 removes the measured post-fetch bulk mirror: the same large-cache
fixture now peaks at about 76 MiB on macOS and 82 MiB on Linux, versus the earlier
252–253 / 228 MiB. All values and receipts remain intact. Checkpoint 74 closes
the reproduced wire recapture race and retires its manifest repair closure. Checkpoint 76 retires the bucket manifest fallback; checkpoint 77 prevents
repair from discarding registered identities after marker loss. Next finish retained-reader
and shared-shard closure plus other stored-v1 maintenance authority, then qualify
sustained FUSE/NFS writes. Checkpoint 75 closes the reproduced classic discovery/promotion gaps. The CI
`native-soak` artifact measures signed S3 gateway PUTs into a native Crab
repository; its degradation is a separate gateway gate, not an OS-mount result.

Checkpoints 57/62 close the reproduced Unix endpoint flock handoff and late
control-binding mechanisms. Linux Unix/TCP readiness and rollback are now proven;
other native identity/lifecycle and Windows proof remain open.

The helper-layout gap and live read-only inspection leak are closed by the
installed-role proofs. Checkpoint-25 closes the reproduced Linux NFS cache
handoff gap, checkpoint-26 closes coordinator-only/mixed batch discovery,
and checkpoint-27 prevents export destinations from overwriting source bytes.
Checkpoint-28 moves live export materialization into the owner and removes
mounted-path re-entry. Next prove concurrent reset/commit, teardown/admission and
lost-owner behavior across remaining owners, qualify a waiting native macOS NFS
barrier, and finish all stored pack-v1 authority removal. Preserve the shared
process-spanning freeze and never retry an uncertain live operation offline.
Checkpoint-32 repairs CLI foreground NFS discovery and qualifies cooperative
foreground/helper teardown. Native macOS mapped-write draining, stalled cleanup
and the remaining product owners still block signoff.
Checkpoint-33 closes the reproduced Unix control-socket and accepted-control
ownership gaps. Checkpoint-36 repairs the shared data owner and busy-unmount
lifetime. Checkpoint-37 closes reproduced serving-stage startup cancellation,
duplicate group signals and late-readiness admission. Checkpoint-39 closes the
reproduced foreground FUSE PID/detach failures and makes shared native queries
error-aware. Early preparation, concurrent managed owners, target identity and
the macOS mapping barrier still require proof before claiming teardown complete.

Checkpoint-45 closes the reproduced helper configuration/telemetry ownership
failure and qualifies the original sequential publication → next-mount flow on
Linux NFS/FUSE without marker deletion or admission bypass. Checkpoint-46 removes shared manifest-only repository initialization. Immediate
format priority: checkpoint-47 retires protected receive's linked legacy paths.
Checkpoint-48 retires standalone auth view's manifest fallback/publisher and binds
its Git/pointer reads to one capture. Checkpoint-49 retires the unused shared manifest preparation/upload/commit APIs.
Checkpoint-50 retires current add-time and staged-proof legacy receipt dispatch,
including the ambiguous advisory-cache domain and receipt-only importer. Its native
replay reproduces missing foreground FUSE commit/control ownership on checkpoint-49
binaries. Checkpoint-51 repairs that serving owner and qualifies live publication
and ordinary teardown. Full control, early-startup, daemon/private and native
mount-identity proofs remain. Checkpoint 68 removes the old CLI producer and
initializer after current-format fixture qualification. Checkpoint 69 retires
metadb/doctor manifest dispatch and fixes read-only cache diagnosis. Next retire
GC, maintenance and replication stored pack-v1 dispatch.
Checkpoints 42/43 repair canonical LFS publication and ignored-byte retention;
checkpoint-44 reduces local verification amplification with bounded block proofs. Remote proof reuse, LFS generation
transitions, payload retirement and crash recovery remain open. Checkpoint-40 closes managed runtime-worker starvation;
engine-drain cancellation and replacement-target races still need native proof. Preserve these ownership/read
boundaries before widening concurrency. Git LFS is a current content type and
must not be deleted as pack-v1 compatibility.

1. **Correctness: coherent Git/Capsule generations.** Atomically install selected
   Git state, pinned catalog and read service for startup, refresh, switch and
   adoption. Retain old readers and their remote roots against GC. Acceptance:
   real RustFS push → refresh → native NFS/FUSE visible hash, including concurrent
   old-generation readers. Both tenth-checkpoint RustFS backend scenarios now pass;
   startup/GC/all-mode proof still blocks signoff.
   The fifteenth and sixteenth checkpoints repair fresh remote-source lazy policy
   fetch and explicit Git-directory authority for push staging/state. Their real
   NFS/FUSE publication probes pass; startup/GC and other owners remain required.
2. **Correctness: stable open-file identity.** FUSE handles must survive rename,
   unlink and replacement, including writes/fsync, without reopening stale
   pathnames. The eleventh checkpoint's native rename/unlink/replacement/COW
   matrix now passes both adapters; the thirteenth checkpoint adds live commit
   adoption and mapped writeback. Acceptance still includes other product owners,
   reset/refresh and generation transitions while files remain open.
3. **Correctness: finish unpublished-content durability.** Complete local recipe
   reads now work. Prove mixed local/remote dedup terms, interrupted publication,
   restart recovery, later real push/independent clone, retirement and preservation
   across owned-cache namespace changes before declaring write durability done.
   The fourteenth checkpoint proves clean-push retirement/later-push/clone for
   retained native descriptors, not all unpublished base readers or mixed recipes.
   The seventeenth checkpoint preserves native read/retry ownership on failed COW
   and access upgrades. The twentieth checkpoint proves unpublished base-pointer
   retirement with engine, physical-compaction and real-provider mounted tests.
   Crash/mixed-recipe recovery, namespace migration and resource reclamation remain
   required; read-pin health classification and many-pin admission also need work.
4. **Correctness/performance: total resource and lifecycle ownership.** Verified
   pointer/blob retention is bounded, but admitted reconstruction, upstream
   decoding, active replies and lower caches need a total memory/admission budget.
   Bound native Git fetch time and prove NFS child-task completion. Complete refresh
   ownership qualification in foreground/helper/coordinator/daemon paths.
5. **Performance: concurrent dispatch and cold ranges.** Warm Git amplification and Linux
   dispatch are repaired: checkpoints 60/61 configure 2–8 FUSE workers and admit
   native writes before control quiescence. FUSE callbacks still synchronously
   await Tokio work, and reads hold the inode content mutex across reconstruction.
   Unrelated cold ordinary blobs additionally share `OdbReader::fills`.
   The installed blocked-read/unrelated-lookup probe now passes all four Linux
   modes. Next measure concurrent same-inode reads while preserving
   promotion/namespace ordering and total memory admission. Upstream rejects
   multiple session threads outside Linux; do not apply that setting to macOS.
   Warm hits already bypass the Git fill gate. Preserve whole-object integrity
   when reducing cold-range amplification; verify decoder buffer ownership before
   removing the extra ordinary-blob copy.
6. **Performance: directory work and prefetch.** Page large directories, release
   inode references correctly and route effective prefetch through canonical
   verified reads with foreground admission priority. Measure request/origin
   amplification rather than counting queued prefetch tasks.
7. **Qualification.** Requalify Linux NFS/FUSE with RustFS and macOS native NFS
   on exact final sources, read-only/writable and foreground/helper/daemon modes.
   Record cold/warm random/sequential reads, origin amplification, throughput,
   p95/p99, peak RSS and teardown/resource recovery. Current fixture throughput
   and passing unit suites do not establish all-mode production performance.

Close the checkpoint-20 bundled SQLite WAL-reset dependency gate before
increasing metadata or read concurrency. Qualify an ordinary workspace upgrade
with all seven direct consumers and their enabled feature slices. Audit SQL
integer conversions against upstream ToSql/FromSql contracts before changing
callers. No SQLite dependency override or vendored patch is authorized.

The checkpoint integration assertion now follows the canonical writer and decoder:
format 9 (`CRBCKP09`), not the stale inline literal 5. This is ordinary current-
contract test maintenance, not a protected baseline update or a stored pack-v1
compatibility decision. Its old unapplied patch is removed. The positive remote-
only consumer now loads published Capsule placement/proof identities instead of
inventing IDs and deleting its only local payload copy. Production rejects those
invented IDs as stale; tracing confirmed all 13 were rejected. The test heap-pins
batch dispatch as production does, and passes on macOS's default test stack.

The current `clone_cache` non-Unix lock helper is a pre-existing no-op. The
checkpoint-25 cache-ownership proof applies to Unix; Windows NFS handoff is not
qualified. Replace that no-op with actual OS locking rather than preserving it.
The workspace MSRV is 1.97, so inspect the standard library's cross-platform
File locking contracts before selecting another dependency or FFI. Require
Windows exclusivity/teardown proof in its dedicated environment.

Correctness gates precede tuning. Generation, cache and descriptor ownership
also determine which performance optimizations are safe. The full hardening
goal remains active; the qualified checkpoint is published in open PR #630.

## Resource and format decisions

- Capsule decoding stays in shared metadata/read crates. A mount generation must
  own its immutable catalog; reopening mutable latest metadata for each file is
  not an acceptable repair.
- Removing a duplicate VFS window cache must preserve canonical range reuse;
  otherwise small kernel reads can repeatedly fetch complete xorbs.
- Disk retention caps and memory admission are different units/lifetimes. Do not
  describe a bounded cache map as a bound on all retained response memory.
- Use one unique external Cargo target per worktree/platform. The Linux VM disks,
  Cargo state and retained logs live on the mounted Workspace volume. The user's
  existing Colima instances are not stopped or repurposed.

## Crab pack v1 removal — explicit updated scope

The user removed the requirement to preserve old Crab pack v1 repositories.
Remaining work includes all reachable format dispatch, replica selection, native
fetch/clone/hydration/publication, migration/doctor policy and affected docs/tests.
No v1 fallback, alias or dual reader may remain merely because it was shipped.
Classic Git remote-helper commands and Git wire protocol versions are separate
from the stored Crab pack format and must not be removed by keyword alone.

Completed admission boundaries:

- Remote-helper v1 capability/list fallback and VFS/managed/doctor admission were
  removed at earlier checkpoints. Fresh empty-prefix v2 initialization and push
  pass the retained RustFS NFS/FUSE probes.
- Replica read-store validation requires a Capsule root. The eighteenth checkpoint
  removes `replica_readiness`'s no-root fallback and the shared legacy checker,
  including its export and private manifest/index/object walkers.
- Protocol/read documentation now states mandatory rooted Capsule admission.
- Shard compaction (`crab compact` and `crab optimize shards`) now requires a
  Capsule root before acquiring maintenance admission, in apply and dry-run.
  Its shard-list reader/publisher and optional unauthenticated file selection
  are removed. Input shards remain immutable GC inputs; catalog replacement
  verifies exact file coverage and complete dependencies before root CAS.
- Xorb optimization removes internal root-missing global inventory and manifest
  publication. The CLI retains its admitted root before journal creation and
  source planning; reconciliation requires a root even for empty mappings.
  Execution/finalization no longer have no-store success paths. Both old remote
  publishers and their newly orphaned candidate-index helpers are gone.

Authority retirement status and remaining boundaries:

- SDK `initialize_remote` was changed upstream in #629 to canonical Capsule
  initialization. Checkpoint-46 removes the exported old initializer and updates
  valid refusal fixtures without retaining a compatibility wrapper.
- Checkpoints 47/48 remove manifest authority from protected receive and standalone
  auth view, including their old producers. Checkpoint 49 removes the unused shared manifest preparation/upload/commit APIs.
  Checkpoint 50 removes add-time manifest selection, staged old-receipt fallback
  and native receipt import. Checkpoint 59 removes the obsolete Native entry
  and its orphaned handoffs. Checkpoints 63/67 migrate the useful producer
  properties to current Capsule fixtures; checkpoint 68 removes the generic
  publisher and initializer. Checkpoint 70's qualified worktree removes the
  orphaned manifest repack publisher. Rooted projections remain current data
  transfer types, not evidence that an old-format reader is required.
- Gateway read/write, listing, checkpoint ordering, completion recovery and
  maintenance now require Capsule authority at checkpoint-38. No production
  gateway caller selects the old initializer, manifest/journal reader/publisher
  or legacy catalog scheduler. Valid-v1 refusal and signed canonical operations
  pass on RustFS; native VFS LFS reads are repaired at checkpoint-41; LFS publication and
  optimized verification reuse remain separate gaps.
- LFS pre-push ref discovery now requires a Capsule root; its missing-root
  manifest/empty-base fallback is removed at checkpoint-35. First and incremental
  real Git hook publication plus an independent eager clone pass on RustFS.
- Checkpoint 69 removes the optional-root metadata owner, diagnosis and rebuild
  dispatch. Checkpoint 71's qualified worktree binds helper read selection to
  its physical Capsule root across wire and classic batches. Checkpoint 72's
  qualified worktree removes the post-fetch shard-sync fallback and generation
  cursor. Wire's optional-root manifest/visibility closure is retired in checkpoint 74;
  classic complete-view promotions gain captured-state ownership in checkpoint 75.
  Checkpoint 76 removes the bucket-maintenance missing-root fallback; checkpoint
  77 retains registered identities when physical markers disappear. Retained
  readers through explicit history pruning still need closure proof. No
  bucket-wide GC command was run.
- Maintenance/publication/replication dispatch and related docs still require an
  exhaustive storage-authority inventory. The locations above are confirmed
  reachable source leads, not complete removal proof. Manifest projections used
  by rooted Capsule readers are not themselves stored pack-v1 compatibility.

Acceptance requires a real v1-only fixture to be refused, empty-prefix v2
initialization/push to work, and v2 clone/fetch/mount/replica behavior to pass.
Removing only the VFS startup no-root path is not full protocol retirement.

## Checkpoint 78 — admit complete pack batches before publication

The shared installer on published `b34a176a96a` rejects an aggregate input limit
inside its publication loop. A valid first pack therefore remains visible when a
later pack pushes the batch over budget. Both native and maintenance reproductions
fail on macOS and Linux; maintenance also leaves its kind sidecar. Strengthening
the existing real captured-base/thin-candidate test reproduces the same prefix
publication through the store-backed caller on both OSes. The initial caller
selector lacked `publication` and ran zero tests; that result is excluded and
retained separately. Corrected probes produce three real failing cases per OS
with two passing controls.

Preflight the complete resident pack-body sum at the shared installer, before
pack or kind-sidecar publication, and remove the incremental check. Checked
addition, zero-as-unlimited, exact-budget acceptance, external-base ordering,
per-pack integrity checks and verified local reuse retain their contracts.
Previously installed files remain byte-identical on rejection. This is a batch
size limit, not total-memory admission or rollback for every later I/O error.
Production grows by three lines; no new policy, public API, setting, dependency,
override, inventory, serialized format or threshold. Four tests use a flat sibling
module and real three-object Git packs. README states the enforced contract.

Final frozen source passes **185 tests per OS**, none ignored: four minimal-reader,
58 Capsule-reader, 48 checkpoint and 75 protected-helper library cases. The
local-staging check, formatting, **18 guard self-tests + 75 source checks** and
reader Clippy pass on both OSes; reader Clippy reports zero warnings/errors.
Linux initially lacked rustfmt; installing the component for the pinned 1.97.1
toolchain resolves the tooling failure without source edits. Both protected-helper
binaries build, and all four product roles install through Make per OS.

Fresh RustFS/native qualification passes **eight mounted cases**: Linux FUSE/NFS
read-only/writable plus managed FUSE; macOS NFS read-only/writable plus managed
NFS. Exact 6-MiB full/range reads, read-only refusal, applicable fsync → commit/push
→ independent eager clone/strict fsck and normal detach pass with unchanged role
hashes and no forced cleanup. Reuse checks: 25 macOS / 25 Linux. Fresh
repack proof passes 15 commands macOS / 15 Linux, including payload-free preview,
convergence, unchanged refs, strict clone/fsck, complete deep closure and missing
authority refusals. No bucket sweep runs. These debug roles provide functional
proof; sustained mounted throughput remains unqualified.

Runtime source: **2064 files**, archive SHA-256
`94e9acb42ffab3cf64fbbebc5f7369dba135876fcbc8a27610b782e0b61ef617`.
Core SHA: macOS `04093e0622e2069e89f3e64f7cfce8a8e5f88f9d10d042cde683e5294411242c`;
Linux `1635e67e5fb0ddc2b8283a42c01a3c498f062967f898e91a1a6ca7b7e6fd0ba9`.
README is bound separately; this ledger is excluded from the frozen runtime
archive. All 2,156 raw audit files remain preserved and excluded from the PR.
Evidence: dedicated `resident-intake-admission` directories on the workspace
volume, plus `layered-payload-memory` for the separate pre-change profiler.

Evidence map: resident/candidate API and protected receive → generic payload
builder → shared batch installer → crab-git per-pack file verification/atomic
publication. Maintenance uses that owner with raw packs and `.kinds`. Selected
fetch/historical checkpoints retain their existing range admission; optimized
cold clone already preflights body+sidecar reads at its separate file-backed
owner. Current main has no shared Capsule installer; comparison uses the current
PR parent.

Is this the best fix? Preflight at the batch owner covers native and maintenance
siblings without caller-specific patches or rollback that could remove existing
immutable packs. Per-pack limits cannot enforce aggregate admission.

Next performance opportunity is measured: with 32 sources / 96 objects fixed,
8→32 MiB random native blobs raise generic-install peak RSS from **273.03–273.58
MiB** to **1040.88–1041.23 MiB** across native, maintenance, pre-admitted fetch and
sidecar-first fetch. All eight isolated install processes preserve every blob
and pass strict Git fsck in separate untimed processes. Native/maintenance/combined
fetch use 32 requests; split fetch uses 64; selected bytes match per fixture size.
These are macOS debug baselines on b34 production with test-only instrumentation,
not mounted or optimized-throughput measurements. Stage pack bodies to private
files through canonical bounded transport; retain authentication, coalescing,
aggregate admission, cancellation/draining and exact byte/request proof. Metadata
and total VFS memory need separate bounds. The profiler stays outside the PR.

The b34 gateway CI job passes integrity with **94,820 acknowledgements**, zero
failed/missing/duplicate/unexpected entries, but throughput retention **0.737223×**
fails the unchanged 0.8 threshold. p95 **1.468728×** passes the 1.5 limit. This is
signed HTTP PUT qualification, not mounted writes. Job:
https://github.com/crabbuild/crab/actions/runs/40563256083/job/122397638041.
Retained-reader closure through explicit pruning, remaining stored-v1 authority,
actual mounted sustained writes, total memory and broader platform/provider
gates remain open. The full hardening goal stays active.

## Checkpoint 79 — stream selected pack bodies into private files


Generic layered pack installation previously retained every selected pack body.
With sources/objects fixed, selecting larger native blobs grew process memory
linearly. Replace the collected-window path with authenticated streaming section
staging shared by historical/native, maintenance, selected fetch, and physical
sidecar reads. Preserve pre-I/O and whole-batch admission, coalescing, exact EOF,
OID admission, thin dependency ordering, local reuse and immutable installation.
Source full-size mismatch now fails even if selected section hashes match.

New flat modules own section streaming, staged file lifetime and six behavioral
fixtures. The public checkpoint late-error test retains a typed ConnectionReset,
proves empty output/private cleanup, and retries against a healthy source.
Remove two tests tied to the deleted private collector; their spawn/coalescing
properties now use real 16-member Git fixtures across four selection modes.
Use existing Git file SHA/trailer verification for thin packs by making that
mechanic public. No dependencies, lockfiles, format, config, protected inventory,
compatibility reader, grace period or performance-guard thresholds change.

Is this the best fix? A single format-aware read owner passes private files to
the existing Git installer; storage keeps its canonical streaming transport.
ScratchSpace only writes/reads complete Bytes and hides paths, so adapting it
would reintroduce materialization or widen the storage API. Copying cold-clone
policy would miss the generic thin/maintenance boundary. Net production growth
is justified by section demultiplexing, exact framing and owned/drained writes.

Limits and explicit contract changes: sidecars, OID maps, caller-supplied Capsule
bodies and total VFS memory remain separate. Streaming opening retries and
complete bounded-range retries preserve their existing storage contract;
mid-body failures after delivery fail the operation and permit caller retry.
The removed collector's signed range accelerator is not used. Over 512 MiB the
canonical stream uses one HEAD plus 32-MiB requests with four shared slots,
replacing the reader's 128-MiB / six-slot collector; oversized request count is
reported. Explicit cancellation drains; abandoned futures retain private workers
and require Tokio/destination lifetime through drain.

Evidence map: historical recovery/checkpoint suffix/selected fetch → generic
builder → streamed pack/sidecar owner → Store range framing/retry → crab-git
SHA/sidecar/thin/immutable install. Optimized cold clone/cache already stages
files and retains its separate complete visibility proof. Current main has no
shared Capsule reader; exact baseline is PR parent 53010575f4d.

Performance excludes the initial stale Linux candidate (Cargo reused old owner
libraries under source overlays) and initial Mac large-source timing overlapped
with input copying. Both are retained and labelled excluded. Corrected Linux
artifacts require fresh compilation; final profile production hashes bind to the
frozen source. All binaries come directly from separate Cargo test targets;
temporary manifest targets/profiling source are removed from the PR. Verification
runs in separate untimed processes; generated ODBs are removed only after exact
blob hashes, strict Git fsck and installed-file hash receipts are retained.

Broader qualification caught a source regression before publication: streaming
returns `ReadError::Cancelled` during intake, before checkpoint's next phase
check. Derived conversion wrapped it as a read failure. The two existing phase
cancellation tests failed on both OSes. Normalize that outcome at the checkpoint
error owner; preserve typed sources for every other read error. No assertions
changed. Server and S3 maintenance already distinguish the canonical cancelled
outcome. Transfer already exposed read cancellation through its earlier cold
path; browse owners retain their existing token check.

Final frozen source passes **257 tests per OS**, none ignored: nine minimal
reader, 62 Capsule reader, 62 Git pack, 49 checkpoint and 75 protected-helper
cases. Local-staging, formatting, **18 guard self-tests + 75 source checks** and
reader Clippy pass on both OSes. Reader Clippy has zero warnings/errors. Product
build warnings match the published parent's sets: 59 macOS / 58 Linux. The
Linux disposable container initially lacked rustfmt; install the pinned component
and resume from formatting without changing source or repeating passing tests.
Both protected-helper binaries build; all four product roles install via Make.

Fresh RustFS/native qualification passes **eight mounted cases**: Linux FUSE/NFS
RO/RW plus managed FUSE; macOS NFS RO/RW plus managed NFS. Exact 6 MiB full/range
bytes, RO refusal, applicable fsync → commit/push → independent eager clone and
strict fsck, normal detach, **25 reuse checks per OS** and stable role hashes
pass. Repack passes **15 commands per OS**, unchanged refs, exact reconstruction,
strict fsck, deep closure and missing-root refusal. No bucket sweep or forced
cleanup. The first macOS cross-repository smoke observed a bucket-wide xorb count
change during concurrent Linux source publication. Its own consumer had zero
local/prepared payloads. Retain that failure separately; the unchanged macOS
fixture passes when run alone. Runtime assertions were not weakened.

Paired public-API measurements retain exact blob hashes, strict Git fsck, Store
request/consumed-byte counts and installed-file hashes. These are debug builds,
with untimed verification and immutable fixture inputs; they are not optimized
or mounted sustained-throughput signoff. With 32 sources / 96 Git objects and
1 GiB of native blob data, the final RustFS measurements are:

| API case | Peak process RSS, before → after | Install time, before → after |
| --- | --- | --- |
| macOS native | 1333.0 → 42.6 MiB | 32.53 → 19.83 s |
| macOS maintenance | 1365.7 → 44.7 MiB | 39.46 → 15.87 s |
| Linux native | 1242.7 → 28.2 MiB | 10.93 → 10.23 s |
| Linux maintenance | 1245.7 → 26.7 MiB | 10.52 → 10.02 s |

All four pairs retain **32 Store read requests** and **1,073,959,277 consumed
bytes**. The corrected Linux local-file 1-GiB pairs fall from 1037.0–1037.1 to
17.1–17.8 MiB RSS; timings overlap and include slower candidate samples. Earlier
macOS local paired measurements used the pre-format prototype and retain their
own source/executable bindings; they are not final-source throughput proof.

A single 576-MiB native blob exercises oversized transport. Native installation
has three paired samples per OS: median **18.758 → 18.574 s macOS**,
**6.262 → 6.092 s Linux**. Candidate Linux samples span **6.005–20.938 s**; the
20.938-s observation remains included, so tail performance is unresolved. Peak
RSS ranges fall **1686.5–1703.8 → 151.8–152.9 MiB macOS** and
**965.2–975.9 → 139.3–147.8 MiB Linux**. Maintenance's one pair per OS is retained
separately. Every oversized pair reads **604,076,417 bytes**; Store requests rise
**5 → 20** under canonical bounded streaming. No universal speedup is claimed.
All final provider/profile package inputs bind to the frozen reader/Git source;
checkpoint's later cancellation conversion is outside that profiling dependency
closure. No binaries were manually copied. Temporary Cargo profiling targets and
source were removed before source freeze.

Runtime source: **2,066 files**, archive SHA-256 `79c25b64f69a84ab782530995176723124a5aaaeef689d6e32754fc9021c0379`. Core SHA: macOS `a0ea762a437422404eb276759b4dbd893aecb637c4efdcead89fb8843ee073e6`; Linux `2e51d3805ff463c5e296913b197f670008370cba8f5b3272e2ca22987bb71a18`. README bindings and the hardening ledger are separate from the runtime archive. All 2,156 raw audit files remain preserved and excluded. The dedicated workspace evidence directories were removed after the completed audit, as described below.

Is this the best fix? The canonical format-aware read owner stages selected
sections and passes their owned files to the existing Git installer. Storage
keeps transfer/retry mechanics; Git keeps SHA, sidecar, thin repair and immutable
publication. Added non-test code pays for demultiplexing, exact framing and
cancel-safe file ownership while deleting the collected-window implementation.
No caller-specific collector, broad ScratchSpace API, dependency override,
compatibility reader or new setting is added.

The latest parent gateway job passes integrity with **93,126 acknowledgements**,
zero failed/missing/duplicate/unexpected keys. Throughput retention **0.758438×**
fails the unchanged 0.8 limit; p95 **1.480864×** passes 1.5. This measures signed
HTTP PUTs, not mounted writes:
https://github.com/crabbuild/crab/actions/runs/40578901550/job/122452682786.

Next priorities: retained closure through explicit pruning and remaining stored-v1
authority; actual sustained mounted writes and gateway degradation; oversized
stream tails plus metadata/caller-resident/decoder/reply memory; lifecycle/cache
and broader provider/Windows/MSRV gates. The hardening goal remains active.

After the final audit passed, the separate **Clean up Cargo build data** task
removed `crab-ios-02d01345-vfs` at the user's request, stopping/restarting its VM
and services during removal. This deleted the source archives, installed binaries,
profiling inputs/controllers and detailed phase receipts for this and earlier
checkpoints. The results above completed before cleanup; the execution transcript
and this ledger retain their summaries, source/core hashes and exclusions. They
are not a claim that those detailed artifacts are still available. The source
checkout and 2,156 excluded original raw audit files remain intact. New-head CI
must regenerate retained proof; bulk artifacts were not rebuilt merely to undo
the requested cleanup. Publication metadata uses a separate small directory.
