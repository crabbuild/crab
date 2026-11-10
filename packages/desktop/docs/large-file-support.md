# Desktop large-file completion

The interface may expose fewer navigation destinations; the capabilities remain
complete. Completion requires a user action, a real native/storage side effect,
and an accurate visible result. Compilation and mocked tests alone do not prove
completion. This record tracks implementation and qualification separately. The acceptance
matrix and remaining priorities below are the current scope; checkpoint sections
record historical evidence rather than separate work queues.

## Acceptance matrix

| Capability | Required behavior | Current gap / qualification |
| --- | --- | --- |
| File inspector | Distinguish Git, Crab chunks, and Git LFS; show identity, logical/local/cache bytes, availability, pending uploads, and expandable chunk/xorb details. Unknown values remain unknown. | Retained sources and real chunk pages have packaged macOS proof, including pointer replacement during pagination. Local/cache bytes, availability and pending-upload reporting remain open. |
| Bounded reads | Capture a source/revision, read bounded ranges with cancellation and integrity checks, and reject mutation or unavailable content. Cover working tree, index, committed history, and remote snapshots. | Owned native sessions, bounded complete reads and structured errors have focused native/S3-compatible and packaged macOS proof. Packaged local/staged media seeking and close are qualified. Cold 2 GiB SDK LFS ranges and warm proof reuse have dedicated RustFS proof; packaged cold-read progress/interruption and other profiles remain open. |
| Format previews | Parquet schema/sample rows, safetensors headers/tensors, archives, NumPy, notebooks and existing media previews use the captured source. Expensive reads have explicit limits. | Parsers now use captured ranges and respect `rev`; NPZ reads headers. Packaged Crab/LFS/Git audio and VP9 video seeking are qualified through retained ranges. Decoded parser budgets and query paths remain open. |
| Diffs | Correct text/binary/pointer classification across working tree, index, commits, branches and reviews. Actual chunk reuse/change metrics and format comparisons; size delta is not changed bytes. | Captured Crab/LFS and nbformat 4 comparisons have packaged macOS and real RustFS proof. Notebook versions, errors, 40,000 cells, output paging and actual worker cancellation are qualified locally. Ordinary Git capture validation, inline reviews, modes, other formats and remaining profiles are open. |
| Repository operations | Track, stage, commit, push, fetch, pull, clone, hydrate and dehydrate through the UI. Preserve attribute routing and LFS identity; explicit migration and supported lock/verification operations. | Captured single-file Crab/LFS materialization has packaged macOS and real lazy-clone proof. Bulk LFS operations, extensions/profiles and the complete lifecycle remain open. |
| Transfers | Shared operation ownership, real logical/network counters, acknowledged cancellation, supported retry/resume, restart recovery and visible terminal failures. | Single-file materialization has repository-owned native work and global Activity state with actual written bytes. Packaged macOS SSH progress and user cancellation preserve pointers and drain temporary files. Cold-transfer interruption, network counters, durable/restart ownership and component-owned publishing remain open. |
| Storage | Inspect usage/budget, retain offline content, pin active reads and safely evict cache. Never remove dirty working files or referenced staging. | Native usage/cleanup and packaged Settings preview/cleanup are qualified; retained workspace data and subsequent staged previews survive. Budget editing/enforcement, SDK namespace lifecycle and offline retention remain open. |
| Connections | The same supported operations work through local and SSH connections with the selected storage identity. | Packaged macOS arm64 SSH trust, paired deployment, remote open, preview/materialization, independent sessions, reconnect replay, empty-start access and saved-repository restart/retry are qualified. Local/direct-SDK identity replacement, removal and cold bindings have real two-store proof. SDK GUI opening, separate same-URL identities, active/deferred restart and cancellation while storage remains blocked are now qualified. Complete RPC parity, real multi-hop cancellation and other targets/providers remain open. |
| Qualification | Native Crab and LFS byte-identical round trips; multi-GiB bounded previews; small-edit dedup; interruption/cancel/restart; missing/corrupt objects and changed range validators; visible app results. | Focused native, S3-compatible and packaged macOS preview/integrity/cleanup proof passed. Full lifecycle, dedup, interruption/restart and cross-platform/provider evidence remain required. |

## Remaining priorities

Work proceeds in this order. A gap closes only after the corresponding user
action produces the real side effect and an accurate visible result. CI and
provider qualification run alongside each slice, rather than after all features.

