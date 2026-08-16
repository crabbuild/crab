# Plan 013: Add Git protocol v2 and partial clone to Crab

> **Executor instructions**: Plan 012 must land first. Execute this plan as
> independently reviewable phases. Do not advertise `stateless-connect` or
> `filter` until the corresponding end-to-end gate says to do so. A passing
> unit test or protocol transcript is not sufficient: release requires real
> Git to create a usable promisor repository against RustFS and AWS S3.
> Read `plans/designs/client-side-git-v2-partial-clone.md` completely before
> implementation. Its deployment boundary and invariants are normative.
>
> **Drift check (run first)**:
>
> ```bash
> git diff --stat 5713bd4d..HEAD -- \
>   crab/src/git crab/src/cmd/clone.rs crab/tests \
>   crates/crab-read crates/crab-remote-git crates/crab-metadata \
>   crates/crab-git crates/crab-vfs \
>   crab/docs/architecture crab/docs/design crab/docs/guides/mount.md \
>   .kiro/specs/gitiox-gitoxide-adoption \
>   .kiro/specs/gitiox-smart-http-parity \
>   .kiro/specs/gitiox-transport-gaps
> ```
>
> If an in-scope surface drifted, reconcile the current-state evidence and
> companion design before coding. Semantic mismatch is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: XL, approximately 4–8 engineer-weeks for protocol v2 plus
  `blob:none`; broader filters and release qualification add more work
- **Risk**: HIGH
- **Depends on**: Plan 012; a reviewed, current-main version of the remote Git
  object reader currently developed on `agent/object-store-remote-git-reader`
- **Category**: feature / architecture / security / performance / tests / docs
- **Planned at**: `origin/main` commit `5713bd4d`, 2026-08-14

## Outcome

Crab will support a precisely defined Git wire-protocol v2 profile over a
serverless `crab://` remote and will provide real partial-clone semantics over
object storage.

The first generally available profile is:

1. Remote-helper `stateless-connect git-upload-pack`; no stateful `connect` and
   no receive-pack takeover. Existing helper `push` remains canonical.
2. Protocol v2 capability advertisement, `ls-refs`, and `fetch`.
3. `ls-refs` support for ref prefixes, symrefs, peeled tags, and unborn HEAD.
4. Fetch negotiation for wants, haves, done/ready, tags, shallow/deepen, pack
   responses, sideband/progress, cancellation, and bounded malformed input.
5. `filter=blob:none` initial clone and incremental fetch, followed by batched
   lazy fetch of promised objects.
6. Generation-pinned authorization for commits, trees, blobs, and tags. A
   locator hit is location proof, not authorization proof.
7. Git-owned promisor configuration and pack installation on the v2 path,
   verified through `.promisor` sidecars and repository config.
8. The existing line-oriented helper fetch path remains available to Git
   versions that do not select v2. It must remain byte-compatible and must not
   silently take over after a v2 session has started.

“Full protocol v2” does not mean every optional or future Git extension.
`packfile-uris`, `object-info`, `ref-in-want`, and stateful `connect` are not
part of the first profile unless their complete contracts and tests are added
to this plan before implementation. Documentation must list supported and
unsupported extensions explicitly.

“Full partial clone” means the complete lifecycle for every filter Crab
advertises: correct omission, promisor metadata, lazy retrieval, maintenance,
offline errors, authorization, and measured object-store savings. Phase 5 adds
the broader upstream filter grammar only after `blob:none` is production-ready.

## Non-negotiable client-only deployment invariant

The entire data path runs inside processes on the user's machine. There is no
deployed Crab Git server, pack-generation service, protocol gateway, callback,
queue, or database in the clone/fetch/push path. The only durable remote
dependency is the configured object store and its native credential APIs.

`stateless-connect git-upload-pack` names a Git protocol role, not a deployment
topology. For the lifetime of one Git invocation, the locally spawned
`git-remote-crab` process performs that role over stdin/stdout while reading
repository state directly from object storage. It listens on no network port
and terminates with the Git command.

All heavy work is local:

