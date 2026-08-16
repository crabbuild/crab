# NFS Mount Architecture

## Status

This document describes the long-term target design for Crab's NFS mount
backend. It is intentionally more forward-looking than
`crab/docs/architecture/virtual-filesystem.md`, which documents the current VFS
implementation.

The current NFS adapter already exposes Crab's resolver, hydration service, and
overlay engine through a loopback NFSv3 server. It now also uses engine-owned
read leases, a bounded read-lease pool, and an NFS write journal for unstable
write/commit behavior. Native NFS preflight, control-endpoint bind probing,
`auto` backend selection, and backend/log/control metadata in the mount
registry are also part of the current baseline. `crab mount doctor --backend=nfs`
exposes the same preflight contract before users attempt a native mount. On
Unix hosts, the NFS helper exposes a local control socket for live
status/runtime stats, refresh, ref switch, and graceful shutdown. Runtime status
now includes NFS protocol read and
`READDIRPLUS` counters, shared VFS read/source/adaptive counters, and hydration
window/cache/prefetch counters so adaptive prefetch work can be measured
against real OS client pressure. `READDIRPLUS` status now separates returned
entries from materialized candidates, attr resolutions, cookie resumes/misses,
cookie-skipped entries, large-directory sightings, prefetch errors, and bounded
directory-page cache counters. The VFS engine now also owns a bounded
read-source cache with hit/miss, eviction, stale-eviction, invalidation, and
memory counters. The target design keeps that shape: NFS remains a thin protocol
adapter, while byte-source identity and correctness stay in the VFS engine so
NFS and FUSE share one model.
The latest backend-alignment finding is that `VfsReadLease` is now the shared
read token, not an NFS-only cache object. NFS retains leases in a bounded
file-id pool because NFSv3 is stateless; FUSE retains a lease on each
read-capable kernel file handle because FUSE has real open/release lifecycle
events. Both adapters still delegate byte validity, stale retry, and hydration
to `VfsEngine`.
The synthetic engine benchmark now covers pointer reads and overlay-modified
rereads through both path-level reads and reused VFS read leases. Its retained
report can be verified on its own or compared against a baseline retained
report to catch throughput or lease/path-ratio regressions across commits.
Native NFS smoke tests remain the required OS-client proof, and each native
smoke now leaves a common `nfs-smoke-report.json` plus a native sequential-read
benchmark artifact with NFS protocol, VFS source-cache/adaptive, and hydration
before/after/delta counters that can be retained, verified, and compared
against a retained baseline. The artifact also stores derived efficiency ratios
so CI and release reviews can trend NFS client read amplification without
re-parsing raw counters.

The key architecture finding from the hf-mount comparison is that Crab has
different state problems that should not be merged. NFSv3 needs a protocol-side
read bridge because reads arrive without open/close handles. NFS `READDIRPLUS`
also benefits from protocol-side candidate caching because cookie-resumed pages
otherwise repeat resolver and id-table work. Crab separately needs an
engine-side byte-source cache because resolver, overlay, and hydration source
selection are expensive and must be shared by NFS and FUSE. The best design is
therefore ownership-specific state: `ReadLeasePool` for NFS file-id reuse,
FUSE file-handle leases for stateful open/release reuse, NFS-owned
directory-page versions for `READDIRPLUS`, and the `VfsEngine` read-source
cache for content-identity reuse.
The latest compile-proof finding adds one more boundary: NFS and FUSE must stay
independently buildable adapters over the shared VFS engine. NFS-only builds
must not import FUSE IPC/coordinator types, because the preferred backend has to
ship and test without optional FUSE dependencies.
The latest writeback finding is the same ownership rule applied to dirty data:
NFS should track protocol stability, not own writable file handles. `COMMIT`
and graceful shutdown drain `NfsWriteJournal` through
`VfsEngine::sync_overlay_path`, so successful paths are cleared only after the
engine syncs backing content and checkpoints overlay metadata, while failed
paths remain pending with NFS status diagnostics and do not prevent other
pending paths from draining.
The latest native-smoke finding makes control endpoints part of the architecture
rather than a helper detail: Unix sockets must live under a short per-user
runtime path so deep workspaces and retained smoke artifact roots do not exceed
macOS socket path limits. Retained native smoke reports also carry the exact
Git commit and must be re-verifiable after the mount exits, because preferred
backend status depends on reproducible OS-client evidence, not terminal logs.
The latest retained-evidence finding is that proof has tiers. A local or PR
run can prove the implementation shape and produce advisory thresholds, but it
must not become release policy by itself. Current local evidence has proven the
exact-commit smoke path on macOS and Linux plus the synthetic read-path
benchmark, while native Windows Client for NFS proof remains mandatory for
release-grade default-backend confidence. The benchmark signal is also
workload-specific: sequential pointer reads are the main lease/source-cache win,
while random and overlay-reread scenarios are regression guards rather than a
blanket claim that every leased read is faster. That is why threshold
suggestions stay reviewed, platform-aware, and opt-in until multiple retained
attempts exist.
The latest control-evidence finding is that local control authority is not
evidence. The live registry and control client may retain the full endpoint
needed to operate a helper, but every user-facing or retained JSON artifact must
redact TCP bearer tokens, and the retained-report verifier must reject raw tokens
for each artifact that can carry a control endpoint. This makes native smoke
artifacts useful for release review without turning uploaded logs into a control
secret transport.
The latest mutation-state finding is that NFS protocol state must follow the
engine mutation, not lead it. `remove` and `rename` first change the canonical
overlay/resolver state through `VfsEngine`, then update NFS directory pages,
stable ids, read-lease entries, and write-journal paths as derived protocol
state. That ordering makes rollback unnecessary for ordinary protocol caches:
if the engine rejects the mutation, the NFS adapter has not moved ids or
journal entries; if a derived protocol update fails afterward, the failure is
visible and the next refresh/control operation can rebuild from the canonical
VFS state.

## Goals

- Make NFS the preferred mount backend on macOS, Linux, and Windows when the OS
  client is available.
- Preserve Crab's existing VFS semantics: snapshot resolution, overlay
  precedence, copy-on-write writes, explicit publish, and byte-identical
  hydration.
- Reduce per-read overhead for NFS readahead and random-access workloads without
  bypassing the canonical resolver/overlay/hydration pipeline.
- Keep backend-specific code small enough that a developer can reason about
  NFS, FUSE, and mount lifecycle behavior independently.
- Provide actionable platform diagnostics instead of exposing native mount
  command failures directly.
- Make the optimized path observable enough that performance tuning decisions
  are backed by resolver, hydration, lease, and platform metrics.

## Non-Goals

- Do not vendor or transplant hf-mount's `VirtualFs` handle model. Crab's VFS
  source of truth is the resolver plus overlay plus hydration engine, not an
  inode-owned open-file table.
- Do not introduce a second NFS-only read/write engine. Optimizations must live
  at the VFS engine interface or below it.
- Do not add remote NLM support. NFS locks remain local/client-side because Crab
  serves a loopback export for one local mount process.
- Do not silently make Linux rootless NFS promises. Linux native NFS mounting is
  still governed by the kernel mount syscall and host policy.

## Current Baseline

Current NFS request flow:

1. `crates/crab-vfs/src/nfs.rs` maps stable NFS file ids to paths.
2. Reads pin a cached `VfsReadLease` from `ReadLeasePool` or open one through
   `VfsEngine::open_read`.
3. Reads delegate to `VfsEngine::read_at`, which validates source identity
   before serving bytes.
4. Stale leases are evicted and retried once through the canonical engine path.
5. Base pointer reads call `HydrationService::read_range`.
6. Overlay reads use the local overlay backing file.
7. Writes remain path-based engine mutations and are tracked by
   `NfsWriteJournal` until stable sync, `COMMIT`, or shutdown drain.
8. Background NFS mounts register backend, log, and control metadata so
   `mount list`, `mount status`, `mount refresh`, `mount switch`, and
   `unmount` can identify and operate on the selected backend.

Important current source paths:

| Surface | Current file |
|---------|--------------|
| NFS adapter | `crates/crab-vfs/src/nfs.rs` |
| NFS helper control | `crates/crab-vfs/src/nfs_control.rs` |
| Native NFS lifecycle | `crates/crab-vfs/src/nfs_mount.rs` |
| FUSE adapter | `crates/crab-vfs/src/fuse.rs` |
| Shared VFS engine | `crates/crab-vfs/src/engine.rs` |
| Shared live refresh/switch runtime | `crates/crab-vfs/src/mount_runtime.rs` |
| Shared live mount control | `crates/crab-vfs/src/mount_control.rs` |
| Read lease pool | `crates/crab-vfs/src/read_lease_pool.rs` |
| Resolver | `crates/crab-vfs/src/resolver.rs` |
| Hydration and read windows | `crates/crab-vfs/src/hydration.rs` |
| Overlay write layer | `crates/crab-vfs/src/overlay.rs` |
| Mount registry | `crates/crab-vfs/src/mounts_registry.rs` |
| Backend selection and status UX | `crab/src/cmd/mount.rs` |

This baseline is correct and already avoids the worst stateless-NFS read
overhead. The engine now also records per-lease read-pattern classification and
runtime counters for source reads, stale lease rejections, hydration window
cache pressure, in-flight waits, prefetch decisions, and remote bytes fetched.
Sequential pointer reads now schedule the next hydration read-through window
through the shared hydration cache. Confirmed positive-stride reads schedule one
target-window lookahead instead of assuming a full scan. Duplicate speculative
requests for the same window are suppressed until a prefetch failure permits
retry. Random and repeated reads do not prefetch. The remaining target work is
to run threshold suggestions across multiple retained benchmark/native-smoke
runs, promote platform-specific baseline run ids and threshold budgets only
after the retained evidence is stable, add streaming directory pagination only
if retained pressure evidence justifies it, and collect first green retained
native smoke workflow runs on the release commit.

## Source Review Findings

The long-term design is based on these current code facts:

- `crates/crab-vfs/src/nfs.rs` already treats NFS as a protocol adapter: it maps ids to
  paths, resolves attrs, delegates reads/writes to the engine, uses
  `ReadLeasePool` for stateless read reuse, and tracks pending stable-write
  state in `NfsWriteJournal`. It also owns protocol-level read and
  `READDIRPLUS` counters because those describe NFS client behavior rather than
  byte-source correctness. The adapter now avoids attr resolution and directory
  prefetch scheduling for entries skipped by a cookie-resumed `READDIRPLUS`,
  and caches generation plus directory-versioned candidate pages so cookie
  resumes do not repeat resolver listing and NFS id-table work. Directory-page
  versions are owned by the NFS adapter: ordinary writes invalidate exact parent
  pages or affected subtrees, while refresh/ref-switch generation changes clear
  the cache deliberately.
- `crates/crab-vfs/src/nfs.rs` also shows the right mutation ordering for protocol
  state. `remove` and `rename` call the engine first, invalidate affected
  directory pages, checkpoint overlay metadata, then remove or move ids, evict
  affected read leases, and remove or rename write-journal paths. That is the
  correct split: engine mutations decide namespace and bytes; NFS tables remain
  fast, bounded, rebuildable views for the native client.
- NFS id-table and exclusive-create verifier transformations are staged before
  they are committed to the live table. If the verifier store cannot be
  persisted, the in-memory id table and verifier map remain unchanged, so the
  caller cannot observe a half-moved protocol namespace or a half-recorded
  exclusive create.
- Ordinary NFS `CREATE` uses the same stable-write path as metadata updates:
  `setattr_path` marks and syncs the new file once, so the handler does not
  issue a second overlay sync before returning attrs.
- NFS metadata updates are fail-closed around the attributes Crab can actually
  persist and report. The adapter accepts supported `mtime` changes through the
  engine and write journal, but rejects explicit `atime` changes before any
  overlay or journal mutation because the current resolver reports one
  second-precision timestamp for `atime`, `mtime`, and `ctime`, and the overlay
  does not persist a separate access time. FSINFO therefore reports whole-second
  timestamp precision and does not advertise full NFS set-time capability.
- Child namespace operations validate that the parent file handle still names a
  directory before constructing a child path. That keeps NFS `NOTDIR` behavior
  aligned with native clients and prevents file-backed handles from creating
  synthetic `file/child` overlay paths or recording exclusive-create verifiers.
- NFS filename components and mount-created symlink targets are validated at
  the adapter boundary before resolver lookup or overlay mutation. Empty names,
  empty symlink targets, slash separators in components, NUL bytes, non-UTF-8
  names or targets, components above Crab's portable 255-byte limit, and
  symlink targets above the NFSv3 1024-byte path limit return protocol-specific
  NFS statuses instead of falling into host filesystem IO errors or
  platform-specific overlay behavior.
- Synthetic protocol files are handled from their advertised NFS identity before
  resolver fallback. The root `.git` file reports as a regular read-only file,
  rejects directory enumeration with `NOTDIR`, rejects readlink as an invalid
  object type, never schedules resolver-backed directory prefetch, and remains
  a harmless `COMMIT` no-op. Directory handles, including root, still reject
  `COMMIT` with `INVAL` because they can never carry NFS writeback state.
- `crates/crab-vfs/src/fuse.rs` already uses the shared read-lease interface from its
  open/read path. FUSE opens a `VfsReadLease` for read-capable file handles,
  reuses it across reads, replaces it after a stale retry, clears it after a
  successful write on that handle, and drops it on release. FUSE has a real
  open/release lifecycle, so it should keep this per-handle retention model
  instead of copying NFS's stateless lease pool.
- `crates/crab-vfs/src/engine.rs` already owns the canonical byte-source decision:
  overlay backing file first, then base pointer hydration, then base Git blob.
- `crates/crab-vfs/src/hydration.rs` already has the right remote-read primitive: an
  8 MiB read-through window cache keyed by file content and window range.
- `crates/crab-vfs/src/engine.rs` validates read leases with generation and overlay
  view versions. It also routes path, subtree, rename, snapshot-generation, and
  overlay-reset invalidations through the first-class `VfsInvalidation` event
  vocabulary before touching the read-source cache. That keeps fail-closed lease
  validation and cache invalidation on the same engine-owned boundary.
- `crates/crab-vfs/src/engine.rs` now also owns VFS read metrics, a bounded
  read-source cache, and per-lease adaptive read classification. Sequential
  reads schedule the next hydration window, confirmed positive-stride reads
  schedule one target window, and repeated/random reads stay unspeculative. That
  confirms the hf-mount lesson is being implemented at Crab's byte-source
  boundary rather than in the NFS adapter.
- `crates/crab-vfs/src/hydration.rs` now exposes read-window/cache/in-flight/prefetch
  and remote byte counters. It also suppresses duplicate speculative prefetches
  for the same read window, with retry allowed after a failed prefetch. The
  same bounded claim/dedup path handles both sequential next-window prefetches
  and strided target-window prefetches. That is the right layer for object-store
  amplification evidence because NFS and FUSE both use the same hydration
  primitive.
- `crates/crab-vfs/src/mount_runtime.rs` is now the shared live refresh/ref-switch
  implementation used by the FUSE coordinator and the NFS control server.
  Engine-level source caches are cleared after generation changes published by
  this runtime, and the NFS control path also clears protocol read leases plus
  directory pages at the same boundary so native clients repopulate hot entries
  without a stale-lease retry storm.
- `crates/crab-vfs/src/mount_control.rs` now owns backend-agnostic mount discovery and
  live control routing. `list` starts from the durable registry and annotates
  entries with NFS helper status or FUSE coordinator data when available, while
  `status`, `refresh`, `switch`, and shutdown route to the selected backend.
  The CLI still assembles the final status payload because it merges live helper
  data with persisted cache/overlay state. The registry and live control client
  keep full TCP control tokens for local operation, but public `mount list` and
  `mount status` payloads redact those tokens at the CLI boundary.
- `crates/crab-vfs/src/nfs_mount.rs` owns native mount lifecycle and platform command
  quirks; that makes it the right home for NFS preflight and doctor output, not
  for byte-source caching. Its preflight now also asks `nfs_control` to prove
  the per-mount control endpoint is bindable before NFS is selected, and its
  shutdown path logs native-unmount, write-journal drain, and total shutdown
  latency.
- `crab/src/cmd/mount.rs` owns the product-level backend decision. The important
  finding is that `auto` fallback must be driven by explicit preflight blockers,
  not by catching arbitrary native mount failures after state has been created.
  Explicit `--backend=nfs` also enforces that preflight before the pipeline is
  built, while `--backend=fuse` stays independent of NFS checks. `crab mount
  doctor` uses the same preflight report so readiness checks and mount selection
  do not drift. JSON doctor output now also includes a machine-readable
  `nfs_preflight` aggregate with platform gate booleans, blocker/warning counts,
  first next action, and the structured blocker/warning lists. The code review
  narrowed the `auto` contract further: missing native NFS tooling, unsupported
  NFS platforms, and Linux privilege blockers may fall back to FUSE when FUSE is
  compiled and ready; mountpoint conflicts, invalid NFS mountpoints, loopback or
  control-endpoint failures, helper identity failures, and post-startup mount
  failures remain visible NFS failures because falling back would hide broken
  local state.
- `crates/crab-vfs/src/mounts_registry.rs` is the durable bridge between background
  helpers and CLI UX. Backend, log path, and control endpoint metadata
  belong there so `mount list`, `mount status`, `unmount`, and doctor commands
  do not need backend-specific discovery rules.
- `crates/crab-vfs/src/nfs_control.rs` now gives NFS helpers a local-only status,
  refresh, switch, and shutdown channel. Unix uses a user-private Unix socket;
  platforms without Unix sockets use an authenticated loopback TCP endpoint.
  The same module owns the preflight bind probe for those endpoints, so helper
  startup and doctor/backend-selection checks share one control-channel
  contract.
  Unix endpoint generation deliberately uses a short per-user path under
  `/tmp/crab-nfs-<uid>/control` rather than `$HOME`, because macOS rejects long
  Unix-domain socket paths and retained smoke artifacts often live under deep
  temporary directories. Preflight and helper startup fail closed if Crab cannot
  make that directory user-private. Preflight replaces stale generated sockets
  or empty placeholders, then completes a bind/remove probe before NFS is
  selected. The Unix control server removes its socket on graceful shutdown so
  later preflight does not inherit stale helper state from prior mounts.
  Status includes helper lifecycle timing for server bind, native mount, and
  total startup so operators can distinguish slow native mount commands from
  Crab read-path pressure. This validates the backend-agnostic control-plane
  direction while keeping native retained-smoke runs as the release evidence
  that proves the control path on every platform.
