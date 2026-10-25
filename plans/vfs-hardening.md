# VFS hardening — active implementation

Objective: byte-correct, performant Crab v2 VFS through the real NFS and FUSE
product paths. Crab pack v1 backward compatibility must be removed; the user
explicitly added this requirement during the catalog-transition checkpoint. Linux native mounts use the isolated Colima environment and
RustFS; macOS qualification focuses on native NFS. This ledger preserves the
full objective across implementation checkpoints. No release qualification is
claimed by a partial checkpoint.

Baseline: `origin/crab-v2` at `33f8e6c360d922b3ab1e2c0e5115a2d200eccd97`.
Working branch: `codex/vfs-hardening`. Draft PR:
https://github.com/crabbuild/crab/pull/630 (base `crab-v2`). Follow-up hardening
remains active. The PR checkpoint is rebased onto `9cc0fdab60a`; fresh integration
and installed-role evidence is recorded at the twenty-third checkpoint.

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
| Consistent lifecycle in every mode | Foreground/helper/coordinator/daemon; read-only/writable; automatic/manual refresh; reset/commit; stalled clients and unmount during reads | Open; foreground FUSE refresh and NFS child-task ownership remain gaps |
| Retire Crab pack v1 compatibility | Reject v1 stored repositories at all read/write/selection dispatch boundaries; delete fallback readers/writers and obsolete compatibility tests/docs; preserve empty-repository v2 initialization | Admission/helper/doctor/selected-replica validation and eighteenth-checkpoint replica readiness require v2. Protected receive, SDK and maintenance/publication dispatch remain open |
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
`repository/tests.rs`; `clone_cache` retains cache naming and lock ownership.
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
Runtime tests moved into `mount_runtime/tests.rs` to keep the production module
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

## Remaining priority order

The checkpoint-21 helper-layout gap is closed by the shared Unix client and
checkpoint-23 installed main-CLI proof. Next, align live overlay inspection/export
ownership and read-only policy: the retained RustFS diagnostic shows FUSE rejecting
read-only diff while NFS reads its local overlay paths. Preserve the existing
process-spanning export freeze and prove concurrent reset/commit plus lost-owner
behavior. Do not add an offline retry after an uncertain live operation.

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
callers. No dependency override or vendored patch is authorized.

Correctness gates precede tuning. Generation, cache and descriptor ownership
also determine which performance optimizations are safe. The full hardening
goal remains active; the qualified checkpoint is published in draft PR #630.

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

Remaining located storage-authority boundaries:

- `crab/src/cmd/compact.rs`: missing roots still dispatch to
  `run_legacy_compact_inner`; remove that path and qualify Capsule compaction.
- SDK `initialize_remote` was changed upstream in #629 to canonical Capsule
  initialization. The exported old initializer in
  `crates/crab-write/src/initialize.rs` and its legacy fixtures still need removal
  alongside gateway authority; no current production SDK caller remains.
- `crates/crab-auth-server/src/receive.rs`: manifest-based protected push plans
  accept schemas 1/2 beside the separate Capsule plan. Audit routing, preparation,
  finalization and recovery contracts before deleting those storage-authority paths.
- `crates/crab-s3-gateway/src/repository.rs`: missing-root detection selects
  legacy catalog and generation maintenance at both background owners. Require
  Capsule authority across the scheduling, read-view and checkpoint callers.
- `crab/src/cmd/optimize/xorbs.rs::enumerate_sources`: missing roots fall through
  to global xorb listing rather than the authenticated Capsule inventory. Audit
  rewrite planning/finalization and dry-run consumers together.
- `crab/src/cmd/lfs/push.rs::load_remote_ref_tips`: missing roots still read
  manifest refs. Preserve fresh empty-prefix behavior through canonical v2
  initialization, without retaining manifest-only admission.
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