- credential resolution and object-store requests;
- manifest/ref/index reads and generation pinning;
- want/have negotiation and reachability checks;
- object range reads, delta reconstruction, filtering, and pack generation;
- promisor lazy-fetch handling, caching, integrity checks, and diagnostics;
- push packing, locking, CAS, and metadata publication.

No phase may introduce a required Crab-hosted endpoint as a shortcut. Optional
cache or managed products may accelerate a separately approved topology, but
direct object-store operation remains complete and canonical and must pass the
same correctness suite by itself.

## Architecture decision

### The local CLI performs the upload-pack protocol role

The current scaffold is inverted. `crab/src/git/fetch_transport.rs:292`
implements `gix_transport::client::Transport`, while `gix-protocol` and
`gix-negotiate` are client-side fetch APIs. After Git sends
`stateless-connect git-upload-pack`, Git is already the protocol client; the
local Crab helper must perform the peer upload-pack role. This does not create
or require a hosted server.

Do not implement `StdioTransport::handshake` or `request` as the solution.
Replace the production design with:

```text
User machine
  Git process
    -> local git-remote-crab child process
       1. helper line command: stateless-connect git-upload-pack
       2. terminal stdio handoff
       3. local protocol-v2 upload-pack session
       4. generation-pinned fetch planner
       5. verified object-store range reader
       6. local filtered pack producer
    <- pkt-line/sideband response over the same stdio pipes

Remote dependency
  S3/GCS/Azure/R2/MinIO object store only
```

Use `gix-packetline` for framing and use the gitoxide object, pack, and
traversal crates where their server-neutral mechanics fit. Do not wait for a
`gix-protocol` upgrade: the checked current and newer releases remain
client-oriented.

### Owner boundaries

- `crab/src/git/remote_helper.rs`: remote-helper command parsing, terminal
  stdio ownership transfer, capability advertisement, store/auth selection,
  and CLI diagnostics.
- New `crab/src/git/upload_pack_wire.rs`: protocol v2 request/response framing
  inside the local helper process only. It must not own storage policy, pack
  selection, a listener, or a network service.
- New `crates/crab-read/src/upload_pack.rs`: generation-pinned session,
  admission, negotiation, shallow/filter planning, and the smallest semantic
  `PackPlan` needed by the producer.
- `crates/crab-remote-git/`: verified range-backed object reads and bounded
  pack production. This crate is not on `origin/main` at the planning commit;
  land it cleanly before depending on it.
- `crates/crab-metadata/`: immutable, versioned visibility/object metadata and
  exact coverage. Do not overload the locator with authorization policy.
- `crates/crab-git/`: local Git pack install contracts used only if legacy
  helper partial clone is retained. The v2 path lets Git install the pack.
- `crates/crab-vfs/`: consumer of the standard promisor repository. It must not
  grow a second private lazy-fetch protocol.

The companion design at
`plans/designs/client-side-git-v2-partial-clone.md` owns process boundaries,
state ownership, data flows, failure semantics, and terminology. If an
implementation choice conflicts with it, STOP and update the reviewed design
before changing code.

### Snapshot invariant

One upload-pack session pins all of these to the same manifest generation and
pack-index hash:

- advertised refs and HEAD;
- peeled refs and hidden-ref policy;
- canonical pack inventory;
- object-locator coverage;
- commit graph and shallow boundary data;
- all-object reachability/visibility evidence.

If any required index has different coverage, fail before advertising refs or
starting a pack. Never combine refs from one generation with object locations
or visibility evidence from another.

## Current evidence map