- `crab/benches/nfs_read_path_bench.rs` is the current release-mode proof
  surface for the shared VFS read path. It exercises pointer-backed
  path reads, reused pointer leases, random pointer reads, and rereads of
  overlay-modified files after copy-on-write promotion. That keeps the
  performance loop focused on the engine source cache and lease interface
  before involving native OS NFS client variability. `make
  nfs-read-path-bench-report` now wraps that bench in retained JSON evidence
  with scenario validation and lease/path throughput ratios, while `make
  nfs-read-path-bench-report-compare` compares retained baseline/current reports
  with optional throughput and ratio regression thresholds.
- `crab/scripts/verify-nfs-smoke-report.py` is the retained-evidence gate for
  native NFS smokes. The macOS, Linux, and Windows smoke scripts now capture
  live `mount status --json` runtime counters and native sequential-read
  benchmark artifacts with protocol, VFS, and hydration deltas, then write the
  same report shape so release evidence can prove helper layout, mount control,
  retained control-status/control-shutdown artifacts, remount behavior, NFS
  helper lifecycle timing, write-sync latency, and OS-client read behavior
  across platforms. Each platform script also verifies its own report with
  required artifacts before returning success.
  The verifier can also revalidate a downloaded artifact directory by resolving
  runner-local artifact paths to files retained beside each `nfs-smoke-report.json`,
  which makes the evidence useful after the native smoke job exits. It also
  compares retained baseline/current reports for the same native workload, with
  optional thresholds for throughput and requested-byte, returned-byte, and
  RPC-density regressions. Mount-status artifact verification also requires VFS
  read/source-cache/adaptive counters and hydration read-window/chunk counters,
  so retained native smoke evidence proves the observability surface used for
  source-cache and prefetch tuning. The verifier recursively rejects raw TCP
  control tokens in retained JSON payloads, including writeback and shutdown
  artifacts that are loaded outside the generic artifact helper. The release
  archive contract pins those artifact-specific redaction checks so future
  verifier refactors cannot silently weaken retained-evidence token handling,
  and `make nfs-feature-gate` runs that contract before the NFS compile/test
  proof.
- `.github/workflows/nfs-mount.yml` is the native NFS evidence workflow. Pull
  requests run the NFS feature gate and collect the retained synthetic
  read-path benchmark report; main and manual runs also execute Linux, macOS,
  and Windows native smoke scripts, upload each platform run root, download the
  retained artifacts, and run `make nfs-smoke-report-verify-dir` to prove the
  retained set has all three suites, the current workflow run-attempt suffix,
  the exact workflow commit, and the required JSON artifacts. The workflow also
  renders the retained benchmark and native-smoke JSON summaries into the GitHub
  step summary, then emits retained threshold suggestion
  env/JSON artifacts so calibration runs can be reviewed without opening raw
  smoke artifacts first. Its path filters include the release-evidence dispatch
  helper, so helper/DX changes still run the NFS feature gate that exercises the
  dispatch self-test.
- `crab/scripts/nfs-evidence-summary.py` now also owns threshold suggestion
  rendering. It can read a retained synthetic benchmark report plus a retained
  native-smoke summary and emit conservative `NFS_READ_PATH_BENCH_*` and
  `NFS_SMOKE_*` verify/compare argument strings. This is deliberately a
  suggestion tool, not an automatic policy writer: maintainers review several
  retained runs before promoting the generated arguments to workflow variables.
  The suggestion payload also classifies the retained evidence tier, marks
  release-grade and calibration readiness separately, and lists missing native
  suites/platforms, unverified summaries, mismatched or missing git commits,
  inconsistent summary headers, dirty or malformed benchmark reports, and
  insufficient retained-run depth as blockers. Release-grade suite/platform
  coverage is derived from the retained report rows, not trusted from the
  summary header alone, and benchmark promotion requires numeric record fields,
  matching summary totals, recomputed lease/path ratios, and the complete
  expected scenario set rather than ratio fields alone. That keeps a local
  macOS/Linux or single-run suggestion
  visibly advisory instead of silently looking like release policy. Strict
  calibration/release workflows can also require those tier booleans so missing
  Windows evidence, shallow retained history, non-exact-commit evidence,
  benchmark/native commit mismatch, dirty or partial benchmark evidence, forged
  ratio summaries, or forged summary coverage fails before threshold arguments
  are promoted.
  The hosted NFS evidence workflow now runs the same suggestion path after it
  has both the synthetic benchmark report and retained native-smoke summary.
- `.github/workflows/release.yml` now treats retained NFS evidence as a
  release gate. Before packaging, it verifies that a provided NFS Mount Evidence
  run completed successfully on the exact release commit, downloads the retained
  native smoke artifacts from that run attempt, and runs the shared retained
  smoke verifier with Linux/macOS/Windows coverage and exact run-suffix binding.
- `make nfs-release-evidence-ci` dispatches the NFS Mount Evidence workflow for
  the release ref and forwards the same optional benchmark/native-smoke
  calibration arguments used by manual workflow dispatch. It can also wait for
  the exact matching workflow-dispatch run and print the
  `NFS_RELEASE_EVIDENCE_RUN_ID` plus run-attempt suffix needed by the release
  gate. When `NFS_RELEASE_EVIDENCE_OUTPUT` is set, wait mode also writes those
  values as shell-safe assignments in a sourceable env file. This keeps
  evidence generation and release submission on one documented CLI path.
- `crab/tests/prop_coordinator.rs` keeps FUSE IPC response properties behind the
  FUSE feature while leaving backend-neutral coordinator properties available to
  NFS-only builds. That is the correct test shape for a preferred NFS backend:
  shared VFS behavior is proved without FUSE, and FUSE-specific IPC contracts
  still run when FUSE is compiled in.

The design conclusion is that NFS should gain state only at the protocol edge:
stable file ids, NFS statuses, NFS write stability, and a small lease pool that
bridges NFSv3's missing open/close lifecycle. All byte-source truth belongs in
the VFS engine.

## Key Enhancement Decisions

The best NFS enhancement is not one feature. It is a set of ownership decisions
that let Crab behave like a normal local filesystem while keeping lazy Git/xorb
hydration correct. The code review makes these the key design choices:

| Enhancement | Accepted design | Reject | Why this is the best long-term shape |
|-------------|-----------------|--------|--------------------------------------|
| Preferred backend UX | NFS is preferred only after preflight proves the native client, helper, loopback server, control endpoint, and mountpoint contracts before mount pipeline state is created | best-effort fallback after a partially-created NFS mount | users get actionable macOS/Linux/Windows failures before state is created, and `auto` remains predictable |
| Stateless read bridge | NFS owns a bounded `ReadLeasePool` keyed by stable file id | path-resolve every NFS read, or cache direct file descriptors in `nfs.rs` | NFS absorbs OS readahead bursts without taking ownership of byte-source validity |
| Shared byte-source reuse | `VfsEngine` owns read-source caching, source-key validation, and adaptive read classification | NFS-only source cache, direct pointer/xorb reads from the adapter | FUSE, NFS, hydrate, and future SDK readers keep one correctness model |
| Deduped large-file reads | `HydrationService` remains the only xorb/chunk reconstruction and read-window owner | protocol-specific object-store fetch paths | on-demand hydration stays byte-identical and dedupe-aware across all read surfaces |
| Writes | NFS writes mutate paths through `VfsEngine`; `NfsWriteJournal` tracks stability only | pooled writable handles, read-to-write handle upgrade, NFS-owned dirty buffers | copy-on-write promotion, truncate, rename, remove, sync, and publish stay in one write pipeline |
| Mutation state ordering | engine mutation and overlay checkpoint complete before NFS ids, read leases, directory pages, and write-journal paths are moved or cleared | updating protocol tables before canonical state changes, or trying to roll back engine writes from NFS | NFS state stays a derived view; failed engine mutations leave protocol caches untouched |
| Directory pressure | NFS owns bounded `READDIRPLUS` candidate pages and directory-page versions | full snapshot directory cache in NFS, or immediate streaming pagination | cookie resumes stop repeating resolver/id work while deeper directory streaming remains evidence-gated |
| Cross-platform control | launcher-generated helper control endpoint is stored in the mount registry and used by shared mount control | recomputing endpoints, backend-specific CLI discovery | `list`, `status`, `refresh`, `switch`, and `unmount` work uniformly for background NFS and FUSE mounts |
| Unix control endpoints | Unix helper control sockets live under a short per-user runtime directory | socket paths under `HOME`, mountpoint-derived deep paths, or recomputed paths | macOS path limits are tight, and retained smoke/user workspaces can be deeply nested |
| Control secrecy | Unix control directories are made user-private before probing or serving; TCP endpoints keep fresh per-launch tokens and public JSON redacts them | best-effort chmod, raw token retention in evidence artifacts, or unauthenticated loopback control | helper status, refresh, switch, and shutdown are local control operations, so failure to protect the channel must fail before NFS is selected |
| Workload-specific performance | tune sequential, random, overlay, directory, and native-client thresholds independently | one global "NFS is faster" threshold | lease reuse mainly targets stateless readahead; random reads and overlay rereads protect against overfetch and stale-source regressions |
| Evidence tiers | use advisory local/PR evidence, reviewed calibration evidence, and strict release evidence as separate gates | promoting one local run or a two-platform set to release policy | NFS becomes the preferred backend only when exact-commit macOS, Linux, and Windows native evidence agrees with the portable VFS proof |
| Derived evidence integrity | treat benchmark ratios as derived from retained per-scenario records and revalidate them before promotion | trusting copied summary ratios or summary headers as policy inputs | threshold suggestions stay auditable, and forged or stale summaries cannot become release budgets |
| Performance policy | retained engine benchmarks plus retained native smokes produce reviewed thresholds | self-mutating performance policy from one noisy run | performance gates become empirical, platform-specific, and reproducible |
| Native proof | every retained native smoke report is verified against the exact commit and retained artifacts | trusting stdout, stale artifacts, or same-host assumptions | macOS, Linux, and Windows clients have different mount behavior, so release confidence must be reproducible |

These decisions make NFS performance an extension of Crab's existing VFS model,
not a fork of it. The user-visible result should be simple: mount a repository,
read huge deduplicated files lazily, write to the overlay, refresh or switch
refs, and unmount cleanly. The implementation stays layered:

1. NFS translates native client behavior into stable ids, statuses, read-lease
   pins, directory pages, and write-stability records.
2. `VfsEngine` resolves paths, opens read sources, validates leases, applies
   overlay mutations, and publishes invalidation events.
3. `HydrationService` reconstructs content-addressed xorb bytes through shared
   read windows and chunk cache.
4. mount control and release evidence prove the same architecture works on
   macOS, Linux, and Windows.

The implementation rule for future NFS work is therefore:

- If the state explains NFS protocol behavior, keep it in the NFS adapter.
- If the state proves which bytes are correct, keep it in `VfsEngine` or below.
- If the state fetches remote deduplicated bytes, keep it in `HydrationService`.
- If the state affects user operations across backends, keep it in the shared
  mount registry/control modules.
- If the state changes release confidence, make it retained evidence before
  turning it into policy.

### Best-Decision Operating Model

The best architecture decision is to make Crab's NFS backend a native-client
adapter over the existing Crab VFS, not a second filesystem. That yields a
simple operating model:

1. NFS owns protocol identity and pressure relief: stable ids, status mapping,
   `READDIRPLUS` candidate pages, operation pins, write stability, and the
   file-id keyed `ReadLeasePool`.
2. `VfsEngine` owns semantic truth: resolver generation, overlay precedence,
   source selection, read-source cache, stale-source rejection, and mutation
   invalidation.
3. `HydrationService` owns remote-byte truth: xorb/chunk reconstruction,
   read-through windows, in-flight suppression, and bounded prefetch.
4. Mount control owns user operations: preflight, helper launch, registry
   metadata, live status, refresh, switch, shutdown, and stale cleanup.
5. Release evidence owns default-backend confidence: exact-commit retained
   Linux, macOS, and Windows proof gates before policy changes.

The resulting decision matrix is:

| Area | Accepted now | Evidence-gated next | Rejected |
|------|--------------|---------------------|----------|
| hf-mount lesson | Extract LRU, pins, temporary overflow, stale retry, and bounded counters into `ReadLeasePool` | tune pool limits from retained native lease density and overflow evidence | transplant hf-mount's inode-owned `VirtualFs` handle pool |
| Read cache unit | pool `VfsReadLease` by stable NFS file id | engine-owned local fd reuse for overlay/blob sources if open/seek cost remains material | NFS-owned direct fd/source cache |
| Source validity | validate source keys in `VfsEngine` before every leased read | finer source-cache budgets per source class if retained reports show pressure | path-only validity or NFS-id validity as a byte proof |
| Deduped hydration | keep xorb/chunk bytes behind shared hydration windows | adjust sequential/stride window budgets from remote-byte amplification evidence | NFS-specific object-store reads or pointer decoding |
| Writes | mutate paths through `VfsEngine`; use `NfsWriteJournal` only for NFS stability | richer status for dirty age, sync latency, and publish readiness | pooled writable handles or read-to-write handle upgrades in NFS |
| Directory scaling | bounded `READDIRPLUS` candidate pages keyed by generation and directory version | streaming directory pagination if native pressure remains high | full snapshot directory cache in the adapter |
| Preferred backend | choose NFS only after native-client, helper, loopback, control, and mountpoint preflight | stricter platform budgets after several retained native runs | fallback after partially-created NFS state |
| Control plane | registry-first, live-annotated, backend-agnostic mount control with fail-closed private Unix control paths, authenticated TCP endpoints, and `--live-only` for strict health proof | richer helper diagnostics as retained smoke artifacts require them | backend-specific status discovery, recomputed endpoints, or best-effort local control secrecy |
| Release policy | advisory, calibration, and release evidence tiers | promote reviewed threshold args after retained history exists | self-mutating thresholds from one benchmark run |

This is the long-term developer UX rule: add a cache or lifetime only where its
owner can invalidate it. New NFS work should answer four questions before
implementation: what identity does this state prove, who invalidates it, what
observable counter proves it is useful, and which retained test or native smoke
can fail if it regresses. If the answer crosses ownership boundaries, the code
belongs one layer lower.

### Performance Ladder

The architecture should optimize in this order, because each layer removes
work without weakening the layer below it:

| Order | Optimize | Owner | Promotion gate |
|-------|----------|-------|----------------|
| 1 | NFS file-id lease reuse, pinning, stale retry, memory budget | `ReadLeasePool` and `nfs.rs` | native smoke reports show healthy hit density, bounded overflows, and no stale retry spikes |
| 2 | source selection reuse and adaptive read classification | `VfsEngine` | read-path benchmark reports show lease/path ratio gains and native reports show resolver avoidance |
| 3 | hydration window and prefetch tuning | `HydrationService` | native read artifacts show lower remote-byte amplification without random-read overfetch |
| 4 | local overlay/blob fd reuse | engine-owned `ReadSource` internals | retained benchmarks prove open/seek overhead remains material after layers 1-3 |
| 5 | streaming directory pagination | NFS directory adapter plus resolver support | large-directory `READDIRPLUS` pressure remains high after candidate-page caching |

This ordering is important. Jumping straight to fd reuse or streaming directory
pagination would increase lifetime and invalidation surface before the cheaper
pool/source/hydration wins are exhausted. The best design keeps those larger
optimizations available, but not accepted, until retained evidence shows the
current layers are the bottleneck.

### Correctness Ladder

Every optimization must preserve these ownership checks:

| Question | Required answer before merge |
|----------|------------------------------|
| What identity does the cache key prove? | NFS namespace identity, resolver path/generation, read-source identity, or hydration window identity |
| Who can invalidate it? | the module that owns the mutation or refresh event |
| What happens when validation fails? | evict, retry once if the race is expected, then return the real mapped error |
| Can writes bypass the canonical path? | no; writes enter through `VfsEngine` and stability is tracked separately |
| Does FUSE share the benefit or remain unaffected? | shared source/hydration work benefits FUSE; NFS-only protocol caches must not import FUSE state |
| Can control metadata leak local authority? | no; public payloads redact TCP tokens, Unix control parents are user-private, and retained evidence recursively rejects raw control tokens |
| What retained proof exists? | unit/concurrency tests for the invariant, synthetic read-path evidence for engine behavior, native smoke evidence for OS-client behavior |

This turns the hf-mount comparison into a reusable decision test. hf-mount's
pool behavior passes because NFSv3 is stateless and Crab can express it as
`ReadLeasePool`. hf-mount's pooled inode/file-handle object fails because it
would make the NFS adapter own byte truth and write upgrades. Engine-owned fd
reuse may pass later because it would live behind `VfsReadLease` validation and
would not become an NFS writable-handle model.

### Evidence Tiers

The preferred-backend decision needs a stricter evidence model than ordinary
unit-test proof because the NFS client is part of the product surface. Use three
separate tiers:

| Tier | Purpose | Required shape | Decision it can support |
|------|---------|----------------|-------------------------|
| Advisory | local or PR confidence while iterating | `make nfs-feature-gate`, retained synthetic read-path report, and any available native smoke reports | validates architecture direction, catches obvious regressions, and produces candidate thresholds |
| Calibration | performance-budget review | several retained benchmark reports plus several retained native smoke summaries, preferably across hosted main/manual runs | chooses workflow/release threshold arguments and baseline run ids |
| Release | default-backend confidence | exact-commit retained native Linux, macOS, and Windows smoke reports with required artifacts plus the NFS feature gate and archive/helper checks | allows packaging to ship NFS as the preferred backend |

This tiering avoids two traps. First, a two-platform local run is useful
architecture evidence, but it is not release-grade because Windows Client for
NFS has different mount and loopback constraints. Second, a single benchmark
run can generate helpful threshold suggestions, but those suggestions are
policy inputs, not policy. Maintainers should promote thresholds only after the
worst stable retained values still leave enough margin for expected platform
variance.

Benchmark ratios should be interpreted by workload. The sequential pointer
lease/path ratio is the primary proof that the NFS lease pool and engine
read-source cache are paying for stateless OS readahead. Random pointer ratios
prove that adaptive prefetch is restrained and that cache layers do not add
large overhead. Overlay-modified reread ratios prove source invalidation and
overlay-source reuse stay correct after copy-on-write. This is why ratio
thresholds may be below `1.0` for non-sequential scenarios: they are regression
budgets, not a claim that the lease path should dominate every access pattern.

## Best Architecture Decisions

