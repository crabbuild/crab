# Desktop large-file completion

The interface may expose fewer navigation destinations. That does not reduce
the required capabilities. Completion requires a user action, a real native/storage side effect,
and an accurate visible result. Compilation and mocked tests alone do not prove
completion. This record tracks implementation and qualification separately. The acceptance
matrix and remaining priorities below are the current scope; checkpoint sections
record historical evidence rather than separate work queues.

## Acceptance matrix

| Capability | Required behavior | Current gap / qualification |
| --- | --- | --- |
| File inspector | Distinguish Git, Crab chunks, and Git LFS; show identity, logical/local/cache bytes, availability, pending uploads, and expandable chunk/xorb details. Unknown values remain unknown. | Retained sources and real chunk pages have packaged macOS proof, including pointer replacement during pagination. Local/cache bytes, availability and pending-upload reporting remain open. |
| Bounded reads | Capture a source/revision, read bounded ranges with cancellation and integrity checks, and reject mutation or unavailable content. Cover working tree, index, committed history, and remote snapshots. | Owned native sessions, bounded complete reads and structured errors have focused native/S3-compatible and packaged macOS proof. Packaged local/staged media seeking and close are qualified. Cold 2 GiB SDK LFS ranges and warm proof reuse have dedicated RustFS proof; packaged cold-read progress/interruption and other profiles remain open. |
| Format previews | Parquet schema/sample rows, safetensors headers/tensors, archives, NumPy, notebooks and existing media previews use the captured source. Expensive reads have explicit limits. | Parsers use captured ranges and respect `rev`; NPZ reads headers. Parquet and SQLite SQL use the same source selection and an owned snapshot. Packaged Crab/LFS/Git audio and VP9 video seeking, notebook recovery and local/Jupyter Server execution have bounded macOS proof. Additional decoded/output budgets and database profiles remain open. |
| Diffs | Correct text/binary/pointer classification across working tree, index, commits, branches and reviews. Actual chunk reuse/change metrics and format comparisons; size delta is not changed bytes. | Captured Crab/LFS and nbformat 4 comparisons have packaged macOS and real RustFS proof. Notebook versions, errors, 40,000 cells, output paging and actual worker cancellation are qualified locally. Ordinary Git capture validation, inline reviews, modes, other formats and remaining profiles are open. |
| Repository operations | Track, stage, commit, push, fetch, pull, clone, hydrate and dehydrate through the UI. Preserve attribute routing and LFS identity; explicit migration and supported lock/verification operations. | Exact-path tracking and scoped commits have packaged macOS and real-store local/SSH proof, including plain clones, partial retry, LFS routing, extensionless/literal paths and unrelated staging. Captured single-file and exact-selection Crab/LFS hydration have packaged macOS and real lazy-clone proof. Verified exact-selection dehydration has packaged macOS and real-store SSH proof, including protected/modified/corrupt content and partial retry. A continuous packaged macOS Crab/LFS track, stage, commit, push, lazy-clone, cold-hydrate, small-edit, fetch/pull, literal-path rename and removal sequence is qualified against writable RustFS. Nested missing-parent restoration now has packaged macOS local/SSH Crab/LFS proof. Confirmed index restoration now has packaged macOS Crab/LFS/Git and selected-identity SSH proof. Extensions/profiles remain open. |
| Transfers | Shared operation ownership, real logical/network counters, acknowledged cancellation, supported retry/resume, restart recovery and visible terminal failures. | Single-file and selection transfers now share main-process durable intent, native receipts, cancellation and repository drain. Packaged macOS Crab/LFS proof covers renderer reload, cancellation during preparation, process interruption and explicit Activity retry with a fresh session. Packaged selected-identity SSH proof cancels a held cold LFS request before storage release, preserves pointer/index state and retries exact bytes through Activity. Cold-response cancellation, native/main-process interruption and explicit retry now have bounded packaged macOS Crab/LFS proof. Owned scratch is recovered by the next publication; foreign/replaced objects are retained with visible errors and actual publication counts. Rename/receipt crash boundaries, network counters, version manifests and remaining publishing surfaces remain open. |
| Storage | Inspect usage/budget, retain offline content, pin active reads and safely evict cache. Never remove dirty working files or referenced staging. | Native usage/cleanup and packaged Settings preview/cleanup are qualified; retained workspace data and subsequent staged previews survive. Budget editing/enforcement, SDK namespace lifecycle and offline retention remain open. |
| Connections | The same supported operations work through local and SSH connections with the selected storage identity. | Packaged macOS arm64 SSH trust, paired deployment, remote open, preview/materialization, independent sessions, reconnect replay, empty-start access and saved-repository restart/retry are qualified. Local/direct-SDK and SSH saved identities have real two-store proof, including replacement/removal and cold restoration. SDK GUI opening, separate same-URL identities, active/deferred restart and blocked cancellation are qualified. Device connection editing survives native startup and construction failure, including blocked shell discovery; repaired construction retries in the running app. Complete RPC parity, real multi-hop cancellation and other targets/providers remain open. |
| Qualification | Native Crab and LFS byte-identical round trips; multi-GiB bounded previews; small-edit dedup; interruption/cancel/restart; missing/corrupt objects and changed range validators; visible app results. | Focused native, S3-compatible and packaged macOS preview/integrity/cleanup proof passed. The continuous macOS edit/rename/removal round trip and storage-measured small-edit dedup pass. Whole-file restoration now has bounded macOS local/SSH proof. Cold storage response interruption/restart has bounded macOS proof. Publication/receipt boundaries and cross-platform/provider evidence remain required. |

## Remaining priorities

Work through the P0 rows in order. Extend the qualified Crab/LFS workflow
through Files, Activity and Settings before expanding the format or platform matrix.
The acceptance matrix defines completion. The checkpoints below record evidence.

| Priority | Next closure | Canonical boundary | Required proof |
| --- | --- | --- | --- |
| P0 | Finish transfer recovery and accounting | The main-process [Activity owner](../electron/file-transfers.ts), native publication owner and captured storage readers. | Durable intent, preparation cancellation, renderer reload and explicit restart retry have bounded macOS proof. Cold storage response interruption, completed-sibling preservation, scratch recovery and cleanup-failure counts now have bounded macOS proof. Next: qualify the rename/receipt crash boundary, interrupted SDK cache namespaces and allocation-before-record gaps. Add per-file version provenance and measured network counters. Qualify missing/corrupt objects and remaining component-owned publishing. |
| P0 | Enforce storage policy | Existing verified read/cache owners; Settings remains the policy UI. | Set and enforce a budget, retain chosen offline content, and evict only unpinned verified cache. Active reads, dirty working files and referenced staging must survive pressure, restart and cleanup failure. Measure SDK namespace retention and actual disk reclamation. |
| P0 | Complete recovery parity | [ProviderRegistry](../electron/provider-registry.ts), local/SSH readiness owners, the encrypted vault and existing document/read owners. | Drive auxiliary worktree and cross-repository comparisons, read-only model isolation, Dashboard, Datasets and shared hydration state through their UI entry points during storage rebind and SSH reconnect. Request-owned Git children and native Apply workers now carry the selected saved identity, including filters and hooks. Qualify remaining background Git/status work, removal during SDK opening, real jump-host cancellation and repeated shutdown. |
| P1 | Scale database and notebook workloads | The [captured reader](../agent/src/repository/content.rs), [native query owner](../agent/src/preview/query.rs), [file query session](../src/lib/file-query.ts) and [kernel owner](../electron/jupyter-kernel.ts). | Measure dense multi-GiB scan memory and use captured ranges for selective Parquet scans. Extend WAL and crash qualification across platforms. Exercise notebook installer failure, worker death, output budgets and network loss, including retry during native-reader outage. |
| P1 | Complete advanced repository workflows | Existing provider mutations, [captured diffs](../agent/src/repository/content/diff.rs) and workspace operation owners. | Finish standalone editor RPC parity and task-worktree ownership. Qualify LFS migration, extensions and supported locks, ordinary Git captures, review and mode diffs, plain Git patch cancellation and crash drafts. Distinguish optional SDK configuration absence from missing content objects. |
| P1 | Finish truthful inspection and bounded formats | Captured native/storage metadata, format parsers and exact filesystem-publication ownership. | Report local/cache/available/pending bytes accurately; retain unknown measurements as unknown. Bound plain-text pointers and decoded formats, and show malformed/oversized failures. Load picker profiles from the canonical `crab.toml` policy, removing the retired `.crab/prefetch.toml` parser. Complete remaining format comparisons and consolidate duplicate command/Activity completion notifications. Dataset mtime filtering needs a native timestamp contract and recursive catalog proof. Apply exact temporary-file ownership to workspace polling and native batch dirty counts; qualify very large inventories, network filesystems and external linked-worktree Git-control changes. |

File details, Activity and Settings remain the consolidation points. Reuse one
captured read/reconstruction path, one vault, one repository operation owner and
the existing cache. Fewer navigation destinations must not remove capabilities
or hide unsupported actions. Each row closes with user action → real side effect
→ visible result, including failure and cleanup. The qualified macOS Crab/LFS
round trip and small-edit dedup are bounded results. Multi-GiB bounds, remaining crash
boundaries, corrupt/missing objects, published SSH runtime downloads
and the cross-OS/provider matrix remain release gates for the whole feature.

Consolidate each opportunity at its existing owner: keep selection and file
details in Files, transfer outcomes and retry in Activity, and credentials,
offline retention and budgets in Settings. The continuous lifecycle test reuses
the writable RustFS qualification fixture. It includes confirmed whole-file restoration, renderer reload, preparation cancellation, process interruption and explicit Activity retry. The two-store fixture now also sends a real partial storage response before cancellation or process interruption, and verifies cleanup failures after publication. Extend those cases through the remaining rename/receipt crash boundary. Keep the read-only two-store proxy for identity
isolation. This keeps one ordered queue instead of separate feature, UI and
platform backlogs. P1 work can reuse that fixture once its own P0 owner and
failure behavior are qualified.

### Closure order and proof boundary

1. **Make Activity durable, then enforce storage policy.** Retain repository,
   source/selection and terminal outcomes across restart. A retry must recapture
   current authority and inventory; it must not replay a stale session or claim
   rollback of files already published. Separate written/checked/logical work
   from measured network bytes. Activity now durably records literal selection and
   saved storage identity before main-process dispatch, and retains terminal
   receipts across renderer/app restart. A live native session is never persisted.
   Local/SSH send boundaries reject cancelled preparation. Native admission
   follows cancellation registration and precedes the publication queue and
   filter setup. Cold-response cancellation and native/main-process interruption now extend that proof. The rename/receipt crash boundary and SDK cache namespace lifecycle remain open.
   Budget eviction must respect active reads,
   staging references and explicit offline retention.
2. **Qualify the remaining recovery consumers.** Exercise auxiliary worktrees,
   Dashboard, Datasets, comparison/editor/media state and shared hydration counts
   during rebind/reconnect, including background Git and jump-host cancellation.
   Having a healthy native process is not proof that every repository recovered.
3. **Expand P1 through those owners.** Database/output budgets, supported LFS
   migration/locks, truthful inspection and format comparisons share the captured
   source and operation lifecycle. Repository presets must come from canonical
   `crab.toml` policy; user Settings presets are a separate explicit scope.

Release qualification runs across these slices rather than becoming another
feature queue: macOS/Linux/Windows, supported object providers and architectures,
multi-GiB memory bounds, cold interruption/restart, corrupt/missing objects,
network filesystems and published matching SSH runtime downloads. Close a row
only after visible user-action proof; retain platform/provider limits alongside
the result. Do not infer release completion from the local macOS package.

### Owned publication scratch and cold interruption

Hydration, dehydration and ordinary Git restoration share the native
[publication workspace](../agent/src/repository/content/workspace.rs). A per-worktree
filesystem lease prevents another app profile from reclaiming a live writer's
scratch. Its Git-control SQLite journal uses WAL/FULL and records the worktree,
parent, private directory and individual filesystem object identities before
payload writes. Unix uses device/inode identity; Windows uses volume/file IDs.
The next publication removes only recorded objects whose identities still match,
then removes the empty workspace. No recursive deletion or filename-prefix
ownership inference is used. Replaced objects and unlisted children remain a
visible error with the retained path. Completed publication counts survive cleanup
failure; cancellation never claims rollback of previously published siblings.

A recorded private Git ignore file is written before payload allocation. Ordinary
Git status, Stage All and dirty counts therefore exclude partial payloads even
after a native restart, without a global ignore rule or per-status database
queries. Editor saves retain the shared live path registry; durable editor-draft
recovery and other publishing owners remain separate work. Ordinary Git restore
streams native `cat-file --filters` output into a recorded file using a private
captured index. The index expression retains index-backed attributes when working
attributes are missing. Pipe workers join before scratch is released.

**Activity** lives in the app header, so an empty start still exposes durable
history and **Retry transfer**. An interrupted job labels its counters **Last
recorded progress** and reports unknown outcome. Retry opens current services,
confirms current storage authority and recaptures only eligible paths.

Packaged macOS arm64 qualification uses real read-only RustFS proxies. It sends
part of an LFS or Crab xorb response, holds the remainder, then cancels or kills
the owned native/main process. The completed sibling's SHA-256, pending pointer,
unrelated staged entries and user-owned similarly named file survive. Cancellation
settles before storage release; explicit retry publishes the remaining file and
reclaims abandoned owned scratch. A foreign scratch child deliberately prevents
cleanup after both files are published: Activity retains two published files,
reports the retained path, and the foreign content survives. After removing only
that fixture-owned child, a subsequent verified dehydration recovers the workspace.

This is bounded process-interruption proof. A partial HTTP body does not prove
that the second file already has logical bytes written: LFS may verify its entire
object before returning a range, and Crab may be reading xorb metadata. The native
ownership test separately recovers an abandoned 2 MiB payload. Atomic rename versus
receipt loss, power loss, SDK partial-cache lifetime, eager cleanup on reopen,
network filesystems, external Git-control replacement and cross-platform/provider
qualification remain open. A crash between allocation and its ownership record can
leave empty unclaimed objects; recovery deliberately does not guess their owner.
No chunk resume or measured network-byte total is implied by the transfer counters.

The focused native content gate passes 30 cases; all nine managed content cases
and three file-write ownership cases pass with the matching CLI. The package
passes eight cold-response/recovery cases, the ordinary Git restore regression,
the continuous Crab/LFS lifecycle and selected-identity SSH regression. The 80
focused Electron/renderer cases, project and touched-test types, scoped Clippy
unwrap/expect gate, formatting, build/package and 857 asset/runtime hash checks
pass. No dependency, lockfile or policy-baseline change is needed. The added native
owner pays for durable filesystem ownership and cross-process admission; it
replaces the recursive checkout workspace and reuses the live writer registry.

Prior-head CI is not green: five Ubuntu jobs stopped before tests because package
mirror DNS resolution failed; the Windows runtime found the old checkout workspace's
weak directory identity check. The new owner's Windows file-ID path and replacement
regression still require native Windows CI proof. These are open release gates,
not evidence of cross-platform completion.

### Durable Activity and explicit retry

The main-process [file-transfer owner](../electron/file-transfers.ts) admits
single-file materialization, selection hydration/dehydration and index restoration.
It commits literal paths, repository location and saved storage connection ID
before authentication/setup/native dispatch. Native session and file-capture IDs
remain live-only. Renderer reload subscribes to the owner instead of creating or
cancelling another operation. Repository close and app quit join admitted work;
the shared database closes after the native and transport owners finish.

Receipts retain actual written/checked bytes and published-file counts. A lost
reply cannot overwrite a delivered native terminal receipt. An app restart marks
an unfinished operation **Outcome unknown**, without replay or a rollback claim.
**Retry transfer** opens current services, checks current capabilities/storage
identity, resolves the original literal selection against fresh inventory, and
requires confirmation. Restore retries retain the pending-editor reservation.
Cancellation before native dispatch settles promptly and fences late setup;
after dispatch it waits for native registration, cancellation and cleanup.