| Surface | Current behavior | Evidence |
|---|---|---|
| Helper entry | Line-oriented helper loop owns stdin/stdout throughout | `crab/src/git/remote_helper.rs:562`, `crab/src/git/remote_helper.rs:744` |
| Command parser | No `connect` or `stateless-connect` command | `crab/src/git/remote_helper.rs:373` |
| Capability | v2 takeover is deliberately not advertised | `crab/src/git/remote_helper.rs:1608` |
| v2 scaffold | Client transport; handshake/request fail closed | `crab/src/git/fetch_transport.rs:292`, `crab/src/git/fetch_transport.rs:391` |
| Dependency | `gix-protocol` and `gix-transport` enable client APIs | `crab/Cargo.toml:306` |
| Legacy fetch | Downloads complete immutable packs into the local ODB | `crab/src/git/fetch.rs:155`, `crab/src/git/fetch.rs:337` |
| Filter | Current hardened contract rejects it before object I/O | `crab/src/git/fetch.rs:155`, `crab/src/git/remote_helper.rs:825` |
| Admission | Wants are modeled with ref names and default to visible tips | `crates/crab-read/src/fetch_admission.rs:20`, `crates/crab-read/src/fetch_admission.rs:55` |
| Reachability | Existing manifest helper covers tips/commit ancestry, not all trees/blobs | `crates/crab-metadata/src/manifests.rs:361` |
| Locator | Exact generation-covered OID-to-pack range exists | `crates/crab-metadata/src/git_object_locator/mod.rs:12`, `crates/crab-metadata/src/git_object_locator/mod.rs:76` |
| Remote reader | Bounded range reads, delta reconstruction, CRC and OID verification exist off main | `crates/crab-remote-git/src/reader.rs:83` |
| VFS | Requests `blob:none` and expects `git cat-file` lazy retrieval | `crates/crab-vfs/src/pipeline.rs:260`, `crates/crab-vfs/src/engine.rs:327` |
| VFS size | Missing blobs can be recorded as zero and reported as a placeholder size | `crates/crab-vfs/src/snapshot.rs:583`, `crates/crab-vfs/src/resolver.rs:300` |
| Tests | v2 tests are explicit unsupported/scaffold tests | `crab/tests/v2_fetch_transport.rs:1` |

## Phase 0: Correct the contract and land prerequisites

### Tasks

1. Land Plan 012 and re-run its focused and RustFS proof.
2. Rebase or reimplement the focused commit from
   `agent/object-store-remote-git-reader` onto current `origin/main`. Do not
   merge that branch wholesale; it diverges across unrelated managed-service
   history.
3. Review the reader against the snapshot invariant and all SlateDB close
   paths. Add batch/coalesced reads and expose the smallest API needed by the
   upload-pack planner.
4. Write an ADR correcting the client/server inversion and explicitly stating
   the client-only deployment invariant. Define the exact v2 capability matrix
   above and distinguish a local protocol role from a deployed server.
5. Decide the shipped Rust API treatment for public
   `git::fetch_transport::StdioTransport` and the `gix-transport` feature.
   This API appeared in a release tag. Either deliberately deprecate it for a
   release or approve a breaking removal; do not leave it as the production
   architecture.
6. Replace the nonexistent `scripts/build-matrix.sh` claim in
   `crab/Cargo.toml` with the actual CI contract.
7. Reconcile the canonical design/docs surfaces identified in the companion
   design's “Documentation reconciliation” table. In particular, replace
   “not a Git protocol server” with “no deployed Git protocol server,” remove
   client-side `gix-protocol`/`gix-transport` direction, and reopen the falsely
   completed partial-clone tasks until their real promisor gates pass.

### Exit gate

- The range reader is on current main, has real object-store tests, and proves
  cancellation, delta depth, size limits, CRC, OID, and locator coverage.
- The ADR says the local CLI performs the upload-pack role, requires no Crab
  service, and explicitly rejects client transport APIs for that role.
- Every canonical architecture/spec document uses the terminology in the
  companion design and contains no hosted-service implication.
- No capability advertisement changes in this phase.

### STOP conditions

- The reader cannot support an object size already accepted by Crab push.
- A SlateDB session cannot be closed on every return path.
- The shipped public API decision is unresolved.

## Phase 1: Add generation-bound object visibility

Partial-clone lazy fetch asks for raw OIDs without a ref name. The current
admission model cannot authorize those requests, and `allowAnySHA1InWant` is
not an acceptable shortcut.

### Tasks

1. Define an immutable, versioned all-object reachability index keyed by the
   manifest generation or pack-index hash. It must include commits, trees,
   blobs, and tags reachable from manifest refs.