The best long-term architecture is a hybrid of hf-mount's pool behavior and
Crab's existing Git snapshot, resolver, overlay, and hydration correctness
model. Both projects can mount deduplicated large files and hydrate content on
demand. The important difference is the interface that owns byte truth. In
hf-mount, the `VirtualFs` inode and open-file handle own most filesystem state.
In Crab, byte truth is spread across a Git snapshot, pointer interpretation,
object ids, overlay copy-on-write state, and xorb hydration. Pulling hf-mount's
pooled object into Crab would move correctness into the NFS adapter. Pulling the
pool discipline into Crab's engine keeps the performance win without creating a
second filesystem engine.

The key decision is to cache at the seam that owns the invariant:

| Interface seam | Cached state | Owner | Invariant |
|----------------|--------------|-------|-----------|
| NFS protocol | stable ids, attrs, `READDIRPLUS` candidate pages, pinned read leases, write stability | NFS adapter | make a stateless native client usable without deciding byte truth |
| Crab namespace | paths, resolver entries, overlay versions, refresh generation | `VfsEngine` and resolver | path meaning changes only through refresh or overlay mutation |
| Byte source | `ReadSourceKey`, source kind, source cache metrics, adaptive read state | `VfsEngine` | every read proves content identity plus overlay view before serving bytes |
| Remote bytes | hydration windows, chunk cache, in-flight fetches, prefetch claims | `HydrationService` | all deduped large-file reads use the same verified reconstruction path |
| Platform lifecycle | native client preflight, helper process, control endpoint, registry metadata | mount command/control modules | the preferred backend is selected only when the OS contract is actionable |

That split is the performance answer and the correctness answer. Crab can reuse
hf-mount's pool mechanics because NFSv3 is stateless. Crab should not reuse
hf-mount's pooled inode/file-handle object because Crab's hard invariant is not
"this inode has an open handle"; it is "this read source still matches the
resolver, overlay view, and hydrated content identity." The best cached object
is therefore a `VfsReadLease`, backed by an engine read-source cache and
hydration windows.

### Long-Term Architecture Decision Update

The strongest finding from the hf-mount review is that both systems need state
across stateless NFSv3 RPCs, but they should not cache the same object. hf-mount
must pool `VirtualFs` file handles because its handle is the unit that carries
open-file state, adaptive prefetch state, local staging, write mode, and release
semantics. Crab should pool `VfsReadLease` values because Crab's durable
invariants are split across the resolver, overlay, source cache, and hydration
service. Copying hf-mount's pooled object would collapse those independent
owners into the NFS adapter.

Treat these as the long-term architecture decisions:

| Status | Decision | Architecture rule | Primary proof |
|--------|----------|-------------------|---------------|
| Accepted | Extract hf-mount's pool discipline | LRU, operation pins, temporary overflow, stale retry, and bounded counters belong in Crab's `ReadLeasePool` | `crates/crab-vfs/src/read_lease_pool.rs`, stale retry tests, native read-lease density |
| Accepted | Change the pooled object | NFS pools `VfsReadLease`, never hf-mount-style inode/open-file handles | `crates/crab-vfs/src/nfs.rs` delegates reads through `VfsEngine::read_at` |
| Accepted | Share the lease contract across backends | FUSE stores `VfsReadLease` on read-capable file handles while NFS stores it in a file-id pool | `crates/crab-vfs/src/fuse.rs` open/read/write/release tests and NFS lease-pool tests |
| Accepted | Keep byte-source reuse in the engine | Resolver/source selection, source-key validation, and adaptive read classification stay in `VfsEngine` | read-source cache counters, invalidation tests, read-path benchmark ratios |
| Accepted | Keep deduped bytes in hydration | Pointer/xorb reconstruction and read-window prefetch stay in `HydrationService` | hydration deltas in retained native smoke artifacts |
| Accepted | Keep writes path-based | NFS tracks stability in `NfsWriteJournal`, while writes, truncate, rename, remove, sync, and publish remain engine/overlay operations | write-journal tests and retained writeback artifacts |
| Accepted | Keep directory pressure at the protocol edge | `READDIRPLUS` candidate pages are NFS-owned derived state keyed by generation/path/directory version | directory-page counters and invalidation tests |
| Accepted | Keep mount control backend-neutral | The registry and mount-control modules own list/status/refresh/switch/shutdown routing across NFS and FUSE | mount-control tests and retained control artifacts |
| Accepted | Keep release confidence evidence-owned | NFS becomes preferred only with exact-commit feature gate plus retained macOS/Linux/Windows native smoke evidence | NFS evidence workflow and release gate |
| Deferred | Engine-owned local fd reuse | May live inside `ReadSource` for overlay/blob sources after retained benchmarks prove open/seek cost remains material | retained read-path benchmark and native throughput deltas |
| Deferred | Streaming directory pagination | May extend the resolver/NFS directory contract after candidate-page caching still shows pressure | large-directory `READDIRPLUS` counters |
| Deferred | Wider adaptive prefetch windows | May grow only when native remote-byte amplification stays healthy | hydration requested/returned/remote-byte ratios |
| Rejected | Transplant hf-mount's inode/file-handle pool | It would move source validity, writable handle upgrade, dirty data, and release semantics into NFS | ownership review against `VfsEngine` and overlay writes |
| Rejected | NFS-only direct fd/source cache | It would duplicate engine invalidation and leave FUSE without the same correctness path | shared VFS source-cache metrics |
| Rejected | NFS-specific object-store reads | It would create a second xorb reconstruction path | hydration byte-identical reconstruction tests |
| Rejected | FUSE fallback after NFS startup begins | It hides broken helper, control, registry, mountpoint, or native-client state | explicit `auto` fallback policy and preflight tests |
| Rejected | Self-mutating performance policy | One noisy run must not rewrite release thresholds | threshold suggestion review and retained run-attempt identity |

The resulting NFS read path is deliberately layered:

1. NFS maps a stable file id to a path and pins a `VfsReadLease` from the
   bounded pool.
2. A pool miss opens the lease through `VfsEngine::open_read`, which can reuse
   the engine read-source cache after validating the cached source.
3. `VfsEngine::read_at` validates generation, overlay view, and source identity
   before any bytes are served.
4. Pointer-backed sources call `HydrationService::read_range`; adaptive
   sequential or confirmed-stride decisions schedule bounded hydration-window
   prefetch.
5. Stale lease validation evicts the NFS pool entry, records the stale retry,
   and retries once through the canonical engine path.

The resulting mutation path is equally strict:

1. NFS resolves the id to a path and calls the canonical engine mutation.
2. The engine performs copy-on-write, overlay mutation, checkpoint/sync, and
   read-source invalidation.
3. Only after the engine succeeds does NFS update derived protocol state:
   directory pages, stable ids, read-lease entries, and write-journal paths.
4. If the engine fails, NFS protocol caches remain unchanged. If later derived
   protocol cleanup fails, the next refresh/control/status path can rebuild
   from the canonical VFS state.

This keeps the best hf-mount idea, but makes it Crab-shaped: performance state
exists at the lowest owner that can invalidate it, and the NFS adapter never
becomes the source of byte truth.

The accepted architecture is therefore:

| Decision | Accepted position | Performance reason | Correctness and developer-UX reason |
|----------|-------------------|--------------------|-------------------------------------|
| Pooled unit | Pool `VfsReadLease`, not hf-mount `VirtualFs` handles | avoids reopening the same read source for NFS readahead and random rereads | every pooled entry still carries engine-owned source identity and stale validation |
| Pool owner | Keep `ReadLeasePool` in the NFS adapter | directly absorbs NFSv3's missing open/close lifecycle | the protocol adapter owns only file-id reuse, operation pins, and NFS counters |
| FUSE retention | Store a `VfsReadLease` on read-capable FUSE file handles | avoids reopening the same source during a real kernel open lifecycle | FUSE remains stateful at the handle edge while byte validity stays in the engine |
| Source reuse owner | Keep read-source caching in `VfsEngine` | avoids repeated resolver/source selection after lease misses and benefits FUSE too | mutation invalidation stays next to overlay, refresh, rename, truncate, and remove logic |
| Deduped remote reads | Keep xorb/chunk reads in `HydrationService` windows | sequential scans and confirmed stride reads can prefetch bounded windows | NFS, FUSE, `crab hydrate`, and SDK-style reads use one byte-identical reconstruction path |
| Local fd reuse | Add only as an engine-owned `ReadSource` optimization if benchmarks prove it | can reduce local overlay/blob open/seek overhead without touching pointer hydration | fd lifetime remains source-specific and does not become an NFS writable-handle model |
| Writes | Keep writes path-based through the engine and journal NFS stability separately | avoids lock contention and stale writable handles in the read pool | copy-on-write promotion, truncate, rename, remove, sync, and publish stay in one write path |
| Evidence | Gate with retained engine benchmarks plus retained native smokes | catches source-cache, lease-ratio, and OS-client read-amplification regressions | separates portable VFS proof from macOS/Linux/Windows mount-client proof |

| Area | Decision | Why it is the best fit for Crab |
|------|----------|---------------------------------|
| Source of truth | Keep resolver + overlay + hydration in `VfsEngine` | Crab's correctness comes from path resolution, copy-on-write overlay rules, and verified hydration; duplicating that in NFS would create a second filesystem engine |
| Cache topology | Use a two-tier read cache: NFS `ReadLeasePool` plus engine read-source cache | The NFS pool absorbs stateless NFS readahead; the engine cache avoids repeated source selection and benefits both NFS and FUSE |
| Stateless NFS reads | Pool `VfsReadLease` values keyed by stable NFS file id | This captures hf-mount's LRU/pinning/stale-retry performance lesson without importing inode-owned handles |
| Lease validity | Validate by content identity plus overlay view version | Paths and NFS ids are user-facing identities, not byte proofs; leases must stale after refresh or overlay promotion |
| Stable ids vs bytes | Keep NFS ids and read-source keys as separate identities | NFS ids give the OS a stable namespace; read-source keys prove the exact bytes being served |
| Writes | Keep writes path-based and track NFS stability in `NfsWriteJournal` | Writable handle pools would duplicate overlay promotion, truncation, rename, remove, and sync rules |
| hf-mount extraction | Extract pool behavior, not hf-mount's pooled object | hf-mount's pooled file handles fit its inode-owned `VirtualFs`; Crab's pooled unit must be `VfsReadLease` because Crab's byte truth lives in the engine |
| Deduped large files | Keep hydrate-on-demand below the VFS read-source boundary | Crab can match hf-mount's large-file laziness while preserving Git pointer, xorb, and overlay correctness in one hydration pipeline |
| Remote data | Reuse hydration windows as the only object-store read primitive | Crab keeps one verified reconstruction path for deduped large files and avoids NFS-specific xorb reads |
| Prefetch | Keep adaptive read-pattern state above hydration windows | Sequential scans and confirmed strided reads get bounded lookahead while random model reads avoid broad overfetch |
| Invalidation | Fail closed with source-key validation, exact path/subtree invalidation for overlay mutations, and full invalidation for refresh/reset | Correctness does not depend on every protocol adapter predicting invalidation perfectly |
| Directory scaling | Keep bounded NFS directory candidate caching keyed by generation plus path/subtree directory versions; defer deeper streaming pagination until measured pressure proves it | Cookie-resumed `READDIRPLUS` should not repeat resolver/id-table work, unrelated directory pages should survive ordinary writes, and snapshot streaming adds a larger invalidation surface |
| Benchmark gate | Keep engine benchmarks and native NFS smokes separate | Synthetic engine runs prove source-cache, lease, hydration, and overlay behavior; native smokes prove platform mount commands and OS-client readahead |
| Smoke evidence | Emit common native NFS smoke reports, verify single reports and retained directories, and compare retained baseline/current reports | Release gating can retain platform evidence and catch OS-client read regressions without depending on terminal transcript parsing or runner-local paths |
| Retained control artifacts | Treat retained JSON as evidence, not authority | Verifiers prove helper identity and control-path behavior while recursively rejecting raw TCP control tokens in status, writeback, unmount, shutdown, and remount artifacts |
| Threshold calibration | Generate suggested verify/compare args from retained evidence, then promote reviewed values to workflow variables | Performance policy should be empirical and platform-specific, but release gates should not mutate themselves from one noisy run |
| Cross-platform UX | Prefer NFS only after preflight passes; make fallback explicit | NFS can be the default without hiding Linux privilege rules, Windows Client for NFS setup, or helper packaging problems |
| Auto fallback boundary | Fall back only for environmental NFS unavailability before NFS state is created | Missing native clients or Linux privilege can use FUSE; stale mountpoints, control endpoint conflicts, helper identity failures, and post-startup NFS errors must stay visible |
| Mount discovery | Use a registry-first, live-annotated control plane | Background helpers can crash or be per-mount; the durable registry keeps UX stable while live probes upgrade accuracy |
| Control security | Use user-private Unix sockets where available and random per-launch loopback TCP tokens elsewhere | The control endpoint is powerful enough to refresh, switch, and shut down a mount, so non-Unix authentication cannot be derived from a guessable mountpoint |
| Unix endpoint length | Keep default Unix sockets in a short per-user runtime path, not under `HOME` | macOS has a tight Unix socket path limit, and native smoke artifacts plus user workspaces can live under deep paths |
| Feature boundaries | Keep NFS and FUSE independently compilable over shared VFS modules | A preferred NFS backend cannot depend on macFUSE/libfuse or FUSE IPC types; shared proof belongs in `VfsEngine`, hydration, mount control, and registry code |
| Developer UX | Keep backend adapters thin and put reusable state in shared VFS modules | Contributors can debug protocol, source selection, hydration, and platform lifecycle independently |

### Accepted Decision Record

The implementation findings narrow the design to a few non-negotiable
architecture decisions:

| Decision | Accepted rule | Proof surface |
|----------|---------------|---------------|
| Read reuse | Reuse hf-mount's pool behavior, but pool `VfsReadLease` values instead of inode/file-handle objects | `crates/crab-vfs/src/read_lease_pool.rs`, `crates/crab-vfs/src/nfs.rs`, `crab/benches/nfs_read_path_bench.rs` |
| Lease performance policy | Treat read-lease hit and miss density as release-gated native smoke metrics, not incidental counters | retained `native-read-benchmark.json`, `make nfs-smoke-report-verify-dir`, `make nfs-threshold-suggestions` |
| Byte truth | Keep resolver, overlay, source selection, and stale-source rejection in `VfsEngine` | `crates/crab-vfs/src/engine.rs` invalidation tests plus retained read-path benchmark reports |
| Deduped hydration | Keep xorb/chunk reconstruction under `HydrationService`; NFS never reads object-store bytes directly | `crates/crab-vfs/src/hydration.rs` counters and native read-benchmark hydration deltas |
| Metadata pressure | Cache `READDIRPLUS` candidate pages in the NFS adapter because cookie pagination is protocol-edge state | `crates/crab-vfs/src/nfs.rs` directory-page counters and native `mount status --json` artifacts |
| Writes | Keep writes path-based through the engine and use `NfsWriteJournal` only for NFS stability semantics | write-journal unit tests plus native smoke write-sync latency/status artifacts |
| Control plane | Treat the helper control endpoint as a local secret: parent-generated endpoint, exact registry handoff, private Unix registry files, redacted human display | `crates/crab-vfs/src/nfs_control.rs`, `crates/crab-vfs/src/mounts_registry.rs`, `crab/src/cmd/mount.rs` targeted tests |
| Retained control proof | Keep full control authority out of retained smoke artifacts while proving live control was used | `crab/scripts/verify-nfs-smoke-report.py` artifact-specific redaction checks and self-tests |
| Platform default | Prefer NFS only after preflight proves native-client, helper, mountpoint, loopback, and control-endpoint contracts | `crab mount doctor --backend=nfs`, `make nfs-feature-gate`, native smoke scripts |
| State timing | Run explicit NFS and auto NFS preflight before building the mount pipeline, helper process, registry entry, or native mount | `ensure_mount_backend_prerequisites`, explicit-NFS preflight tests, helper startup tests |
| Helper identity | Background NFS may discover a helper through the normal search path, but it must refuse helpers that are not colocated with the current `crab` binary or that report a different Crab version | mount doctor helper checks, background helper layout/version tests, archive-content gate |
| Unix endpoint length | Generate Unix control sockets under a short per-user runtime directory, not under `HOME` or the mountpoint | `crates/crab-vfs/src/nfs_control.rs`, `endpoint_for_mountpoint_stays_short_for_deep_mountpoints`, native macOS smoke |
| Release gate | Make retained evidence part of the architecture, not a post-hoc CI check | `.github/workflows/nfs-mount.yml`, `.github/workflows/release.yml`, `make nfs-release-gate` |

These rules are deliberately stricter than a direct hf-mount transplant. The
performance state exists where the workload pressure appears, but correctness
stays at the interface that owns the invariant. A future optimization should be
accepted only if it preserves that ownership split and adds proof at the same
interface.

### Best-Decision Findings From Code Review

The hf-mount comparison changes the design from "add a pool" to "put each pool
at the invariant it can actually prove." hf-mount's NFS adapter keeps a
`HandlePool` of `VirtualFs` file handles keyed by inode in
`hf-mount/src/nfs.rs`. That is the right shape for hf-mount because a
`VirtualFs::open` handle owns open-file state in
`hf-mount/src/virtual_fs/mod.rs` and per-handle adaptive prefetch state in
`hf-mount/src/virtual_fs/prefetch.rs`. Its NFS adapter therefore has to pin
handles during reads, flush/release handles during eviction, retry once after a
stale `EBADF`, and upgrade a read handle to a writable handle when macOS sends
WRITE after earlier READ traffic.

Crab should keep the algorithm and reject that ownership model. In Crab, a
write is not a handle upgrade; it is a path mutation that may promote base
content into the overlay, invalidate source keys, update NFS directory pages,
and enter the NFS write journal. A pooled writable NFS handle would duplicate
the engine's copy-on-write and sync rules. The accepted object is therefore
`VfsReadLease`: it carries a `ReadSourceKey`, adaptive read-pattern state, and a
shared source reference, while validation and invalidation remain in
`VfsEngine`. The Crab implementation evidence lives in
`crates/crab-vfs/src/read_lease_pool.rs`, `crates/crab-vfs/src/nfs.rs`,
`crates/crab-vfs/src/engine.rs`, and `crates/crab-vfs/src/hydration.rs`.

The final architecture decision stack is:

1. Prefer NFS only after native-client, helper, mountpoint, loopback, and
   control-endpoint preflight pass.
2. Keep NFS state protocol-shaped: stable ids, statuses, write stability,
   directory pages, and file-id keyed read-lease pins.
3. Keep byte truth engine-shaped: resolver generation, overlay view, source
   keys, adaptive read state, and invalidation events.
