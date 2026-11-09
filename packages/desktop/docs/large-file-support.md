# Desktop large-file completion

The interface may expose fewer navigation destinations; the capabilities remain
complete. Completion requires a user action, a real native/storage side effect,
and an accurate visible result. Compilation and mocked tests alone do not prove
completion. This record tracks implementation and qualification separately.

## Acceptance matrix

| Capability | Required behavior | Current gap / qualification |
| --- | --- | --- |
| File inspector | Distinguish Git, Crab chunks, and Git LFS; show identity, logical/local/cache bytes, availability, pending uploads, and expandable chunk/xorb details. Unknown values remain unknown. | Retained sources and real chunk pages have packaged macOS proof, including pointer replacement during pagination. Local/cache bytes, availability and pending-upload reporting remain open. |
| Bounded reads | Capture a source/revision, read bounded ranges with cancellation and integrity checks, and reject mutation or unavailable content. Cover working tree, index, committed history, and remote snapshots. | Owned native sessions, bounded complete reads and structured errors have focused native/S3-compatible and packaged macOS proof. Packaged local/staged media seeking and close are qualified. Cold 2 GiB SDK LFS ranges and warm proof reuse have dedicated RustFS proof; packaged cold-read progress/interruption and other profiles remain open. |
| Format previews | Parquet schema/sample rows, safetensors headers/tensors, archives, NumPy, notebooks and existing media previews use the captured source. Expensive reads have explicit limits. | Parsers now use captured ranges and respect `rev`; NPZ reads headers. Packaged Crab/LFS/Git audio and VP9 video seeking are qualified through retained ranges. Decoded parser budgets and query paths remain open. |
| Diffs | Correct text/binary/pointer classification across working tree, index, commits, branches and reviews. Actual chunk reuse/change metrics and format comparisons; size delta is not changed bytes. | Captured comparisons replace size-only reports and error fallbacks; actual ordered chunk metrics, explicit versions, same-size LFS identities and empty Git patches have focused proof. Packaged UI and remote/profile/format comparison qualification are being completed. |
| Repository operations | Track, stage, commit, push, fetch, pull, clone, hydrate and dehydrate through the UI. Preserve attribute routing and LFS identity; explicit migration and supported lock/verification operations. | Captured single-file Crab/LFS materialization has packaged macOS and real lazy-clone proof. Bulk LFS operations, extensions/profiles and the complete lifecycle remain open. |
| Transfers | Shared operation ownership, real logical/network counters, acknowledged cancellation, supported retry/resume, restart recovery and visible terminal failures. | Single-file materialization has repository-owned native work and global Activity state with actual written bytes. Native cancellation/drain is qualified; packaged cancellation, network counters, durable/restart ownership and component-owned publishing remain open. |
| Storage | Inspect usage/budget, retain offline content, pin active reads and safely evict cache. Never remove dirty working files or referenced staging. | Native usage/cleanup and packaged Settings preview/cleanup are qualified; retained workspace data and subsequent staged previews survive. Budget editing/enforcement, SDK namespace lifecycle and offline retention remain open. |
| Connections | The same supported operations work through local and SSH connections with the selected storage identity. | Verify provider routing and remote RPC parity; do not silently change credentials or claim unqualified profiles. |
| Qualification | Native Crab and LFS byte-identical round trips; multi-GiB bounded previews; small-edit dedup; interruption/cancel/restart; missing/corrupt objects and changed range validators; visible app results. | Focused native, S3-compatible and packaged macOS preview/integrity/cleanup proof passed. Full lifecycle, dedup, interruption/restart and cross-platform/provider evidence remain required. |

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
- Packaged comparison screenshots currently cover chunk metrics, more than fifty
  changed ranges, same-size LFS and initial commits. The full GUI rerun and final
  matching package are still pending. Ordinary Git patches still read the live
  index/worktree through Git, so concurrent mutation needs capture validation.
  Notebook, inline staging/review, mode-only large-file changes, format comparisons
  and remaining provider/profile surfaces still require implementation or
  qualification; this does not close the matrix.