2. Make direct push, protected push, managed/service publication, maintenance,
   recovery, and repack publish or rebuild the same index contract.
3. Add OID-first admission in `crab-read`; `want-ref` may strengthen proof but
   may not be required for ordinary lazy requests.
4. Apply hidden-ref policy before returning an authorization result. Prove:
   visible-only, hidden-only, shared visible/hidden, annotated-tag, dangling,
   unknown, and stale-generation cases.
5. Extend `crab fsck --store` and doctor/maintenance with an idempotent
   historical backfill and exact coverage report.
6. Keep the existing locator as location proof. Do not treat membership in an
   immutable pack as reachability or visibility.

### Exit gate

- An OID can be accepted or denied without first reading its object bytes.
- Every accepted OID has a proof tied to the same manifest snapshot.
- Historical repositories can be backfilled and rechecked idempotently.
- Filter/v2 advertisement is still disabled when coverage is absent or stale.

## Phase 2: Implement local terminal v2 wire sessions and `ls-refs`

### Tasks

1. Add `HelperCommand::StatelessConnect { service }` and a terminal batch
   outcome. Accept only `git-upload-pack`; return the documented fallback for
   unsupported services.
2. On success, write the required blank line, flush, and permanently transfer
   stdin/stdout to the v2 session. Never return to newline command parsing.
3. Add bounded pkt-line parsing for data, flush, delimiter, and response-end
   packets. Reject overlong frames, invalid lengths, unknown commands,
   duplicate conflicting arguments, and unexpected section order.
4. Advertise protocol v2 version and the exact initial capability set, but
   keep remote-helper `stateless-connect` advertisement feature-gated until
   Phase 3 is complete.
5. Implement `ls-refs` with `ref-prefix`, `symrefs`, `peel`, unborn HEAD,
   hidden refs, empty repositories, and deterministic ordering.
6. Replace scaffold tests with raw byte transcripts and independent stateless
   request/response tests. Property-test framing and limits.

### Exit gate

- Transcript bytes match Git's documented v2 framing.
- A test proves no helper-line parser consumes pkt-line data after takeover.
- Real `git ls-remote` works only in an explicit test build; released/default
  capabilities remain unchanged.

## Phase 3: Implement unfiltered protocol-v2 fetch

### Tasks

1. Parse and validate wants, haves, done/ready, thin-pack, ofs-delta,
   include-tag, shallow/deepen variants, and supported sideband behavior.
2. Create a `PackPlan` from one pinned snapshot. It must contain only values
   used by the producer: admitted wants, proven common haves, shallow state,
   canonical filter, tag inclusion, selected objects, and required bases.
3. Traverse commits and trees through the remote reader. Use the commit graph
   as acceleration only; missing/compacted graph facts must not become a false
   reachability answer.
4. Produce a standard Git pack through a bounded pipeline. Do not buffer the
   complete object closure or complete pack in memory. Verify output checksum
   before the final success boundary.
5. Emit acknowledgments, shallow-info, packfile section, sideband/progress,
   flush/response-end, and clean EOF exactly once.
6. Make thin packs conditional on proof that every external base is available
   from client haves. Otherwise emit a self-contained pack.
7. Enforce cancellation, object/pack/egress limits, range-read budgets, and
   stdout purity. All diagnostics go to stderr.
8. Preserve the line-oriented legacy fetch path for older Git. There is no
   mid-session fallback after v2 takeover.

### Performance gate

Measure normal-blob, deep-history, many-small-file, and pointer-heavy repos:

- bytes read from object storage;
- range GET count and coalescing ratio;
- pack bytes sent;
- peak RSS and temporary disk;
- CPU and wall time;
- comparison with the legacy complete-pack path.

Do not advertise v2 by default if a full clone is unbounded or has a severe
regression. If the first correct producer cannot meet the gate, optimize the
canonical producer; do not add a silent second protocol fallback.

### Exit gate

- Real Git completes v2 clone, incremental fetch, tags, shallow clone, deepen,
  and unshallow against RustFS.