4. Keep deduped remote bytes hydration-shaped: xorb/chunk reconstruction,
   read windows, in-flight suppression, and prefetch claims.
5. Keep writes path-shaped: mutate through `VfsEngine`, then update NFS
   derived state after the canonical mutation succeeds.
6. Keep release confidence evidence-shaped: advisory local proof, reviewed
   calibration proof, and exact-commit macOS/Linux/Windows release proof.

This stack is deliberately boring for developers. When a bug appears, the
owner should be obvious from the failed invariant: NFS protocol identity,
engine byte identity, hydration remote-byte identity, platform control state,
or release evidence.

The best architecture keeps four identities separate:

| Identity | Owner | What it proves | Why it must not be merged |
|----------|-------|----------------|---------------------------|
| NFS file id | NFS adapter | stable namespace handle for the OS client | rename-friendly NFS behavior is not a byte-source proof |
| Resolver path/generation | resolver and `VfsEngine` | current snapshot meaning of a path | refresh/ref-switch can change base content without changing the NFS id |
| Read source key | `VfsEngine` | exact bytes and overlay view served by a lease | overlay promotion, truncate, rename, reset, and remove must stale old bytes |
| Hydration window key | `HydrationService` | verified xorb/chunk byte range | remote dedup reconstruction must stay shared by NFS, FUSE, hydrate, and SDK reads |

This creates a deliberate four-cache hierarchy, each with a different reason to
exist:

| Cache | Owner | Accepted reason | Rejection line |
|-------|-------|-----------------|----------------|
| Read lease pool | NFS adapter | bridge stateless NFSv3 reads to reusable engine leases | must not own writable handles or direct byte sources |
| Directory page cache | NFS adapter | avoid repeated resolver/id-table work on cookie-resumed `READDIRPLUS` | must not become a second snapshot directory store |
| Read-source cache | `VfsEngine` | reuse source selection and adaptive state across NFS and FUSE | must not be bypassed by protocol-specific source caches |
| Hydration windows | `HydrationService` | deduplicate object-store fetches and preserve verified reconstruction | must remain the only remote-byte primitive |

The near-term performance ladder should follow that hierarchy:

1. Tune `ReadLeasePool` capacity, memory budget, stale retry rate, and
   lease-hit density from retained native smoke reports.
2. Tune engine read-source cache size and adaptive prefetch thresholds from
   retained read-path benchmarks plus native OS-client amplification metrics.
3. Tune hydration window and prefetch behavior only when object-store
   requested/returned/remote-byte ratios prove extra lookahead is worthwhile.
4. Add engine-owned local fd reuse for overlay/blob sources only if retained
   evidence shows open/seek overhead remains material after the first three
   layers are healthy.
5. Add streaming directory pagination only if bounded `READDIRPLUS` candidate
   caching still shows repeated large-directory pressure.

Any future NFS performance change should pass this acceptance checklist before
being treated as the best design:

- The owner is the module that can prove the invariant.
- The invalidation source is named and tested.
- Memory is bounded, with visible hit/miss/evict/stale/overflow counters.
- Random reads do not trigger broad speculative hydration.
- Writes stay path-based and `NfsWriteJournal` remains a stability tracker, not
  a data owner.
- FUSE either benefits through the shared VFS layer or is explicitly unaffected.
- The proof includes retained synthetic engine evidence and, for mount-client
  behavior, retained native macOS/Linux/Windows smoke evidence from the exact
  commit under review.

Deferred decisions remain evidence-gated:

| Deferred choice | Gate before acceptance |
|-----------------|------------------------|
| Engine-owned local fd reuse for overlay/blob sources | Retained benchmark evidence that open/seek overhead remains material after the read-source cache |
| Streaming directory pagination beyond bounded candidate pages | Native directory-pressure evidence showing candidate caching is insufficient |
| Broader adaptive prefetch windows | Retained benchmark and native-smoke evidence that extra lookahead improves throughput without object-store amplification |
| Stricter release thresholds | Multiple retained platform runs reviewed through `make nfs-threshold-suggestions`, then promoted to workflow variables or release arguments |
| Dedicated release-mode native benchmark suites | Native smoke artifacts showing the current sequential-read benchmark is too coarse for policy |

This is the key refinement from the hf-mount review: Crab should support the
best idea from hf-mount, but the cached unit is a Crab read lease, not an
hf-mount inode/file handle. The pooled lease carries source identity and
adaptive read state, while shared hydration work and any future local fd reuse
stay behind the canonical VFS engine interface.

The hf-mount extraction rule is:

| hf-mount idea | Crab decision |
|---------------|---------------|
| LRU eviction with operation pins | Extract directly into `ReadLeasePool` |
| Temporary overflow when all entries are pinned | Extract directly, with visible pool counters |
| Stale handle retry | Translate to stale `VfsReadLease` detection and one reopen through `VfsEngine` |
| Per-handle prefetch state | Translate to per-lease adaptive read classification plus hydration-window prefetch |
| Bounded memory from pool capacity | Keep in both `ReadLeasePool` and engine read-source cache snapshots |
| Open-file handle as the cached object | Reject for Crab; it would duplicate resolver, overlay, and hydration ownership |
| Read/write handle upgrade inside NFS | Reject for Crab; writes must stay path-based and journaled for NFS stability |
| Direct pointer/xorb reads from NFS | Reject for Crab; hydration windows are the only remote-byte primitive |

The fd-reuse question should stay deliberately narrower than hf-mount's handle
pool. Crab may add fd reuse for local overlay/blob sources only as an
engine-owned `ReadSource` optimization, after retained benchmark evidence shows
open/seek overhead remains material. It should not become an NFS-owned writable
handle pool, and it should not bypass hydration windows for pointer/xorb data.
That keeps the optimization local to the source type that benefits from it while
preserving the same lease validation, invalidation, and stale-retry behavior.

Decision order:

1. Keep the NFS `ReadLeasePool` because it is the protocol bridge for stateless
   native clients.
2. Keep the engine read-source cache because it is the shared byte-source reuse
   layer for NFS and FUSE.
3. Keep hydration windows as the only remote-byte read primitive because they
   preserve byte-identical xorb reconstruction.
4. Keep writes path-based because overlay correctness is more valuable than any
   writable-handle shortcut.
5. Add fd reuse only inside engine read sources if measurements prove it, never
   as an NFS adapter-owned model.

### Decision Model: Two Caches, One Write Path

The preferred design is intentionally asymmetric:

- NFS gets a protocol cache because the native client repeatedly sends file ids
  without an open lifecycle.
- The VFS engine gets a byte-source cache because source selection is a shared
  Crab concern, not an NFS concern.
- Writes keep one path through the engine because copy-on-write promotion,
  truncate, rename, remove, sync, and overlay persistence are already coupled.

That gives each module a small interface with substantial behavior behind it:

| Module | Small interface | Hidden implementation |
|--------|-----------------|-----------------------|
| `VfsEngine` | `open_read`, `read_at`, path mutations, overlay sync | resolver lookup, source identity, source cache, adaptive read state, hydration windows, overlay invalidation |
| `ReadLeasePool` | `pin`, `insert_and_pin`, `evict`, `invalidate_all`, `record_stale_retry`, `snapshot` | LRU, operation pins, temporary overflow, memory budget, generation invalidation, stale retry counters |
| `NfsWriteJournal` | mark, sync, rename/remove subtree, snapshot | NFS write stability, dirty age, sync errors, shutdown drain |
| `mount_control` | list, status, refresh, switch, shutdown | registry-first discovery, NFS helper control, FUSE coordinator routing |

The deletion test is useful here. Removing the engine read-source cache would
push source reuse and invalidation decisions into both NFS and FUSE. Removing
the NFS read-lease pool would force NFS to reopen a lease for every stateless
read burst. Copying hf-mount's writable file-handle pool would make the NFS
adapter participate in overlay write correctness. The chosen split keeps the
performance state while preserving locality for correctness.

### Enhanced NFS Architecture Opportunities

The strongest design opportunity is to make NFS feel like a normal local
filesystem while the implementation remains a lazy, content-addressed Crab
repository. That means optimizing the lanes where NFS clients create pressure,
not building a second mount engine:

| Opportunity | Design choice | Acceptance signal |
|-------------|---------------|-------------------|
| Hot large-file reads | Keep `ReadLeasePool` plus engine read-source cache plus hydration windows as the fast path | Native read reports show fewer resolver/source opens and stable NFS RPC efficiency for sequential scans |
| Random and strided model reads | Classify random/strided access before prefetch expands | Strided workloads get one target-window lookahead; random object-store bytes fetched stay close to requested hydration windows |
| Dedup-aware laziness | Keep xorb/chunk reconstruction exclusively in `HydrationService` | NFS, FUSE, and `crab hydrate` produce byte-identical data from the same path |
| Overlay correctness | Use engine-owned source keys and path-scoped overlay invalidation | Writes, truncate, rename, remove, reset, and refresh cannot serve pre-mutation bytes, while unrelated hot read sources stay cached |
| Directory pressure | Keep bounded `READDIRPLUS` candidate caching first, add streaming directory pages only with native pressure evidence | Large-directory counters show lower attr resolution and cookie-resume waste without broad memory growth |
| Platform confidence | Treat native smokes and retained reports as product evidence, not demos | Linux/macOS/Windows reports verify helper layout, control, status, writes, remount, and native read counters |
| Developer velocity | Keep reusable behavior in shared VFS modules and keep adapters thin | NFS-only tests compile without FUSE IPC, while FUSE keeps using the same read-source and hydration proof |

This is where Crab can go beyond hf-mount. hf-mount proves that a stateless NFS
server needs a disciplined pool with LRU, pinning, temporary overflow, stale
retry, and bounded memory. Crab can keep that discipline while making the pooled
lease a view into a richer engine: Git pointer resolution, xorb hydration,
overlay copy-on-write, native mount control, and retained evidence all stay
visible at their owning layer.

## Alternatives Considered

### Keep the Current Path-Resolve-Per-Read Flow

Pros: smallest code surface; easy to prove because every read crosses the
canonical engine interface and there is almost no retained adapter state.

Cons: repeats resolver/source selection for every NFS read; cannot reuse
per-source state across NFS client readahead; turns every cache miss into
hydration/source-selection work; leaves FUSE and NFS with different performance
hooks.

Decision: keep as the correctness baseline and fallback mental model only. It
is not the long-term performance architecture.

### Transplant hf-mount's Inode/File-Handle Pool

Pros: proven shape for stateless NFSv3; LRU plus pinning prevents releasing
handles during in-flight reads; temporary overflow avoids dropping active
state; per-handle prefetch state can absorb readahead; write-open reuse can
avoid some repeated local fd work.

Cons: imports hf-mount's inode-owned `VirtualFs` model into Crab; duplicates
source selection, overlay mutation rules, snapshot generation handling, and
write-handle upgrade behavior; makes NFS responsible for releasing and
upgrading handles that may carry dirty data; NFS-only optimization would not
help FUSE; turns Crab's Git snapshot and overlay validity into adapter-local
knowledge.

Decision: do not transplant. Crab should copy the pool discipline, not the
cached object or write-upgrade model.

### Add an NFS-Only Direct Source/fd Cache

Pros: faster to prototype inside `vfs/nfs.rs`; avoids resolving every read.

Cons: puts byte-source validity in the protocol adapter; duplicates
invalidation with `engine.rs`; risks stale overlay reads after write, truncate,
rename, reset, or remove; leaves FUSE behind.

Decision: reject.

### Use Only the Engine Read-Source Cache

Pros: keeps all byte-source reuse behind `VfsEngine`; benefits FUSE and NFS;
keeps invalidation at the mutation owner.

Cons: does not fully bridge NFSv3 statelessness. A native NFS client can issue
many reads for the same file id without an open lifecycle, so each adapter miss
would still cross `open_read` before the engine source cache can help. It also
would not expose protocol-edge lease hit/miss/stale counters that explain NFS
client behavior.

Decision: reject as the only cache. Keep the engine source cache, but pair it
with an NFS file-id `ReadLeasePool`.

### Make the NFS Read-Lease Pool the Only Cache

Pros: simpler mental model for NFS; fewer cache layers to inspect in mount
status; direct fit for NFSv3's missing open/close lifecycle.

Cons: FUSE would not benefit; repeated `open_read` calls would still redo
source selection after an NFS lease miss; source-cache invalidation would remain
implicit inside the protocol adapter; benchmark wins would depend on the NFS id
table instead of the shared VFS interface.

Decision: reject. Keep the NFS pool as the protocol bridge and keep source
reuse in the engine.

### Add Engine-Owned Read Leases Plus an NFS Lease Pool

Pros: reuses the hf-mount pool lesson without moving correctness out of
Crab's VFS engine; benefits NFS and FUSE; cache keys prove content identity;
invalidation stays next to mutations; native NFS counters explain protocol
pressure while engine counters explain source-cache and hydration behavior.

Cons: has more moving parts than an NFS-only cache, so default enablement needs
retained benchmark and native smoke evidence rather than unit tests alone.

Decision: preferred design and current implementation baseline.

Crab can extract the algorithmic lesson from hf-mount's pool: LRU entries,
operation pins, temporary overflow when all entries are pinned, stale-handle
retry, and bounded memory. Crab should not extract hf-mount's pooled object as
the unit of caching. hf-mount pools inode-owned `VirtualFs` file handles; Crab
should pool `VfsReadLease` values whose source identity and invalidation are
owned by the engine.

The distinction is important because both systems lazily hydrate deduplicated
large-file content, but they expose different internal interfaces. hf-mount's
NFS adapter must manufacture a stateful `VirtualFs::open/read/release`
lifecycle because NFSv3 has no open/close RPCs. Crab's state bridge should be
the VFS engine read-source interface instead: open a stable read source once,
reuse its per-source state across stateless NFS reads, and keep all byte-source
validity checks in the engine.

Crab should adopt these hf-mount ideas directly:

- LRU pool keyed by the stable NFS file id.
- Operation pins so eviction cannot release state used by an in-flight read.
- Temporary overflow when all entries are pinned.
- Retry once after a stale read source, then reopen through the canonical VFS
  engine interface.
- Memory budgeting that accounts for per-source buffers and file descriptors.
- Per-source sequential/read-pattern state, equivalent in spirit to
  hf-mount's prefetch state, layered above Crab's hydration window cache.

Crab should not adopt these hf-mount implementation details:

- A pooled writable file handle shared by both reads and writes.
- Inode-owned source selection outside Crab's resolver and overlay engine.
- NFS-specific direct access to pointer hydration, xorbs, or overlay backing
  files.
- Handle-upgrade rules where a read-only pooled handle is converted to a
  writable handle on first write.

## Target Architecture

```
User process
  |
  | syscalls
  v
OS NFS client
  |
  | NFSv3 RPCs
  v
crab NFS protocol adapter
  - stable file ids
  - platform-safe NFS status mapping
  - NFS commit/unstable-write tracking
  - file-id keyed ReadLeasePool
  - bounded READDIRPLUS directory-page cache
  |
  | path + expected node type + pinned VfsReadLease
  v
VFS engine
  - read leases
  - content-identity source cache
  - adaptive read classification
  - overlay mutation invalidation
  - hydration window cache
  |
  +--> overlay backing files
  +--> git ODB blob cache
  +--> pointer hydration / object storage
```

The NFS adapter remains a protocol adapter. It owns NFS ids, request decoding,
NFS status mapping, and platform-specific NFS write stability behavior. It does
not own content handles.

The VFS engine owns read-source selection and caching. That makes the cache
available to FUSE and keeps mutation invalidation next to the code that mutates
overlay state.

## Architecture Decision Summary

| Decision | Choice | Reason |
|----------|--------|--------|
| Preferred backend | NFS when the native client passes preflight | Works on macOS/Linux/Windows without requiring macFUSE or Linux FUSE setup |
| NFS role | Thin protocol adapter | Keeps NFS statuses, ids, and commit behavior separate from Crab byte-source correctness |
| Read optimization owner | VFS engine | NFS and FUSE share the same resolver/overlay/hydration semantics |
| Cache topology | Two-tier read cache: protocol lease pool plus engine source cache | Keeps NFSv3 statelessness handling separate from byte-source reuse |
| NFS state bridge | Shared `ReadLeasePool` used by the NFS adapter | NFSv3 has no open/close, so Crab must synthesize reuse across stateless reads |
| FUSE state bridge | File-handle `VfsReadLease` retention in the FUSE adapter | FUSE already has open/release, so it should not copy NFS's file-id pool |
| Cache proof | Content identity, not path alone | Refresh, rename, overlay mutation, and blobless size discovery can change path meaning |
| Identity split | Stable NFS ids are namespace handles, not byte-source proofs | Rename-friendly OS behavior and stale-source correctness need different identities |
| Remote read primitive | Hydration read-window cache | Avoids a second object-store reconstruction path |
| Prefetch policy | Per-source adaptive state above hydration windows | Sequential workloads get next-window lookahead, confirmed strided reads get one target-window lookahead, and random model reads stay tight |
| Write path | Path-based engine mutations | Avoids pooled writable handles and duplicated overlay rules |
| Write stability | NFS `NfsWriteJournal` | Tracks `UNSTABLE`, `DATA_SYNC`, `FILE_SYNC`, `COMMIT`, shutdown sync, and diagnostics without owning file data |
| Read invalidation | Engine-owned source invalidation | The engine has the mutation/source context needed to stale cached reads correctly |
| Directory invalidation | NFS-owned path/subtree directory-page versions | `READDIRPLUS` candidates are protocol-edge state; ordinary writes should invalidate affected pages without cooling unrelated directories |
| Directory scaling | Bounded NFS candidate cache plus pressure metrics | Avoids repeated resolver/id-table work now; deeper page-by-cookie streaming still needs native pressure proof |
| Platform UX | Preflight/doctor before native mount | Native mount failures are otherwise platform-specific and hard to act on |
| Background DX | Backend-agnostic mount control plane | `status`, `refresh`, `switch`, logs, and shutdown should work no matter which backend `auto` selected |
| Release layout | Ship `crab-nfs-mount` with every NFS-capable archive | The default backend cannot depend on users manually creating helper binaries |
| Feature gates | NFS-only builds compile and test without FUSE IPC modules | Preferred-backend proof must work in the same dependency shape that users install |

## Layered State Model

The best architecture decision is to separate state by ownership, not by
backend. That keeps performance state close to the hot path without letting NFS
grow into a second filesystem engine.