The existing profile database uses SQLite WAL with
[`synchronous=FULL`](https://www.sqlite.org/pragma.html#pragma_synchronous).
A profile has one main owner. History retains 200 finished jobs and every active
job; raw logs and credentials are not persisted with receipts. Admission fails
closed when history cannot be written; receipt failure requests cancellation,
preserves known in-memory counters and exposes **Reload Activity** for recovery.
Corrupt history is reported rather than deleted or reset. These are operation
receipts, not a native chunk-download checkpoint or a measured network counter.

Packaged macOS arm64 proof extends the writable RustFS Crab/LFS lifecycle with
renderer reload during held preparation, cancellation followed by a rejected
late native send, a main-process kill after durable admission, unchanged pointers
on restart and confirmed retry with a new native session and exact content.
The two-store SSH fixture also holds a cold LFS request, cancels through
Activity before storage release, verifies unchanged pointer/index state, then
retries exact content with the selected identity. Command feedback recognizes
acknowledged cancellation and does not claim rollback of published files.
The ordinary Git restore smoke injects a real SQLite admission failure: no file
changes, the Activity error remains visible, and **Reload Activity** restores
admission after the database becomes writable. The local lifecycle also verifies
that a second instance for the same app profile exits while the owner stays live.
All 235 focused cases in 12 suites pass, along with project/touched-test types,
Rust formatting, renderer/Electron builds and 857 packaged asset/hash checks.
That Activity-only checkpoint reused its previously qualified native pair;
the owned-scratch checkpoint above rebuilds the pair. Focused cases
cover partial publication, lost replies, persistence failure, capture/session
isolation, shutdown drain and both native send boundaries.
Mid-stream crash recovery, power-loss durability, very large history/inventories,
network filesystems and other platforms/providers remain qualification gates.

### Confirmed whole-file restoration

Changes rows/context menus, zero-hunk binary/pointer cards, exact batch
selections and both discard palette commands use `files_restore` through the
existing repository transfer owner. Fresh porcelain XY status preserves staging
and excludes untracked files. Every restoration confirms permanent loss of
unstaged changes; the fabricated Undo action is removed. Staged-only previews
show **Index vs HEAD** with whole-file discard disabled. Negotiated capabilities
disable unavailable controls consistently.

The native [restoration owner](../agent/src/repository/content/restore.rs)
captures raw working-file identity and the immutable index OID/mode. Crab/LFS
restoration reuses verified SDK materialization. Ordinary Git verifies its blob
OID, exports a private index with native
[checkout-index](https://git-scm.com/docs/git-checkout-index), retains checkout
conversions, then verifies the blob again. Shared
[publication](../agent/src/repository/content/publication.rs) rechecks observed
working content/metadata and index identity before atomic publication. Missing
paths use non-clobbering publication and owned parent cleanup. Cleanup errors
retain actual published counts; multi-file restoration does not promise rollback.

Clean editor buffers remain reserved through confirmation and native completion.
Pending/admitted saves prevent restoration. Stale reads cannot replace the
restored view, and a late draft is retained without autosaving over publication,
including during recovery. Activity cancellation retains the captured native
session; admission is reported only after native cancellation registration and
before waiting for the publication queue/filter setup. Git conversions can
change byte counts, so restoration reports actual written bytes with an unknown
total rather than inventing a denominator.

The matching unsigned macOS arm64 package passes four dedicated cases: literal
staging/unstaging; ordinary Git restoration with literal row/card/batch scope,
staging/untracked preservation, confirmation dismissal, a real denied editor
save and subsequent clean reload; the full writable RustFS Crab/LFS lifecycle
extended with restoration of staged content that differs from HEAD; and real
two-store SSH palette restoration with the selected saved identity, unchanged
index, identity replacement and saved restart. Screenshots were inspected.
The local case checks Activity's real published-file counts and staged-only
controls. This closes the bounded safe-restore slice, not the release matrix.

Focused desktop tests pass 178 cases in ten suites. Native proof includes 25
content cases, eight restoration cases (including real Crab/LFS reconstruction,
missing/corrupt objects, concurrent edits/index replacement, modes, missing paths,
split indexes and checkout conversions), three ref-checkout cases, six managed
materialization/configuration cases and the dehydration cancellation/replacement
regression. Project/touched-test types, Rust formatting, native/renderer/Electron
builds, packaging and all 853 packaged asset checks pass. Native producer inputs
match the qualified tree. Evidence lives in the external build directory:
`restore-lifecycle-ui.log`, `restore-ssh-ui.log`, `restore-native-qualified.log`,
`restore-managed-regression.log`, `restore-dehydration-regression.log`,
`restore-renderer-tests.log` and `restore-preflight.json`.

The agent's `--no-deps` Clippy `unwrap_used` gate passes; style warnings remain.
The dependency-inclusive gate stops at two unwraps in
[bitmap_remap.rs](../../../crates/crab-metadata/src/git_visibility/bitmap_remap.rs)
(lines 109/112). Its blob is identical to target base `6774625`; no dependency
source or lint baseline was changed. Broader CI, release runtimes and the
cross-platform/provider gates remain required. Cold interruption/restart,
pre-admission cancellation, durable Activity and enforced budgets lead the
remaining queue above.

### Auxiliary transfer consolidation

The palette, Settings prefetch presets, Files/Pointer Map and auxiliary worktree
menus now resolve current Crab/LFS inventory to exact paths and call the existing
repository-owned `runFileTransfer`. The renderer's raw CLI hydrate/dehydrate
wrappers and file-tree fallback are removed. Direction-specific negotiated
capabilities disable unsupported actions. Selection transfers confirm every
dehydration and hydration at 1 GiB/100 files; confirmation retains the original native session so
recovery or a storage rebind cannot adopt the old intent. A dismissed palette
confirmation reports no failure and creates no fictitious operation.

Local auxiliary worktrees open a dedicated handle with the parent's selected saved
identity and retire that handle when their panel ends, including late opens.
The global transfer owner joins admitted work before closing it and rejects late
intent while it drains. Initial capabilities are queried for the captured
session: an open event can precede the panel retaining its handle, and a stale
query cannot enable a replacement. Inventory and native errors remain visible.
Transfer menus require that handle and its inventory; they cannot silently
target the parent tree. Settings reports the actual published file count after
the native terminal reply. This consolidates transfer admission and UI intent;
durable Activity and the recovery matrix above remain open. Whole-file restoration has its separate qualification above.
The agent chat's Files tab still has no native repository/source owner; its task
worktree integration remains in advanced-workflow/recovery scope. It receives no
raw CLI transfer fallback from the shared tree.
Auxiliary SSH panels must not send a remote path to the local agent or display
the parent tree as if it were that worktree. They show an explicit limitation;
the existing **Open worktree as full workspace** action reaches the SSH Files
workflow. Shared SSH handle leases and task-worktree source ownership remain
recovery/advanced-workflow priorities, not qualified auxiliary transfer support.

The matching unsigned macOS arm64 package passes three dedicated cases: literal
staging/unstaging, the complete writable-store lifecycle (including palette
hydrate/dehydrate, dismissed confirmation, Settings Crab/LFS prefetch and exact
local auxiliary-worktree hydration), and the real two-store SSH identity/restart
workflow. The SSH case now exercises literal single-file palette transfers and
the visible auxiliary-panel limitation before continuing missing-parent restore,
selected tracking and identity replacement/removal. Auxiliary hydration counts
are awaited and inspected after publication; bytes and semantic index entries
remain exact, and the parent tree stays unchanged.

All 53 focused transfer/command/status cases pass, including multi-publisher
close drain, late-intent rejection, failed publication cleanup, independent
siblings, literal selection, direction gating, confirmation cancellation and
captured-session retention. Project types, strict touched test types, production
builds, Rust formatting, packaging and all 853 packaged asset checks pass. Native
inputs are unchanged from the preceding qualified reconstruction owner. Evidence:
`selection-owner-tests.log`, `selection-owner-lifecycle-qualified.log`,
`selection-owner-ssh-qualified-final.log`, `selection-owner-preflight.json` and
the accompanying screenshots in the external build directory. The initial GUI
failure exposed missing auxiliary capability initialization; the long regression
also required explicitly selecting Changes after visiting Worktrees. Those
failures remain recorded alongside the passing runs. Cross-platform/provider
release proof remains open.

Production source grows by 98 lines: the shared selection adapter replaces
duplicated CLI callers, and the transfer lifetime owner prevents view teardown
from closing an admitted publisher. No second reconstruction/cache/vault path,
new setting, dependency, native RPC or SDK API is introduced. Consolidating native
and command completion toasts remains a UI opportunity within the Activity owner.

## Missing-directory hydration checkpoint

Missing-file hydration now captures a resolved index pointer even when its
parent tree is absent. It reconstructs into a private file beside the nearest
existing ancestor, verifies the full content, rechecks the pointer identity and
only then creates the required directories. Publication refuses to overwrite a
new file. Index modes and unrelated staging remain unchanged. Existing-directory
and single-file callers retain the same reconstruction/publication owner.

Every created directory has a cleanup owner. Failure removes only empty owned
directories whose observed identity still matches; concurrent content and
replacement directories are retained. Parent links and redirected paths are
rejected. Existing files continue using their captured pointer/metadata checks.
The existing SDK filter installer now belongs to one native owner shared by
tracking and both single-file/bulk transfer directions. Transfers pass their
SDK cancellation scope and drain the configuration client. This repairs plain
Git clones before publication: hydrated bytes must clean back to their indexed
Crab/LFS pointer even after stat-cache invalidation. No new configuration option,
dependency, public API or second reconstruction path is introduced.

All 24 focused native content/diff/publication cases pass, including real Crab
and LFS reconstruction, executable modes, cancellation, concurrent creation,
index replacement and symlink redirection. The packaged writable-store workflow also passes: it removes the clone's nested
parent tree after synchronizing the Crab rename and restores its exact edited
bytes. It then restores a staged LFS pointer into another absent tree, retaining
the staged index and clean working content. Both actions wait for visible
completion before checking filesystem bytes. The packaged two-store SSH case
also restores staged Crab/LFS pointers into a shared missing tree, verifies exact
bytes and index preservation, invalidates stat shortcuts, and proves fresh Git
comparisons clean both formats through the selected identity. The same case
continues through tracking/commit, identity replacement/removal and cold restart.
Other targets and network filesystems remain unqualified.

The SDK filter-configuration cancellation regression also passes without changing
Git settings. The production Clippy `unwrap_used` gate, strict touched smoke
types, native/renderer/Electron builds, packaging and 853 asset checks pass.
Evidence is in `missing-tree-native-final-tests.log`,
`missing-tree-filter-cancellation.log`, `missing-tree-configured-lifecycle.log`,
`missing-tree-configured-two-store.log`,
`missing-tree-configured-ssh-storage-final.log` and the corresponding external
package/proof directories. The packaged SSH bootstrap/read/materialization and
acknowledged-cancellation case also passes on this runtime pair.

The desktop CI consumer now downloads the producer's POSIX release-runtime
artifact as well as its native payload; the SSH fixtures require those exact
hashed files. Earlier wiring uploaded them but did not supply them to the
desktop job. No public-release download availability is claimed by these fixtures.

## Current qualified boundary

The combined `crab-v2` native task/process runtime and this branch pass the
packaged macOS arm64 writable-store lifecycle. Both Crab and LFS files pass
track → stage → scoped commit → push → lazy clone → cold hydrate → small edit →
stage/commit/push → fetch/pull/hydrate. The extension renames the Crab file to a
literal bracket path, deletes the LFS file, publishes and synchronizes those
changes, then verifies the renamed content and clean clone status. Unrelated
staging survives throughout. The test compares captured chunk identities and
actual newly stored xorb bytes; these are storage measurements, not network
transfer counters. Rename/removal adds no Crab xorbs.

The same test refreshes branch metadata while the Pull menu is open, then
performs a real pull. Branch refresh retains the tree and action geometry;
keyboard focus and an open menu retain visible actions. Global Settings commands
reach the mounted repository instead of only recording a history observation.
Reopening the repository queries capabilities for its current native session. Only pending requests for
the same session are shared, and stale responses cannot enable a replacement.

The current SSH deployment owner consumes an exact-version, exact-target
CLI/sidecar pair from official release assets. It verifies the device cache and
immutable remote install; development uses the native build directory. The old
archive catalog and renderer-owned host-trust prompts are retired. A parented
native dialog owns trust decisions, and each sidecar process owns a private
native index. The dedicated trust test scripts the OS decision while exercising
real SSH authentication, rejection, rotation and durable pins. Other SSH
fixtures pin only their owned loopback server's public key in an isolated app
profile; they do not claim to test the native dialog. Unreleased fixture runtimes
are cached through the actual resolver from the build's hashed release outputs.
The public download for this alpha version returned HTTP 404 during qualification;
availability and download of every published runtime pair remain a release gate.

Local process launch retains cancellation through asynchronous environment
preparation and acknowledges initialization writes at the child pipe. The router
retains source/session fencing, saved-identity injection and acknowledged native
EOF cleanup. Native task processes capture one SSH channel and verified endpoint;
they cannot move writes to a replacement. Apply workers inherit only the selected
storage identity, including Git filters and hooks. A real SFTP/native-worker test
checks the selected identity and rejects inherited ambient session credentials.

The rebuilt package also passes six real-store preview/tracking and saved-identity
cases, including SSH cold reads, replacement/removal, restart and distinct
same-URL snapshots. Workspace settings remain disabled for read-only snapshots.
Hydration/dehydration assert pointer IDs, modes and unrelated staging; verified
Git stat-cache updates are intentionally allowed. Eight packaged SSH cases also
pass: paired deployment, real previews/materialization, cancellation, host-trust
rejection/rotation, reconnecting edits, blocked-sibling metadata and saved reopen
with local services both available and unavailable.

Current proof includes 410 focused TypeScript cases, 21 native content cases,
handshake method uniqueness, native process/shutdown cases, real SSH deployment
and Apply worker execution, project types, serial native/renderer/Electron builds,
packaging and producer/asset hash verification. Production native Clippy's
`unwrap_used` gate passes on the combined base; older checkpoint failures at three
base sites are historical. The repository still has no ESLint configuration,
and broad test-tsconfig errors and the cross-platform/provider matrix are not
qualified. Local packages are unsigned and credential fixtures use Electron's
test keychain. These bounded results do not establish full completion.

Resume the ordered P0 queue: safe discard, durable Activity
recovery and budgets, then remaining recovery parity. New formats and providers
reuse those owners and their error/cleanup contract. Evidence is recorded in
`lifecycle-qualified-lifecycle.log`, `desktop-lifecycle-proof/`,
`lifecycle-qualified-two-store.log`, `selected-tracking-remote-proof/`,
`lifecycle-qualified-ssh.log`, `lifecycle-final-ssh-proof/`,
`lifecycle-integration-focused-final.log`, `lifecycle-integration-native-content.log`,
`lifecycle-integrated-native-ssh.log` and `lifecycle-qualified-preflight.json`
in the external build directory.

## Selected tracking and commit checkpoint

One native tracking handler serves the wizard and the large-file warning in the
Changes composer. It registers anchored, quoted literal rules for new paths,
retains existing Crab/LFS routing and validates indexed pointer bytes before
reporting success. The existing SDK local configurator installs the paired Crab
executable as both repository-local filter drivers. This closes the plain Git
clone gap without a second filter installer. SDK clients close before staging.

`git_commit_paths` delegates exact literal selection to Git's `commit --only`.
It commits current working contents, including edits after tracking, and keeps
other index entries staged. Both the wizard and Crab staging section use it.
The ordinary Changes composer keeps its staged-index behavior. The pending
whole `.gitattributes` file belongs to the wizard selection, including user edits
within that file. The wizard shows pending tracking rules and states the
current-contents commit behavior; the Explorer guide records whole-file scope.
An older agent rejects the distinct scoped-commit method instead of silently
committing the whole index. Hydrate's pattern picker decodes the generated quoted rules and
matches their anchored/escaped paths with the existing minimatch dependency.

The packaged macOS arm64 real-store cases verify partial failure and retry,
literal bracket and extensionless names, preserved LFS routing, edits after
tracking, byte-identical committed reads, an unselected decoy and unchanged
unrelated staging. The SSH case starts with a plain Git clone and verifies the
selected storage identity on tracking and scoped-commit wire calls. The SDK
storage-replacement regression and mixed hydration/dehydration round trip pass.
The real-store fixture uses Electron's test keychain for the unsigned package.
The two-store proxy is read-only: these tests do not qualify pushing, offline
tracking, unreachable storage, transfer cancellation or the complete lifecycle.

There are 13 passing native Git/attribute cases and 90 focused TypeScript cases.
Project and strict touched smoke-spec types pass. Native, renderer and Electron
builds, serialized packaging, producer asset verification and Rust formatting
pass. An earlier packaging run overlapped Electron output generation and had
mismatched assets; rebuilding and packaging serially corrected it. The existing
three base Clippy unwrap errors, missing ESLint configuration and unrelated broad
test-tsconfig errors remain unqualified gates. No dependency or lockfile changes
are required. Tracking/parser consolidation removes duplicate product paths;
new tests exercise real Git and packaged UI outcomes.

Evidence lives in the external build directory under
`selected-tracking-final-*.log`, `selected-tracking-remote-proof/`,
`selected-tracking-final-roundtrip/` and `selected-tracking-qualification.json`.

## Verified dehydration checkpoint

Large Files, the working Explorer and Storage Map use the same native transfer
owner for exact Crab/LFS selections. Dehydration requires a resolved regular
index pointer whose mode and identity match HEAD. It hashes both the hydrated
working file and reconstructed saved content before publishing the exact pointer
bytes. The existing CLI applies `crab.toml` always-prefetch protection using its
normal matcher. Index entries, commit identity, working metadata and resolved
paths are rechecked before same-directory atomic replacement. Permissions remain
intact. Reconstruction readers and staging pins survive through publication and
joined cleanup. Existing single-file hydration shares the publication owner.

Activity reports verified bytes across both passes, the actual published file
count and terminal failures. Confirmations retain exact paths and the observed
native session. Local and SSH controls require the negotiated native method.
History views cannot mutate the working tree. Primary Explorer and Storage Map
callers use this owner; palette, Settings and auxiliary-worktree callers remain
in the ordered closure queue.

The unsigned macOS arm64 package passes mixed round trips and partial retry.
The local case preserves same-size edits, protected files, corrupt backing
content, unrelated staged changes and executable modes. Explorer hydration also
uses the native path. It passed an isolated retry and three consecutive repeats.
The packaged native replay additionally preserves files with missing backing or
changed index entries. The real two-store SSH case verifies native method and
storage-identity routing, exact pointer/index bytes and subsequent cold restart.
The direct SDK storage-replacement case and local SQLite/WAL case pass.
Real-store credential setup uses Electron's test keychain for this unsigned Mac
package. Linux, Windows and additional providers remain unqualified.

There are 21 passing native content cases, including six real CLI cases with
cancel-at-publication and same-size replacement with restored mtime. The five
focused TypeScript suites pass 133 cases. Project and focused strict smoke types,
Rust format, native/renderer/Electron builds and packaging pass. An initial local
hydration assertion and an initial SSH SQLite assertion each timed out before
the new dehydration checks; retries passed without assertion or timeout changes.
Their original causes remain unproven. Diagnostic capture is retained. Production
Clippy still fails on the three unchanged base unwrap sites in forge handlers
and directory-size scanning. The broad test tsconfig also retains unrelated
configuration/type errors; focused smoke probes pass.

Evidence lives in the external build directory under
`bulk-dehydration-packaged-native-proof.json`, `bulk-dehydration-rust-tests.log`,
`bulk-dehydration-rust-real-tests.log`, `bulk-dehydration-ts-tests.log`,
`bulk-dehydration-local-repeated/`, `bulk-dehydration-remote-proof/` and
`bulk-dehydration-qualification.json`. Product code grows to own the two-source
verification and shared batch/publication lifecycle. This adds no dependency,
second reconstruction implementation or new navigation destination.

## Mixed bulk hydration checkpoint

Large Files → Hydrate captures the exact selected paths for one repository
operation. Each file uses the same verified writer as File details. Missing
files with an existing parent use the resolved index pointer, preserve the Git
executable bit and use no-clobber publication. Activity shows each file that
reached disk and reports a terminal error if a later file fails. The selection
survives a status refresh so the next attempt can skip files already hydrated.

The prior packaged inventory classified a 128-byte LFS pointer as hydrated when
its logical size was also 128 bytes. The new inventory parses the bounded
working bytes before comparing sizes. The native and packaged fixtures exercise
literal bracket characters in paths, mixed Crab/LFS content, restoration of a
missing LFS file, unchanged index entries and an untouched out-of-scope pointer.
A damaged LFS object stops the batch after an earlier file has reached disk.
Activity retains the published count, and the failing pointer stays intact.

## Captured database query checkpoint

Browse and SQL share the file revision. SQL remains available while Browse is
loading or has failed. It cancels an unfinished browse when the user switches
modes. The repository session retains SQL drafts, results and recent statements
across file switches and native-session recovery. Late completions from a retired
session are ignored.

The storage vault advances the source generation for repositories whose binding,
endpoint or credentials change. It retires affected SQL results at commit time.
The same fence prevents new SQL from reaching the old source during reopening
and discards a reply that arrives after the binding changed. The editor keeps
the draft and recent statements for an explicit rerun. Native repair and
connection display-name changes preserve results from the same storage source.
The macOS arm64 SDK test reproduces three stale rows in the prior package
after switching endpoints. The updated package clears the result and returns
the second store's four rows on rerun. SSH connection removal and repair also
clear the old result, and a same-source native restart retains it. The two
real-store tests use Electron's test keychain for the local unsigned package.

The native query owns one private snapshot at a time per cache root, using the
existing cache lease to exclude other processes and cleanup. Agent startup and
new query admission reclaim the named snapshot only after acquiring that lease.
Git reads use a single sequential blob stream, and Crab/LFS reads verify the
complete file before SQL can open it. Source capture, admission, copying and
execution share a 60-second budget. Private snapshots are capped at 8 GiB. The
engines return at most 1,000 rows and about 2 MiB of serialized results. Large
integers and decimals retain exact text. Cancel joins the engine and removes the
snapshot before the query reports its terminal status.

DuckDB opens the captured Parquet file through the `data` view and streams
result batches. Parquet still copies the full captured input for each operation.
Process-wide memory limits and selective Parquet reads remain open. The current
DuckDB Rust table-function API supports column projection. A replacement scan
also needs SQL predicate admission and parity with the bundled Parquet reader's
type conversions.

The live SQLite baseline returned a checkpointed value of 17 while its WAL
contained a committed value of 91. Browse and SQL now pin a read-only
transaction in an isolated process for an active working database. SQLite can
maintain its shared-memory index while the transaction is open. The process
boundary protects SQLite's POSIX locks from unrelated file closes in the sidecar.
[SQLite locking](https://www.sqlite.org/howtocorrupt.html#posix_advisory_locks_canceled_by_a_separate_thread_doing_close_)

Git history and the index select both entries in one tree or index listing. The
SDK checks the same snapshot's parent tree for the companion. The private pair
uses SQLite's WAL recovery after checking the current frame sequence. Cleanup
covers the private database, WAL and shared-memory index.
[SQLite WAL format](https://www.sqlite.org/fileformat2.html#the_write_ahead_log)

The packaged macOS arm64 test saw committed value 113 in Browse and SQL while a
writer held uncommitted value 991. It recovered historical value 91, selected
staged value 113, read a working LFS-pointer WAL, and rejected a damaged WAL and
an attempted SQL update. Cancel waited for the live reader process to exit.
The database, WAL, pointer and index bytes were unchanged. In a native replay,
killing the sidecar also stopped its live reader and preserved those bytes.
The paired SSH package read value 81 from a lazy clone with a Crab database
pointer and an LFS WAL pointer, which both stayed intact. The direct SDK read
value 82 from the second RustFS store after a storage rebind. The unsigned test
package used Electron's test keychain for both storage cases.

The unsigned macOS arm64 package returned historical rows for SQLite, LFS Parquet
and Crab-chunked Parquet. It retained SQL drafts and results through file changes
and a real native restart, cancelled a running DuckDB aggregation, removed the
temporary snapshot, and showed result truncation and exact large-number values.
The database, pointers and index stayed byte-identical. The desktop workflow runs
the same packaged case on its platform matrix.

The bundled native agent also queried an isolated RustFS repository through its
SDK session, with a fresh content cache. Historical and latest Crab and LFS
Parquet counts differed as expected, and SQLite returned the selected revision's
value. With the LFS object read held at the transport, cancellation joined and
removed the snapshot before storage was released. Retry succeeded. The read-only
proxy observed 42 GETs, one HEAD and zero storage writes across the run.

The macOS arm64 app now also cancels cold LFS Parquet SQL through the direct SDK
and an SSH-deployed native runtime. Both tests hold the real RustFS object read,
observe the query's private snapshot, click Cancel, and verify that the snapshot
is gone while storage is still held. Retry shows the expected result and the
read-only proxy records zero writes. These runs used Electron's test keychain
because the unsigned build could not access the local macOS Keychain.

The local packaged test starts a second agent while the query owns its snapshot,
then kills the query owner and reopens the repository. The second agent leaves
the live snapshot intact. The replacement agent removes the abandoned snapshot,
and the next query succeeds with the source files and index byte-identical.

The packaged native agent queried a 3 GiB sparse local Parquet fixture with the
original three rows and footer preserved. The full 3 GiB snapshot took 42.24
seconds end to end, with 46.7 MiB peak sampled agent RSS and cleanup on success.
The fixture exercises the input copy. Dense multi-GiB column decoding and
selective reads remain qualification gaps.

## Local notebook execution checkpoint

The prior Node kernel module is byte-identical to the requested `crab-v2` base.
It times out with a supported Jupyter runtime after that runtime creates its
connection file. Its stderr path handling discards the actual directory; its
interrupt signals the Jupyter launcher, whose signal handler shuts down the
kernel. It also has no shell-reply consumer or authenticated receive path.

The selected Python environment now owns a packaged worker using Jupyter
Client's `AsyncKernelManager` and async client channels. Runtime/spec discovery
and optional installation use the shared asynchronous login environment; guessed executable
lists, synchronous shell commands and the Node protocol implementation are
removed. Jupyter owns connection files, authentication, readiness, the kernel
provisioner and platform-specific interrupt/shutdown behavior. The worker is
explicitly unpacked for Python access under Electron's ASAR contract. The npm
ZeroMQ dependency and its three unused lockfile entries are removed; no npm
dependency is added. The Jupyter web-server sidecar remains a separate server
service under its existing owner.

Execution completes after both its shell reply and IOPub idle. The renderer
associates a request before IPC admission, preserves live outputs/running cells
in one notebook owner across tab and Settings navigation, and stops Run All on
failure. Same-document native repair keeps kernel variables and live outputs;
replacement documents or kernels retire old request associations. Clear-output
and display-update messages use that same output owner. Shell reply plus idle
releases the running state, not the output area: each cell retains its latest
parent/request route until rerun or document/kernel replacement. Post-idle
background output reaches its original cell; retired runs cannot overwrite a
replacement output area. There is no second kernel client or component retry queue.

Routing follows the transmitted parent header. A standalone reference-client
control with ipykernel 7.4.0 reproduces bare-thread output carrying the following
cell's parent; explicitly copying the originating Python context retains the
original parent. The fixtures carry that context on both qualified runtimes;
they keep the original-cell and retired-route assertions. The prior package
still fails the original-cell assertion with that fixture. No parent guessing
or runtime-specific routing is added. This follows the kernel's
[ContextVar-based stream parent](https://github.com/ipython/ipykernel/blob/v7.4.0/ipykernel/iostream.py#L551)
and Python's [explicit context management](https://docs.python.org/3.12/library/contextvars.html#manual-context-management).

Main admits a renderer-assigned connection ID before environment preparation.
The existing owner exposes Cancel while preparation/readiness/restart is pending;
cancellation retires the renderer generation and joins cleanup before retry.
The worker keeps command admission active while the reference client waits for
readiness. It joins provisioner launch/restart before cancelling the client wait,
so cancellation cannot leave the manager's readiness future unresolved or abandon
an owned process. Final native admission and worker creation share one synchronous
turn after preparation. Main binds initial local startup to the caller's native
session and the agent-owned physical working tree, and restricts controls/events to the initiating window. A remote or SDK
path cannot become a local kernel directory. Repository close, window teardown
and normal Quit join worker shutdown. EOF makes the worker close its kernel,
channels and temporary connection data before exiting. Failed admission without
a process uses resource cleanup. Runtime-command admission closes on Quit;
queued environment lookups and read-only probes cancel, while admitted pip
mutation retains ownership through completion.

The native admission check runs after environment preparation. A held lookup
plus a real repository-owner replacement reproduced stale startup in the
intermediate package; the final package rejects it before creating any Python
worker, displays the failure, and connects successfully on explicit retry.

The combined recovery smoke also exposed a native crash-loop modal: a second
native failure within 30 seconds blocked the main process in `NSAlert.runModal`,
including independent notebook and Settings controls. The existing native owner
keeps its one automatic restart and crash diagnostics. On budget exhaustion it
now publishes repository errors and the existing failure notification, blocks
background starts, and leaves explicit local/SDK repository reopening to reset
the budget after old streams close. No app-wide Restart/Quit dialog or second
restart owner remains. Local/SDK unit checks also cover a manual reopen already
waiting for the old process to drain.

Ten real-kernel cases cover immediate output/completion, native stdout pipe
isolation, variable-preserving interrupt, process-replacing restart, busy
shutdown, failed startup cleanup, late output/rerun retirement, held preparation
and blocked startup/restart cancellation. Eight notebook-owner cases cover early
and post-idle outputs, detached views, replacement documents/kernels, display
clearing/update, visible execution failure and retired connection/restart replies.
A blocked shared-environment case proves zero late runtime processes after
shutdown. Together with the existing format/read regressions and 41 native lifecycle cases, seven focused suites pass 104 cases.

Two packaged notebook cases drive Python selection, a real failed kernelspec
and retry, stale startup admission with zero workers, sequential Run All,
physical side effects, tab/Settings disposal, repeated native
repair, interrupt, restart, stop-on-error and busy disconnect/normal Quit. They
also cancel held environment preparation and blocked startup/restart readiness,
connect again, and deliver post-idle output to its original cell. The prior
package drops that output; the prior worker takes about 31 seconds to disconnect
from each blocked readiness phase. The fixed cancellation cases finish within
the eight-second cleanup assertion without waiting for readiness timeout. They
verify retired worker/kernel PIDs, actual working directory, unchanged notebook
and index bytes, and compact controls. Twenty packaged recovery/media/output
cases pass together. macOS arm64 is the qualified local target; CI adds the same
pinned runtime and notebook proof on the desktop platform matrix.

The final notebook rerun creates a fresh isolated Python environment and drives
Check runtime → Install Jupyter Client + ipykernel → kernel selection → execution.
It records Jupyter Client 8.10.0/ipykernel 7.4.0, verifies the running kernel's
actual environment prefix, then passes the full cancellation/recovery/output
case. The other case retains 8.6.3/6.29.5. All ten real-kernel ownership cases also
pass independently on 8.10.0/7.4.0. The installer changes only the owned test
virtual environment; no repository or global Python dependency is changed.

Installer failure/retry, controls during native-reader outage, abrupt worker/kernel
recovery, output/decoded budgets, SQL queries and broader platform qualification
remain release gates.
Protocol and packaging contracts follow the [Jupyter messaging specification, including asynchronous output after idle](https://jupyter-client.readthedocs.io/en/stable/messaging.html#kernel-status)
and [Electron ASAR limitations](https://www.electronjs.org/docs/latest/tutorial/asar-archives).

## Remote notebook execution checkpoint

Notebook Connect accepts a Jupyter Server base URL and optional kernel name. It
uses the selected Python environment's Jupyter Client and Tornado, which Jupyter
Client already requires. The server starts a named or default kernel and runs
cells in its configured environment. Required files must be accessible there.
Tokens travel in authorization headers over HTTPS or loopback HTTP. The same
notebook owner handles request routing, cancellation and window/repository/app
cleanup for both local and remote execution. The server transport waits for the
matching kernel-info reply and idle event before exposing run controls.
[Jupyter Server REST API](https://jupyter-server.readthedocs.io/en/stable/developers/rest-api.html#post-api-kernels) ·
[Jupyter kernel WebSocket protocol](https://jupyter-server.readthedocs.io/en/stable/developers/websocket-protocols.html)

The packaged macOS arm64 test uses Jupyter Server 2.17.0 with a token and base
path prefix. It proves rejected authentication and retry, cancellation while
creation is admitted but its response is held, remote file side effects and
visible output, interrupt, restart, busy disconnect and normal Quit. Cancellation
joins the create response and deletes the acknowledged kernel before allowing
retry. An unrelated running kernel survives all controls. Notebook and index
bytes stay identical. Both packaged local notebook cases and 104 focused cases
also pass. The desktop CI matrix now exercises the same server fixture.

A connection lost before the server acknowledges creation can leave a kernel
requiring cleanup on the server. Network-loss recovery, abrupt worker death,
TLS deployment and broader provider/platform qualification remain open.

## Notebook and media recovery checkpoint

The prior package kept the notebook's old text after a same-ID native repair.
Its read effect did not observe the session. The view now carries its caller's
native identity, aborts obsolete reads, waits visibly during recovery and offers
Retry notebook after read failure. A real permission failure and repair/retry
are qualified. Reading replacement contents clears output/message associations
belonging to the previous document; the separate kernel state is retained, with
live execution qualification still open.

Audio and video previously reopened at time zero with default volume, mute and
rate. The shared media owner now retains scalar playback state for the selected
repository/path/revision, releases the old element before React detaches its
ref, and restores controls and a duration-clamped position when the replacement
reader's metadata arrives. Teardown events cannot overwrite user intent. Paused
media stays paused; playing media attempts to resume. A denied automatic play
shows a manual Play hint without hiding the recovered controls. Both media tags
use the same handlers and state; no extra content cache or reconstruction path
is introduced.

Saved HTML and SVG output checks also reproduced handlers executing inside the
app document and calling the desktop bridge without running a cell. Both rich
formats now share a sandboxed document with no script or same-origin exemptions
and a restrictive resource policy. Embedded data images/fonts and rich markup
remain displayable. The existing effective-theme owner supplies the frame's
color scheme. Notebook comparisons already display escaped output text; PNG/JPEG
outputs and standalone SVG images retain their image-element path.

The bounded macOS checks cover paused/playing audio and VP9 video, denied
automatic resume with manual recovery, detached-element release, old-reader
revocation, unchanged index bytes, notebook permission retry and native repair,
and static HTML/SVG output in dark/light themes with no handler execution or
parent bridge access. Broader source/profile/platform recovery and live kernel,
query, decoded-budget and malformed-document requirements remain open.

Qualification passes 44 focused cases and 18 grouped packaged cases. The two
rich-output cases also pass the expanded 960 × 640 layout checks; screenshots
confirm visible controls and readable output in dark and light themes. The
additional product code owns playback state and one shared rich-output frame;
it adds no reader, retry queue or content cache.

An isolated Jupyter Client 8.6.3 / ipykernel 6.29.5 control becomes ready, executes
code and shuts down cleanly. The byte-matched packaged Node kernel module instead
reports Kernel startup timed out after Jupyter creates its connection file in
the explicit runtime directory. The qualification controller then closes its
launcher and verifies no owned kernel descendants remain. This is an actionable
kernel-client gap, not evidence that notebook execution is complete.

## Physical file caller-session checkpoint

A packaged SSH regression held a completed settings read while closing and
reopening the real native repository owner. Runtime notification delivery was
held independently, so the renderer still owned its earlier binding. The prior
package acknowledged the task mutation and overwrote the replacement document
with that earlier read. The physical-file bridge had selected the provider's
current session instead of retaining its caller's session.

Repository-scoped `fsReadFile` and `fsWriteFile` now carry the caller's session
through preload, main, provider and credential preparation to the existing final
wire guard. The document owner uses the same session for its read and write;
the shared editor writer captures its observed session on each save attempt.
SSH cannot substitute a replacement or missing session. Runtime delivery can
then rebind and retry the retained buffer under the new owner. Local filesystem
I/O continues independently of native readiness; local workspace-document
bindings use a stable null owner. Physical tracking reads carry their observed session
and refresh when its native epoch changes. No second writer, recovery queue,
native RPC shape or dependency is introduced.

The new package rejects the stale task write before any native write frame,
preserves the replacement document and task input, rejects the stale editor
save and physical read, and acknowledges the retained buffer after runtime
delivery. A new task action preserves the replacement document's sibling keys.
Exact UTF-8 bytes, mode, index bytes and temporary cleanup are checked. Focused
transport checks also replace the session during credential preparation and
prove neither text read nor write reaches the wire. Ten focused suites pass
166 cases; nine packaged local/SSH cases pass together, including the existing
large-file bootstrap, reconnect, native-offline save and Quit cleanup flows.
Broader workflow, recovery
and provider/platform requirements remain in the matrix above.

## Local save shutdown checkpoint

Packaged macOS checks held a real local save during rename and during cleanup
after a rename failure. Both prior-package runs exited after the Discard edits
prompt but before releasing the filesystem operation, leaving an owned temporary
behind. Native shutdown already joins its requests; the independent Electron
writer was missing from the main process's shutdown join.

`electron/file-write.ts` now owns each admitted save from destination preparation
through publication or checked cleanup. Once Quit passes the renderer's decision,
`will-quit` closes local save admission and joins those complete promises before
`app.exit`. Write success or failure still belongs to the original caller.
Cancelling Quit does not close admission. The prompt explains that saves already
in progress finish; later unsent edits can be discarded. The existing local
provider is the only caller, so text, Markdown and workspace-document saves use
the same owner without depending on native readiness. Native and SSH publication
retain their existing owners; physical create/rename/delete parity remains open.

Three real-filesystem cases cover preparation, publication, cleanup and rejection
of late saves. Nine focused suites pass 139 cases. Six packaged local cases pass
together, including both discard outcomes, keep-editing, failed-buffer recovery,
native-offline retry and metadata monitoring. Exact UTF-8 bytes, existing modes,
unchanged index bytes and removal of owned temporaries are checked after exit.
These are bounded macOS arm64 results; the remaining matrix is still open.

## Local monitor and repository-switch checkpoint

The prior local watcher paths duplicated `fs.watch` and synchronously closed its
handles on Electron's main thread. A grouped packaged run reproduced the
Markdown repository-switch timeout: the click selected the repository, then
failed to acknowledge within 30 seconds. DOM events, main/renderer heartbeats
and CPU profiles placed the pause after the click and before new native metadata
requests. The previous package also paused for 13.4 seconds while its renderer
was idle. An OS stack sample captured the main thread in
`uv_fs_event_stop → uv_sem_wait`; the window was visible and focused.
The bundled runtime is Electron 33.4.11 / Node 20.18.3 / libuv 1.46.0, whose
[close implementation waits for FSEvents deinitialization](https://github.com/libuv/libuv/blob/v1.46.0/src/unix/fsevents.c#L811).

Offloading the same native watcher was insufficient: real event-delivery checks
failed, and a separate Node 22.20 probe received no directory changes
through 25 seconds of writes on the Workspace volume. The canonical local path
is now a metadata monitor in a worker. It folds eligible path, inode, mode, size,
mtime and ctime records into a fixed-size fingerprint, with no file payload reads
or per-file retained cache. One inventory runs at a time, followed by a one-second
idle interval. The initial inventory invalidates any concurrently loaded view.
Later metadata loss and repair also invalidate views, including repair to an
identical fingerprint; consumers reread and display their source's actual error.

IPC and `LocalProvider` share one owner per physical directory and one artifact/
Git-path policy. Subscribers release callbacks immediately. The final release
stops and joins the worker after its current directory traversal drains. New
subscriptions cannot join a draining owner; app shutdown joins both active and
draining workers and rejects late acquisition. Local monitoring is independent
of native readiness. Immutable SDK snapshots have no physical watch target.
SSH keeps its existing admitted provider/native polling ownership. No native
watcher fallback, backend setting or dependency change is added.

Six real-filesystem cases cover shared subscriptions, stop/restart, shutdown
admission, invalid targets, a two-GiB sparse file and temporary root loss/repair.
The existing build/runtime checks pass 28 cases together; eight focused desktop
suites pass 136 cases. A red packaged check observes a main-thread native watch
acquisition in the prior package. The new packaged check sees zero, and proves
external create/delete visibility, restart, unchanged index bytes and normal
Quit joining the owner. Three instrumented Markdown repository-switch runs
pass with click acknowledgement at 60–65 ms, without forced clicks or added
UI delays. Diagnostic probes are removed from source after recording evidence.
The final unsigned macOS arm64 package passes all 23 grouped UI cases in
1.8 minutes, including local/SSH saves and document edits, external-file
monitoring, selected-source/media recovery, device settings before construction,
real two-store identity replacement and both cold-restoration scenarios. All
913 renderer/Electron assets match their producers. The bundled CLI/agent/NFS,
remote helper and paired Darwin SSH archive match the qualified native producer;
all 1,682 Rust/Cargo inputs remain unchanged after that native build.

A read-only checkout measurement records a 295 ms first inventory, about 23 MB
process RSS growth including worker startup, and a maximum 12.2 ms main-thread
heartbeat gap during the observation. These are bounded local measurements;
very large file counts, network filesystems and other provider/platform costs
remain qualification work. Linked worktrees whose Git control directory is
outside the watched root still need dedicated metadata-invalidation proof.
The original cold-shutdown timeout had no stack capture; its exact cause is
not inferred solely from the repository-switch diagnosis. Other recovery and
full Crab/LFS workflow requirements remain in the matrix above.

## Publication visibility checkpoint

Concurrent native saves exposed a file-tree selection race in the packaged SSH
flow: Git reported active same-directory publication temporaries as untracked
files, then removed their rows as saves completed. Pointer-event and tree-row
traces showed the requested row moving before pointerdown; the editor correctly
opened the row actually clicked. Two of three instrumented repetitions failed.
A separate deterministic local test held rename and proved the real status
response included the acquired temporary while it existed.

Native and Electron publication owners now retain exact temporary identities
across overlapping status scans. Acquisition and registration are coordinated;
a scan retains names observed before or during its Git query, including names
already removed before the query returns. Only untracked entries with those
exact identities are omitted. There is no filename-prefix blacklist: real user
files named `.crab-write-*` remain visible. Native registries are scoped to the
physical worktree; Node snapshots wait only for overlapping exclusive opens
inside their physical root. A blocked repository cannot stall another one's
status scan. Publication, checked cleanup, byte acknowledgement and native
shutdown ownership retain the existing canonical paths.

Eleven native publication checks and thirteen local filesystem checks pass.
The packaged SSH failure scenario passes three consecutive runs, preserving
exact bytes, modes and unrelated index entries while clicking the intended
files. The packaged local test holds an actual save before rename, observes its
temporary on disk, and confirms the real status response omits only that owned
name while retaining the similarly named user file. Publication failure remains
visible, and native-offline retry still persists the latest buffer. No forced
click, filename blacklist or extra tree delay is used to obtain this proof.
Selected-repository file-tree/Changes status uses these RPC/provider boundaries.
Local workspace polling (`electron/workspace-poller.ts`) and native background
batches (`agent/src/batch_git_status.rs`) still derive dirty counts through
separate Git subprocesses. Their publication-aware count consolidation remains
explicit follow-up; no background-count parity is claimed by this checkpoint.
The broader workflow, recovery and provider/platform requirements above remain
separate gates.

## Shared workspace-document checkpoint

Theme, settings and tasks now share one acknowledged document and one serialized
read-modify-write owner in the application's renderer. Every mutation reads the
current complete document after the preceding acknowledgement, so independent
views and repeated task additions preserve sibling keys. Failed reads are never
an empty-document default. Invalid JSON, invalid supported values/tasks,
non-text sources and documents over 64 KiB block mutation visibly. Missing files
are the explicit create case. Failed saves retain the acknowledged state; task
input remains available for retry. Pending document saves participate in the
existing quit decision before native teardown.

Physical reads and writes use the selected repository provider. Local bounded
Node reads/saves remain independent of native readiness. Remote reads use the
required repository ID, relative path, byte limit and admitted native session.
Only the typed missing-path response is absence; unavailable repositories,
transport errors and truncated replies propagate. Mutable reads have no stale
reconnect cache. A binding replacement observed by the document owner before publication
rejects its old read. Separate repositories have independent queues. Tracking configuration now
uses this provider-correct physical read path too; SDK snapshot read failures
stay visible, with missing optional SDK paths still requiring precise native
classification in the broader workflow qualification.

Settings scope reaches actual update handlers and effective consumers. User
preferences persist separately; supported workspace overrides reach application
theme, Monaco, previews, comparisons and other settings consumers together.
Use User value removes one override. Device paths, credentials, agent execution,
telemetry and global services remain User-controlled. Read-only SDK snapshots
cannot enable workspace or task mutations. The separate workspace-theme hook
and competing theme effect are removed, along with all path-only document
writers and their write IPC overload. The task file viewer still has a path-only
local read surface; canonical task-worktree/SSH ownership remains explicit
follow-up work rather than a claim of complete editor parity.

Fourteen document/filesystem cases cover concurrent theme/settings/task updates,
missing versus unreadable/malformed/oversized data, failed publication, native
owner replacement, per-repository queue independence and read-only/device
boundaries. Seven focused suites pass 120 cases. Thirteen native RPC/publication
cases pass, including typed path absence distinct from an unavailable repository
and worktree-scoped temporary ownership.
Project/spec types, Rust format, native/renderer/Electron builds and unsigned
macOS arm64 packaging pass. The native CLI/agent pair and Darwin SSH archive are
rebuilt for the changed error contract; all 909 renderer/Electron assets match.

Packaged local and real SSH UI actions qualify setting/task serialization and
consumer behavior. Local proof holds a settings publication, queues another
field, cancels Quit with native ping still working, then preserves both fields
and unrelated keys. User/theme override changes reach the actual app and editor
font; Use User value restores the underlying preference. A real read failure
retains the task input and original document; retry, toggle and delete persist.
Editor and settings saves work while repository metadata is offline and native
reopening is blocked. SSH creates a missing document, preserves externally
updated keys and tasks, and edits again after reconnect with the correct
repository/path on the native wire. Modes, index bytes and temporary cleanup
remain qualified by these scenarios. All 22 packaged cases pass together in
one grouped run, including native/media recovery, selected-source repair, device
settings before native construction, local/SSH identity replacement, encrypted
cold restoration and same-URL SDK bookmark restoration. The SDK GUI confirms
workspace/task controls are disabled while the User theme still applies; the
read-only proxies record zero writes. Suite-load repeatability remains a release gate. A later grouped run completed
all cold-restart byte/pointer assertions but timed out while closing one app
(21 passed, one failed). Three isolated lifecycle-instrumented repetitions
passed with every observed shutdown owner completing; the grouped timeout
cause remains unproven. It is not claimed fixed by publication filtering.

Clippy is not passing: its production target retains the three existing forge/
directory-scanner errors. All-target checking also reports existing test
`unwrap`/`expect`/`panic` lints from the preceding checkpoint. The current
production check confirms all three failing source locations are unchanged;
the new publication owner and RPC changes add no production diagnostics. Current
`origin/main` lacks these desktop paths, so no main-baseline pass is claimed.
ESLint still lacks configuration. These checks are not reported as green.

Cross-platform/provider profiles and the complete large-file acceptance matrix
remain open. This owner serializes this application's writers; it does not add
cross-process document locks, crash drafts or durable transfer journals. No
dependency, lockfile, SDK API snapshot or inventory changes are introduced.

## Local file-publication and workspace-state checkpoint

`LocalProvider.fsWrite` and the existing path-only local configuration writer
now share `electron/file-write.ts`. A unique, exclusively opened sibling owns
the prepared text; the target changes only after write, mode restoration,
synchronization and close succeed. Failed publication removes only that owned
temporary, and cleanup failure retains both errors. UTF-8 bytes are acknowledged
after rename. Target access is checked without truncation; existing internal
symlinks remain links and dangling links fail. Non-absolute targets, including
SSH/storage URLs, fail before filesystem I/O. A repository-scoped call without
a provider fails instead of falling through to a local write.

Node owns local physical saves so they remain available while a repository's
native reader cannot reopen. Native text/binary saves retain their own scoped
writer for the SSH filesystem boundary. Captured materialization continues to
own pointer validation, content verification and cancellation; a plain editor
write must not bypass those checks. This adds no reconstruction or retry path.

Three red local cases reproduced partial target destruction, premature source
mutation and mutation of an already-open source. Eleven filesystem cases now
pass, covering partial-write/rename/cleanup failures, exclusive-open ownership,
UTF-8/empty acknowledgement, read-only targets, symlinks and modes. The four
focused local/buffer/provider/native-lifecycle suites pass 73 cases; project
and strict specification types pass.

Eleven cases pass together on the unsigned macOS arm64 package. An actual
workspace-theme action preserves another settings key and mode `0640`. An
editor save encounters a real main-process rename failure, visibly retains the
pending edit and leaves original bytes/mode intact with no owned temporary.
After the repository metadata is taken offline and only the owned sidecar is
killed, Retry saving persists exact multibyte text, clears dirty state and
preserves mode `0751` and the index while Retry reopening repository remains
visible. Text/Markdown tab disposal, repository switching, cancelled quit,
denied/held writes and selected-source native repair also pass in that run.
Existing failure adapters now follow the acquired temporary handle instead of
assuming writes target the destination path directly; their behavioral
assertions remain intact. The earlier suite-load timeout cause is still unknown.

The Electron build and packaging pass. All 907 packaged renderer/Electron assets
match; all 1,682 Rust/Cargo inputs match the native-publication producer, so its
qualified CLI/agent/NFS/helper and paired Darwin SSH archive are retained. This
slice changes no dependencies, lockfiles, inventories or API snapshots. POSIX
modes are qualified; ACL/xattr/ownership and hardlink behavior are not. File
synchronization does not claim power-loss durability of the directory rename.
Cross-platform/provider proof and the complete acceptance matrix remain open.

That checkpoint's settings audit found three independent writers, swallowed
read errors, path-only remote writes and a cosmetic scope selector. The shared
workspace-document checkpoint above supersedes those implementations; its
provider/platform limits remain explicit. Remote CRUD, the standalone editor
and task-file ownership are separate unfinished workflow surfaces.

## Native file-publication checkpoint

Native `write_text` and `write_bytes` now use one plain-file writer. Every request
owns a unique same-directory temporary until atomic publication or checked
cleanup. Existing mode bits are restored before publication; new files retain
normal create/umask semantics. The writer checks target write access rather than
using rename to bypass a read-only file. Blocking I/O stays off Tokio workers,
and native shutdown joins admitted requests before releasing their owners.

Write paths must be relative and resolve inside the opened working tree. An
internal symlink writes its target without replacing the link. Escaping and
dangling links fail visibly, and an escaping missing parent is rejected before
creating directories. Publication synchronizes the prepared file; this does not
add a restart journal or claim power-loss durability of the directory rename.

Three native red cases reproduced executable-mode loss, deletion of an unrelated
same-stem `.crab-tmp` user file and incomplete competing text/binary publications.
The old packaged SSH editor also lost executable mode on an actual save. Eight
native checks now pass, including nested/empty/raw-binary files, UTF-8 byte
acknowledgements, read-only targets, symlink boundaries and no temporary residue.
Fifty-five adjacent buffer/provider/router cases and project/spec types pass.

The freshly built native pair and Darwin SSH archive pass two packaged macOS
arm64 cases. Native file/media recovery remains working. SSH editing preserves
mode `0751`, the unrelated same-stem file and the index through a real permission
failure, tab disposal and reconnect. Twelve concurrent text/binary RPC writes
publish exact bytes; refreshing Files and opening both outputs shows their real
content. No owned temporaries remain after acknowledgement. All 905 packaged
renderer/Electron assets, native hashes, executable modes and the paired archive
match the build. This checkpoint uses a new native build, not the earlier pair.

At this checkpoint, `LocalProvider.fsWrite` still used Node's in-place write;
the local publication checkpoint above now supersedes that implementation.
Complete remote create/rename/delete remains open. Captured materialization
keeps its separate pointer checks, integrity verification and cancellation owner;
it already uses a unique `NamedTempFile` and restores the captured target mode.

Cargo format, build and packaging pass. Clippy reports three existing `unwrap`
errors in forge comment assembly and directory-size scanning; the forge file is
byte-identical to the requested `origin/crab-v2`, and the directory scanner has
the same failing expression there. The new writer has no Clippy diagnostics.
Current `origin/main` does not contain these desktop paths; no main-baseline pass
is claimed. Cross-platform/provider CI and suite-load recovery repeatability
remain release gates, and the complete acceptance matrix is still open.

## Acknowledged working-buffer checkpoint

Text and Markdown now share one physical working-buffer owner. The editor view
may unmount; pending text, dirty state, save error and the admitted writer remain
owned by the repository/file identity. Returning to a tab or reopening it does
not replace pending text with a disk read. An older save acknowledges only its
own content; newer edits drain through the same serialized writer. Clean blur
is a no-op. Read tickets reject obsolete replies after editing, new reads,
native replacement or an earlier buffer lifetime.

The existing repository-runtime event stream updates retained buffers even when
no editor is mounted. SSH disconnect retains pending text and shows a waiting
status; a newly admitted native session retries it through the repository
provider. Repeated announcements of the same owner do not create retries.
Failures retain the buffer and expose Retry saving. Markdown Preview renders
that same retained text. Read-only revisions and orchestrator snapshots use
isolated Monaco models instead of overwriting a physical working-file model.

Repository-scoped remote writes now call the existing native `write_text` with
its required repository ID, relative file path and native session. The provider
rejects paths outside the selected repository and incomplete acknowledgements.
Both local and SSH acknowledgements report UTF-8 bytes. This qualifies isolated
Files editor saves. At that checkpoint, native temporary-file collision and
mode preservation were still open; the native publication checkpoint above now
qualifies their bounded macOS cases; the local checkpoint above adds atomic
Node saves. Remote create/rename/delete and the standalone file editor still
need a canonical working path and actual UI proof.

Closing/reloading the application asks before losing unacknowledged edits. Quit
first resolves that decision, then tears down terminal/SSH/native owners. Keep
editing leaves services running so Retry saving remains usable. Buffers are
memory-owned; crash recovery and durable transfer journals remain separate open
work. No disk draft store or additional retry queue is introduced.

Focused proof passes 118 cases across six buffer/provider/native/router/diff
suites. Three packaged macOS arm64 cases use actual Git repositories and verify
unchanged unrelated index bytes. Text and Markdown encounter real provider
write failures, survive tab switching/close/reopen and repository switching,
cancel Quit with native ping still working, then persist exact bytes through
Retry saving. A real SSH/native fixture saves multibyte text, hits a filesystem
permission failure, retains pending Markdown across tab closure and connection
loss, then automatically writes the latest text after reconnect. Native wire
requests carry the correct repository and relative path. Eleven source/diff/media
regressions also pass on this package. The fourteen distinct scenarios pass
across the final targeted runs; the broader run had two repository-switch
timeouts before both traced repetitions passed without renderer errors or
unexpected native prompts. Their cause remains unproven, so suite-load
repeatability in CI stays a release gate. Project/spec types, Rust format,
renderer/Electron builds and unsigned packaging pass; all 905 assets and the
unchanged qualified native pair match. These checks qualify the bounded buffer
behaviors; the complete acceptance matrix remains open.

## Storage identity checkpoint

### Selected sources and active editor recovery

Selected text/LFS/notebook comparisons and the history comparison worker retain
selection and comparison mode while replacing their reads on native epoch
changes. Each captured comparison now includes the expected native session in
its request and cancellation. The local agent and SSH router reject an old
session after repository admission, before it can enter a replacement reader.
The worktree panel updates its auxiliary handle through the existing runtime
event; cross-repository comparison supplies its current head from app state.
Those two auxiliary UI flows still need packaged recovery qualification.

Changes uses one effect per selected patch or full-file read. Choosing an
untracked file uses that same full-file path; staging refreshes status and the
patch pair rather than starting a second imperative selected-file fetch. Effect
cleanup aborts its previous read and suppresses late content/errors. Full-file
errors are visible. Native loss retains selection and shows recovery status.
Git patch subprocesses retain their existing terminal execution after view
cancellation; stopping those subprocesses is separate work from suppressing
obsolete results. Captured comparisons retain their existing native cancellation.

Clean text/Markdown buffers re-read after native recovery. Pending local content
and in-flight/failed remote saves prevent automatic replacement of an active
buffer. Local text and Markdown saves use the existing repository provider,
serialize writes and acknowledge only the content actually persisted. Failures
retain pending content and offer Retry saving. A clean editor's blur is a no-op,
so clicking repository Retry cannot submit an old unedited buffer. Generic
workspace configuration still used unscoped IPC at this checkpoint; the shared
workspace-document owner above now replaces that path.
At this earlier checkpoint, the remote save hook only rejected older
acknowledgements and recorded active writes. The shared buffer checkpoint above
replaces it and qualifies tab ownership and the repository-scoped SSH writer.

The packaged fixture keeps the logical repository ID, removes its real Git
directory, kills its owned native process, changes content, restores Git and
uses visible Retry. Text, Markdown, Changes content and text/LFS/notebook
comparisons must refresh without reselection. Focused clean editors must not
write old content on blur. Two held provider-write cases keep the latest edit
through recovery while an earlier write is outstanding; the Markdown case then
fails that write, shows the save error and persists the retained buffer through
Retry saving. A final staging action proves the repaired patch/status path.
Reads preserve the working file and unrelated index; intentional edits/staging
are checked separately. The existing large-file and notebook comparison smokes
also exercise the shared history/capture callers.

The final checkpoint passes 97 focused cases across native admission, SSH
routing, remote save acknowledgements, notebook comparison and binary decoding.
Project and strict focused spec types, Cargo format, script syntax, renderer
build and unsigned macOS arm64 packaging pass. Eleven current packaged cases
pass on the final 905 matching assets: the eight source/buffer cases above,
actual Crab chunk/LFS comparison, notebook comparison and native file/media
recovery. All 1,680 Rust/Cargo inputs and native hashes match the qualified native
producer. No dependency, lockfile, snapshot or inventory updates are needed.

At this earlier checkpoint, remote editable buffers and switching/closing tabs
were still open; the shared buffer checkpoint above now qualifies those bounded
cases. Notebook document/kernel/query state, media playback position,
storage-rebind/SSH selected-source UI qualification, plain Git patch cancellation
and the complete acceptance matrix remain open.

### Dashboard and dataset metadata recovery

Dashboard data belongs to a native epoch. Replacement clears its old data and
error state, cancels publication from previous effects and admits cached code
frequency only after the replacement's own summary arrives. The metric hook
clears disabled data and revalidates its existing logical cache when re-enabled;
no second session/cache owner is introduced. UI controls retain their state.
The existing availability-toast owner now also retains crash notices, so native
negotiation clears them while leaving unrelated operation errors visible.

Datasets retains a selected path rather than an obsolete entry object. Its
listing generation rejects earlier refresh results, and selection derives from
the fresh listing and classification rules. Rules, Safetensors and Parquet
metadata refresh after native replacement; existing abort ownership prevents
obsolete previews from publishing. Read-only dataset inspection is available
under Large Files without configuring Crab storage. Tracking and storage views
retain their setup requirement.

The shared hydration store consumes repository-native ownership events. Loss
clears the old inventory, admission refreshes it, and the existing version guard
drains earlier reads. Repeated announcements of the same owner share one read;
an unrelated repository's event cannot clear a healthy sibling. Components do
not acquire another hydration cache or retry queue.

Four packaged macOS arm64 cases remove a real repository's Git directory, kill
the owned native child, restore the directory with a new commit/content and use
the visible Retry action. Dashboard shows the new commit and updated persisted
code-frequency values while retaining Month selection. Safetensors datasets
with and without local Crab project configuration show a new listing and tensor
count while retaining their filter and selected path. Parquet changes from one
column/three rows to two columns/four rows, and a changed workspace rule moves it
from Training Data to Evaluation Data. Content and the unrelated index remain
byte-identical after reads. The tiny generated Parquet inputs require no Python
dependency in the packaged smoke.

All six current packaged cases pass, including native Crab/LFS recovery and SSH
independent replay regressions. The seven focused owner/routing suites pass 104
cases, and the standalone classifier, project/spec types, Cargo format and build
checks pass. All 905 packaged renderer/Electron assets match the fresh build;
the 1,680 Rust/Cargo inputs still match the qualified native producer. This
closes those local repair and read-only catalog cases. Other provider/profile UI
recovery, remaining P0 state and the full acceptance matrix stay open.

### Independent queued metadata recovery

Native process readiness and each repository's admission are separate owners.
Deployment registers repository openings before announcing readiness and joins
opening/replay completion on teardown. The router replays cacheable metadata in
repository order, with independent progress across repositories and at most four
active calls. Waiting for a repository does not occupy a dispatch slot. A failed
call does not reject a healthy sibling waiting for a slot.

The queued entry remains cancellable through credential preparation. Cancelling
unsent work settles it locally and prevents late authentication from dispatching
it. Each request captures its channel before async authentication; replacement
or loss settles old work rather than moving it onto a new native owner. Captured
content and mutations retain their existing live-session behavior.

Changes status and the staging patch pair refresh when the native epoch changes.
Generation checks reject obsolete replies while keeping the component's pending
draft and selection state. Status list updates do not invalidate an explicit
staging patch refresh; only a newer patch request, owner change or unmount does.
Selected diff/editor content and sibling badge refreshes still need qualification.

`ssh-metadata-replay-ui.spec.ts` qualifies a packaged macOS arm64 reconnect with
two real Git repositories. One repository's config is held on a FIFO while the
healthy sibling's previously queued metadata completes, Changes shows a new file
and retains its commit draft, and a 4 MiB Safetensors preview opens. An unsent
queued read cancels with code 1010 and never reaches the wire. Restoring the
blocked config releases its queued metadata exactly once and its file tree opens.
Content and the unrelated index remain byte-identical. This closes independent
queued metadata replay and that bounded Changes recovery case; it does not close
the remaining P0 views, lifecycle races or full acceptance matrix.

Current proof: 489 TypeScript cases across 23 routing/lifecycle suites, plus 50
adjacent cache/provider/capability cases; project and strict spec types pass.
Sixteen packaged cases pass, including the real two-store SSH/local/SDK identity
and cold-restart regressions. All 905 renderer/Electron assets match the fresh
build. This TypeScript slice reuses the qualified native pair: all 1,680 Rust/Cargo
inputs remain byte-identical to its producer. Cross-platform/provider CI remains
required; ESLint has no configuration and is not reported as passing.

### Device settings during native failure

Settings is reachable through More actions and Cmd/Ctrl+, before opening any
repository. The app shell owns that navigation; the workspace shell retains
only its sidebar shortcut. Connection metadata uses the existing encrypted
vault before native negotiation. Missing agent/CLI files and execution failure
cannot prevent save, edit, forget or cold restoration. Connection testing still
starts and negotiates the paired native runtime and reports its actual failure.

`nativeBinaries` locates the pair; `Agent.start` validates its presence before
spawning. Packaging continues to verify version, target, hashes and SSH archives.
Live storage identity edits retain the existing per-repository replacement
owner. Offline edits persist the desired binding for replay rather than opening
another reader. Successful local negotiation clears the service-outage toast;
repository failures and unrelated operation errors retain their own reporting.

Previously, binary lookup could fail before the vault/LocalProvider existed,
and `Agent.request` waited for native readiness before connection metadata.
Focused regressions cover absent services, blocked handshake, denied execution,
offline selection/replay and recovery notices. There are 73 passing cases in
the four touched owner/vault/locator/publisher suites; project and focused strict
spec types pass. Packaged macOS arm64 tests independently remove the agent,
remove the CLI and deny execution. Each saves/edits/removes encrypted profiles,
restarts while the fault remains, shows a connection-test error, repairs the
runtime and opens a real Safetensors preview. The shortcut also works with an
active repository, and the obsolete outage notice disappears after repair.

This closes those native-binary failure cases. An unlocked OS keyring remains
required for credential persistence; cross-platform qualification and the
remaining recovery, workflow, transfer and inspection scopes stay open.

Device metadata now dispatches directly through that same encrypted vault in
`main.ts`, before native construction. The vault's existing identity-change
callback still joins local/SDK and SSH owners; native connection tests retain
one credential decoration and real negotiation. A shared construction promise
coalesces callers, clears on prerequisite failure and retries after repair.
Once constructed, `Agent` continues owning process readiness and restarts.

The real blocked-shell fixture exposed a separate startup deadlock: the
synchronous login-shell PATH probe could hold Electron's main loop even after
its nominal timeout. Detection, terminal/native startup and CLI adapters now
share one asynchronous login-environment read. The duplicate manifest PATH
builder and second shell fallback are removed. Each CLI launch can revoke its
own environment wait without cancelling that shared discovery; a late reply
cannot start the revoked process. The orchestrator registers the launch and its
timeout before awaiting preparation.

Two packaged macOS arm64 cases hold real zsh startup or fail the production log
mkdir. With no native agent process, both save/edit/remove encrypted profiles
and cold-restore metadata under the same fault. Repair admits exactly one native
child, an actual connection test and a visible Safetensors preview. The blocked
shell case also runs real CLI marker processes: an admitted launch starts only
after release, while a cancelled launch never starts. The earlier package timed
out with the main loop held by its synchronous PATH probe. Focused environment,
protocol, vault and native lifecycle cases cover the affected sibling owners.
The current checkpoint passes 135 focused cases across 14 environment, protocol,
vault and native-lifecycle suites; project/strict spec types, Cargo format and
renderer/Electron builds pass. Eleven packaged cases pass: these two construction
faults plus three binary faults, native read/media recovery, independent SSH
metadata replay and four real two-store identity/restart scenarios. All 905
packaged assets match build output; all 1,680 Rust/Cargo inputs and bundled native
hashes match the qualified native producer. This is unsigned local macOS arm64
proof; current-head platform/provider CI remains required.
An unlocked OS keyring remains required. Other P0 UI state recovery, admitted
removal, jump hosts and the full acceptance matrix remain open.

### Reader identity replacement

Settings can edit a saved connection without closing repositories manually.
Secret fields start empty and require complete replacement credentials. The
vault persists the selected identity before reconnection; `Agent` then retires
the old native session and reopens every affected local/SDK context through its
existing per-repository admission owner. Removal awaits native close and retains
the logical repository for repair. Failed selections remain visible and never
silently reuse the old or ambient identity. Settings requests bypass a failed
repository's read guard, so another connection can be selected.

Native pointer capture snapshots task-local storage authentication in
`agent/src/repository/native.rs`; it does not inherit a credential selected only
at local repository open. `StorageConnections` now supplies the saved binding to
file, preview and complete-read captures, including a cold encrypted vault.
Expected native identity is checked again at the local/SSH wire boundary after
any queued repository replacement. Old file close becomes a no-op; old reads
fail with `NATIVE_SESSION_LOST`. Direct SDK snapshots advertise no physical
workspace mutations. Existing editable local workspaces retain Node filesystem
ownership.

The prior vault implementation reopened SDK repositories directly, left local
native readers alive on bind/removal and rejected profile edits while remote
repositories were open. Moving lifecycle work to `Agent` deletes that second
reopen path. Credential encryption/selection stays in the vault; native SDK and
read actors retain their existing close/cancellation contracts. Active CLI
publishers still finish under their captured credentials; changing transfer
identity or durable cancellation policy is a separate deliverable.

Packaged macOS arm64 proof uses two isolated RustFS 1.0.3 stores with different
content at the same logical URL, read-only proxies and invalid ambient AWS
credentials. GUI actions test/save/select/edit/forget connections, preview and
hydrate LFS, reject old file/media readers after failed selection, display exact
replacement-store bytes and recover by selecting another profile. A separate
app restart reads a never-browsed LFS object through the encrypted local binding,
checks its full SHA-256 and leaves its working pointer intact. Both transports
record reads and zero writes. The SDK snapshot checkpoint below extends this
proof to automatic direct-URL bookmark restoration.

Focused owner/router/vault tests pass (187 cases in eight files); renderer and
Electron typechecks, focused strict spec types and unsigned packaging pass.
`tests/e2e/storage-identity.spec.ts` contains both real-store proofs;
`scripts/qualify-v2.py` supplies their owned services to the existing smoke suite.
Native restart and pending-open cancellation/close packaged regressions pass.
Cross-platform/provider qualification remains pending. The desktop ESLint task
still lacks an ESLint configuration; no lint-pass claim follows from typechecks.

## Selected storage identity over SSH

The application constructs one encrypted vault for local/SDK and SSH requests.
Its identity-change callback joins both transport owners; one failure cannot
skip the other. `RpcRouter` supplies the selected connection to each remote
capture through the native `__storage_auth` task-local contract. Remote bindings
use the full literal `ssh://profile/path` identity. Requests cannot select another
profile's repository or inherit a local path binding when they lack a repository
ID. Successful content results remain byte-identical; diagnostic redaction is
confined to errors and notifications. Settings tests the connection on the active
SSH transport rather than silently testing it on the local machine.

`SshSessionManager` retains desired repositories before native setup and gives
each successful opening its own reader epoch. Replacing one identity keeps the
SSH process and healthy siblings alive. Removal joins native close while keeping
the logical binding repairable. Serialized identity changes prevent an old close
from revoking a replacement. Explicit Retry joins failed-open cleanup before
starting a fresh attempt. Cacheable metadata joins the target repository's open
or rebind promise; process readiness alone does not imply repository readiness.
Independent reconnect queue release is qualified in the checkpoint above.

The packaged macOS arm64 proof uses real SSH trust/deployment and two isolated
S3-compatible stores with invalid ambient credentials. GUI save/test/select,
profile editing/removal/reselection, distinct previews and exact 4 MiB LFS
hydration pass. The index pointer remains unchanged. An invalid bucket produces
a visible connection-test failure and a failed content read; physical Git opening
itself does not probe cloud access. Old file/media handles are rejected and the
healthy sibling retains its epoch. After restart, active and deferred repositories
read previously untouched LFS objects with exact SHA-256 results, additional reads
from both proxies and zero writes. The SSH fixture records connection IDs at the
native wire without recording credentials. Other providers, actual jump hosts,
complete remote file operations and the remaining acceptance matrix stay open.

There are 483 passing focused cases across 23 touched suites. Project and strict
integration types, production renderer/Electron builds and unsigned macOS arm64
packaging pass. Fourteen packaged regressions pass on the final assets, including
startup failure/repair, pending-open cleanup, native restart, SSH bootstrap and
saved recovery, local/SDK identity changes and the new SSH cloud case. All 905
renderer/Electron assets match output. Rust/Cargo inputs are unchanged from the
previously qualified native pair; no fresh native-build claim follows from the
TypeScript package build. Cross-platform/provider CI remains required.

## SDK snapshot identity and GUI opening

The existing repository actions menu includes **Open storage snapshot**. It
reuses Storage connections for credential selection and opens a read-only SDK
source without cloning a worktree. Snapshots start in Files; physical workspace
statistics are not the initial destination. Cancellation retains the dialog
until the native terminal result. A successful open racing cancellation is
closed before dismissal, so no unlisted native owner remains.

`repo_id` is the snapshot's persisted logical identity; `native_session_id`
belongs only to one live native opening and is never persisted. Recent entries,
active selection, pinning, ordering and view/scroll state use the logical ID for
SDK URLs. Local/SSH entries continue to use their physical/profile paths.
Startup, deferred sidebar selection and Retry share URL classification in
`rpc.reopen`. The vault retains bindings across reader close and resolves a
cold `open_url` by logical ID. A URL alone cannot identify credentials: two
different endpoints may expose the same bucket and repository name. Replacing
a handle resolves its destination binding rather than the previous worktree's
credentials; focused tests cover local path replacement and local-to-SDK opening.

The packaged GUI test opens both stores at that identical URL, then restarts
without any explicit credential selection. The active snapshot restores
automatically; selecting the deferred sibling restores its own saved binding.
Each displays its distinct tensor marker and returns an exact SHA-256 match for
an object never browsed before restart. The restart adds 80 GETs and two HEADs
across the proxies, with zero writes. Before those opens, Cancel drains a real
SDK metadata request while the storage gate stays closed; the dialog disappears
without adding a recent repository. This qualifies the S3-compatible fixture,
not every provider or actual SSH jump host.

The focused source/vault/provider/session checks pass (185 cases in 12 files),
as do project/strict spec types and three real-store packaged cases. The first
GUI replay exposed a test clicking the previous repository before deferred
selection completed; it now awaits the selected logical ID. The same run
exposed SDK deferred opening on Overview, which requested local-only statistics;
SDK sources now default to Files. Editable buffer/tab reconstruction and cached
dashboard/dataset state still require their own recovery proof.

The rebase onto `crab-v2` at `e60de177853` retains the terminal lifecycle APIs
and Git-root initialization contract. The root-initialization regression now
uses the RPC dispatcher, while the lifecycle module keeps its private request
types. Rebased checks pass: 244 focused TypeScript cases in 18 files and 26
native repository/root/layout/cleanup cases. Six fixture-dependent native cases
remain ignored and are not counted as passing.

Operation-routing checkpoint: 187 focused tests across ten provider/router/connection
files pass, following four reproduced operation-boundary failures. The packaged
SSH smoke covers exact 4 MiB hydration, Activity progress, cancellation of a
512 MiB payload without changing either pointer or leaving a temporary file,
and native exit while SSH remains connected. The obsolete optional browser
prefetch tests are removed with their retired path; real packaged opening and
repository registration remain covered. The native Linux arm64 producer's CI
failure identified the configured cross-linker missing from the native runner;
the desktop build now selects `cc` unless Cargo's caller explicitly chooses a
linker. The Linux arm64 producer passed on the earlier published head; fresh
cross-platform qualification for subsequent recovery changes remains required.

## Repository-open ownership checkpoint

The native `handlers/lifecycle.rs` module owns local/URL replacement and close
under one per-repository admission lock. Opening subscribes to cancellation
before removing an existing owner. Prepared sessions remain owned until commit
or awaited close; cleanup failures remain terminal failures, including when the
original operation was cancelled. Local discovery and forge Git reads reuse the
existing joined process owner. URL opening passes cancellation into the SDK and
always awaits client close on failed setup. The blocked SDK GUI test qualifies
this path with S3-compatible storage; other providers remain required.

SSH setup exposes negotiated process readiness independently of repository
replay. Its existing deployment owner still joins replay and queued metadata on
disconnect. Manual opens join their repository's replay outcome. Late cleanup
checks channel/session ownership before invalidating a replacement process.
`open_local` and `open_url` use the router's existing owned-operation deadline:
signal cancellation, retain the original reply, then report its terminal result.
The old 30-second metadata timeout cannot orphan an opening session.

Read-only format previews share a native-session boundary in `EditorPane`.
Native loss unmounts old requests; a recovered repository mounts fresh previews.
The live SSH fixture reproduced a retained safetensors error before this fix.
Editable views, notebook kernel/output state and media retain their existing
owners and require their separately listed recovery proof.

Evidence map:

| Surface | Caller / owner / callee | Proof and limit |
| --- | --- | --- |
| Native opening | Local `Agent.request` or SSH open → lifecycle admission → `BrowseSession` → joined Git child / SDK request options | Pre-cancelled real Git open retains the original session; cancelled setup with failed cleanup remains a failure. Direct-URL storage cancellation is wired, not yet live-qualified. |
| SSH readiness | `ensureAgent` → deployment readiness plus retained replay completion → per-repository open | A blocked sibling cannot hold healthy readiness; old replay cleanup cannot revoke the next session. Disconnect still sends EOF before joining. |
| Preview recovery | Repository runtime event → `EditorPane` session boundary → existing captured preview RPC | Packaged safetensors preview recovers while a sibling is blocked. Dashboard queries, editor buffers, notebook execution and media playback remain explicit follow-up surfaces. |
| Previous behavior | `origin/crab-v2` opening handlers and router | Opens replaced state without cancellation ownership; the router discarded replies after its metadata timeout. This branch also previously awaited every replay before exposing readiness. |

The implementation consolidates lifecycle ownership rather than adding another
reconstruction path, retry queue, credential store or navigation destination.

Qualification on the rebuilt unsigned macOS arm64 package:

- 192 focused TypeScript cases across nine transport/provider/connection files;
  20 native repository tests plus the cleanup-error regression. Six additional
  dependency/large-payload fixture cases remain ignored in the filtered run and are not counted
  as passing. Project types, focused smoke/spec types, renderer/Electron builds,
  Cargo formatting and diff checks pass.
- Six packaged cases pass on the same final package: slow/failed SSH sibling
  recovery and saved restore (40.2 s); SSH startup with local native services
  deliberately unavailable (22.9 s); native restart, blocked-open cancellation,
  shutdown while opening and SSH materialization/progress/cancellation (28.7 s).
  The blocked Git child is reaped before the FIFO fixture is released. The
  healthy sibling displays its logical preview while that child is still alive;
  the failed sibling retries on the same native process after access returns.
- Exact Crab/LFS bytes, unchanged index pointers, revoked old file/media readers
  and 512 MiB transfer cancellation/temp cleanup repeat successfully. All 903
  packaged renderer/Electron assets match the final build; both native binaries
  and the POSIX SSH archive are rebuilt and hash-checked. Archive round-trip
  handshake reports 194 methods and successful EOF shutdown.
- Evidence: external `repository-open-qualification.json`,
  `repository-open-ssh-ui-proof-final/`, `repository-open-no-local-ui-proof/`
  and `repository-open-regression-ui-proof/`. The peer application is restored
  after every smoke. Cross-platform/provider CI remains a release gate; the
  desktop lint script still has no ESLint configuration.

This slice adds 108 net production lines. They provide one native setup/release
owner, preserve cleanup errors, split process readiness from retained replay
completion, and add one shared read-only preview session boundary. The old
opening handlers are removed; no SDK public API or dependency snapshot changes
are needed.

## Ownership

- Reuse `crab-read` for verified Crab reconstruction and local staging selection.
- Reuse `crab-lfs` for SHA-256 identity, verified local/remote reads and validators.
- Remote desktop browsing already owns SDK snapshots; use SDK range options
  instead of collecting a complete stream and slicing afterward.
- Keep file-source capture and range limits below format parsers, so Explorer,
  Datasets, history and the inspector cannot disagree about revision semantics.
- Keep transfer lifecycle outside individual mounted views. Terminal UI status
  follows acknowledged native completion, cancellation or failure.

## Evidence log

- Baseline: `origin/crab-v2`, navigation consolidation already merged.
- Source audit: desktop repository/read/preview/diff handlers; SDK remote content
  streams; local workflow API; `crab-read::PointerReader`; CLI diff and selective
  download; LFS verified-range contracts.
- SDK metadata/range foundation: `cargo test -p crab-sdk --features files
  --test remote_blob raw_blob_reads_survive_source_removal_and_keep_snapshots_pinned
  --locked` passed. The fixture removes its source repository and verifies
  captured revisions, Git/Crab/LFS classification, metadata under a budget that
  rejects complete blob decoding, and existing verified content range behavior.
- The same integration fixture passed with `--no-default-features --features
  remote`, and the `files` run passed again after adding metadata response-budget
  accounting. Metadata obeys cancellation, deadlines, response and decode limits.
- `cargo check -p crab-desktop-agent -p crab --locked` passed for the initial
  source/preview wiring; repeat after subsequent edits. Existing warnings remain.
- Captured local ranges: `cargo test -p crab-desktop-agent --locked
  repository::content -- --nocapture` passed (3 tests): sparse 3 GiB ranges,
  EOF/overflow, changed/replaced worktree files, and distinct captured index,
  committed and working content.
- Remaining proof: format-parser tests, the new real native Crab/LFS pointer
  integration fixture, remote/provider/SSH coverage, failures/cancellation,
  packaged desktop actions and the other acceptance rows. No complete feature
  claim follows from compilation or these focused tests.
- Initial parser qualification passed (4 tests): Parquet schema/sample rows,
  NumPy archive headers under a 256 KiB input budget, sparse 3 GiB safetensors
  headers and historical-versus-working safetensors. Additional safetensors
  validation and credential capture changes require a repeat.
- Parser qualification repeated successfully (5 tests), now including invalid
  safetensors shapes, offsets and overflow. Valid headers must cover the logical
  payload exactly; tensor data remains unmaterialized by the parser.
- Native bridge qualification: `cargo test -p crab --locked --test
  e2e_read_pointer -- --nocapture` passed (3 real CLI/filter tests). Staged Crab
  and LFS ranges matched original bytes across boundaries and EOF without
  replacing worktree pointers; corrupt LFS returned no bytes; a changed content
  remote was rejected before storage access.
- Real desktop parser/native wiring: the ignored
  `repository::content::tests::native_crab_and_lfs_pointer_previews_use_staging_without_hydrating`
  passed with `CRAB_BIN` set to the matching built CLI. Real Git filters staged
  both formats, working files became pointers, and the desktop parser produced
  actual tensor metadata without replacing those pointers. Packaged GUI proof
  remains open.
- Isolated RustFS 1.0.3 integration passed with the built desktop agent and CLI:
  native publication of Crab and LFS, a real lazy clone, and local-clone plus
  direct-remote `stat`, boundary ranges and safetensors previews. Both worktree
  files remained pointers. Deliberately invalid ambient keys/session/endpoint
  proved that the saved connection reached the SDK and blocking preview/native
  subprocess paths. Payload SHA-256:
  `c0dc29d89b4955a39e39db80d44eaa4df9e04331b7ba5d9b655dacdbe2398888`.
  Evidence is in the external build directory's
  `large-files-proof.sjopzysk/report.json`; fixture credentials are not committed.
- Read-only transport qualification repeated all local-clone/direct-remote
  Crab/LFS metadata, ranges and tensor previews through a proxy that rejects
  writes: 136 GET/HEAD requests, zero write attempts. Evidence:
  `large-files-proof.j0atfdgy/report.json` in the external build directory.

## Open implementation details

- One-shot RPC previews capture and close their source per operation. The new
  file inspector retains a source across metadata pages, with independent query
  cancellation and close/repository-close drain. Media uses the same retained
  owner for bounded byte delivery. Checked LFS proofs survive independent query
  drains; cold multi-GiB remote verification budgets/progress remain open.
- Complete preview resource limits include decoded Parquet pages and rendered
  cells, not only source bytes. Preserve access to full data through explicit
  transfer/query operations rather than silently reporting incomplete results.
- Preserve the explicit setup contracts: CLI Crab installation intentionally
  installs only `filter.crab`; native SDK worktrees configure both drivers;
  `crab lfs install --local` selects the standalone LFS driver/transfer profile.
  The first cloud fixture omitted LFS installation and correctly staged that
  file as ordinary Git. Desktop setup must expose and qualify the intended
  profile instead of assuming attributes alone install an executable driver.
- Native pointer previews now use the SDK's read-only captured-catalog ownership
  pattern. Mount retention remains in its existing composition boundary; no
  mount source changes remain. The write-denying transport passed for the warm
  S3-compatible fixture; archived-content restoration and other provider/profile
  qualification remain open.

### Owned reader lifecycle

Each captured local pointer owns one private native reader session. The opening
handshake captures configuration and reports its content remote before storage
reads; the desktop validates it against the selected connection. The session
retains its staged recipe pin, verified LFS read session and captured read-only
catalog until close. Ranges remain capped at 8 MiB.

A per-browse-session actor set owns native children and stderr readers. Closing
one source or its repository cancels and drains active work, acknowledges native
close, reaps processes and awaits tracked tasks. Unexpected termination, protocol
errors and cleanup timeouts remain failures. Repository shutdown also closes the
SDK client even when native cleanup fails. The old one-shot/inspect/expected-remote
protocol was removed; it was an unreleased private implementation.

RPC reads and supported format previews register cancellation before capture.
They await parsers, range workers and source close before returning cancellation.
Preview components and dataset metadata panels pass abort signals on replacement
or unmount. The renderer keeps the native terminal result instead of rejecting
optimistically; search and timeline callers reject stale results independently.

- `cargo test -p crab --locked --test e2e_read_pointer -- --nocapture`
  passed (5 tests): repeated Crab/LFS ranges, full LFS integrity rejection,
  captured configuration, cancel/EOF reuse and pin retention through confirmed
  publication retirement plus staging maintenance/compaction.
- The real native desktop parser test passed again, including repository close
  with open Crab/LFS sources and rejection of subsequent reads/admission.
- Captured-content tests passed (3); parser tests passed (5). The RPC cancellation
  test proves cancellation awaits admitted work and closes its source.
- Matching debug CLI/agent repeated the isolated read-only RustFS proof:
  `large-files-proof.gptg8ywb/report.json`, 136 GET/HEAD requests, zero writes.
- Renderer cancellation tests passed; TypeScript renderer/Electron checks passed.
  Matching release CLI/agent/NFS, renderer, Electron and unsigned macOS arm64 ZIP
  builds passed. Packaged GUI cancellation and complete feature qualification
  remain open; the focused packaged smoke now covers a real staged 3 GiB Crab
  file, LFS previews and visible integrity failure/repair.

- Packaged macOS arm64 smoke passed for checkpoint `98bc6f87b6b` (16 seconds):
  real Git filters staged a sparse 3 GiB Crab file and a 4 MiB LFS file; actual
  Explorer clicks rendered tensor metadata while both working files remained
  pointers. Committed previews and boundary ranges also matched expected data.
  Corrupting the LFS object produced a visible integrity failure; repairing it
  restored the visible preview. Screenshots and logs are in the external build
  directory's `large-files-session-ui-proof` and `large-files-session-ui-smoke.log`.
  This qualifies that read-session checkpoint, not the remaining acceptance rows.

### Complete-read consolidation checkpoint

`read_text` and `read_bytes` now use captured sources instead of index-size checks
followed by a separate whole-file loader. Local, index, committed and remote
responses share a finite 200 MiB ceiling; each UI request keeps its smaller
selected limit. Complete Crab/LFS responses also verify the assembled file's
BLAKE3/SHA-256 identity. Remote complete reads use the SDK's non-range integrity
path, and a complete ordinary Git read uses one captured object process.

Text/Markdown editors and external reloads use the same bounded read owner.
Cancellation includes the repository ID so SSH requests reach their remote
owner. The tensor summary's former “On disk” label now says “Logical size”.
New focused tests cover grown working files, untracked symlinks outside the
repository, stitched native responses and empty-pointer identity. Repeat builds,
remote proof and packaged qualification after this consolidation.

- The native fixture passed with 10 MiB payloads, crossing the 8 MiB native
  frame boundary and validating complete reconstructed bytes plus incorrect
  empty-pointer hashes. Repository/source tests passed (6), complete-read
  handler tests passed (2), and renderer cancellation tests passed (5).
- Matching debug binaries passed the read-only RustFS fixture with 10 MiB
  payloads and complete `read_bytes` responses for local-clone/direct-remote
  Crab/LFS sources: 167 GET/HEAD requests, zero writes, no hydration.
- Captured-source and real native tests passed again after cancellation error
  preservation; parser tests passed (5). Cancellation waits for admitted work
  and preserves an integrity failure even when cancellation was also observed.
  Renderer/Electron typechecks and eight renderer cancellation/text-decoding
  tests passed. Invalid UTF-8 selects hex view rather than lossy editable text;
  failed or oversized external reloads show an actionable warning.
- Editor save concurrency and draft persistence still need qualification;
  reload notification alone does not make autosave conflict-safe. The packaged
  checkpoint predates this consolidation and must be rebuilt for GUI proof.

### Cache cleanup checkpoint

Desktop cache inspection and cleanup now use native `cache stats --json` and
`cache clean --json`; the retired command and recursive directory-deletion
fallback are removed. Settings reports real logical/allocated usage, configured
budget, family availability and inspection issues. Cleanup preview and actual
cleanup expose retained, busy and unsafe counts. Repository configuration is
resolved by the native owner; SSH requests keep their repository route and
cleanup is rejected during reconnection instead of queued for replay.

- CLI cleanup integration passed (3 tests), including both command surfaces,
  JSON/dry-run behavior, retained workspace preservation and active-mirror
  refusal. The sidecar's real native integration and partial-report policy
  passed (2 tests). Test caches use private owner-created payloads, matching
  the native permissions contract; unsafe public paths remain unavailable.
- Renderer/Electron typechecks and 38 focused renderer/router tests passed.
  Matching package build and GUI cleanup proof are pending. Cache budget
  editing/enforcement, SDK namespace lifecycle, offline retention and durable
  operation ownership remain open acceptance work.

### Structured desktop errors

The native JSON-RPC error's code and details now cross Electron IPC as plain
reply data. Preload rejects with a plain value; the renderer client reconstructs
an `Error` without discarding classification. Direct renderer callers now use
that client. This removes Electron's transport prefix from ordinary native
failures and preserves integrity and cancellation data for UI policy.

- The installed Electron runtime probe preserved a rejected plain object's
  code, message and nested data across its sandboxed context bridge. The main
  IPC envelope, renderer error reconstruction and domain payload distinction
  passed focused tests; 41 renderer/router tests and typechecks passed.
- Renderer and Electron production builds passed. Packaged integrity and cache
  GUI verification is pending the matching native release build.
- The first packaged error-detail check found another boundary: storage
  connection redaction preserved the error code but discarded `data`. That
  owner now recursively redacts structured details while retaining their shape
  and classification. Its credential/signed-URL tests, renderer error tests and
  typechecks passed (35 focused tests). The matching package is being rebuilt;
  the failed smoke has not been counted as successful GUI qualification.
- Documentation link validation requires a built Next.js site. Dependencies
  were installed and the check retried; it stops because this checkout has no
  production `.next` build. The PR documentation-quality CI job builds the site
  before running that check. Installer-only lock metadata was restored; no web
  dependency versions changed.

### Packaged consolidation proof

The rebuilt unsigned macOS arm64 package passed the focused GUI smoke in
15 seconds, including the latest connection-redaction fix. Real Explorer
actions previewed a sparse 3 GiB Crab file and an LFS file without replacing
their working pointers. Corrupt LFS content produced a visible failure with
code `1012` and `content_read` data across the real Electron boundary, without
the transport prefix; repairing the object restored the preview.

Settings displayed actual cache usage. Preview cleanup kept the disposable
payload; actual cleanup removed it while preserving retained workspace data.
The subsequent 3 GiB staged preview still worked, with its working pointer
unchanged. Screenshots were inspected for the cleanup and error states.

Evidence: external build directory `large-files-verified-ui-proof` and
`large-files-verified-ui-smoke.log`. Application:
`packages/desktop/release/large-files-verified/mac-arm64/Crab.app`. The earlier
ZIP predates the redaction fix; this proof uses the rebuilt unpacked package.
The other chat's Crab application was reopened and its saved workflow view
restored after each smoke. The complete feature acceptance matrix remains open.
  Evidence: `large-files-proof._xhbivbf/report.json` in the external build root;
  payload SHA-256 `4c22df11d3ecfef30df347707e9070fd9f96a0112170cb85b736818ef56001fd`.
  Renderer/Electron type checks passed; repeat packaged proof remains required.

Remaining integration details found during qualification:

- The visible Electron error currently includes its IPC wrapper text; preserve
  structured content errors across that boundary and provide an actionable retry.
- A working file that grows past the editor's reload budget needs a visible
  unavailable/conflict state while preserving the user's draft.
- Large remote LFS first reads may require complete SHA-256 verification. The
  native reader now matches the SDK's five-minute default owner, but explicit
  verification cost/progress, read budgets and the transfer queue still need
  coherent behavior for objects larger/slower than a preview budget permits.

## Captured chunk inspection checkpoint

- Shared staged/remote readers expose ordered metadata pages of at most 512
  occurrences. The SDK uses its pinned catalog; local inspection uses retained
  recipe pages. Chunk hashes/offsets/lengths and published placement come from
  real metadata. Unknown placement remains unknown; no dedup ratio is invented.
- Desktop `file_open` / `file_chunks` / `file_close` retain the source across
  pages. Each query has independent cancellation. Close waits for active queries
  before releasing native ownership; repeated callers also wait for that drain.
  Repository close
  rejects admission and drains retained files. SSH reconnection rejects these
  stateful calls instead of replaying or serving cached results.
- Focused checks passed: staged metadata pagination, captured remote catalog and
  term resolution, five native CLI integration cases, SDK files and metadata-only
  profiles, four captured-source tests, the native Crab/LFS staging fixture and
  RPC cancellation drain. Renderer/Electron type checks and 38 focused IPC/SSH
  tests passed. Release native, renderer and Electron builds passed.
- Unpacked macOS arm64 smoke passed in 17.2 seconds. Real filters staged a sparse
  3 GiB Crab safetensors file and a Git LFS file. The inspector reported 24,577
  chunk occurrences, navigated pages after replacing the working-tree pointer,
  and retained the captured recipe. Header previews, visible LFS integrity
  failure/repair and safe cache preview/cleanup still passed. Worktree pointers
  remained unchanged after fixture restoration and cleanup preserved staging.
- Evidence: `large-files-inspector-ui-proof/qualification.json` and screenshots
  in the per-worktree external build directory. The local unpacked artifact is
  `packages/desktop/release/large-files-inspector/mac-arm64/Crab.app` and is
  unsigned. This does not qualify Windows/Linux, other storage profiles,
  cold multi-GiB remote LFS, media delivery, durable transfers or the remaining
  acceptance matrix.

## LFS read ownership and integrity checkpoint

- Verification sessions now separate per-operation jobs from bounded shared
  proof retention. The native helper drains each query before acknowledging
  cancellation and keeps checked local sources for subsequent requests. SDK
  file and archive streams use the same runtime-owned proof cache.
- Remote proof reuse requires a canonical bound transport, store incarnation,
  object key, SHA-256 and fresh matching metadata. Ranged GET responses still
  match the verified strong validator. Local and remote proof memory is bounded.
- An isolated regression reproduced acceptance of a corrupt byte through a
  different read route when a persisted receipt's opaque validator coincided.
  Receipts now bind the canonical transport; routed/unbound sources cannot reuse
  canonical receipts. Wire version 2 / verifier 3 invalidates older cache records
  from v2.0.0-alpha.1 safely through fresh verification; file data is retained.
- Declared-size mismatches reject a range before opening the payload. Focused
  checks passed: 59 object-store cases, 11 selected cache/read cases, SDK files
  and metadata-only fixtures, and the VFS consumer build. Five native pointer
  cases passed for scoped query ownership; later receipt/size changes affect
  remote object reads and are covered by the object-store/SDK fixtures.
- The matching native release build and unpacked arm64 package passed. The
  isolated RustFS sidecar fixture covered published chunk pages for lazy-clone
  and direct-remote sources, Crab/LFS ranges, complete bytes and tensor previews:
  230 GET/HEAD requests, zero writes, unchanged working pointers. Evidence:
  `large-files-proof.fr8_x69z/report.json` in the external build directory.
- Packaged GUI smoke passed in 15.1 seconds, including 3 GiB chunk pagination
  after pointer replacement, LFS corruption/repair and safe cache cleanup.
  Evidence: `large-files-lfs-ui-proof/qualification.json` and screenshots.
  The unsigned unpacked artifact is `desktop-lfs-cache/mac-arm64/Crab.app`
  beneath the per-worktree external build directory. Cold multi-GiB LFS budget/
  progress, media delivery and the remaining acceptance matrix stay open.


## Retained media checkpoint

Audio and video use one captured file behind an opaque main-process capability.
The old complete base64 loaders and their 100/200 MiB limits are removed. The
private `read_range` request takes a retained `file_id`; both ranges and chunk
pages bind independent query scopes. Seeking does not reopen a pathname.

The Electron protocol streams 1 MiB windows with backpressure and single-range
206 responses, including suffix/open-ended ranges. HEAD reads no payload;
invalid ranges return 416. Cancellation awaits that response's native query.
Tab cleanup, repository close, main-frame navigation, renderer failure and app
exit close owned resources after draining native work. Structured native errors
appear in the preview; unsupported containers/codecs show a decoder error and
reopen action. File size is logical; no disk/cache availability is inferred.

- Thirteen focused media cases passed, plus five renderer cancellation and
  thirty SSH router cases. They cover bounded streaming, range semantics,
  per-response cancellation, independent seeks, structured failures, late
  capture cleanup, close acknowledgement and owner isolation.
- The retained-range Rust handler regression passed: cancel a query, replace
  the working pathname, read the original bytes through the same file identity,
  then close and reject another range. Renderer/Electron typechecks passed.
- Matching release native build (6m 06s), renderer/Electron builds and the
  unsigned macOS arm64 package passed. Read-only RustFS qualification repeated
  lazy-clone/direct-remote Crab/LFS ranges, complete reads, previews and chunk
  pages with 175 GET/HEAD requests, zero writes and unchanged pointers. Evidence:
  `large-files-media-proof.f3tup_jg/report.json` in the external build directory.
- Packaged large-file preview/integrity/cache smoke passed in 14.5 seconds.
  The media smoke passed in 21.5 seconds: real Explorer actions played and
  sought 320 MiB Crab/LFS PCM WAV files, a 240 MiB VP9 WebM and an ordinary Git
  WAV. Captured ranges survived working-pointer replacement, expired URLs
  rejected reads after tab replacement, corrupt LFS showed its native failure,
  no complete byte RPC was used, each range was at most 1 MiB, and all captures
  closed. The initial media run stopped in its test-only recording hook before
  playback; correcting its module loader produced the successful run.
- Screenshots in `large-files-media-playback-proof` were visually inspected,
  including the sought video frame and LFS integrity failure. Application:
  `desktop-media/mac-arm64/Crab.app` in the per-worktree external build directory.
  The other Crab build was restored to its saved workflow after both checks.
  Remote media UI, cold multi-GiB LFS and the remaining matrix are unqualified.


## Hydration ownership audit

The native reproduction found two independent paths behind the inspector's
hydrate action. `crab hydrate` returned success with zero selected files for LFS,
leaving its pointer untouched. A newly Git-cleaned Crab pointer failed to hydrate
although its native captured reader returned the correct bytes.

Crab staging reads now use `StagedFileReader`, including delegated Git-clean
leases, rather than a second reconstruction loop that required a published path
head. The shared reader retains the exact recipe, bounds coalesced payload reads
and verifies complete BLAKE3 before atomic publication. Selected staging errors
propagate through both configured and unconfigured hydrators instead of being
replaced with the earlier remote lookup error. No publication authority changes.

- The delegated-recipe command regression failed before the fix and passed
  afterward. Seven native pointer cases passed, including literal bracketed
  paths, unchanged Git pointers after hydration, cancellation/pin retention and
  selected staging corruption that leaves both working pointer and index intact.
- The existing published-recipe hydration and actual replica-backed hydration
  cases passed. Reconstruction ownership moved to the existing staging reader;
  non-test source decreased by 66 lines.
- Matching release build (5m 24s), unpacked arm64 package and packaged GUI smoke
  passed. In 44.4 seconds the smoke repeated previews, integrity and cache
  cleanup, then clicked **Hydrate to disk** for a 4 MiB staged Crab file. Its
  bytes matched the fixture, the Git pointer stayed unchanged and reopened
  details displayed **Working file**. The screenshot was inspected; its footer
  still showed the earlier hydration counts; the shared-status checkpoint below
  resolves that observed refresh gap.
  The LFS no-op and complete materialization/repository lifecycle remain open.
  Evidence: `large-files-staged-hydrate-ui-proof`; artifact:
  `desktop-staged-hydrate/mac-arm64/Crab.app` in the external build directory.
- Reproduction: `hydrate-desktop-repro.yxrf8inq/report.json` in the external build
  directory. Native regression logs use the `large-files-hydrate-` prefix.


## Shared hydration status checkpoint

Hydration status has one subscribed owner per repository. The footer, Explorer,
Large Files and hydration picker share its result and path map instead of
independent requests. Native hydrate/dehydrate completion invalidates active
snapshots, including failed operations that may have changed only some files.
The repository watcher invalidates the same owner for filesystem changes.

An invalidated in-flight result drains before one fresh read starts; it cannot
replace the post-mutation view. Releasing the last subscriber drops snapshot and
event ownership, and late replies cannot overwrite a newly opened repository.
Failures clear measured values and display **File status unavailable**; explicit
refresh can recover. Per-view hydration completion callbacks are removed.

- Five focused owner tests passed: shared reads, coalesced invalidation, late
  results, unknown failure/retry and subscriber cleanup. Renderer/Electron
  typechecks and the production renderer build passed.
- The unpacked arm64 package smoke passed in 18.6 seconds. After real staged
  materialization, Explorer showed **1 hydrated / 2 pointer** and the footer
  showed **1 / 3 hydrated** with measured disk bytes, without manual refresh.
  Closing the native repository produced the unavailable state; reopening and
  refreshing restored measured counts. Screenshots were inspected, and the peer
  Crab build was restored to its saved workflow.
- Evidence: `large-files-hydration-status-ui-proof`; application:
  `desktop-hydration-status/mac-arm64/Crab.app` in the external build directory.
  Shared request/lifecycle ownership accounts for the non-test source growth.
  LFS materialization, durable transfers and the remaining matrix are open.

## Captured materialization checkpoint

File details no longer routes single-file hydration through the Crab-only CLI
inventory. `file_materialize` uses the captured working Crab or LFS source,
streams 1 MiB ranges to a same-directory temporary file, and verifies its whole
BLAKE3 or SHA-256 identity before atomic replacement. It preserves permissions
and the Git pointer. Detected external edits survive; failed and cancelled writes
explicitly close and remove their temporary output. Historical, remote and
symlink captures do not advertise materialization.

Native reader lifetime is separate from the opening request and ordinary query
scopes. The materializer holds a file guard and repository scope, so closing its
view waits for the write instead of cancelling it. Repository close first stops
that scope, then drains guards and native actors. Capture cancellation still
drains a late native handshake. The original terminal reply wins a cancellation
that loses to atomic publication.

Activity owns the operation across view changes. Progress reports actual logical
bytes written to temporary output, without calling them network bytes. Its
cancel button routes the operation ID through the same repository. Acknowledged
cancellation is distinct from failure; a lost transport reply leaves outcome
unknown instead of claiming rollback. Failures retain native classification and
appear in the inspector and redacted Activity log.

- Three real-native regression cases passed in 25.9 seconds: both formats above
  the 200 MiB complete-response cap, byte identity and unchanged index, literal
  comma/bracket filenames, executable permissions, non-materializable index and
  symlink captures, cancellation and external-edit preservation with temporary
  cleanup, view-independent completion and repository-close cancellation/drain.
  The existing Crab/LFS native preview fixture also passed after the reader
  lifetime change. CI runs the new native cases against its bundled CLI.
- Forty-three focused renderer IPC/cancellation/SSH/shared-status checks, thirteen
  media cases, four captured-source cases and renderer/Electron typechecks
  passed. Production native, renderer and Electron
  builds and unsigned arm64 packaging passed. Agent Clippy remains blocked by
  three unchanged `unwrap()` violations already present on `origin/crab-v2`:
  two in `forge/handlers.rs` and one filesystem fallback in `handlers.rs`.
  No Clippy diagnostic remains on the new materialization/read-owner code.
- The isolated RustFS fixture passed publication, lazy/direct-remote captured
  reads and both lazy-clone materializations through selected read-only storage.
  Exact bytes, unchanged Git pointers, actual progress and one successful native
  completion per job were checked: **230 GET/HEAD requests, zero writes**.
  Evidence: `large-files-materialize-proof.enxiyall/report.json` in the external
  build directory.
- The matching packaged GUI smoke passed in 19.0 seconds, including sparse
  3 GiB preview/chunk inspection, actual Crab and LFS **Hydrate to disk** actions,
  exact 4 MiB payloads, clean Git diffs and two updated hydrated-file counts.
  Corrupt LFS materialization showed the native failure and preserved its
  pointer. Activity visibly displayed both verified results and the failed
  attempt. Screenshots were inspected, and the peer app was restored to its
  saved workflow. Evidence: `large-files-materialize-ui-proof`; package:
  `desktop-materialize/mac-arm64/Crab.app` in the external build directory.
- Non-test growth owns actual streaming publication, integrity, temporary-file
  cleanup, lifecycle and visible operation state. It replaces the inspector's
  status-only CLI call; it adds no second preview or reconstruction engine.
  Bulk LFS routing, decoded limits, diffs, durable transfers, cache budgets and
  the remaining acceptance matrix are still open.

## Cold LFS read budget checkpoint

A direct-remote LFS capture opened metadata for a 2,147,483,665-byte file, but
its first 16-byte range failed. SDK reads default to 512 MiB fetched bytes;
partial LFS delivery first verifies the complete SHA-256 object, so that finite
default could not admit the cold object. The failure is recorded in
`large-files-cold-lfs-proof.jk7ii6wh/report.json` in the external build directory.

Desktop content options now add the captured LFS size to the SDK's existing
finite metadata/retry allowance. Checked addition rejects overflow. This applies
only to remote LFS content reads; SDK defaults, other content kinds, response and
decode limits, request ceilings, deadlines and cancellation remain unchanged.
The shared LFS reader still checks actual declared size before consuming body
bytes and verifies the complete identity before partial delivery. Range replies
remain bounded to 8 MiB, and complete preview responses retain their 200 MiB cap.

- The same dedicated RustFS scenario passed after the caller policy changed:
  a cold EOF range returned exactly 16 correct bytes after full verification in
  6.398 seconds. An independently scoped warm query took 0.007 seconds and the
  proxy forwarded only 16 payload bytes; no second full download was needed.
- Observed sidecar peak RSS was 33,328 KiB, sampled every 100 ms. The qualification
  proxy also streamed bounded windows rather than buffering its 2 GiB response.
  The read phase used **19 GET/HEAD requests and zero storage writes**, despite
  deliberately invalid ambient storage identity. Working-clone pointers stayed
  unchanged. Report: `large-files-cold-lfs-fixed-proof.3bczcjt8/report.json` in
  the external build directory. Its `sha256` field describes the small fixture
  seed; the cold file's SHA-256 comes from its independently cleaned LFS pointer.
- Four captured-source tests and the matching native release build/unpacked
  package passed. The policy adds no runtime configuration or dependency/API
  surface. Packaged remote cold-read progress, interruption, slow transfers and
  the remaining acceptance matrix still require qualification.

## Captured comparison checkpoint

`file_diff` now owns explicit working, index, revision and empty-side captures.
Local refs resolve once, Git blobs retain physical object identity, and native
pointer readers open only when actual content or recipes are needed. Matching
pointer identities return without fetching a recipe. Ordinary Git patches,
including textconv, remain Git's responsibility; comparison errors remain errors.

Crab comparisons read actual ordered chunk pages and use the shared `crab-diff`
engine. Hydrated working files use the canonical Gear chunker. Metrics describe
ordered matching content independently of xorb placement; they are not network
transfer estimates. Same-size LFS changes compare SHA-256 identities and leave
Crab chunk metrics unknown. Existence is separate from zero size, including
unborn HEAD and initial commits. Changed ranges have accessible pagination.

The desktop's Git read/compare supervisor and CLI now share the same owned
process-group/job-object boundary in `crab-git`. Cancellation and output limits
stop descendants before joining pipe readers; partial Git ranges discard input
incrementally and complete reads retain their separate 200 MiB ceiling.

- Focused captured-source/comparison tests passed: twelve cases plus the real
  native recipe fixture, two shared process-owner cases and nine ordered chunk
  comparison cases. Existing native materialization and pointer-preview fixtures
  passed after the deferred-reader change. Four SSH/provider transport test
  files passed with 86 cases; live comparisons are never replayed from cache.
- Dedicated RustFS comparison/materialization proof passed through a write-denying
  proxy: local lazy clone and direct remote, Crab and LFS, same-size revisions,
  empty baselines, exact materialization and unchanged index pointers. Equal
  pointer comparisons made no storage requests. The read phase recorded 290
  GET/HEAD requests and zero write attempts. Evidence:
  `large-files-diff-proof.fhs5ucge/report.json` in the external build directory.
- SDK CI exposed provider body retries escaping a completed header-admission
  scope. The shared storage wrapper now scopes every payload poll, preserving
  request/byte limits and cancellation during provider range resumes. The exact
  S3 fixture failed before this fix and passed afterward against isolated RustFS;
  its filesystem sibling, nineteen storage admission cases and storage Clippy
  also passed. Typed rejection sources remain available through transport wrappers.
- Hydration outside a Git worktree again preserves the original remote failure;
  real local staging still restores an unpushed pointer. Both focused regression
  tests passed. Shared chunk-page Clippy and wire-contract admission checks pass.
- Packaged comparison screenshots cover chunk metrics, more than fifty changed
  ranges, same-size LFS and initial commits. Final matching-package proof follows
  below. Ordinary Git patches still read the live
  index/worktree through Git, so concurrent mutation needs capture validation.
  Inline staging/review, mode-only large-file changes, other format comparisons
  and remaining provider/profile surfaces still require implementation or
  qualification; this does not close the matrix.

## Staging-writer admission checkpoint

The final comparison smoke exposed `CRAB-E0081` while a background Git clean
filter held staging admission. Shared file readers previously used a short,
synchronous lock attempt. They now queue asynchronously using staging's existing
120-second budget and the caller's cancellation token. Started metadata work is
joined before return; contention remains distinct from missing local content.
No alternate source or new runtime setting is introduced.

Native `Open` also listens for close/EOF before `Ready`, cancels queued admission,
and drains its setup work before acknowledging `Closed`. Chunk, range, complete,
CLI hydration and VFS pin owners share the same source-selection boundary.
The tokenless Rust entry points shipped in `v2.0.0-alpha.1` delegate to that path;
current owned operations supply their existing cancellation token.

- Twelve staging-reader cases passed, including single-thread cancellation,
  writer release, complete retained bytes, sibling cleanup, corruption and
  retirement. Shared staging/read Clippy and the NFS consumer build passed.
- Eight real CLI pointer/filter cases passed with the desktop pathspec feature,
  including close/EOF before `Ready` while an exclusive writer stays active.
  The lease restoration fixture now selects its single pointer with `--all`:
  optional pathspec syntax is separate from the exact-byte and unchanged-index
  lease assertions. Both minimal and desktop-pathspec CLI profiles passed all
  eight cases after that fixture fix.
- The matching optimized native build and unsigned macOS arm64 package passed.
  Both packaged GUI tests passed in 36.7 seconds: real 64 MiB same-size Crab
  changes, more than fifty changed ranges and pagination, index/working selection,
  same-size LFS, ordinary documentation patches, unchanged Git patches and initial
  commits. Exact fixture bytes and index pointers remained unchanged. The sibling
  sparse 3 GiB preview, cache cleanup, corruption/repair, Crab/LFS materialization,
  hydrated counts and visible Activity outcomes also passed.
- Matching-runtime RustFS proof repeated the local lazy-clone/direct-remote
  comparisons and exact materializations: 290 GET/HEAD requests and zero writes.
  Evidence: `large-files-diff-proof.rps7thf2/report.json` in the external build
  directory. The cold SDK LFS data path is unchanged from the preceding 2 GiB
  proof; packaged cold progress/interruption remains open.
- Evidence: `large-files-staging-admission-ui-proof/qualification.json` records
  tested commit `112356576500d48c6310186902f3c0b41c65c8b0`, source/native/package
  hashes and fourteen screenshots. Selected screenshots were inspected. The peer
  Crab app was restored to its saved workflow, Save disabled, Activity empty and
  port 17789 listening. Package: `desktop-staging-admission/mac-arm64/Crab.app`.
  The failed prior smoke remains negative evidence, not a passing qualification.

## Captured notebook comparison checkpoint

Notebook comparisons use the same `file_diff` capture and cancellation owner as
other files, including index and explicit empty sides. Plain Git blobs retain
their physical object IDs; working files retain open descriptors; Crab/LFS
content uses verified complete reads. Only a proven absent file becomes `null`.
Empty files, malformed JSON, unsupported formats and failed reads remain errors.
The 16 MiB combined input ceiling is checked before reading either complete
document; parsing also caps the pair at 100,000 cells. Limits reject the request,
never a partial notebook comparison.

The parser follows [Jupyter's nbformat 4 description](https://nbformat.readthedocs.io/en/latest/format_description.html):
string/list sources, unique IDs for 4.5+, code execution counts and preserved
metadata, attachments and extension fields. Persistent IDs align edited cells.
Earlier notebooks match source occurrences in order; an increasing subsequence
keeps alignment monotonic in O(n log n) work and linear memory. This need not
produce a minimal edit script, but retains both complete cell sequences.
Object key order does not create false changes. Unsupported major versions
report an error; no implicit conversion is performed.

Parsing, equality, alignment and output serialization run in a dedicated worker.
The UI renders 25 cells and bounded text pages with direct page selection.
Unchanged cells and long outputs stay accessible, individual output toggles
remain usable after Show all, and outputs are inert text. Cancel terminates the
real worker; Retry and version changes create a new captured comparison.
History uses the same notebook renderer and explicit initial-commit baseline.

- Eight incorrect cases reproduced before the fix. The active desktop suite now
  passes 22 notebook cases, including malformed input, absence, metadata, IDs,
  duplicate/reordered sequence preservation and 40,000 repeated legacy cells.
  This replaces the excluded source-tree test file and its obsolete empty-on-error
  expectations. Ten focused native comparison cases pass; the native recipe
  case is exercised separately by the packaged sibling smoke.
- Matching-runtime RustFS proof passed for Crab/LFS notebooks in a lazy clone and
  direct remote: exact captured old/new documents and initial comparisons,
  retained pointer files, selected connection authentication, 397 GET/HEAD
  requests and zero writes. Evidence: `large-files-notebook-proof.yafvncc4/report.json`.
- The unsigned macOS arm64 package passed the notebook GUI smoke in 14.1 seconds:
  all version selectors, 40,000 cells with page 1,600 reachable, output paging,
  global/local toggles, inert output, additions/deletions, empty-notebook addition,
  malformed/empty/unavailable/oversized errors and initial history. DevTools
  paused the real worker at startup; clicking Cancel detached that worker, and
  Retry completed. Application shutdown completed cleanly. The first run's
  ambiguous test locator and interrupted cleanup remain negative evidence.
- The same package passed the existing large-file comparison/materialization
  smokes in 34.9 seconds. Native build, renderer/Electron builds, type checking
  and formatting passed. Desktop ESLint is blocked by a missing configuration,
  also absent on `origin/crab-v2`; no lint configuration or dependency changed.
- Evidence: `notebook-ui-proof-v2/qualification.json`, six notebook screenshots
  and fourteen sibling screenshots. Package: `desktop-notebook-comparison/mac-arm64/Crab.app`.
  The peer app was restored to its saved workflow, Save disabled, Activity empty
  and port 17789 listening. Cross-platform/current-head CI and the remaining
  acceptance matrix remain open; this checkpoint does not close the feature.

## Owned SSH source checkpoint

SSH content operations now retain their native owner through completion. The
30-second metadata deadline does not discard file captures, bounded reads,
previews, comparisons or materializations. An explicitly configured operation
deadline signals `rpc/cancel` and waits for the original terminal reply. A
cancellation acknowledgement alone does not release the request; a raced success
or integrity failure remains authoritative. Release operations wait for cleanup.
If cancellation signalling fails, the channel sends EOF and waits for native
shutdown instead of inventing completion.

Pending calls and byte buffers belong to a specific channel. Closing or replacing
one channel settles its calls without affecting sibling profiles. NDJSON framing
precedes strict UTF-8 decoding, preserving multibyte text split across SSH packets.
Batches use the same single-request protocol, with four calls in flight; the
native agent does not accept JSON-RPC arrays. Reconnection only replays the five
explicitly cacheable metadata methods. Captures, mutations, cancellation signals
and unknown methods cannot cross agent sessions. Stale reconnect prefetch results
do not refresh metadata TTLs.

Native admission latches cancellation before a handler subscribes. On stdin EOF,
the sidecar stops admitted and newly subscribing reads, joins their cleanup and
then closes the indexer and repository owners. Each local application profile
passes its own index database directory, avoiding a shared home-directory SlateDB
writer. Standalone/SSH launchers retain their existing default; remote index
namespacing and legacy uncancellable handlers remain open.

- Seven router regressions and the early-cancellation latch failed before the
  fixes. Thirteen focused source lifecycle cases now cover timeouts, terminal
  integrity/success races, default long reads, channel ownership, fragmented
  UTF-8, replay cancellation/disconnect, bounded batches and stale prefetch.
  The five focused router/cache/timeout/operation files pass 167 cases; four
  sibling provider/transport files pass 46. Four
  native cancellation registry cases pass. Renderer/Electron type checks, the
  new integration spec's strict type probe, formatting and native/renderer/
  Electron builds pass.
- A real authenticated loopback ssh2 server runs the packaged native sidecar and
  CLI with isolated index/cache directories. The exact packaged router receives
  actually split UTF-8 packets, reads bounded Crab/LFS ranges and materializes
  both 4 MiB files byte-identically with real progress and unchanged Git index
  pointers. A live exclusive staging writer proves explicit timeout cancellation
  joins the CLI child before rejecting. Channel EOF also joins that child before
  clean sidecar exit while the writer remains locked; a second profile stays
  usable. The final smoke passed in 6.6 seconds. It uses POSIX flock and explicitly
  skips Windows; it does not qualify GUI deployment, providers or credentials.
- The same unsigned macOS arm64 package passes the existing desktop large-file
  GUI smoke in 22.7 seconds: 3 GiB previews, captured chunk pages, integrity
  failures, cache cleanup and Crab/LFS materialization. The peer build was
  restored to its saved workflow, Save disabled, Activity empty, port 17789 live.
- Evidence: `ssh-owned-final-proof/` and `ssh-owned-qualification.json` in the per-checkout external build directory.
  Package: `desktop-ssh-owned/mac-arm64/Crab.app`. Production desktop SSH setup
  remains unqualified: the application does not inject deployment dependencies
  into its session manager, capability negotiation routes through a deploying
  status, and the bundled sidecar lacks that capabilities method. These are
  follow-up implementation gaps, not passing checks. Current-head CI and the
  remaining acceptance matrix also remain open.

## SSH host identity checkpoint

Every SSH client created by the session manager now verifies the server key
before authentication: direct connections, connection tests, reconnects and each
jump hop use one connection owner. The network/authentication deadline pauses
while the user reviews a host key. Verification remains active for rekeying;
closing the client aborts an outstanding trust request. An omitted verifier
rejects host identity rather than accepting it implicitly.

The existing host-key dialog is now connected to the main-process trust owner.
Unknown keys require an explicit decision; changed keys show both fingerprints
and require the confirmation checkbox. Prompts are queued by opaque request ID,
keyboard focus stays inside the active dialog, and rejection/closure cannot be
accepted by a late response. The store is owned by the Electron user-data
profile. A failed write cannot create an in-memory trusted pin, and corrupt or
unreadable stores fail visibly instead of being overwritten. No profile or OS
keychain entry is saved by the connection-test smoke.

System `known_hosts` handling follows the
[OpenSSH file rules](https://man.openbsd.org/sshd.8#SSH_KNOWN_HOSTS_FILE_FORMAT)
for exact ports, hashed names, wildcard/negated patterns and revoked keys. System
revocations reject even an app pin; explicit app pins are not bypassed by stale
system trust. SHA-256 fingerprint text omits Base64 padding, and the randomart
uses the same digest, matching a real `ssh-keygen -lv -E sha256` fixture. The
[OpenSSH fingerprint implementation](https://github.com/openssh/openssh-portable/blob/master/sshkey.c)
and installed ssh2 verifier/key-exchange source were checked. Certificate-authority
host certificates are not qualified by this checkpoint.

- Real loopback SSH first reproduced credentials sent without a trust decision.
  Separate regressions reproduced incorrect port trust, ignored hashed names,
  stale-system pin bypass and ignored revocation. Independent OpenSSH output
  exposed the old MD5 art, malformed borders and padded fingerprint text. Eight
  focused files pass 148 cases, including real reconnect/rekey, delayed/rejected
  trust, aborted requests, persistence failures and the OpenSSH fixture.
- The unsigned macOS arm64 package passes the actual connection form: reject
  sends no credentials; reviewing beyond its five-second network deadline still
  permits acceptance; saved trust is reused; changed fingerprints reject until
  explicitly accepted and persist the new pin. Field labels, focus cycling and
  disabled changed-key acceptance are exercised. The GUI smoke takes 13.2 seconds.
  The same package repeats the real staged Crab/LFS SSH read/materialization and
  native timeout/EOF cleanup proof in 8.6 seconds; both finish in 22.6 seconds.
- Renderer/Electron builds, type checks, strict integration-spec type probes and
  formatting pass. All 901 packaged renderer/Electron files match build output.
  Evidence: `ssh-host-qualified-proof/`, `ssh-host-packaged-assets.json` and
  `ssh-host-qualification.json` in the per-checkout external build directory.
  Package: `desktop-ssh-host-trust/mac-arm64/Crab.app`. Selected screenshots were
  inspected; the peer app was restored to its saved workflow with Activity empty.
- Earlier smoke failures remain negative evidence: the empty start screen has no
  SSH entry, one shutdown was interrupted, and the rotation fixture skipped its
  listener close. ssh2's runtime Server lacks the `listening` property declared
  by its types; both SSH fixtures now close through its actual API. The passing
  GUI fixture starts from an opened repository. Debug instrumentation was removed.
- Parent commit `5126ca9c2e9` passes the full SDK matrix, including the approved
  public API snapshot gate in [SDK run 42684177152](https://github.com/crabbuild/crab/actions/runs/42684177152).
  The desktop/architecture jobs were still running at this checkpoint.

This does not qualify the complete desktop SSH path. Agent deployment/runtime
pair delivery, capability negotiation, remote repository opening, selected
storage identity forwarding, reconnect/disconnect ownership and cross-platform
proof remain open, as does the empty-start SSH entry. The complete large-file
acceptance matrix above remains the completion boundary.

## Native SSH capability checkpoint

- The native dispatcher now answers `capabilities` with protocol 1, its exact
  package version and its registered method names. One dispatch table owns the
  advertisement and handlers. A source comparison confirms all 193 pre-existing
  handler arms are unchanged apart from macro-required comma separators.
- SSH negotiation queries the attached channel while the session is deploying,
  before repository routing is available. Malformed/incompatible responses fail;
  clearing or superseding a session prevents late replies from restoring stale
  capabilities. Feature checks require every RPC used by watching, compression
  and delta operations. The nonexistent feature-flag/minimum-version protocol
  and fabricated fallback capability response are removed.
- Red evidence: the native regression fails with `UnknownMethod`; the prior
  package fails the actual SSH transport smoke with `status: deploying`.
  The new native regression and 129 focused desktop cases pass. Renderer and
  Electron builds, type checks, strict integration-spec type probes, formatting
  and the optimized native build pass.
- The unsigned macOS arm64 package passes real authenticated SSH in 8.8 seconds:
  two independently indexed sessions negotiate before opening repositories,
  then repeat exact 4 MiB Crab/LFS ranges and materialization, unchanged index
  pointers, actual progress, UTF-8 fragmentation, timeout cancellation behind a
  live writer and EOF child cleanup. All 901 packaged renderer/Electron files
  match build output; packaged native hashes match their runtime manifest.
  Evidence: `ssh-capabilities-package-proof/`,
  `ssh-capabilities-packaged-assets.json`, `ssh-capabilities-dispatch-proof.json`
  and `ssh-capabilities-qualification.json` in the external build directory.
  The peer desktop application remains running on port 17789.
- This resolves the capability negotiation blockers recorded above. Complete
  desktop SSH startup still needs deployment dependency injection, paired native
  runtime delivery, remote repository registration and profile/provider identity
  qualification. Empty-start SSH navigation, remote index ownership and the rest
  of the acceptance matrix remain open. This transport fixture supplies the real
  native process through SSH; it does not qualify the application's deployer.
- Follow-up broad Linux CI for `6b8a8ce059a` passes the native tests, packaging
  and packaged smoke, then fails six obsolete replay/message expectations and
  three real-network host tests using a 100 ms handshake budget. The routing
  failures reproduce locally. The removed non-cacheable replay tests are
  consolidated into the router's explicit rejection table, including forge,
  staging, worktree and stash methods; cacheable metadata replay remains tested.
  Error assertions use the stable `SSH_RECONNECTING` code. The host fixture uses
  a two-second network deadline and holds trust longer than that deadline, so it
  still proves the human-review pause. All 108 cases across those seven files
  pass locally. This corrects the test contract; it does not widen replay or
  change production connection deadlines. Broad CI still needs a green rerun.

## Paired SSH runtime producer checkpoint

- Linux/macOS native builds now produce a compressed, checksummed CLI/sidecar
  pair and target descriptor beneath `resources/native/ssh-runtimes/`. Both
  inputs must match the native build manifest before archiving. Hard links keep
  staging from duplicating the large binaries; hashing is streamed. Only the
  native pair and its metadata enter the archive. The NFS helper remains local.
- All 22 build-script cases pass, including actual archive extraction, exact
  inventory/bytes/metadata, checksum validation and rejection of a modified
  input without replacing the previous archive. An actual macOS arm64 archive
  is 85,257,915 bytes: extracting it preserves both hashes, its CLI reports
  `2.0.0-alpha.1`, and its agent reports all 194 methods and exits cleanly.
  The final package repeats the real SSH transport smoke in 7.9 seconds and
  verifies all 901 renderer/Electron files and native manifest checksums.
  Evidence: `ssh-runtime-archive-proof.json`, `ssh-runtime-package-proof/` and
  `ssh-runtime-packaged-assets.json` in the external build directory.
- This is the deployment input producer. The application's deployer still uses
  its old binary-only path; archive consumption, all-target assembly and actual
  desktop bootstrap remain open. No remote-deployment completion is claimed.

## Native SSH bootstrap checkpoint

- The application now consumes the verified archive catalog. One session owner
  performs connect, manual deployment and reconnect bootstrap. It verifies the
  local archive and remote native pair, uploads through SFTP into a unique
  temporary directory, and atomically publishes an immutable runtime directory.
  Repeated profiles reuse the pair without uploading again. Each native process
  receives an absolute `CRAB_BIN`, a matching helper path and a private index;
  shutdown drains the process before the shell removes that index.
- Native routing, deployment and SFTP repository browsing initialize before
  optional SSH IDE services. Opening a repository sends its actual `ssh://`
  identity to `open_local`, registers the remote provider, and opens it in the
  main Files view. Reconnect reopens tracked identities before metadata replay.
  Disconnect revokes pending reconnect authentication and joins native cleanup.
- The real packaged macOS arm64 UI smoke passes in 11.7 seconds: key-file SSH,
  actual host trust, 85,257,915 uploaded bytes, remote repository navigation,
  a 4 MiB Safetensors preview through native RPC, and user-triggered hydration
  producing byte-identical content while preserving the Git index pointer.
  The smoke exposed and fixed a zero-height remote file tree. It also verifies
  custom app-profile isolation without changing `HOME` or saving a password.
- The production manager/router/deployer smoke passes in 9.7 seconds: Crab and
  LFS EOF ranges, exact materialization, two independent sessions, runtime reuse,
  explicit reconnect, automatic reconnect and queued Git metadata replay. Both
  sessions drain; no upload/index temporary directories or SFTP handles remain.
  Evidence lives in `ssh-bootstrap-ui-proof-v6/` and
  `ssh-bootstrap-reconnect-proof-v2/` in the external build directory.
- CI now produces five native payloads and assembles all four POSIX SSH archives
  into each desktop package. This workflow change still needs fresh CI proof.
  The Windows archive test exposed GNU tar interpreting a drive letter as a
  remote host; archive creation and extraction now use relative archive names.
  All 22 build-script cases and 156 related SSH cases passed locally, including
  pending-authentication disconnect. The user approved exactly three production
  dependency inventory entries for the shared comparator/chunker/contracts;
  the actual 164-edge workspace inventory and its two focused tests pass.
- This does not complete the acceptance matrix. Empty-start SSH navigation,
  persisted SSH repository reopening, complete remote file-operation parity,
  visible remote transfer progress/cancellation and multi-hop ownership remain
  open. Unknown pointer formats, including plain `.txt`, currently open metadata
  rather than a bounded text preview. Cross-OS/architecture and provider proof
  remains required; this local package contains only macOS arm64 native payloads.

## Saved SSH repository checkpoint

- Startup and deferred sidebar selection share `rpc.reopen`. A saved `ssh://`
  identity resolves its saved profile, joins connection/bootstrap, and opens
  through the remote provider. Explicit repository opening requests native
  services even when automatic deployment is disabled for SFTP browsing.
  The literal-path parser now lives in the existing Node-free SSH contract;
  renderer and main-process callers use the same implementation.
- The store loads inert saved entries before its first persistence effect.
  Failed connections preserve entries and the selected path; the startup error
  remains visible with Retry. A delayed startup success does not take focus
  from a repository selected meanwhile. Disabling automatic reopen leaves
  saved entries available for manual selection without starting SSH work.
- Concurrent opens join one profile connection, including background reconnect.
  Authentication cancellation is joined; each jump attempt owns its hops.
  Pending SFTP opens are keyed by authenticated client, and late old-client
  callbacks close their wrapper rather than adopting it after reconnect. A
  transport lost during reconnect bootstrap stays reconnecting; the revoked
  deployment cannot report a dead SSH connection as ready.
- The packaged outage test reproduced an unhandled workspace `error` event.
  Workspace polling now owns only open workspaces, revokes late replies on
  close/reconnect, and reports failures through existing repository status
  errors. Unrelated profile failure cannot crash workspace orchestration.
- A fresh optimized native pair was built after rebasing onto `crab-v2`
  `2f6d36ec12a`. The matching unsigned macOS arm64 packaged smoke passes in
  22.4 seconds: the empty home screen opens
  the existing SSH Connections view without first opening a local repository,
  followed by real key-file authentication/trust, manual native deployment,
  failed restart with saved entries/selection intact, visible Retry, logical
  4 MiB Safetensors preview,
  deferred second-repository opening over the same connection, another restart
  without an archive upload, and manual access with automatic reopen disabled.
  Working-tree and index pointers remain unchanged. The peer application was
  restored to its original idle workflow after each prepared smoke.
- The real packaged bootstrap and native Crab/LFS transport checks pass in
  21.2 seconds together, covering independent profiles, explicit/automatic
  reconnect, repository replay, exact bytes, cancellation and drained cleanup.
  The packaged progress/cancellation smoke passed in 51.3 seconds on
  `0aadf7c4afb`; the later sync changes only forced-push visibility, with
  identical desktop and captured-read sources.
  There are 160 passing focused cases across eight touched ownership/routing
  surfaces; renderer/Electron and strict integration-spec types pass. Evidence:
  `desktop-reopen-latest-ui-proof/`, `desktop-reopen-synced-ui-proof/`,
  `desktop-reopen-latest-backend-proof/` and
  `desktop-reopen-qualification.json` in the external build directory.

## Native recovery checkpoint

- Local startup joins one process and negotiates the same protocol/method
  contract as SSH. Pending replies belong to that process. Signal exits trigger
  recovery only after stdio closes, matching Node's
  [child-process close contract](https://nodejs.org/api/child_process.html#event-close).
  Shutdown joins native EOF cleanup instead of imposing an eight-second kill.
  Crash records use the application profile and retain interrupted request IDs.
- Successful opens retain repository identity for replay. Recovery resolves the
  current saved storage binding; one local repository's failed/blocked replay
  does not prevent its siblings from serving requests. Explicit reopen revokes
  the old capture session before native replacement, including a failed reopen
  in a still-running process. Retry can start services after startup failure.
- Capabilities and native session identity reach the renderer together. Local
  Node filesystem editing remains available during sidecar loss; native
  materialization requires the captured repository and complete method set.
  SSH native loss revokes editing and single-file hydration while retaining the
  connection. A restored SSH repository reuses its current native handle;
  opening it a second time had removed readers created by the ready event.
- Inspectors recapture on session replacement. Inspector/media requests carry
  their expected native session; stale reads, materialization and cancellation
  are rejected, while stale close cannot reach the replacement. Media URLs are
  revoked. In-flight cleanup still joins its original work. Single-file
  `canMaterialize` and bulk `canHydrate` have separate controls and contracts.
- The matching unsigned macOS arm64 package passes two prepared GUI smokes in
  26.1 seconds: local SIGKILL with two repositories, stale file/media rejection,
  exact 4 MiB Crab/LFS hydration with unchanged index pointers; and actual SSH
  trust/deployment, visible progress, 512 MiB cancellation with pointer/temp
  preservation, native EOF with live SSH, visible Retry and fresh inspector
  recovery. SSH single-file hydration is enabled while bulk hydration remains
  unavailable. The peer application was reopened after each smoke.
- 235 focused tests across 15 owner/provider/transport files pass. Production
  renderer/Electron builds, project types, strict smoke-spec types and Rust
  formatting pass. Packaged source/asset/native hashes are recorded externally
  in `native-recovery-qualification.json`; GUI results and screenshots live in
  `native-recovery-qualified-ui-proof-v2/`. Rust/native inputs are unchanged
  from the qualified `2f6d36ec12a` pair. The desktop lint command currently has
  no ESLint configuration; this is not a passing lint result. The preceding
  published head also has an unqualified NFS TCP port-release CI failure.
- Remaining recovery work is explicit in the P0 row above. This checkpoint
  does not qualify storage rebind capture revocation, failed SSH sibling replay,
  pending editor buffers, playback recovery, durable transfers or the complete
  large-file acceptance matrix.

## SSH repository isolation checkpoint

- A negotiated SSH process and an opened repository have separate availability.
  Each replay retains its own outcome; a missing repository no longer tears down
  a successfully deployed process or disables a healthy sibling. Retry opens
  only the failed repository. Concurrent opens share one native outcome; close
  during replay drains a late native open without restoring its logical entry.
- Local and SSH owners use one repository runtime contract. Capability events
  cover all opened repositories, including inactive views that stopped watching.
  Failed repositories reject dispatch with their retained recovery error. Their
  native session remains unavailable even while the SSH process is healthy.
- Explorer and quick-open listings follow repository session identity. A
  recovered view reloads refs/files and ignores late results from the old
  session. Pending editor state is not replaced by a synthetic filesystem event.
- Disconnect sends native EOF before joining bootstrap/replay, then waits for
  native close and the original bootstrap owner. The opposite order waited on
  work whose shutdown signal had not been sent. Focused tests reproduce and
  protect that ordering alongside failed replay, concurrent open and late-close
  ownership.
- The package rebuilt after syncing `crab-v2` `54e5120f134` passes the expanded
  real SSH GUI smoke in 25.3 seconds. The fixture removes one repository's Git
  metadata, ends the native process while SSH stays connected, and retries the
  healthy repository. The healthy sibling hydrates exact 4 MiB content while
  preserving its index pointer. Selecting the failed inactive sibling shows its
  own error and disabled controls. Restoring its Git metadata and using Retry
  reloads its file tree without replacing the native process or SSH connection.
  The fixture restores the healthy working pointer for the subsequent saved
  restart, failed restore/retry, archive reuse and manual-access checks.
- 242 focused cases across 15 ownership/provider/transport files pass. Project
  types, strict smoke-spec types, production renderer/Electron builds and Rust
  formatting pass. The peer app is restored after every prepared smoke.
  Evidence: `ssh-isolation-latest-ui-proof/`,
  `ssh-isolation-final-tests.log` and `ssh-isolation-qualification.json` in the
  external build directory. Native inputs remain identical to the earlier
  branch build recorded by `desktop-reopen-qualification.json`; the target
  branch sync changes only the Changes sidebar layout and its guide.
- Slow lifecycle RPCs remain a separate gap: bootstrap still waits for replay
  outcomes, and metadata timeouts can discard a pending native open reply.
  Storage identity replacement, pending editor/media recovery, multi-hop
  cancellation and the full acceptance matrix remain unqualified.
- Startup-independence qualification also passes in 27.5 seconds. A separate
  case temporarily removes access to this task's packaged local agent and
  verifies local `ping` fails. With that failure present across every launch,
  real SSH trust/deployment, logical preview, exact sibling materialization,
  per-repository recovery, saved restart/retry and manual access still pass.
  The SSH archive remains intact; the local executable's original permissions
  are restored in cleanup. Evidence: `ssh-local-unavailable-ui-proof/`.
- The same synced package repeats local SIGKILL/Crab/LFS capture recovery and
  the SSH Activity/progress/512 MiB cancellation/native-retry regression smokes
  successfully in 24.9 seconds. Evidence: `ssh-isolation-regression-ui-proof/`.
- This checkpoint closed the saved-repository slice, not the acceptance matrix.
  The current remaining-priorities section supersedes its earlier work order;
  subsequent recovery and single-file capability evidence does not qualify the
  complete bulk lifecycle, provider matrix or durable transfer behavior.