- `git fsck --strict` passes for complete clones.
- Concurrent manifest publication does not mix generations.

## Phase 4: Add `blob:none` and the complete promisor lifecycle

### Tasks

1. Advertise v2 fetch `filter` only for repositories whose selected snapshot
   has complete locator and visibility coverage.
2. Strictly parse `blob:none`. Initial and incremental filtered packs include
   required commits, trees, and tags and omit ordinary blob objects.
3. Support batched raw-OID lazy wants. Admit them through Phase 1 visibility,
   return the requested object and required delta bases, and reject hidden,
   dangling, unknown, or stale-generation objects.
4. Let standard Git own pack installation on the v2 path. Verify that Git
   writes `extensions.partialClone`, `remote.<name>.promisor`,
   `remote.<name>.partialCloneFilter`, and matching `.promisor` sidecars.
5. If legacy helper partial clone is also retained, extend its pack installer
   and rollback contract transactionally for `.promisor`; do not share a pack
   install path that can forget the sidecar.
6. Compose shallow and filter semantics. Neither option may silently disable
   the other.
7. Test `git cat-file`, checkout, diff, log, fetch, merge, `git fsck`, GC,
   repack, and push from an incomplete ODB.
8. Prove offline access to present objects works and a missing promised object
   returns a clear retryable error without corrupting repository state.
9. Add telemetry for protocol version, canonical filter, negotiation rounds,
   planned/omitted/transferred objects and bytes, full/range reads, lazy-fetch
   latency, authorization rejection, and failure code. Redact URLs, refs, and
   OIDs where policy requires.

### Exit gate

- Before lazy access, a known ordinary blob is absent from the ODB.
- Access fetches that blob in a separate request and returns byte-identical
  content.
- Partial clone transfers materially fewer object-store bytes and creates a
  materially smaller initial ODB than a complete clone on the normal-blob and
  deep-history fixtures. Pointer-only fixtures are reported separately because
  Crab pointers can mask the Git-level benefit.
- Hidden-ref and arbitrary-OID security tests pass.

## Phase 5: Fix VFS and broaden the advertised filter matrix

### Tasks

1. Consolidate duplicated blobless clone/fetch policy in `crates/crab-vfs/`.
2. Remove placeholder size behavior for omitted blobs. Persist trustworthy
   blob sizes in versioned metadata or fetch exact metadata before `getattr`;
   applications must see correct file sizes.
3. Qualify ordinary blobs, Crab pointer blobs, and LFS pointers separately.
   Avoid recursive lazy Git fetch plus Crab smudge/hydration deadlocks.
4. Add upstream filter specs one by one. For each, read the pinned Git
   documentation/source, add a canonical AST, authorization rules, object
   selection tests, promisor lifecycle tests, and performance proof before it
   is accepted. Expected order: `blob:limit`, tree depth, object type, then
   sparse/combine forms.
5. Unsupported filter syntax must produce a protocol error before object I/O;
   never acknowledge and download complete packs.
6. Treat optional v2 extensions (`packfile-uris`, `object-info`,
   `ref-in-want`) as separate product decisions. Add them only with complete
   client compatibility, credential, authorization, and failure contracts.

### Exit gate

- RustFS VFS tests prove initial omission, exact `stat` size, lazy fetch, exact
  bytes, refresh, concurrent reads, and cache reuse.
- The public support matrix exactly matches accepted filters and v2 commands.

## Phase 6: CI, release, rollout, and downgrade safety

### Required matrix

| Layer | Minimum qualification | Gate |
|---|---|---|
| Unit/property | pkt-line framing/limits, filter grammar, visibility, delta closure, transactional promisor state | PR |
| Transcript | Legacy helper byte compatibility; v2 capability, `ls-refs`, fetch, shallow/tag/error exchanges | PR |
| Real Git | Minimum supported Git, representative intermediate versions, current Git; Linux/macOS/Windows | PR/nightly/release |
| Partial lifecycle | `blob:none`, absent-blob proof, config/sidecars, fsck, lazy checkout, offline error, GC/repack, push | Release |
| Object stores | RustFS on PR/nightly; AWS S3 before S3 claim; GCS/Azure/R2 before their claims | Release |
| Security/chaos | hidden refs, arbitrary OIDs, stale generation, corrupt/truncated objects, disconnect, concurrent lazy fetch | Release |
| Performance | normal blobs, deep history, many small files, pointer-heavy repos | Release |
| Provenance | source SHA, binary digest, Git/provider versions, redacted retained report | Release |