| Layer | Owner | State | Lifetime | Correctness rule |
|-------|-------|-------|----------|------------------|
| User namespace | Resolver + overlay | path, node type, attrs, overlay entry | mount lifetime | path meaning changes only through resolver refresh or overlay mutation |
| Byte source | `VfsEngine` | `ReadSourceKey`, source type, source cache, source metrics | source lease/cache lifetime | content identity and overlay view version prove bytes |
| Stateless read bridge | `ReadLeasePool` | NFS file id to pinned `VfsReadLease` | hot read burst | eviction never releases an in-flight read; stale lease retries once |
| Stateful read bridge | FUSE adapter | file handle to optional `VfsReadLease` | kernel open/release lifetime | read-capable handles reuse leases; writes clear the handle lease before later reads |
| Directory metadata bridge | NFS adapter | generation/path directory-page versions, cached candidates, pressure metrics | hot `READDIRPLUS` pagination | exact parent/subtree invalidation keeps changed listings cold while unrelated listings stay hot |
| Protocol session | NFS adapter | NFS ids, status mapping, write verifiers, write journal | NFS server lifetime | protocol state never decides byte-source validity |
| Operational registry | mount registry + control plane | backend, PID, log path, control endpoint | background mount lifetime | CLI commands discover mounts uniformly across NFS and FUSE |
| Platform contract | `nfs_mount` preflight | native client, privilege, mountpoint, loopback endpoint, control endpoint | mount attempt | `auto` chooses NFS only when required platform contracts pass |
| Compile contract | Cargo features | NFS adapter, FUSE adapter, shared VFS modules | build/test target | backend-specific tests never import the other backend's private IPC/control types |

This model is the concrete answer to the hf-mount comparison. Crab should
extract hf-mount's pool behavior because NFSv3 is stateless, but the pooled
object must remain a VFS read lease. Crab should extract hf-mount's lifecycle
discipline because native mount UX is platform-specific, but the operational
metadata must live in Crab's backend-agnostic registry and control surface.

## Core Design Decisions

### 1. Rename the Engine Interface

`FuseEngine` has become `VfsEngine`. The old name was historical: both NFS and
FUSE already called it. The rename clarifies that source selection, read leases,
overlay promotion, and hydration dispatch are backend-neutral VFS concerns.

The old type name should not return as a long-lived alias unless a shipped
public interface requires it. Inside the CLI crate, call sites should use the
canonical `VfsEngine` name directly.

### 2. Add Engine-Owned Read Leases

Use a VFS read lease interface:

```rust
pub struct VfsReadLease {
    key: ReadSourceKey,
    source: Arc<ReadSource>,
}

impl VfsEngine {
    pub fn open_read(&self, path: &str) -> Result<VfsReadLease>;
    pub async fn read_at(&self, lease: &VfsReadLease, offset: u64, size: u32) -> Result<Bytes>;
}
```

`open_read` resolves the path, validates that it is a regular file, computes a
stable source identity, and returns a lease for that source. The long-lived
lease is not itself an eviction pin; protocol adapters pin pooled leases only
for one in-flight operation, then release the pin before returning to the
client.

NFSv3 has no open/close RPC, so the NFS adapter should not expose leases to the
client. Instead, the adapter keeps an internal LRU of read leases keyed by NFS
file id. FUSE uses the same lease type differently: a read-capable `open` or
`create` stores the lease on the FUSE file handle, `read` reuses it, a stale
read replaces it after reopening through `VfsEngine`, `write` clears it, and
`release` drops it with the handle. That is the correct split because FUSE is
stateful at the kernel handle boundary while NFSv3 is not.

The existing `read(path, offset, size)` method remains a convenience wrapper
around `open_read(path)` plus `read_at`. That keeps one implementation of source
selection while older call sites migrate.

### 3. Cache by Content Identity, Not Path Alone

The read cache key must prove what bytes are being served:

```rust
enum ReadSourceKey {
    BasePointer {
        generation: i64,
        overlay_version: u64,
        file_hash: [u8; 32],
        size: u64,
    },
    BaseBlob {
        generation: i64,
        overlay_version: u64,
        object_oid: String,
        known_size: Option<u64>,
    },
    BaseEmpty {
        generation: i64,
        overlay_version: u64,
        path: String,
    },
    OverlayFile {
        path: String,
        overlay_version: u64,
        size: u64,
        mtime_ns: i64,
    },
}
```

Base pointer files are immutable by `file_hash + size`. The snapshot generation
keeps path-to-content lookups honest across refresh. Base Git blobs are
immutable by object id, but blobless snapshots may not know the blob size before
the ODB reader has fetched it, so their cache key carries `known_size` as
optional metadata rather than as a required correctness proof.

Base sources also carry the current overlay view version. This is intentionally
conservative: a write can promote a base path into the overlay without changing
the snapshot generation, so a cached base lease must stale when the overlay view
changes. Overlay files are mutable, so their key includes the same version.
Start with a global overlay mutation version if that is the smallest correct
interface; move to per-entry versions only after measurements show global
invalidation is too broad. A cached source must be rejected after write,
truncate, rename, remove, reset, or refresh reconciliation changes the overlay
entry.

### Stable NFS Ids Are Not Stable Byte Sources

NFS file ids should remain path-stable across normal rename because that gives
the OS client a usable local filesystem view. Read leases should remain
content-stable because they prove which bytes are being served. Those are
different identities and must not collapse into one table.

When a path survives a refresh but points at different base content, the NFS id
may continue to name the path, but the old read lease must be rejected by
generation/content identity. When a subtree is removed, replaced by a different
node type, or renamed over, the NFS id should become stale where the NFS status
surface can express that. This gives users stable path behavior without letting
old byte sources leak across refresh or overlay mutation.

### 4. Keep Writes Path-Based

Writes should continue to enter through the canonical path-based mutation
interfaces:

- `write`
- `truncate`
- `set_mode`
- `set_mtime`
- `create`
- `remove`
- `rename`

Those interfaces already own copy-on-write promotion, overlay mutation locks,
reset gating, and persistence. A write-handle pool would duplicate the hardest
part of the VFS correctness model.

After each successful mutation, the engine invalidates affected read sources
before returning success to the protocol adapter. For a completed write,
subsequent reads of that path must not use a source from before the write. Reads
that began before the write may complete on the older source; operation-level
read-after-write correctness starts when the write returns.

### 5. Keep Hydration Window Cache as the Remote-Read Primitive

Crab already has an 8 MiB read-through window cache for pointer-backed reads.
The long-term read-handle design should build on it, not replace it.

The read lease should add:

- a bounded hot in-memory source lookup
- sequential and strided-read detection for bounded prefetch lookahead
- shared inflight work across NFS readahead RPCs
- optional local-file descriptor reuse for overlay/blob-cache reads

It should not introduce a second object-store reconstruction path.

### 6. Add Adaptive Read Pattern State Above Hydration

hf-mount's strongest performance lesson is not its inode-owned handle model; it
is the per-open state that distinguishes sequential readahead from random reads.
Crab should copy that behavior at the `ReadSource` layer.

Each read source should track recent read ranges and classify the workload:

- sequential forward reads: prefetch the next hydration window once per target
  window and grow the window budget within the source memory cap
- bounded strided reads: prefetch one confirmed target window without assuming a
  full scan
- random reads: avoid speculative fetch beyond the requested hydration window
- short repeated reads: share in-flight window work and keep the source hot

The adaptive layer should request hydration windows; it should not reconstruct
xorbs itself. That preserves the current verified hydration path while letting
NFS absorb OS readahead behavior and model loaders that issue many small reads.

### 7. Make Invalidation Engine-Owned

The current lease-key validation is the first invalidation layer: stale
generation or overlay-view versions fail closed during `read_at`. The engine now
tracks path-scoped overlay invalidation versions and removes only affected
source-cache entries after successful overlay mutations. Exact file changes
invalidate one path, directory removal invalidates the subtree, and rename
invalidates both the old and new subtrees. Refresh/ref-switch and overlay reset
remain full invalidations because they can change broad path meaning. The
path-scoped invalidation maps are bounded; if they fill, the engine safely
compacts them into a full read-source invalidation instead of growing without
limit.

```rust
impl VfsEngine {
    fn invalidate_path(&self, path: &str);
    fn invalidate_subtree(&self, path: &str);
    fn invalidate_rename(&self, old_path: &str, new_path: &str);
    fn invalidate_generation(&self, generation: i64);
}
```

NFS id-table changes remain in `nfs.rs`, but byte-source invalidation belongs
in `engine.rs` because the engine knows which source was selected and which
mutation invalidates it.

The current engine routes invalidation through an explicit event vocabulary:

```rust
enum VfsInvalidation {
    PathChanged { path: String },
    SubtreeRemoved { path: String },
    SubtreeRenamed { old_path: String, new_path: String },
    SnapshotGenerationChanged { old_generation: Option<i64>, new_generation: i64 },
    OverlayReset,
}
```

The mutation interface applies these events while the engine still has enough
context to identify the affected source keys. Runtime status exposes event
counts for path, subtree, rename, generation, reset, and compaction-driven full
invalidations. Protocol adapters may observe the same event vocabulary later
for their own handle tables, but they do not decide which byte sources are
stale.

The invalidation contract should cover refresh/ref-switch paths as deliberately
as write paths. A generation advance is not a write, but it can change the
meaning of a path-to-source lookup. Cached base sources may remain readable only
when their generation/content key still proves the same bytes; otherwise the
next read retries through `open_read`.

### 8. Scale Metadata and Directory Reads Deliberately

NFS `READDIRPLUS` can become a performance cliff in large repositories because
the client asks for names and attrs together. The adapter should remain correct
with the current resolver-backed path, avoid wasted work for cookie-resumed
pages, and grow directory caching only when metrics show large-directory
pressure.

The current best design is staged deliberately:

- Keep the resolver-backed path as the correctness baseline.
- Treat the NFS cookie as the id of the last returned entry and skip attr
  resolution plus prefetch scheduling for earlier candidates.
- Count materialized entries, returned entries, attr resolutions, cookie
  resumes, cookie misses, cookie-skipped entries, large-directory sightings, and
  prefetch errors in runtime status.
- Cache resolver-produced directory candidate pages at the NFS adapter boundary,
  keyed by generation, path, and a cache-owned directory-page version.
- Advance directory-page versions only where ordinary NFS mutations can make a
  cached listing or `READDIRPLUS` attr stale: parent pages for creates, writes,
  setattr, mkdir, and symlink; parent pages plus affected subtrees for remove
  and rename; all pages for refresh/ref-switch generation changes.
- Bound the directory-page invalidation maps and compact to a full cache reset
  when the map fills, mirroring the hf-mount pool lesson of bounded memory plus
  safe stale retry instead of unbounded precision.
- Add deeper streaming/page-by-cookie support only after native client
  benchmarks show repeated large-directory pressure beyond the bounded candidate
  cache.

Recommended shape:

- key directory pages by `generation + path + directory_page_version`
- page by NFS cookie without materializing an entire huge directory when the
  snapshot layer can stream or page entries
- reuse attrs already present in snapshot/overlay metadata instead of
  re-resolving each child path
- invalidate exact parent pages for child attr/name changes, subtrees for
  remove/rename, and all pages for refresh, ref-switch, or overlay reset

This is lower priority than read-source leases, but it is the next likely
scaling limit after large-file reads improve.

### 9. Make Mount Control Backend-Agnostic

NFS should be the default backend only if users get the same operational control
surface they expect from mounts. Background NFS mounts currently run through a
helper process; FUSE background mounts use the coordinator path. The design
target should hide that difference behind one mount-control Interface.

Recommended control surface:

```rust
trait MountControlIndex {
    async fn list(&self) -> Result<Vec<MountListEntry>>;
}

trait MountControl {
    async fn status(&self) -> Result<MountStatus>;
    async fn refresh(&self) -> Result<MountRefreshResult>;
    async fn switch_ref(&self, git_ref: &str) -> Result<MountSwitchResult>;
    async fn shutdown(&self) -> Result<()>;
    async fn stats(&self) -> Result<MountRuntimeStats>;
}
```

The registry entry for a running mount should identify the backend and the
control endpoint:

```rust
struct MountRegistryEntry {
    name: String,
    backend: MountBackend,
    mountpoint: String,
    source: String,
    git_ref: String,
    read_only: bool,
    pid: u32,
    control_endpoint: Option<String>,
    log_path: Option<String>,
}
```

FUSE currently satisfies these operations through coordinator IPC. NFS currently
satisfies `status`, `refresh`, `switch`, and graceful shutdown from the
`crab-nfs-mount` helper over a Unix-domain socket on macOS/Linux and an
authenticated loopback TCP endpoint on platforms without Unix sockets.
The non-Unix TCP endpoint keeps a deterministic mountpoint-derived loopback port
for collision stability, but the token is generated fresh for each helper
launch. The parent launcher passes that endpoint to the helper and writes the
same value into the registry, so live control commands never recompute a stale
token. The registry therefore carries a local control secret: Unix writes keep
the mount registry directory private and the registry/lock files owner-only, and
human status output, logs, and control-endpoint diagnostics redact TCP tokens.
Retained CI smoke artifacts apply the same redaction boundary before upload: the
live registry/control plane keeps the full endpoint, but `mount-list.json`,
`mount-status.json`, `control-status.json`, `writeback-check.json`,
`unmount-check.json`, `control-shutdown.json`, and `remount-check.json` may only
retain `?token=<redacted>` for TCP endpoints.
`vfs/mount_control.rs` now routes list, live status, refresh, switch, and
shutdown across both backends. `list` is intentionally registry-first:
persisted entries are the durable index, and reachable helpers annotate those
entries with live backend facts. That avoids requiring a global NFS daemon just
to discover per-mount helpers. The CLI should route `crab mount list` through
the shared index and route `status`, `refresh`, `switch`, `unmount`, and future
doctor commands through the per-mount live control surface first, then fall back
to persisted registry/cache inspection only when the helper is gone.

This keeps NFS as an adapter at the protocol seam while still making the whole
mount product feel like one system.

## NFS Adapter Responsibilities

The NFS adapter should stay intentionally narrow:

- Convert NFS file handles to internal stable ids.
- Preserve ids across rename and mark replaced/removed subtrees stale.
- Map Crab errors to NFSv3 statuses.
- Track unstable NFS writes until `COMMIT` or shutdown.
- Hold an LRU of `VfsReadLease` values for stateless NFS reads.
- Drop cached leases when an id is removed, renamed over, or explicitly
  invalidated by the engine.
- Treat directory pages, id tables, read leases, and journal paths as derived
  protocol state updated after canonical engine mutations succeed.

The NFS adapter should not:

- Open overlay backing files directly.
- Resolve Crab pointers directly.
- Fetch xorbs directly.
- Own write handles.
- Decide when cached content is still valid.

## Read Lease Pool

NFS needs an internal pool because NFSv3 lacks open/close. The pool should reuse
the useful part of hf-mount's design while changing what is cached.

Recommended shape:

```rust
struct ReadLeasePool {
    entries: Mutex<LruMap<u64, PooledReadLease>>,
    max_entries: usize,
    max_estimated_bytes: usize,
}

struct PooledReadLease {
    lease: VfsReadLease,
    pin_count: u32,
    last_access: u64,
    estimated_bytes: usize,
}
```

Rules:

- Pin before async read work and unpin with an RAII guard.
- Evict only unpinned entries.
- If every entry is pinned, allow temporary overflow and shrink on later insert.
- Hold the pool mutex only for map/pin bookkeeping; never hold it across
  `open_read` or `read_at`.
- On EBADF, stale source, generation mismatch, or overlay version mismatch,
  evict the lease and retry once through `open_read`.
- Bound the pool by estimated memory, not just entry count.

The pool caches `VfsReadLease`, not a protocol-specific file descriptor. That
keeps correctness decisions inside the engine.

Use a monotonic access counter for LRU ordering instead of wall-clock time so
eviction behavior is deterministic in unit tests and independent of clock
resolution.

The retry path is intentionally narrow. One stale-source retry is a correctness
repair for races with refresh or mutation. A second failure should return the
mapped Crab/NFS error so the caller sees the real state instead of spinning
inside the protocol adapter.

## Memory, Backpressure, and Allocation Boundaries

The read-source design must make memory use predictable under OS readahead and
many concurrent readers. The budget should be per mount, not process-global,
because each NFS helper owns one export while FUSE/coordinator deployments may
serve more than one mount.

Budgeted state:

- read leases and their metadata
- adaptive prefetch state
- in-flight hydration windows
- optional overlay/blob-cache file descriptors
- directory pages, if enabled

Backpressure rules:

- Prefer sharing in-flight work over spawning duplicate reconstruction.
- Evict unpinned read leases before refusing new reads.
- Allow temporary overflow only when all candidates are pinned.
- Disable speculative prefetch before blocking user reads.
- Keep the hydration disk cache as the large byte store; do not keep full
  remote files in process memory.

The protocol boundary may still allocate. The current NFS read trait returns a
`Vec<u8>`, so the first optimized design should keep `Bytes` inside the VFS
engine and accept a final adapter copy at the NFS boundary. Only remove that
copy after proving the upstream NFS trait can return a shared byte buffer.

## VFS Engine Read Sources

### Base Pointer Source

`BasePointer` reads call `HydrationService::read_range`. The source cache avoids
re-resolving path metadata and can prefetch adjacent read windows. The data
cache remains keyed by `file_hash + window_start + window_end`.

Correctness proof:

- The file hash is content-addressed.
- The expected window length is checked.
- Cached windows are written through a temp file plus atomic rename.
- Corrupt or incomplete windows are discarded/refetched.

### Base Blob Source

`BaseBlob` reads use the ODB reader and blob cache. The long-term optimization
can keep an open read-only fd to the cached blob file after the blob is cached,
but only as an engine-owned read-source optimization with retained benchmark
evidence. For blobless snapshots, EOF detection must continue to work when the
source key starts with `known_size: None` and learns the size only after the
first successful ODB read.

Correctness proof:

- Git object ids are content-addressed.
- Blob cache paths are derived from object ids.
- If the cached file is missing, the source reopens through the ODB reader.

### Overlay File Source

Overlay sources are the riskiest cache target. Start conservative:

1. Cache overlay source metadata only.
2. Reopen the backing file for each read.
3. Add fd reuse later behind overlay version validation.

Correctness proof before fd reuse:

- Every completed overlay mutation increments an overlay version.
- A cached overlay source is valid only if its version matches the current entry.
- Rename/remove/truncate/reset invalidates the affected subtree.

## Write Stability and Write Journal

NFS writes should remain path-based engine mutations. The adapter should track
NFS write stability, but it should not own writable byte handles.

The adapter uses a small write journal:

```rust
struct NfsWriteJournalEntry {
    path: String,
    overlay_version: u64,
    last_write_stability: NfsWriteStability,
    dirty_since: SystemTime,
    last_sync_error: Option<NfsStatus>,
}
```

The journal is not a second persistence layer for file data. Overlay backing
files remain the source of pending writes. The journal exists to make NFS
`COMMIT`, shutdown sync, `mount status`, and support diagnostics precise.

Rules:

- `UNSTABLE` writes leave a pending journal entry and may return before local
  sync.
- `DATA_SYNC` and `FILE_SYNC` sync the affected overlay path before returning
  stable status.
- `COMMIT` syncs the requested file/range at path granularity, then clears the
  journal entry only after successful sync.
- Shutdown sync attempts every pending path and reports failures with path and
  status.
- Rename and remove update or clear journal entries using the same subtree
  logic as NFS ids.

This keeps NFS write semantics visible without moving write correctness out of
the engine.

## Background Helper and Control Plane UX

The `crab-nfs-mount` helper is part of the NFS backend, not an implementation
detail users should debug manually. Its DX contract should be explicit:

- The foreground `crab mount --backend=nfs --foreground` path and background
  helper path run the same mount pipeline.
- Background startup waits for the native mount to appear and for the helper
  control endpoint to answer, then registers PID, backend, source, mountpoint,
  log path, and control endpoint.
- The current registry contract stores backend, log path, and local control
  endpoint immediately; Unix uses a user-private Unix socket, while platforms
  without Unix sockets use a loopback TCP endpoint with a fresh per-launch token
  supplied from the parent launcher to the helper. Both support status, refresh,
  switch, and graceful shutdown.
- The registry is the local secret handoff for non-Unix control endpoints; human
  display, logs, diagnostics, and retained CI smoke artifacts redact the token,
  while live internal JSON/registry consumers retain the full endpoint needed to
  contact the helper.
- Startup failure returns the helper exit status or timeout plus the log path;
  when preflight now has blockers, the parent launcher appends that blocker
  summary and next action so users do not have to infer platform readiness from
  helper logs alone.
- `crab mount list` shows NFS mounts as first-class running mounts, not as a
  weaker fallback mode.
- `crab mount status --json` includes backend, helper PID, control endpoint
  availability, current head, pending write count, read-lease stats, protocol
  read/directory counters, VFS source/adaptive counters, hydration window
  counters, hydration prefetch counters, and preflight warnings.
- `crab unmount` first asks the helper to shut down, then invokes native
  unmount, then escalates to process termination only after a timeout.

The control channel must be local-only and scoped to the current user. It must
not expose repository contents or control operations to other local users.

## Platform UX

### Shared UX Contract

`crab mount --backend=auto` should prefer NFS when compiled in and when a
platform preflight says the native client can be used. Failures should identify
the exact missing contract:

- NFS feature not compiled.
- Native NFS command/client missing.
- Mountpoint shape invalid.
- Permission/CAP_SYS_ADMIN denied.
- Windows Client for NFS unavailable.
- Loopback NFS port unavailable.
- Helper control endpoint unavailable.

The command should print the next action, not just native stderr.

### Preflight and Doctor

Add a platform-neutral report before invoking native mount commands:

```rust
struct NfsPreflightReport {
    backend_available: bool,
    native_client_available: bool,
    mountpoint_ready: bool,
    loopback_bind_ready: bool,
    control_endpoint_ready: bool,
    privilege_ready: bool,
    warnings: Vec<NfsPreflightMessage>,
    blockers: Vec<NfsPreflightMessage>,
}
```

`crab mount --backend=auto` uses this report to decide whether NFS is viable.
Explicit `crab mount --backend=nfs` uses the same report before creating the
mount pipeline, registry/control metadata, helper process, or native OS mount.
`crab mount doctor --backend=nfs` prints the same report without starting a
mount, and `crab mount doctor --backend=nfs --json` exposes the aggregated
booleans, counts, next action, blockers, and warnings as `nfs_preflight`. The
important design rule is that backend selection, explicit NFS startup, and
doctor output use the same checks, so users do not get conflicting answers or
half-created mount state.
`crab mount doctor --backend=auto` also emits an `auto_decision` summary in
human and JSON output, including the selected backend when one is usable, the
reason, and the NFS next action when the preferred backend is blocked.

Auto fallback to FUSE after an NFS preflight failure is an explicit CLI policy,
not an implementation accident. If auto falls back, the command prints the NFS
blocker summary, the first next action when available, and the selected FUSE
backend so users understand why the preferred backend was not used.

Recommended fallback policy:

- `--backend=nfs`: fail if any required NFS preflight blocker exists.
- `--backend=fuse`: do not run NFS preflight.
- `--backend=auto`: prefer NFS only when required NFS preflight passes; fall
  back to FUSE only when FUSE is compiled in and its own prerequisites pass.
- FUSE fallback is allowed only for blockers that mean "NFS is unavailable on
  this host": missing `mount_nfs`, missing `mount.nfs`, missing Windows Client
  for NFS `mount.exe`, unsupported NFS platform, or Linux NFS mount privilege
  unavailable.
- FUSE fallback is not allowed for blockers that mean "this mount attempt or
  local Crab control state is unhealthy": invalid or occupied mountpoints,
  loopback bind failures, control-endpoint conflicts, helper layout/version
  failures, registry/control mismatches, or native/helper errors after startup
  begins.
- After NFS startup begins, do not reinterpret helper, control, or native-mount
  failures as a FUSE fallback; those are real NFS startup failures with logs and
  doctor next actions.
- If neither backend is viable, print one grouped report with the NFS blockers
  and FUSE blockers.

That policy makes `auto` convenient without hiding the reason the preferred
backend was not selected. It also makes `auto` a backend-selection decision, not
a retry strategy after partially-created NFS state.

### macOS

Use `mount_nfs` with loopback, NFSv3, TCP, local locks, explicit port and
mountport, and large read/write sizes. macOS ships the client; the main user
failure modes are mountpoint permission, stale mounts, or local firewall policy.

Developer checks:

- `command -v mount_nfs`
- mountpoint exists and is not already mounted
- helper can bind loopback
- smoke can read, write, rename, remove, unmount, remount

### Linux

Use `mount.nfs` from `nfs-common` or `nfs-utils`. Native Linux NFS mounting may
require root, passwordless sudo, or container `CAP_SYS_ADMIN`. Crab should not
hide that platform rule.

Developer checks:

- `mount.nfs` present
- effective uid is root, or `sudo -n mount.nfs` is available, or the host
  policy permits the mount syscall
- container diagnostics explain `CAP_SYS_ADMIN` when mount is denied

### Windows

Use Windows Client for NFS with drive targets such as `Z:`. Bind the server to
a generated loopback address on the standard portmapper port because the
Windows client does not expose per-mount NFS port options.

Developer checks:

- `mount.exe` and `umount.exe` from Client for NFS are available
- requested drive letter is unassigned
- generated loopback IP can bind portmapper/NFS service
- authenticated loopback TCP helper control endpoint keeps a stable per-mount
  port, uses a fresh per-launch token, can bind, and rejects the wrong token
- parser recognizes mounted drive output

## Release and Installation Contract

NFS is the default backend only when the shipped artifact includes everything
needed to start the helper path.

Release rules:

- NFS-capable macOS and Linux archives include `crab-nfs-mount` next to `crab`.
  The helper may be a symlink to `crab` when the CLI binary itself was built
  with NFS and without the FUSE loader constraint.
- Windows archives include `crab-nfs-mount.exe` next to `crab.exe` because
  Windows does not use symlink helper layout in the same way.
- Install scripts and Homebrew packaging install the helper with the same
  version as `crab`.
- `crab mount doctor --backend=nfs` reports a missing, mismatched, or
  non-colocated `crab-nfs-mount` helper before a mount attempt, and background
  NFS startup refuses to use a missing, non-colocated, or version-mismatched
  helper so `auto` cannot hide a broken install.
- Helper lookup may still inspect the normal search path for diagnostics, but
  the accepted background helper identity is stricter: it must live next to the
  current `crab` executable and report the same Crab version before any helper
  process is spawned.
- Native NFS smoke scripts are release evidence, not optional local demos. Each
  script must verify its retained report against the full Git commit it records
  before returning success. Expected-commit verifier inputs must be full Git
  object ids, not branch names or short SHAs.
- Hosted release packaging requires a retained NFS Mount Evidence workflow run
  from the exact release commit unless the release workflow is explicitly run
  with the NFS evidence gate disabled for a non-release smoke build.
- The hosted release gate accepts promoted native-smoke verifier thresholds
  through `NFS_RELEASE_VERIFY_ARGS` or the `CRAB_NFS_RELEASE_VERIFY_ARGS`
  repository variable, so reviewed calibration can block packaging instead of
  remaining advisory.
- `make nfs-release-evidence-ci` is the supported local entry point for
  dispatching that retained evidence run. `make release-ci` then consumes the
  resulting run id through `NFS_RELEASE_EVIDENCE_RUN_ID` and forwards
  `NFS_RELEASE_VERIFY_ARGS` to the hosted release workflow. The dispatch helper
  forwards optional baseline run ids, verification/comparison args, and strict
  threshold-suggestion minimum counts so local release evidence requests match
  hosted manual calibration runs. With `NFS_RELEASE_EVIDENCE_WAIT=1`, the same
  helper resolves the target ref to a commit, waits for the matching
  workflow-dispatch run, fails if that run fails, and prints the exact run id
  and `NFS_RELEASE_EXPECTED_RUN_SUFFIX` for release verification. If
  `NFS_RELEASE_EVIDENCE_OUTPUT` is set, it writes the run id, suffix, evidence
  URL, and commit as shell-safe assignments in a sourceable env file so local
  release scripts can consume the result without scraping terminal output.

The existing release checks should remain strict about archive contents so a
release cannot accidentally ship a default NFS CLI without its helper.

Release-grade native smoke evidence should have its own retained artifact
contract:

- Each platform smoke uploads `nfs-smoke-report.json`, `mount-doctor.json`,
  `mount-list.json`, `mount-status.json`, `control-status.json`,
  `native-read-benchmark.json`, `writeback-check.json`, `unmount-check.json`,
  `control-shutdown.json`, `remount-check.json`, and logs from one run root.
- The report may contain the original runner paths, but the verifier must be
  able to resolve retained artifacts by basename beside the downloaded report.
- The report verifier requires `helper_version` to exactly match
  `crab_version`, so retained evidence proves the native helper and CLI came
  from the same release build.
- Every report embeds the full `git_commit` that produced the native helper and
  CLI, and retained-directory/release verification can require that value to
  match the workflow or release commit.
- The retained `mount-list.json` verifier requires a running NFS entry with
  source, mountpoint, log path, and redacted control endpoint metadata, so
  release evidence proves backend-agnostic discovery survived outside the runner
  without uploading a live TCP control token.
- The retained `mount-doctor.json` verifier requires `--backend=nfs` doctor
  output with `ok` NFS feature/helper/helper-version/helper-layout/preflight
  checks, a ready summary whose ok/warn/fail counters match the checks, zero
  failures, ready `nfs_preflight`, no blockers, warning/blocker counts that
  match their lists, a mountpoint matching the retained running NFS mount-list
  entry, and all platform gate booleans true, so release evidence proves the
  same helper identity and preflight users see before any native mount state is
  created.
- The retained `mount-status.json` verifier requires the same mount identity,
  PID, redacted control metadata, full protocol/`READDIRPLUS` counters, and
  positive read-lease hit/miss counters to match the running NFS list entry, so
  release evidence proves status and discovery refer to the same controlled
  helper and that the preferred read path exercised the lease-pool hot path. It
  also
  verifies the full write-journal diagnostic shape: pending paths, oldest dirty
  age, paths with sync errors, sync attempts, latency counters, poisoned state,
  and per-path last-sync errors must be present and internally consistent.
- The retained `control-status.json` verifier requires the same live helper
  identity and runtime status as `mount-status.json`, so release evidence proves
  the status sample came through `crab mount status --live-only --json` and the
  authenticated NFS helper control path, not only persisted registry/cache
  fallback.
- The retained `native-read-benchmark.json` verifier requires protocol,
  read-lease, VFS, and hydration before/after/delta counters plus a mountpoint
  matching the retained running NFS mount-list entry, so native throughput
  evidence can prove both OS-client read amplification and lease-pool reuse for
  the same measured workload.
- The retained `writeback-check.json` verifier requires the same helper
  identity plus append, rename, exclusive-create, delete, `.git` preservation,
  `.git` overwrite rejection, and rename-over-`.git` rejection checks. Linux
  and macOS retained evidence must also prove created symlinks are readable
  through the mount, while Windows evidence intentionally stays on the portable
  file/directory contract because Windows symlink creation depends on host
  policy outside Crab. Together these checks prove NFS writeback semantics
  while the native helper is alive and discoverable, without retaining the
  helper's TCP bearer token.
- The retained `unmount-check.json` verifier requires the same helper identity,
  control endpoint, and a false `mounted_after` result, so release evidence
  proves the orderly shutdown check survived outside the runner transcript.
- The retained `control-shutdown.json` verifier must match
  `unmount-check.json`, so release evidence keeps the graceful control shutdown
  proof as a standalone retained artifact.
- The retained `remount-check.json` verifier requires a running remounted NFS
  helper plus preserved content checks. Linux and macOS must retain symlink
  preservation across remount; Windows must retain the portable file,
  directory, exclusive-create, and `.git` checks. This keeps the design honest:
  Crab validates richer POSIX behavior where the OS contract has it, without
  weakening the cross-platform default-backend gate or retaining the helper's
  TCP bearer token.
- `make nfs-smoke-report-verify` verifies one retained platform report and
  requires its JSON artifacts to resolve beside the report, while `make
  nfs-smoke-report-verify-dir` verifies a downloaded retained-evidence
  directory, requires Linux/macOS/Windows suites, requires the JSON artifacts to
  be present, including the pre-mount doctor artifact, rejects mixed Git commits
  or mixed workflow run-attempt suffixes inside the directory, and can emit a
  summary JSON with the evidence-set Git commit, run-attempt suffix, each
  platform's native-read workload, protocol/VFS/hydration deltas, and derived
  trend metrics. Threshold-suggestion code validates those summary identity
  fields against the per-platform report rows before treating the summary as a
  policy input.
- The same optional `NFS_SMOKE_VERIFY_ARGS` thresholds apply to single-report
  and retained-directory verification, so release jobs can fail on throughput,
  read-amplification, or read-lease hit/miss density regressions without
  changing smoke scripts.
- `make nfs-smoke-report-compare` compares retained baseline/current platform
  reports for the same native sequential-read workload. Optional
  `NFS_SMOKE_COMPARE_ARGS` thresholds can fail release policy when throughput,
  requested-byte amplification, returned-byte amplification, RPC density,
  read-lease hit/miss density, VFS read-call density, resolver-avoidance
  density, or hydration remote-byte amplification regresses beyond a
  platform-calibrated budget.
- `make nfs-threshold-suggestions` converts retained benchmark and native-smoke
  summaries into candidate `NFS_READ_PATH_BENCH_VERIFY_ARGS`,
  `NFS_READ_PATH_BENCH_COMPARE_ARGS`, `NFS_SMOKE_VERIFY_ARGS`, and
  `NFS_SMOKE_COMPARE_ARGS`. Use it after retained evidence runs, review the
  generated values across multiple run attempts, then promote stable budgets to
  repository or workflow variables. The NFS Mount Evidence workflow also uploads
  `nfs-threshold-suggestions.env` and `nfs-threshold-suggestions.json` beside
  the retained smoke summary so hosted calibration runs retain the exact
  suggested args, margins, evidence tier, release blockers, and calibration
  blockers. Threshold suggestions can enforce calibration depth with
  `NFS_THRESHOLD_MIN_BENCHMARK_REPORTS` and
  `NFS_THRESHOLD_MIN_SMOKE_SUMMARIES`, which pass through to
  `--min-benchmark-reports` and `--min-smoke-summaries`. Keep the defaults at
  one for advisory PR/main evidence, then raise them for calibration or release
  policy review once multiple retained attempts exist. For multi-run
  calibration, pass repeated `--benchmark-report` and `--smoke-summary` flags
  directly. When reviewing downloaded workflow artifacts, pass `--benchmark-dir`
  and `--smoke-dir`, or use `NFS_THRESHOLD_BENCHMARK_REPORTS`,
  `NFS_THRESHOLD_BENCHMARK_DIRS`, `NFS_THRESHOLD_SMOKE_SUMMARIES`, and
  `NFS_THRESHOLD_SMOKE_DIRS` through the Make target. Set
  `NFS_THRESHOLD_REQUIRE_RELEASE_GRADE=1` when a threshold suggestion must
  prove Linux/macOS/Windows retained smoke coverage, and set
  `NFS_THRESHOLD_REQUIRE_CALIBRATION_READY=1` when it must also prove the
  configured retained-run depth before emitting promotable policy.

This keeps release defaulting honest: the build gate proves NFS compiles without
FUSE dependencies, the archive gate proves the helper is shipped, and the
release NFS evidence gate proves the retained native OS-client path was green
on the exact commit being packaged. Threshold suggestions complete the loop by
turning that retained evidence into reviewed performance policy rather than
hard-coded guesses.

## Developer UX

Keep implementation boundaries aligned with how developers debug the system:

| Module | Responsibility |
|--------|----------------|
| `vfs/engine.rs` | canonical read/write interface, read leases, source identity/cache, adaptive prefetch state, source metrics, mutation invalidation |
| `vfs/read_source.rs` | future split point for source identity, adaptive prefetch state, and source metrics if `engine.rs` grows too large |
| `vfs/read_lease_pool.rs` | bounded LRU, pinning, stale retry, memory accounting |
| `vfs/hydration.rs` | pointer window reconstruction and cache |
| `vfs/nfs.rs` | NFSv3 protocol adapter, stable id table, current write journal |
| `vfs/nfs/write_journal.rs` | future split point for NFS unstable-write and commit tracking if `nfs.rs` grows too large |
| `vfs/nfs_mount.rs` | native NFS platform lifecycle |
| `vfs/nfs_preflight.rs` | future split point for platform preflight and doctor report |
| `vfs/mount_runtime.rs` | shared live refresh/switch implementation for mounted views |
| `vfs/mount_control.rs` | registry-first list plus backend-agnostic live status/refresh/switch/shutdown routing |
| `vfs/nfs_control.rs` | NFS helper control channel and current NFS Adapter for mount control |
| `vfs/fuse.rs` | FUSE protocol adapter and stateful file-handle `VfsReadLease` retention |
| `cmd/mount.rs` | backend selection, helper spawning, user-facing options, mount doctor diagnostics |