| Priority | Deliverable | Consolidation boundary | Closure proof |
| --- | --- | --- | --- |
| P0 | Repository and native recovery | `ProviderRegistry` owns routing; local process and SSH bootstrap owners own readiness; each repository/capture belongs to its live native session. `Agent` owns local/SDK identity replacement; the encrypted vault owns credentials and bindings. | Packaged local/SSH recovery, local/direct-SDK storage replacement, SDK bookmark restore and blocked SDK cancellation have bounded proof below. Remaining: Settings without native startup, queued metadata/UI state recovery and real multi-hop cancellation. |
| P1 | Complete file and repository workflow | Native captured reads serve all views; physical file mutations have one provider owner; attribute routing selects Crab/LFS correctly. | Create/edit/rename/delete, track/stage/commit/push/lazy clone, bulk hydrate/dehydrate, then compare exact bytes and unchanged unrelated index entries. Remote file-editor RPC parity, ordinary Git captures, review/mode diffs and bounded plain-text pointers remain open. |
| P1 | Recoverable transfers and safe storage | Repository-owned operations and verified read/cache owners; no component-owned retry queue or second reconstruction path. | Interrupt a cold transfer, close the view, disconnect/restart, retry from supported state, and prove cleanup/publication outcomes. Durable journals, network counters, budgets, SDK namespace eviction and offline retention remain open. |
| P1 | Truthful inspection and bounded formats | Inspector data comes from captured native/storage metadata; parsers share decoded/query budgets. | Local/cache/available/pending bytes agree with storage; unknown values stay unknown; malformed/oversized inputs fail visibly without blocking the renderer. Other format comparisons and parser/query budgets remain open. |

The first slice removes three duplicated capability definitions, optimistic
capability defaults and unregistered-SSH local handling. The existing native
request owner supplies repository identity for progress events that omit it;
unmatched, mismatched and late operation events never update desktop jobs.
Optional SSH IDE initialization no longer owns core event delivery. Native
filesystem RPC parity and durable recovery are separate deliverables, not
implied by a working read/materialization smoke.

### Next bounded slices

1. Finish recovery parity before extending transfer features. Local/direct-SDK
   storage rebind, profile replacement and removal now revoke captures through
   the repository owner. SDK snapshots now persist logical IDs separately from
   native sessions; startup and deferred selection use `open_url` with the saved
   encrypted binding. Keep Settings usable when native startup itself fails. Use the existing
   session identity to refresh dashboard/cached metadata and notebook reads,
   preserve pending editor saves and restore media playback deliberately.
   Dashboard effects currently depend on repository ID/deferred state, so a
   same-ID retry can retain a previous loading error. Queued reconnect metadata
   still waits for all replay outcomes; release it through the target repository
   owner rather than introducing a component retry queue. Direct SDK opening
   cancellation has real blocked-storage proof; qualify other providers and
   cancellation through actual jump hosts.
   Removal during an admitted open has focused lifecycle proof; live blocked
   storage proof remains required. Local/SSH Git opening, independent
   readiness and read-only preview recapture have the bounded proof below.
2. Complete one real repository round trip for each Crab/LFS profile: physical
   file create/edit/rename/delete, attribute setup, track, stage, commit, push,
   lazy clone, bulk hydrate and dehydrate. Remote physical mutations need one
   working provider path; unsupported native filesystem RPCs must not masquerade
   as complete remote editing. Preserve content identities, unrelated index
   entries, executable modes and review/history source semantics.
3. Deepen existing operation and cache owners: durable terminal state, supported
   retry/resume after cold-transfer interruption, acknowledged cancellation,
   logical versus network counters, active-reader pins, budget enforcement and
   offline retention. Closing a view must not discard ownership or report a
   cancelled transfer before native cleanup finishes.
4. Finish metadata and format bounds on the existing captured-reader path:
   physical/cache/available/pending bytes, decoded Parquet/query limits,
   bounded plain-text pointers and remaining format comparisons. Keep unknown
   measurements unknown and give oversized or malformed content a visible error.

These slices consolidate functionality into File details, Activity and Settings.
They do not require additional primary navigation destinations or a second
reconstruction, credential, retry or cache implementation. Every slice retains
its own byte-identity, failure and packaged-action evidence; provider/profile
and cross-platform CI qualification remains a release gate for the whole matrix.

## Storage identity checkpoint

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