### Rollout

1. Compile and test the feature in CI without advertising it.
2. Enable explicit prerelease/canary advertisement and collect the telemetry
   above. Do not add an undocumented environment switch; use the existing
   build/release feature decision or an approved product configuration.
3. Qualify one complete release cycle before default enablement.
4. Preserve legacy fetch as the compatibility path for older Git through at
   least one fully qualified release.
5. Once a release can create promisor clones, rollback cannot simply disable
   lazy service: that strands existing repositories. A rollback release must
   continue servicing raw promised-object wants or explicitly refuse downgrade
   before installation. Document and test this rule.
6. Bind release evidence to the exact source commit and binary digest. The
   RustFS report must also record Git and backend versions without secrets.

### Final release gate

- Zero ignored v2/partial-clone qualification tests.
- Default, v2, and production feature combinations compile and test in CI.
- The released artifact, not a developer build, passes RustFS and real AWS S3
  qualification.
- Docs distinguish Git wire protocol v2 from Git filter-process protocol v2.
- No capability is advertised beyond the published support matrix.

## Verification commands

Use a unique target directory for the executor's checkout. The example name
below must not be shared with another worktree.

```bash
test -d /Volumes/Workspace && \
  mkdir -p /Volumes/Workspace/crabbuild-target/crab-013-git-v2-partial && \
  test -w /Volumes/Workspace/crabbuild-target/crab-013-git-v2-partial

cargo fmt --all --check

CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-013-git-v2-partial \
  cargo test -p crab-remote-git -p crab-read --locked

CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-013-git-v2-partial \
  cargo test -p crab --locked --features gix-transport \
    --test v2_fetch_transport --test remote_helper_transcript

CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-013-git-v2-partial \
  cargo test -p crab-vfs --locked

CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-013-git-v2-partial \
  cargo clippy -p crab -p crab-read -p crab-remote-git -p crab-vfs \
    --all-targets --locked -- -D warnings

CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-013-git-v2-partial \
  cargo build -p crab --release --locked --features gix-transport
```

Extend `crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py` or add one
focused sibling runner for v2/partial clone. The runner must invoke real Git,
capture `GIT_TRACE_PACKET` without secrets, inspect promisor config/sidecars,
prove initial blob absence and later byte identity, record object-store byte
and request counts, and emit source/artifact provenance.

## Scope and non-goals

In scope:

- a local CLI upload-pack implementation over `stateless-connect` stdio;
- generation-pinned object visibility and remote range reads;
- bounded pack planning/production;
- partial-clone promisor lifecycle;
- shallow/filter composition;
- VFS correctness on real partial clones;
- CI, telemetry, docs, performance, and release evidence.

Not in the first GA profile:

- protocol v2 receive-pack or stateful remote-helper `connect`;
- smart HTTP or SSH servers;
- Git SHA-256 repositories;
- unproven optional v2 extensions;
- any required Crab server, listener, gateway, queue, database, callback, or
  service-only pack-generation dependency;
- locator-existence authorization;
- silent fallback from a failed v2 session to complete-pack fetch.

## Success statement

After all phases pass, Crab may truthfully say:

> Crab supports Git wire protocol v2 fetch over `crab://` object-store remotes,
> including the documented partial-clone filters. A `blob:none` clone omits
> blobs, records standard Git promisor state, lazily retrieves authorized
> objects from the same generation-pinned repository, and is qualified against
> real Git and S3. All protocol, traversal, filtering, and pack work runs in
> the locally installed Crab CLI; no Crab server is deployed.

Until the final release gate passes, documentation must continue to say that
wire protocol v2 and partial clone are unsupported.