This split gives a contributor a direct path:

- protocol issue: start in `nfs.rs` or `fuse.rs`
- platform mount issue: start in `nfs_mount.rs`
- background mount command issue: start in `mount_control.rs`, then follow the
  backend-specific edge to `nfs_control.rs`, `ipc_client.rs`, or
  `mount_runtime.rs`
- stale read issue: start in `engine.rs`
- remote read performance issue: start in `hydration.rs`
- CLI UX issue: start in `cmd/mount.rs`

Avoid splitting files until the new responsibility exists. `vfs/read_lease_pool.rs`
is justified because it owns LRU, pinning, memory accounting, and stale retry
behavior; future files should meet the same bar.

## Observability

Add structured metrics and trace fields before optimizing behavior. The current
NFS runtime status already exposes the protocol-level NFS read and
`READDIRPLUS` counters, lease-pool state, write-journal state, VFS source and
adaptive read counters, VFS source-cache counters, stale lease rejection
counters, resolver calls avoided by source-cache hits, and hydration
read-window/prefetch pressure, plus helper lifecycle timing for server bind,
native mount, and total startup. NFS write-journal status also exposes sync
attempt counts and latency, and the helper logs native-unmount,
write-journal-drain, and total shutdown latency. NFS runtime status also exposes
directory-page cache hit/miss/eviction pressure. Native smoke reports also carry
native sequential-read benchmark artifacts with NFS protocol, VFS source-cache,
adaptive-read, and hydration deltas plus efficiency ratios. Retained evidence
verification also records the report set and platform coverage in a summary JSON
when requested, and retained native smoke comparison can enforce throughput and
amplification trend thresholds once platform baselines exist. Retained
mount-status and native-read verification also require the VFS
source-cache/resolver-avoidance and hydration read-window/chunk counters, so
native smoke artifacts cannot pass after losing the metrics needed to tune the
preferred read path. Platform preflight aggregation is exposed through
`crab mount doctor --json`; the workflow summary renders the same retained
benchmark/native-smoke evidence as Markdown tables for quick calibration review.
`make nfs-threshold-suggestions` uses those retained summaries to emit
candidate verify/compare argument strings with configurable safety margins, so
the calibration artifact names the exact gate that would enforce the observed
budget. Remaining metrics should focus on retained platform calibration.

- NFS read RPC count, bytes, size histogram
- NFS read lease hit/miss/evict/stale-retry count
- read lease memory budget, temporary overflow count, and open fd count
- VFS source-cache hit/miss/evict/stale-evict/invalidation count and memory
  budget
- resolver calls avoided by source-cache hit
- hydration read-window hit/miss/inflight-wait/prefetch count
- object-store bytes fetched versus bytes returned
- overlay source invalidation count by operation
- adaptive read classification count by source type
- `READDIRPLUS` materialized entries, returned entries, attr resolutions,
  cookie resumes/misses, skipped entries, large-directory sightings, and prefetch
  errors
- directory page hit/miss/evict/stale-evict count
- NFS write journal pending path count, sync attempts, failures, latency,
  poisoned state, paths with sync errors, and per-path last sync errors
- mount preflight failure reason by platform
- helper server-bind, native-mount, startup, control-channel status, and
  shutdown latency
- retained native NFS smoke report path, suite, platform, run id, `git_commit`,
  helper version, and `mount status --json` artifact with lifecycle timing plus
  native sequential read benchmark artifact with NFS read
  RPC/requested-byte/returned-byte deltas and requested-byte/returned-byte/RPC-per-MiB
  efficiency ratios
- retained NFS read-path benchmark report path, scenario set, workload options,
  platform, toolchain, full Git commit, dirty-worktree status, and lease/path
  throughput ratios

Performance work should be accepted only with before/after evidence from these
metrics or benchmarks.

## Benchmark Matrix

Benchmarks should run in release mode and compare the current lease-pooled read
flow against the adaptive read-source design. Debug builds are not useful
evidence for NFS read-path changes.

`make nfs-read-path-bench` runs the local synthetic pointer benchmark
`cargo bench --bench nfs_read_path_bench --no-default-features --features nfs`.
It emits JSON records for sequential and random pointer reads through both
path-level `VfsEngine::read` and reused `VfsReadLease` reads, plus
overlay-modified rereads through both path-level reads and reused leases after
copy-on-write promotion. This is the portable engine-level baseline for the NFS
design; the native NFS smoke scripts remain the OS-client proof.

`make nfs-read-path-bench-report` is the retained-evidence form. It runs the
same release-mode benchmark, validates that all expected scenarios are present,
and writes a JSON report with platform/toolchain metadata, full Git commit
identity, dirty-worktree status, and lease/path throughput ratios. Those ratios
are derived fields: verification recomputes them from the retained scenario
records before thresholds can use them. `make nfs-read-path-bench-report-verify`
verifies an existing report without rerunning the benchmark and can require the
embedded commit to match the workflow commit.
Optional `NFS_READ_PATH_BENCH_VERIFY_ARGS` thresholds can fail verification when
pointer-sequential, pointer-random, or overlay-modified lease/path throughput
ratios fall below platform-calibrated release gates.

`make nfs-read-path-bench-report-compare` compares retained baseline/current
reports without rerunning the benchmark. It validates both reports, rejects
incompatible workload shapes, emits an optional JSON comparison summary, and can
fail when any scenario throughput or lease/path ratio regresses beyond
configured `NFS_READ_PATH_BENCH_COMPARE_ARGS` thresholds.

`make nfs-smoke-report-compare` is the native counterpart. It compares retained
baseline/current smoke reports after their platform smoke has already run,
requires the same smoke suite, platform, and native sequential-read workload,
emits protocol, read-lease, VFS, and hydration trend metrics, and can fail on
throughput, NFS read-amplification, read-lease hit/miss density, VFS read-call
density, resolver-avoidance density, or hydration remote-byte regression
thresholds via `NFS_SMOKE_COMPARE_ARGS`.

`make nfs-threshold-suggestions` is the calibration bridge between evidence and
policy. Given retained synthetic benchmark reports, retained native smoke
summaries, or downloaded retained artifact directories from several attempts, it
emits shell-ready candidate verify/compare arguments plus a JSON record of the
evidence paths and margins used. Benchmark ratio thresholds are derived from the
lowest observed lease/path ratio; native smoke throughput uses the lowest
observed platform throughput; amplification and RPC-density limits use the
highest observed values; read-lease hit density uses the lowest observed value,
and read-lease miss density uses the highest observed value. The best practice
is to run it on several retained main/manual evidence attempts, use the worst
stable platform values with margin, raise `NFS_THRESHOLD_MIN_BENCHMARK_REPORTS`
and `NFS_THRESHOLD_MIN_SMOKE_SUMMARIES` when strict calibration should fail on
too little evidence, require clean benchmark/native commits for promotable
evidence, and then promote the reviewed strings to workflow variables. A single
noisy run or copied ratio summary should never become the release policy by
itself. Strict benchmark and native-smoke calibration also count distinct
retained run-attempt suffixes, so copied or duplicated reports from one
workflow attempt cannot satisfy a multi-attempt evidence budget.

Required workloads:

| Workload | Proves |
|----------|--------|
| Sequential read of one large pointer file | adaptive prefetch and lease reuse improve scan throughput without extra correctness risk |
| NFS client readahead against one large pointer file | lease pool absorbs stateless NFS read bursts |
| Random safetensors-style reads | adaptive prefetch does not overfetch random workloads |
| Hot reread of cached pointer windows | resolver/source cache avoids repeated source selection |
| Overlay-modified hot reread through path and lease | invalidation prevents stale overlay bytes and overlay-source reuse stays in `VfsEngine` |
| Refresh while reads are in flight | generation/source keys stale safely and retry once |
| Huge directory `READDIRPLUS` | bounded candidate caching avoids repeated resolver/id-table work; deeper streaming pagination is justified only if pressure remains |
| Background helper startup/status/refresh/switch/shutdown | NFS DX matches the preferred-backend promise |
| Retained native smoke report verification | release evidence proves the helper/control/status path after the native smoke exits |

Acceptance criteria:

- Optimized reads must not regress byte-for-byte reconstruction.
- Sequential and NFS-readahead workloads must reduce resolver/source-selection
  work per returned byte.
- Random reads must not fetch materially more object-store bytes than the
  requested hydration windows.
- Memory use must stay within the configured per-mount budget outside
  documented temporary overflow.
- Any claimed improvement must include the baseline number, optimized number,
  workload description, platform, and backend options.

## Correctness Invariants

1. A completed write, truncate, remove, rename, reset, refresh, or switch
   invalidates every cached read source that could serve stale bytes; NFS
   refresh/switch also clears protocol read leases and directory pages.
2. NFS ids remain stable across rename and become stale when their subtree is
   removed or replaced.
3. Base pointer reads are keyed by `file_hash + size`; the path alone is never a
   cache proof.
4. Overlay reads are keyed by overlay version; path alone is never a cache proof.
5. NFS write journal entries are cleared only after successful local sync.
6. No `std::sync::Mutex` guard is held across `.await`.
7. No NFS-only read path bypasses `VfsEngine`.
8. Refresh either preserves a source's generation or invalidates that source.
9. One stale-source retry is allowed; repeated stale failures are returned to
   the caller.
10. Directory page caches are invalidated by generation changes plus NFS
    adapter path/subtree directory-page versions; unrelated directory pages stay
    valid after ordinary overlay mutations.
11. NFS write journal entries never claim data is stable until the local overlay
    sync succeeds.
12. NFS shutdown drain continues after a per-path sync failure; successful
    paths are cleared, failed paths stay pending, and status reports the
    failure.
13. The NFS server and helper control channel are local-only.
14. Backend-agnostic mount commands do not silently degrade for NFS mounts.
15. A release that defaults to NFS includes an installed `crab-nfs-mount`
    helper for that platform.
16. NFS-only builds never depend on FUSE IPC/coordinator modules.
17. `--backend=auto` never hides mountpoint, helper identity, control endpoint,
    registry, or post-startup native NFS failures behind a FUSE fallback.
18. NFS mutation handlers never move ids, clear read leases, or rewrite journal
    paths before the corresponding engine mutation succeeds.
19. FUSE file-handle read leases are opened only for read-capable handles,
    replaced only through a canonical stale retry, cleared after a successful
    write on that handle, and dropped on release.
20. NFS id-table and exclusive-create verifier operations must persist verifier
    updates before replacing the live in-memory table.

## Rollout Plan

Current implementation has completed the foundation of the engine rename, read
lease identity, NFS read-lease pooling, write journaling, native preflight,
control-endpoint bind probing, `auto` backend selection, mount registry/status
metadata, shared live mount doctor output, protocol read/directory counters,
control for list, status, refresh, switch, and shutdown, the shared
refresh/switch runtime, shared VFS source/adaptive counters, hydration
read-window counters, sequential pointer-read prefetch, confirmed strided
target-window prefetch, bounded engine read-source caching with
resolver-avoidance counters, structured helper shutdown-latency logs, and
cookie-aware `READDIRPLUS` work avoidance with directory-pressure metrics plus a
bounded NFS directory-page cache.
The FUSE adapter now uses the same `VfsReadLease` contract from its real
open/read/write/release lifecycle: read-capable handles preopen a lease, reads
reuse the handle lease, stale reads reopen through the engine and replace the
handle lease, writes clear the lease, and release drops it. This is deliberately
not the NFS `ReadLeasePool`; it is the stateful-backend half of the same shared
read contract.
The NFS control plane now generates non-Unix loopback TCP control tokens per
helper launch, passes the selected endpoint from the background launcher to the
helper, registers that exact endpoint for future live control commands, redacts
TCP tokens in human status output, logs, diagnostics, and retained native-smoke
artifacts, and keeps Unix registry files private because the registry is now a
local control-secret handoff.
The shared engine tests now cover cached read leases racing with write,
truncate, rename, and remove mutations, and the synthetic NFS read-path
benchmark covers pointer and overlay-modified read-lease workloads.
Backend-agnostic mount-control tests now cover registry-discovered NFS list,
live status, graceful shutdown, and refresh/switch routing through the NFS
helper endpoint. `crab mount status` now has explicit NFS-only coverage proving
that a stale helper control endpoint falls back to persisted registry/cache
status instead of failing the status command, while
`crab mount status --live-only` fails loudly when live helper control is
unavailable. `crab unmount --all` now attempts the same live control shutdown
per registered mount before falling back to stale PID/native unmount cleanup, so
bulk unmounts preserve the NFS helper drain path.
`crab mount refresh` and `crab mount switch` now fail with an actionable
no-live-mount error when no controlled mount is found, instead of falling
through into ordinary mount argument validation or backend-specific fallback
messages.
`make nfs-read-path-bench-report` now turns that benchmark into retained,
validated JSON evidence, and `make nfs-read-path-bench-report-compare` compares
retained baseline/current reports with optional trend-regression thresholds.
Native NFS smoke scripts now emit and self-verify a common JSON report, and
`make nfs-smoke-report-verify` validates a single retained report plus its
artifact files.
`make nfs-smoke-report-verify-dir` validates a downloaded retained-evidence
directory, requires Linux/macOS/Windows suite coverage, resolves retained
artifacts beside each report, and can write a JSON summary with per-platform
native-read workloads, derived trend metrics, and raw protocol, read-lease, VFS,
and hydration deltas.
`make nfs-smoke-report-compare` compares retained baseline/current native smoke
reports for the same suite/platform/workload, emits an optional comparison
summary, and can fail on configured throughput, read-amplification,
read-lease-density, VFS call-density, resolver-avoidance, or hydration trend
regressions through `NFS_SMOKE_COMPARE_ARGS`.
`make nfs-smoke-report-compare-dir` compares full retained baseline/current
native smoke directories by suite, requires platform coverage, and emits a
single JSON summary for release review. The
`.github/workflows/nfs-mount.yml` workflow wires those pieces together: PRs run
the NFS feature gate and upload a verified synthetic read-path benchmark report,
while main/manual runs also execute native Linux, macOS, and Windows smokes,
upload each run root, download the retained artifacts, and verify the retained
set. It also appends concise Markdown tables for the benchmark ratios,
native-smoke metrics, optional trend comparisons, and suggested threshold args
to the GitHub step summary, and uploads the threshold suggestion env/JSON files
with the retained smoke summary.
When `NFS_READ_PATH_BENCH_BASELINE_RUN_ID` or
`NFS_SMOKE_BASELINE_RUN_ID` is configured, the workflow downloads retained
baseline evidence from that run and compares current benchmark/smoke evidence
against it. Manual dispatch can also override the benchmark and native-smoke
verify/compare argument strings, so threshold calibration runs do not require
changing repository variables. The verifier accepts optional native-read
threshold arguments through `NFS_SMOKE_VERIFY_ARGS`, so CI can fail on
throughput, amplification, or read-lease-density regressions without changing
the smoke scripts.
NFS-only targeted tests now compile without FUSE IPC/coordinator modules, while
FUSE IPC properties remain covered when the FUSE feature is enabled. `make
nfs-feature-gate` is the local DX gate for that compile-time contract plus the
native-smoke, benchmark-report, evidence-summary, and release-evidence dispatch
self-tests. `make
nfs-release-gate` is the local release evidence gate for downloaded retained
native smoke artifacts, including exact run-attempt suffix binding when release
evidence is required. The hosted release workflow blocks packaging on both the
NFS feature gate and the exact-commit retained native evidence gate.
`make nfs-release-evidence-ci` is the matching dispatch helper for creating the
retained evidence run with optional calibration inputs, including strict
minimum-count threshold-suggestion controls, before `make release-ci` submits
the release workflow. Its wait mode closes the operator loop by returning the
successful run id and attempt suffix instead of requiring manual `gh run list`
lookup, and its optional env-file output lets scripts hand those exact values to
the release gate without parsing logs. The release workflow forwards promoted
`NFS_RELEASE_VERIFY_ARGS` into `make nfs-release-gate`, so calibrated
native-smoke thresholds can be enforced at packaging time. `make
nfs-threshold-suggestions` now converts
retained benchmark and native-smoke summaries into candidate verify/compare
argument strings with explicit margins, giving maintainers a reproducible way
to review performance budgets before promoting them to repository variables.
Its evidence tier keeps retained benchmark and native-smoke run-attempt
suffixes visible and requires each retained smoke summary to be release-shaped
on its own, so calibration can use multiple complete retained runs without
stitching Linux, macOS, and Windows fragments from different runs into a
misleading policy input. Strict calibration also requires the requested number
of distinct benchmark and native-smoke run-attempt suffixes, not merely
duplicated report or summary files.
The dispatch helper self-test covers successful wait/output handoff, benchmark
and native-smoke calibration input forwarding, and fail-closed rejection for
failed hosted evidence runs and unexpected workflow head commits, so the
release helper cannot silently hand a stale, failed, or under-calibrated run to
packaging.
The remaining rollout should focus on adaptive prefetch tuning from retained
baselines, deeper streaming directory pagination, collecting first green
retained native smoke workflow run ids on release commits, and choosing
platform-specific baseline run ids and threshold budgets through that reviewed
suggestion loop.

### Phase 0: Baseline Proof

- Keep NFS protocol read and `READDIRPLUS` counters exposed through mount
  status.
- Keep VFS source-cache/source/adaptive counters and hydration-window/prefetch
  counters exposed through NFS runtime status.
- Keep directory-pressure and directory-page cache metrics exposed through NFS
  runtime status.
- Keep helper lifecycle timing exposed through NFS runtime status.
- Keep write-journal sync latency exposed through NFS runtime status.
- Keep helper shutdown, native-unmount, and write-journal drain latency in
  structured helper logs.
- Keep resolver-cache avoidance visible through VFS source-cache hit/miss,
  stale-eviction, invalidation, memory, and `resolver_calls_avoided` counters.
- Keep `nfs_read_path_bench` as the release-mode engine benchmark for
  sequential pointer reads, random pointer reads, reused read leases, and
  overlay-modified path/lease rereads.
- Keep `make nfs-read-path-bench-report` and
  `make nfs-read-path-bench-report-verify` as the retained benchmark evidence
  path.
- Keep `make nfs-read-path-bench-report-compare` as the retained synthetic
  benchmark trend comparison path for baseline/current reports.
- Keep synthetic benchmark ratio thresholds opt-in through
  `NFS_READ_PATH_BENCH_VERIFY_ARGS` so CI can fail on lease/path regressions
  after platforms have retained baselines.
