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
installed-role evidence is recorded through checkpoint 46.

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
| Bounded dispatch and directories | Concurrent blocked hydration must not serialize unrelated FUSE operations; large-directory paging/inode lifetime; effective bounded prefetch | Open |
| Consistent lifecycle in every mode | Foreground/helper/coordinator/daemon; read-only/writable; automatic/manual refresh; reset/commit; stalled clients and unmount during reads | Open; managed engine-drain cancellation, foreground FUSE controls/refresh, macOS mapped writeback and Windows ownership remain gaps |
| Retire Crab pack v1 compatibility | Reject v1 stored repositories at all read/write/selection dispatch boundaries; delete fallback readers/writers and obsolete compatibility tests/docs; preserve empty-repository v2 initialization | Admission/helper/doctor/selected-replica validation and eighteenth-checkpoint replica readiness require v2. Protected receive, legacy initialization, metadb/GC and maintenance/publication dispatch remain open |
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

## Remaining priority order

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
format priority: retire protected receive's v1 layout/prepare/plan/commit paths,
then the remaining CLI publisher/fixture initializer, metadb/GC and
maintenance/replication stored pack-v1 dispatch.
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
   Bound native Git fetch time and prove NFS child-task completion. Unify refresh
   ownership in foreground/helper/coordinator/daemon paths.
5. **Performance: concurrent dispatch and cold ranges.** Warm Git amplification is
   repaired. `mount.rs::mount_config` leaves fuser 0.17 `n_threads` unset; upstream
   `Session::run` defaults it to one. FUSE open/read callbacks synchronously await
   Tokio work, and reads hold the inode content mutex across reconstruction.
   Unrelated cold ordinary blobs additionally share `OdbReader::fills`.
   Reproduce a blocked origin read plus an unrelated native lookup, then measure
   bounded Linux dispatch and concurrent same-inode reads while preserving
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

Remaining located storage-authority boundaries:

- SDK `initialize_remote` was changed upstream in #629 to canonical Capsule
  initialization. The exported old initializer in
  `crates/crab-write/src/initialize.rs` and its legacy fixtures still need removal
  after the gateway retirement; no current production SDK or gateway caller remains.
- `crates/crab-auth-server/src/receive.rs`: manifest-based protected push plans
  accept schemas 1/2 beside the separate Capsule plan. Audit routing, preparation,
  finalization and recovery contracts before deleting those storage-authority paths.
- Gateway read/write, listing, checkpoint ordering, completion recovery and
  maintenance now require Capsule authority at checkpoint-38. No production
  gateway caller selects the old initializer, manifest/journal reader/publisher
  or legacy catalog scheduler. Valid-v1 refusal and signed canonical operations
  pass on RustFS; native VFS LFS reads are repaired at checkpoint-41; LFS publication and
  optimized verification reuse remain separate gaps.
- LFS pre-push ref discovery now requires a Capsule root; its missing-root
  manifest/empty-base fallback is removed at checkpoint-35. First and incremental
  real Git hook publication plus an independent eager clone pass on RustFS.
- `crab/src/cmd/metadb.rs::capsule_owner_root`: optional roots select alternative
  generation, maintenance and recovery paths. `crab/src/cmd/gc/bucket.rs` also
  has missing-root fallback at registered-repository root collection. Read and
  change these authority contracts together; no bucket-wide GC command was run.
- Maintenance/publication/replication dispatch and related docs still require an
  exhaustive storage-authority inventory. The locations above are confirmed
  reachable source leads, not complete removal proof. Manifest projections used
  by rooted Capsule readers are not themselves stored pack-v1 compatibility.

Acceptance requires a real v1-only fixture to be refused, empty-prefix v2
initialization/push to work, and v2 clone/fetch/mount/replica behavior to pass.
Removing only the VFS startup no-root path is not full protocol retirement.
