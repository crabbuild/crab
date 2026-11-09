# Desktop large-file completion

The interface may expose fewer navigation destinations; the capabilities remain
complete. Completion requires a user action, a real native/storage side effect,
and an accurate visible result. Compilation and mocked tests alone do not prove
completion. This record tracks implementation and qualification separately.

## Acceptance matrix

| Capability | Required behavior | Current gap / qualification |
| --- | --- | --- |
| File inspector | Distinguish Git, Crab chunks, and Git LFS; show identity, logical/local/cache bytes, availability, pending uploads, and expandable chunk/xorb details. Unknown values remain unknown. | `repository.rs::pointer_info` currently returns no chunk/xorb metadata. |
| Bounded reads | Capture a source/revision, read bounded ranges with cancellation and integrity checks, and reject mutation or unavailable content. Cover working tree, index, committed history, and remote snapshots. | Implemented captured sources and a native pointer bridge; operation cancellation, selected-connection scope and real native/cloud qualification remain open. |
| Format previews | Parquet schema/sample rows, safetensors headers/tensors, archives, NumPy, notebooks and existing media previews use the captured source. Expensive reads have explicit limits. | Parsers now use captured ranges and respect `rev`; NPZ reads headers. Parser and desktop qualification are in progress; media and query paths still need audit. |
| Diffs | Correct text/binary/pointer classification across working tree, index, commits, branches and reviews. Actual chunk reuse/change metrics and format comparisons; size delta is not changed bytes. | Desktop rejects the CLI JSON envelope, manufactures size-only chunk reports, and treats empty Git patches as large files. |
| Repository operations | Track, stage, commit, push, fetch, pull, clone, hydrate and dehydrate through the UI. Preserve attribute routing and LFS identity; explicit migration and supported lock/verification operations. | Existing workflows need qualification together, including uncommitted staged content and failures. |
| Transfers | Shared operation ownership, real logical/network counters, acknowledged cancellation, supported retry/resume, restart recovery and visible terminal failures. | Publish state is component-owned and cancellation is optimistic. |
| Storage | Inspect usage/budget, retain offline content, pin active reads and safely evict cache. Never remove dirty working files or referenced staging. | Cache controls and active-reader ownership need end-to-end qualification. |
| Connections | The same supported operations work through local and SSH connections with the selected storage identity. | Verify provider routing and remote RPC parity; do not silently change credentials or claim unqualified profiles. |
| Qualification | Native Crab and LFS byte-identical round trips; multi-GiB bounded previews; small-edit dedup; interruption/cancel/restart; missing/corrupt objects and changed range validators; visible app results. | Not yet run for this change. Cross-platform/provider claims require their corresponding dedicated proof. |

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

- The native pointer bridge currently starts one bounded request per process.
  Retain a native read session for the full preview lifetime before claiming
  efficient repeated LFS reads or active-content pinning. Reuse its verified LFS
  session, staged pin, captured configuration/catalog and cancellation owner.
- Existing whole-file `read_text` / `read_bytes` still validate against index
  metadata and use a separate read path. Move these onto the captured source;
  a grown working file must not bypass its response limit, and complete reads
  must preserve whole-file hash verification.
- Media still uses complete base64 responses (video up to 200 MiB). Introduce
  range-backed media delivery so large playback/seeking stays bounded.
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

### Next reader lifecycle change

The one-shot bridge is an implementation checkpoint, not the final lifecycle.
Replace it with one private native reader session per captured pointer source.
The initial handshake captures configuration and reports its remote before any
network access; the desktop validates the selected connection against that
target. Keep the staged pin, verified LFS session and captured remote runtime
until explicit close. This removes repeated LFS verification and configuration
recapture between requests.

Own native session actors in a per-browse-session runtime with a cancellation
token and task tracker. Dropped requests signal cancellation and actors drain
native cleanup; `BrowseSession::close` must await them before Tokio shuts down.
The native protocol needs range, cancel and close acknowledgements. A cancelled
range must finish its reconstruction/verification workers before acknowledging
cancellation; an abandoned source must release its staging pin and close its
captured lookup. Preview handlers must await close so cleanup failures remain visible.
Remove the one-shot/inspect/expected-remote path in the same change; it is an
unreleased private protocol, so no compatibility alias is needed.