- Keep synthetic benchmark trend thresholds opt-in through
  `NFS_READ_PATH_BENCH_COMPARE_ARGS` so release policy can fail on throughput or
  lease/path-ratio regressions after platforms have retained baselines.
- Keep native NFS smoke reports carrying retained sequential-read artifacts with
  protocol, VFS source-cache/adaptive, and hydration deltas plus efficiency
  ratios as the current native client readahead evidence path.
- Keep native NFS smoke reports carrying retained `mount-doctor.json` artifacts
  so exact-run evidence proves the platform preflight/doctor contract before
  the native mount is created.
- Keep `make nfs-smoke-report-verify-dir` as the retained native-smoke
  evidence-set verifier so downloaded CI artifacts can be revalidated with
  Linux/macOS/Windows coverage, local artifact resolution, and a summary of
  per-platform native-read metrics plus protocol/VFS/hydration deltas.
- Keep `make nfs-smoke-report-compare` as the retained native-smoke trend
  comparison path for baseline/current reports from the same platform workload.
- Keep `make nfs-smoke-report-compare-dir` as the retained native-smoke
  trend comparison path for full baseline/current platform evidence sets.
- Keep `.github/workflows/nfs-mount.yml` as the CI evidence path that runs the
  feature gate and retained synthetic benchmark report on PRs, runs native
  smokes on main/manual invocations, uploads per-platform run roots, downloads
  them, verifies the retained set, and optionally compares retained
  baseline/current benchmark and native-smoke evidence when baseline run ids are
  configured. Keep manual-dispatch inputs for benchmark and native-smoke
  verify/compare args so calibration runs can test thresholds before promoting
  them to repository variables. Keep the GitHub step summary rendering for
  retained benchmark ratios, native-smoke metrics, trend comparisons, and
  threshold suggestions so the calibration signal is visible without artifact
  spelunking. Keep release-evidence dispatch helper changes inside the workflow
  path filters because `make nfs-feature-gate` owns that helper's local
  self-test. Keep the threshold suggestion env/JSON files uploaded with the
  retained smoke summary.
- Keep native smoke verification thresholds opt-in through
  `NFS_SMOKE_VERIFY_ARGS` so each platform can set realistic release gates from
  retained evidence instead of sharing brittle hard-coded numbers.
- Keep native smoke trend thresholds opt-in through `NFS_SMOKE_COMPARE_ARGS` so
  release policy can fail on throughput, read-amplification,
  read-lease-density, VFS read-call density, resolver-avoidance, or hydration
  remote-byte regressions after each platform has a retained baseline.
- Keep `make nfs-threshold-suggestions` as the reviewed calibration path from
  retained evidence to candidate benchmark/native-smoke verify and compare args.
  Use its minimum-count knobs to fail calibration when too few retained attempts
  are present.
- Add release-mode native NFS client benchmark suites only after the smoke
  artifact shows that trend thresholds need tighter isolation.
- Keep the current implementation as the correctness baseline.

### Phase 1: Engine Interface Cleanup

- Keep `VfsEngine` as the canonical shared engine name.
- Do not reintroduce `FuseEngine` aliases unless a shipped public contract
  requires them.
- Keep FUSE and NFS adapters on the same engine interface.
- Run existing NFS, FUSE, hydration, overlay, and mount tests.

### Phase 2: Read Source Identity

- Keep `ReadSourceKey` and source resolution in the engine.
- Keep `read(path, offset, size)` as a wrapper around `open_read(path)` plus
  `read_at`.
- Keep unit tests for source key generation for base pointer, base blob, and
  overlay entries.

### Phase 3: Engine Source Cache and Adaptive Prefetch

- Keep adaptive read-pattern classification on VFS read leases.
- Keep sequential pointer reads scheduling the next hydration window through
  the shared hydration cache, with duplicate speculative requests suppressed
  until prefetch failure permits retry.
- Keep confirmed positive-stride reads scheduling one target hydration window
  through the same bounded prefetch claim path.
- Keep the bounded LRU read source cache in the engine.
- Keep path/subtree cache invalidation after successful overlay mutations and
  full invalidation after refresh/ref-switch generation changes or overlay
  reset.
- Keep VFS invalidation events and event counters as the shared vocabulary for
  path, subtree, rename, generation, overlay reset, and compaction-driven full
  reset behavior.
- Extend prefetch decisions only when observed adaptive classifications justify
  more lookahead.
- Allow protocol adapters to observe the same invalidation event vocabulary only
  when their own handle tables need that shared stream.
- Keep concurrency tests for read/write, read/truncate, read/rename, and
  read/remove races.

### Phase 4: NFS Read Lease Pool

- Use the shared `ReadLeasePool` keyed by NFS file id.
- Retry once on stale lease.
- Evict on id removal/replacement and subtree rename/remove.
- Keep unit tests for pinned eviction and stale retry.
- Keep adapter-level remove/rename tests proving ids, read leases, directory
  pages, and write-journal paths move or clear after canonical engine mutation.
- Validate native macOS and Linux smokes before defaulting release builds to
  NFS.

### Phase 5: Write Journal and Metadata Scaling

- Keep `NfsWriteJournal` as the canonical NFS write-stability tracker.
- Expose pending-count and last-sync-error snapshots through mount status before
  relying on it for user-facing diagnostics.
- Keep directory-pressure metrics for materialized entries, attr resolutions,
  cookie resumes/misses, skipped entries, large-directory sightings, and prefetch
  errors.
- Keep the bounded NFS directory-page candidate cache keyed by generation, path,
  and cache-owned path/subtree directory-page versions.
- Add deeper streaming/page-by-cookie directory support only after
  large-directory measurements justify the extra surface.

### Phase 6: Mount Control Plane

- Keep registry-first list and backend-agnostic live
  status/refresh/switch/shutdown routing in `vfs/mount_control.rs`.
- Keep backend, log path, and control endpoint in the registry for background
  NFS mounts; the registered endpoint must be the endpoint the helper actually
  bound, not a recomputed value.
- Keep background NFS startup from reporting success until both the native mount
  and helper control endpoint are live.
- Keep Unix NFS helper status/refresh/switch/shutdown reachable through the
  local control socket, fail closed if the socket directory cannot be made
  user-private, remove the socket on graceful shutdown, and keep the registry
  directory/files owner-only on Unix.
- Keep non-Unix NFS helper status/refresh/switch/shutdown reachable through an
  authenticated loopback TCP endpoint with a stable port and fresh per-launch
  token.
- Keep NFS refresh/switch invalidating the protocol read-lease pool and
  directory-page cache after the shared mount runtime publishes the new
  generation.
- Keep persisted registry/cache inspection as the fallback for stale helpers.

### Phase 7: Sequential Prefetch Tuning

- Keep per-source sequential detection and next-window prefetch.
- Keep confirmed positive-stride reads limited to one target-window lookahead.
- Keep repeated and random reads unspeculative.
- Tune adaptive thresholds from retained benchmark and native-smoke evidence,
  using `make nfs-threshold-suggestions` to turn stable retained runs into
  candidate verify/compare args.
- Keep the hydration window cache as the only remote byte cache.
- Tune memory budget from measured workloads, not static guesses.

### Phase 8: Platform UX and Release Hardening

- Keep `crab mount doctor --backend=nfs` wired to the same preflight report as
  `--backend=auto`.
- Keep preflight failures platform-specific and actionable.
- Keep NFS control-endpoint bind probing in preflight so live status, refresh,
  switch, and shutdown are not discovered broken after a native mount succeeds.
  Unix preflight must replace stale generated sockets and empty placeholders,
  bind the endpoint, then remove the probe socket.
- Keep `make nfs-feature-gate` compiling NFS-only tests without FUSE
  IPC/coordinator imports and running FUSE-specific IPC/coordinator properties
  with the FUSE feature enabled. Keep the native NFS smoke report, NFS
  read-path benchmark report, evidence-summary, and release-evidence dispatch
  self-tests in the same gate. Keep `make nfs-smoke-script-check` in that gate
  so Linux/macOS smoke syntax, Linux/macOS/Windows retained-evidence wrapper
  contracts, and, when the configured `POWERSHELL` executable is installed,
  Windows PowerShell syntax fail before release evidence wrappers drift. The
  wrapper contract check is local proof of artifact shape, helper layout,
  platform-specific write/remount coverage, and redaction; it does not replace
  native macOS/Linux/Windows NFS smokes.
- Keep release packaging dependent on both the NFS feature gate and the retained
  native NFS evidence gate.
- Keep helper version/layout checks in mount doctor and release validation.
- Keep native smoke scripts for macOS, Linux, and Windows as structured release
  evidence and verify retained reports, including lifecycle, write-sync, and
  native sequential-read counters plus pre-mount doctor output, with
  `make nfs-smoke-report-verify`. Each platform script must pass its measured
  full Git commit to that verifier before reporting success.
- Keep retained native smoke artifact downloads verifiable with
  `make nfs-smoke-report-verify-dir`, which requires all three platform suites
  and validates the downloaded artifacts rather than only the report envelope.
- Keep retained native smoke baseline/current comparisons verifiable with
  `make nfs-smoke-report-compare` for one platform and
  `make nfs-smoke-report-compare-dir` for full retained evidence sets once
  platform baselines exist.
- Keep `.github/workflows/nfs-mount.yml` wired to the native smoke scripts,
  synthetic benchmark report, artifact upload/download, retained-directory
  verification, and optional baseline/current trend comparison so release
  evidence is reproducible outside an individual runner session. Keep its
  benchmark and native-smoke step summaries wired through
  `scripts/nfs-evidence-summary.py` so release reviewers see the evidence shape
  before opening artifacts.
- Keep `.github/workflows/release.yml` wired to a retained NFS evidence run id
  so hosted release packaging refuses to proceed unless the NFS Mount Evidence
  workflow succeeded on the exact release commit and the downloaded smoke
  reports verify with Linux/macOS/Windows coverage and promoted release
  verifier thresholds.
- Keep `make nfs-release-evidence-ci` as the documented way to dispatch the
  NFS Mount Evidence workflow for a release ref and forward optional threshold
  calibration inputs, including strict retained-run minimum counts for threshold
  suggestions. Keep `NFS_RELEASE_EVIDENCE_WAIT=1` available so the same command
  can wait for the exact matching workflow-dispatch run and print the release
  run id plus run-attempt suffix. Keep `NFS_RELEASE_EVIDENCE_OUTPUT` available
  so scripted release flows can persist the successful run id, run-attempt
  suffix, evidence URL, and Git commit as shell-safe assignments in a
  sourceable env file.
- Keep `make nfs-threshold-suggestions` as the documented way to generate
  candidate threshold args from retained evidence before those args are copied
  into workflow variables or release commands. Keep
  `NFS_THRESHOLD_MIN_BENCHMARK_REPORTS` and
  `NFS_THRESHOLD_MIN_SMOKE_SUMMARIES` available so strict calibration runs can
  require multiple retained attempts.

## Test Gates

Minimum proof before making NFS the preferred backend by default:

- Unit tests for source identity and invalidation.
- Multi-threaded tests for concurrent reads and mutations.
- NFS id table tests for rename, remove, and replaced subtree staleness.
- Hydration read-window tests for boundaries, EOF, and oversized reads.
- Adaptive read tests for sequential next-window prefetch, strided target-window
  prefetch, random-read restraint, and shared in-flight work.
- Engine source-cache tests for repeated opens, stale generation, and mutation
  invalidation.
- `make nfs-feature-gate` proof for NFS-only compile/test coverage,
  `ReadLeasePool` LRU/pinning/overflow/invalidation tests, NFS adapter
  stale-pooled-lease retry through the canonical VFS path, shared VFS
  read-lease/source-cache identity tests, hydration read-window boundary/EOF
  tests, adaptive read-pattern tests for sequential/strided/random restraint,
  NFS protocol pressure counters, `READDIRPLUS` cookie and directory-page cache
  behavior, adapter-level `UNSTABLE`/stable `WRITE` and `COMMIT` protocol
  tests, write-journal stability/diagnostic and shutdown-drain tests, explicit
  NFS preflight enforcement, NFS helper layout/version tests, authenticated TCP
  control wrong-token rejection, native smoke wrapper syntax, FUSE IPC response
  contracts, native smoke report verifier
  self-test, benchmark report verifier self-test, evidence summary self-test,
  and release evidence dispatch wait/output self-test.
- `make nfs-read-path-bench-report-verify` for each retained synthetic
  read-path benchmark report, with optional lease/path ratio thresholds once a
  platform baseline is established.
- `make nfs-read-path-bench-report-compare` for retained synthetic benchmark
  baseline/current report pairs, with optional throughput and lease/path-ratio
  trend thresholds once a platform baseline is established.
- Release workflow proof that packaging jobs depend on the NFS feature gate and
  the retained native NFS evidence gate.
- Adapter-level write tests proving `UNSTABLE` writes remain pending until
  `COMMIT`, while stable writes sync and clear the journal before reply.
- Write journal tests for unstable write, commit, rename, remove, and shutdown
  sync behavior, including clearing successful drains and retaining failed
  drains with diagnostics while other paths continue to sync.
- `READDIRPLUS` tests for cookie resume, skipped attr/prefetch work, directory
  page cache hit/miss/stale eviction, and large-directory pressure before
  enabling deeper streaming directory pagination.
- Backend-agnostic mount-control tests for list, NFS `status`, `refresh`,
  `switch`, and shutdown.
- NFS control tests proving Unix control endpoint probes create a user-private
  socket directory, replace stale sockets and empty placeholders before binding,
  remove probe sockets afterward, and remove the real control socket on graceful
  shutdown.
- NFS adapter read tests proving a stale pooled lease is evicted, counted, and
  retried once through `VfsEngine::open_read`/`read_at`.
- NFS adapter mutation tests proving `remove` and `rename` update derived NFS
  state after the engine mutation: old ids become stale or move, affected read
  leases are evicted, directory pages are invalidated, and write-journal entries
  are removed or renamed with the subtree. Rejected engine mutations must leave
  the same derived protocol state unchanged.
- Release/archive tests proving `crab-nfs-mount` is shipped and installed with
  NFS-capable builds.
- `make nfs-smoke-report-verify` for each retained native NFS smoke report and
  its artifact files.
- `make nfs-smoke-report-verify-dir` for a retained native NFS smoke artifact
  directory containing Linux, macOS, and Windows reports plus their JSON
  artifacts, including `mount-doctor.json`, with a summary JSON carrying
  native-read workloads, metrics, and protocol/VFS/hydration deltas.
  Release-grade verification also requires the embedded `git_commit` in every
  report to match the release commit.
- `make nfs-smoke-report-compare` for retained native NFS smoke
  baseline/current report pairs, with optional throughput, read-amplification,
  VFS read-call density, resolver-avoidance, and hydration remote-byte trend
  thresholds once a platform baseline is established.
- `make nfs-smoke-report-compare-dir` for retained native NFS smoke
  baseline/current evidence directories, with Linux, macOS, and Windows coverage
  required by the Make target.
- `.github/workflows/nfs-mount.yml` proof that platform smoke artifacts are
  uploaded, downloaded, and reverified as one retained evidence set for the
  current workflow run-attempt suffix, and that a verified synthetic read-path
  benchmark report is uploaded for the same commit after its embedded
  `git.commit` matches the workflow commit.
  When baseline run ids are configured, the workflow also downloads baseline
  artifacts and emits benchmark/native-smoke comparison summaries. Manual runs
  can override verification and comparison args for threshold calibration. The
  workflow also appends human-readable benchmark/native-smoke summaries and
  threshold suggestions to the GitHub step summary, and retains the suggestion
  env/JSON artifacts.
- `.github/workflows/release.yml` proof that the retained NFS evidence run id
  belongs to a successful NFS Mount Evidence run on the exact release commit,
  downloads the run-attempt artifacts, verifies Linux/macOS/Windows reports with
  `make nfs-release-gate`, and blocks packaging when the gate is required and
  missing or stale.
- `make nfs-release-evidence-ci` proof that the local release workflow can
  dispatch NFS Mount Evidence for a branch/tag ref while forwarding retained
  benchmark and native-smoke calibration inputs, and that opt-in wait mode
  prints or writes the exact successful run id plus run-attempt suffix for the
  release gate.
- `make nfs-threshold-suggestions` proof that retained benchmark/native-smoke
  evidence can be converted into reviewed candidate verify/compare args before
  policy is promoted.
- Native macOS NFS smoke.
- Native Linux NFS smoke in a privileged/container-capable environment.
- Native Windows Client for NFS smoke on a Windows host.
- Retained native smoke read-benchmark artifact with positive bytes, reads,
  checksum, throughput, NFS protocol read deltas, VFS source-cache/adaptive
  deltas, hydration deltas, and efficiency ratios.
- Retained native smoke report includes explicit `control_status` and
  `control_shutdown` checks plus retained `control-status.json` and
  `control-shutdown.json` artifacts, proving the CLI used the authenticated NFS
  helper control path for status and orderly shutdown after the runner exits.
- Optional retained native smoke threshold check for minimum throughput and
  maximum requested-byte, returned-byte, and RPC-per-MiB amplification.
- Release-mode benchmark showing no regression for current sequential and
  random read workloads.

## Recommended Decision

The long-term best design is an ownership-specific cache hierarchy:
NFS-owned `ReadLeasePool` for stateless native-client reads, engine-owned
content-identity source caching for reusable byte-source decisions, and
hydration-owned read windows for deduped remote bytes.

This takes the useful lesson from hf-mount's handle pool - cache state across
stateless NFS reads and pin entries during in-flight operations - without
copying its inode-owned `VirtualFs` model. Crab keeps one canonical VFS
pipeline, gains the performance hooks needed for NFS readahead, and preserves
the correctness guarantees required by overlay writes, refresh, and publish.
For writes, the same decision is stricter: NFS may remember which paths still
need protocol stability, but only the engine may decide how overlay backing
files, metadata checkpoints, copy-on-write promotion, rename, remove, refresh,
and publish interact. Future fd reuse should be an engine read-source
optimization only after retained benchmarks prove it is needed; it should not
become an NFS-owned writable handle pool or a second hydration path.
Performance gates should follow the same
ownership rule: retained engine benchmarks and retained native smoke summaries
generate candidate thresholds, but reviewed release policy lives in workflow
variables and release commands rather than in self-mutating scripts.
The same ownership rule applies outside the read path: NFS helper control is a
local secret handed from launcher to helper to registry, and preferred-backend
status is earned by exact-commit retained evidence on macOS, Linux, and Windows,
not by assuming native clients behave alike.
