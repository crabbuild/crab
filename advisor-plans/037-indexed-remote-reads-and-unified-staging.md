# Plan 037: Bound remote reads and unify large-file staging

Planned at `51435f76bd21142bc20a94745984a48b6c7db43b`, 2026-10-13.
Priority P1; effort XL; correctness and publication risk HIGH.
Status reconciled at `d8d6927c0f9a894dfc0e23d76ff76bf4906434cf`. Existing
indexed-read stages 1–3 are partially qualified. PR #528 supplies the paged graph;
PR #530 supplies authenticated ancestor sets and component qualification. These
remain open PRs. Foreground conversion, bounded opening and later slices are
incomplete. The canonical ledger records actual evidence and remaining gates.

## Executor contract

Read this complete plan and the root and nearest scoped `AGENTS.md` before work.
One reviewable PR per vertical slice. Preserve the existing stacked branch base;
inspect parent PR status before choosing the next base. Use `codex/` branches.
The user authorized implementation and PR creation, but this document is a plan,
not completion evidence. Attach every created PR to the chat.

First run:

```sh
git diff --stat 51435f76bd21142bc20a94745984a48b6c7db43b..HEAD -- \
  crates/crab-metadata crates/crab-remote-git crates/crab-read crates/crab-write \
  crates/crab-staging crates/crab-remote crab/src/git crab/src/cmd/add.rs
```

Reconcile changed contracts before execution. Do not redo independently completed
work. Update `crab/docs/architecture/indexed-remote-reads.md` with actual receipts
when implementing; do not replace its incomplete gates with assertions.

## Outcome and scope

Keep Git commit/tree/blob identity and Xet content formats intact. Use immutable,
authenticated indexes in object storage to answer fixed queries without decoding
unrelated history. Keep ref visibility independent of physical object retention.
Native add and Git clean retain their different entry points and publication
policies, but share one streaming staging engine, recipe authority and payload
ownership model.

Include canonical reads, HTTP attribution projections, native push, protected
receive, hydration, checkpoint/compaction, fsck and GC. No new Crab data server,
second authoritative SQL database, dependency override, or speculative config.
HTTP projection remains derived and bound to the captured repository state.

Do not change chunking policy, Xet hash algorithms, xorb/shard wire format, or
public CLI flags in this program. Full logical line blame on 10-GiB Xet text is
a later capability: provide version attribution without hydrating the payload.
Ordinary Git blobs continue to use the existing validated pack locator and
range reader; change that locator only after measurements identify excess work.

## Current state and evidence

The canonical ledger is `crab/docs/architecture/indexed-remote-reads.md`.
It records baseline, format, native Git oracle and RustFS receipts. Cloud latency,
broader lifecycle and full blame parity remain incomplete.

| Surface | Current implementation | Change owner |
| --- | --- | --- |
| Path attribution | `PathStateReader` authenticates bounded range pages; foreground uses it, maintenance streams it | `crab-metadata`, `crab-remote-git`, HTTP projection |
| Graph opening | `load_split_commit_graph` reads every layer; `CommitGraphIndex::load` builds full pinned reachability | `crab-metadata`, `crab-remote-git` |
| Graph/path binding | `SplitCommitGraph::ordinal_digest` hashes every positional record | `crab-metadata` |
| Repository control opening | `open_view_from_root_with_control` loads complete controls/frontier; selected-ref view already exists | `crab-read` |
| File history/blame | `snapshot.rs` visits first-parent commits and trees in batches; existing result cache is bounded | `crab-remote-git` |
| Historical Xet lookup | `xet_publication::prepare_delta` reconstructs the catalog, then scans historical xorb chunks | CLI publication, shared read/write/catalog owners |
| Native staging | `crab add` uses `crab_staging::stream`, prepared plans, batch rollback and close-before-index | `crab-staging`, CLI add |
| Git clean | Separate CDC/staging loop, recipe lease publication and flush-before-pointer | `crab-staging`, CLI clean/filter |

Drift anchors, copied from current source:

```rust
// crates/crab-remote-git/src/commit_graph.rs
struct CommitGraphIndex {
    graph: SplitCommitGraph,
    pinned_reachable: Vec<bool>,
}
// crates/crab-metadata/src/split_commit_graph.rs:772
for reference in &descriptor.layers { /* fetch and decode each layer */ }
// crab/src/git/xet_publication.rs:69
let base_catalog = view.pointer_catalog()?;
// crates/crab-metadata/src/capsule_protocol/pointer.rs:188
pub fn apply(&mut self, delta: &Self) -> Result<()> { /* validate closure */ }
```

Important catalog semantics: immutable xorb/shard identity conflicts are errors;
a newer file-to-shard mapping may replace an older one only with the same declared
file size. Reproduce `PointerCatalog::apply`, not generic last-write-wins.

Current local baseline: cold attribution rises from 8.001 ms at 100 commits to
142.636 ms at 10,000; decoded bytes rise from 137,704 to 13,052,443. Lazy path
attribution already reduced 10,000-commit query p95 to 8.305 ms with at most
77,406 decoded bytes. Opening still scales and must be measured independently.
These are recorded local receipts, not new tests run for this proposal.

## Architecture decisions

### Capture authority, then resolve indexes

Every operation holds an immutable captured view: repository/storage identity,
root identity, checked ref positions and activations, relevant inventory/catalog
roots, graph identity and attribution semantics. These are conceptual fields;
extend existing view types rather than introducing a parallel snapshot framework.

An index hit supplies a location or summary. It never supplies authorization.
Selected-ref capture is authoritative only for the selected names. A missing
selected ref says nothing about another ref. Arbitrary-OID queries requiring
membership in any visible ref need a complete visibility proof.

Do not claim constant-cost arbitrary-OID visibility from independently mutable
per-ref heads. Initially charge its ref capture explicitly; selected-ref reads
must avoid unrelated heads. Removing that ref-count cost requires an additional
publication protocol with an atomic visibility-directory fence. Design and prove
that separately before changing the current per-ref publication authority.

### Reuse authenticated page mechanics

Use the path-state format as the concrete starting point: immutable object,
typed range references, hashes, exact key bounds/counts, bounded internal pages,
bounded height, strict decoding and range validation. Target 32-KiB pages; retain
the existing 2-MiB hard page bound unless measured graph/catalog needs justify a
smaller limit. Bound descriptors, level manifests and caches as well as leaves.

Extract only mechanisms actually shared by graph and path-state consumers; keep
semantic validation in each owner. Do not build a general database abstraction.
Batch keys by common pages, deduplicate concurrent reads and charge bytes before
allocation/I/O. Cache verified immutable pages by repository/origin identity,
object hash and complete page reference; misses also bind the captured state.
Cache locks must not cover network waits or retain operation admission tokens.

Authenticated means bound to the captured, verified root. Hashes alone do not
prove that arbitrary client-provided graph/catalog data describes valid Git.
Writers must construct indexes from verified objects under the existing authority
boundary; maintenance validates whole closure. Point reads validate the dependency
closure they consume. Corruption is an error, never an ordinary missing-key result.

### Write immutable dependencies before publishing authority

Order: payload/recipe dependencies → index pages → verified descriptor or capsule
→ existing ref/root CAS and activation → acknowledgment. Use existing storage
conditional-write contracts. Never advertise partial coverage. A CAS conflict
replans against fresh captured state; it must not silently reuse a stale closure.

Amazon S3 conditional writes compare the object's ETag; they do not provide a
multi-object transaction. Verify equivalent behavior on each supported provider.
Reference: https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html

## Execution order

Existing stages 1–3 establish baseline and path-state foundations. Finish their
outstanding qualification without rebuilding them. Execute A → B → C → D → E;
F may proceed after the shared recipe/ownership audit and joins G. H follows D.
Lifecycle tests accompany every slice; G is the combined release gate.

### Next executable slice: use the graph in foreground operations

Start from the qualified paged graph/ancestor-set head, after checking its CI and
parent PRs. Do not rebuild the codecs or claim their tests prove foreground use.

1. In `crab-remote-git/src/commit_graph.rs`, replace the capsule reader's owned
   `SplitCommitGraph` and `pinned_reachable` bitmap with the immutable
   `CommitGraphReader` and roots captured from the authorized ref view. Retain
   the released manifest contract through an explicit format boundary, not an
   error-triggered full-scan fallback.
2. Convert OID resolution, parent validation, generation reads and membership
   callers in repository/snapshot code to asynchronous batches. Preserve input
   order and duplicates. Membership checks use the captured tip sets; retained
   records and a guessed root are insufficient. Resolve tags through the verified
   object reader and preserve shallow-boundary behavior.
3. Remove foreground path-state dependence on `graph()` and full
   `ordinal_digest()` computation. Bind the path reader to the captured verified
   descriptor's positional identity; validate consumed path records with batched
   graph records. Keep full DAG/ordinal validation in the maintenance owner.
4. Pass admitted stores and cancellation through each operation, including cache
   hits. Finish locator sessions on success/error/cancellation. Do not retain an
   operation token in the reusable graph/page cache.
5. Prove the real repository APIs resolve commits, list attributed trees and read
   files using pages. Instrument an explicit failure if the capsule foreground
   path calls the eager graph loader or allocates a history-sized bitmap.

Acceptance: native Git agrees for both merge parents, tags, rewind/deleted refs,
raw paths and file/tree replacement; corruption cannot become absence; cancellation
closes the operation. At 100/1k/10k history, fixed point queries meet the existing
byte/page limits without historical graph decode. Report opening separately:
slice C still owns the remaining full capsule/ref-control capture.

### Attribution contract before history/blame work

Keep three results distinct. A tree entry's current OID/mode comes from the
selected tree. File/directory last-change attribution comes from the path-state
index under the chosen first-parent semantics. A blame line's origin comes from
comparisons of relevant content versions; it cannot be inferred from a chunk
hash or a file's latest author. Mode-only changes affect entry attribution without
transferring ownership of unchanged lines. Directory attribution reflects changes
under that path, rather than inventing an author on the immutable tree object.

For an Xet pointer, return version attribution from Git metadata without hydration.
Full logical line blame over a 10-GiB text payload remains a separately budgeted
capability. Git's pointer-blob blame is not logical-payload blame.

### A. Page graph lookup and make positional identity immutable

Scope: `crates/crab-metadata/src/split_commit_graph.rs`, path-state readers/codecs,
their tests, and graph builders in `crates/crab-write/src/generation*`.

1. Audit released graph producers/readers, not just format file presence. The
   ledger lists six upstream release commits containing graph version 1. Define
   explicit rebuild/migration behavior before replacing a published reader.
2. Encode paged OID→ordinal and ordinal→record indexes. Keep parent ordinals and
   corrected generation in records; put variable summaries in admitted pages.
   Use a bounded directory over layers, not an ever-growing inline layer list.
3. Introduce narrow async batched lookup with caller-owned `Store` admission.
   Preserve duplicate input positions. Verify OID↔ordinal agreement, ordered
   parent edges, counts, page routing and generation/inventory bindings.
4. Publish a writer-verified positional identity in the authenticated descriptor.
   Path-state binds that identity without hashing all graph records on opening.
   Do not memoize a digest behind publicly mutable `descriptor`/`layers` fields.
5. Keep streaming complete validation for maintenance, rebuild and compaction;
   migrate foreground callers only when slice B supplies visibility proof.

Tests: equivalence with eager graph; duplicate OIDs; mismatched OID directory;
future/cyclic parents; malformed routing/hash/count/length; overflow/allocation
limits; cancellation during page fetch; compaction preserving positional identity.
Negative controls must make the corresponding regression fail.

Verify: metadata commands V1/V2 below. Done: point lookup instrumentation shows
only the bounded descriptor/directory and requested pages; no historical layer
decode. Codec-only success does not complete the graph/opening milestone.

### B. Replace full reachability materialization with explicit proof

Scope: metadata graph builder and `crab-remote-git` graph, visibility, repository,
revision, operation and snapshot consumers; shared write publication.

1. Prototype an immutable ancestor-membership set per commit/tip, keyed by ordinal.
   Use authenticated copy-on-write radix/bitmap pages and structural sharing:
   membership(child) = union(membership(parents)) + child. Reuse identical pages
   and short-circuit identical unions. Represent shallow/external boundaries
   explicitly; absent closure is not proof of non-reachability.
2. Build sets from verified Git DAGs. Bind each root to graph positional identity,
   tip OID and coverage. Full scanner verifies the set/DAG recurrence before
   trusted publication. Reader verifies selected membership page chains.
3. Capture current visible tips and activations, peel annotated tags through the
   existing verified object path, then check candidate membership in those roots.
   A retained old tip must remain rejected after rewind/force-push.
4. Replace `pinned_reachable: Vec<bool>` and synchronous graph-dependent readers
   with batched async operations. Keep complete traversal output-sized when asked
   for complete ancestry, fetch negotiation or shallow output.

This representation is a design candidate with a mandatory build-cost gate.
Persistent sets can amplify storage on merge-heavy/disconnected DAGs; do not hide
that cost. Measure linear, branch-heavy and criss-cross merge fixtures. If sharing
does not bound incremental cost acceptably, stop this slice and revise the proof
representation. Proposed prototype rejection gate: on fixed-shape fixtures at
1k/2k/4k/8k commits, doubling history must not exceed 3× total membership-index
bytes or verified build CPU at the two largest sizes. Freeze the fixture and
limits before coding; report absolute costs too. This detects quadratic growth,
but is not a worst-case storage-complexity proof. Do not substitute an unbounded
foreground DFS or Bloom filter.

Tests: every ancestry answer against native `git merge-base --is-ancestor`;
retained/invisible commits; both merge parents; duplicate candidates; tags;
shallow boundaries; forged membership; stale root; interrupted publication.
Verify V1/V3 plus RustFS visibility transition tests. Done: fixed membership
lookups avoid full graph traversal; build bytes/time/amplification are reported.

### C. Remove eager repository control opening

Scope: `crab-read/src/capsule_protocol*`, metadata capsule/checkpoint/control
shapes, `crab-remote-git/src/repository.rs`, shared writers and locator consumers.

1. Reuse `open_ref_view_from_root_for_refs` for operations naming refs. Preserve
   exact transaction/activation checks; never treat this partial view as global.
2. Add authenticated bounded directories for capsule control dependencies and
   inventory/index roots. Read footer/control ranges without retaining complete
   pre-checkpoint capsules. Both frontier and compacted opening must be bounded.
3. Bound the number of delta runs inspected by a point read. Compact under the
   existing owner and CAS boundaries; use explicit admission/backpressure when
   the bound cannot be maintained. Do not silently exceed limits.
4. Wire graph/path opening to these roots. Preserve verified pack locator lookup,
   pack ranges and Git object hash checks. Explicitly finish every operation and
   close all locator/SlateDB instances on success, error and cancellation.
5. For the broader goal of ref-count-independent arbitrary-OID reads, create a
   separately reviewed protocol slice: authenticated visibility directory plus
   an atomic publication fence tying it to ref positions/activations. Model
   concurrent disjoint-ref writers, force-push and multi-ref transactions first.
   Existing independently mutable heads cannot validate a stale aggregate root
   without additional work. Do not enable this path before its authority,
   migration and writer-contention gates pass. Until then, report the remaining
   O(visible refs) capture cost as an open goal, not a completed optimization.

Tests: pre-checkpoint and compacted sources; named refs among many unrelated refs;
partial multi-ref activation; deleted refs; history rewind; long frontier;
cancelled opening; malformed footer/root; zero unfinished work after close.
Verify V3/V4 and read scaling below. Done: selected opening has no full graph,
catalog or capsule decode. Report complete-ref enumeration separately and expose
any remaining arbitrary-OID ref-count cost; do not conceal it in setup time.

### D. Use change points for file history and first-parent blame

Scope: `crab-remote-git/src/snapshot.rs`, path-state query, traversal cursors,
runtime caches, object-reader call sites and native oracle fixtures.

1. Find the latest path change at the selected commit. Jump to that change's
   first parent and query its preceding change. Keep path/mode changes distinct
   from content changes; handle deletion, recreation and file/tree replacement.
2. History returns requested change records with a cursor bound to snapshot,
   raw path and semantics. A small history page never walks unrelated commits.
3. Blame reads the selected blob, carries unresolved line mappings through each
   relevant blob comparison, and stops at origins or explicit boundaries.
   Identical blobs skip comparison while preserving origin semantics.
4. Reuse existing bounded result caches. Add comparison reuse only for measured
   repeated pairs; key by parent/child blob OIDs, algorithm/options version and
   identity scope. Revalidate visibility before returning cached results. Charge
   warm output/work and preserve complete-or-error resource behavior.

Tests: current first-parent semantics and matching native Git options on linear
and merge fixtures; repeated lines; empty files; no final newline; mode-only
changes; recreation; UTF-8/binary behavior; cursor/cache identity; cancellation.
Native Git follows whole-file renames automatically; first-parent fixed-path
behavior is not default-blame parity. Reference: https://git-scm.com/docs/git-blame

Verify V3/V4 and 20-sample scaling. Done: fixed 20-edit file with growing unrelated
history visits bounded relevant changes; all origins match the selected semantics.

### E. Index historical Xet catalog through publication and hydration

Scope: metadata capsule `pointer.rs` and checkpoint/run descriptors; read dependency
proof, term resolution/hydration; shared write/checkpoint/compaction; CLI
`git/xet_publication.rs`; protected receive, fsck, GC and SDK/VFS consumers.

1. Implement authenticated indexes for file hash→binding, chunk hash→placement,
   shard hash→dependencies and xorb hash→descriptor. Paginate variable recipes
   and chunk tables too; a single 10-GiB recipe must not require a huge leaf.
2. Store indexed checkpoint plus visible immutable delta runs. Use a bounded
   leveled manifest with key ranges and compaction; cap overlapping runs per
   lookup. An index on every delta without bounded run count is insufficient.
3. Batch outgoing file hashes in `prepare_delta`, then stream recipe pages and
   batch only their distinct needed chunk hashes. Fetch referenced descriptors,
   validate placements/sizes/hashes and preserve file-binding precedence.
   Do not rebuild `view.pointer_catalog()` on the normal push/read path.
4. Construct the new delta from verified selected closure. Origin-verify new
   payloads and pages before capsule/ref publication. Preserve external-chunk
   authority and protected receive checks. Stream full closure for fsck/GC.
5. Switch hydration, protected receive, maintenance and sibling consumers in the
   same format migration; do not leave one path with an unchecked old authority.

Tests: indexed lookup versus `PointerCatalog::apply`; duplicate chunks/occurrences;
mapping replacement with same size; conflicting sizes/xorb/shard descriptors;
missing dependencies; corrupted range; CAS retry; compaction with pinned readers.
Verify V1/V5/V6, then the 10-GiB harness. Done: fixed recipe/edit does not decode
unrelated catalog history; every cold historical version reconstructs byte-for-byte
with all occurrence positions preserved. Count recipe-sized work honestly.

### F. Unify native-add and Git-clean staging mechanics

Scope: `crab-staging/src/stream.rs`, recipe, index, batch/lease and push-plan owners;
CLI `cmd/add.rs`, `git/clean.rs`, `git/filter_process.rs`; relevant push consumers.

1. Inventory native and filter lifecycle states, durable payload references,
   prepared xorbs, provisional file identity and recipe leases. Add crossover
   tests before deleting either loop. Both paths already share `StagingArea`;
   sharing its type alone does not unify payload storage or plan ownership.
2. Refactor the existing streaming engine to accept a bounded byte source/sink
   without requiring seek, reopening a path or trusting stat metadata. Native
   source retains descriptor stability checks; filter source is the exact packet
   stream Git supplies. Handle backpressure outside locks; preserve CPU admission.
3. One canonical engine owns CDC, hashes, chunk occurrence recording, payload
   deduplication and recipe/prepared-plan creation. A chunk may have one payload
   location and many file-version occurrences. Keep every occurrence in recipes.
4. Native policy owns its multi-file batch and rollback; clean owns one file and
   emits a pointer only after its durable recipe/payload barrier. The engine
   returns a durable staged version, not a pointer before durability.
5. Native publication writes Git ODB/index; clean returns bytes to Git. Since clean
   cannot know whether Git later writes its index successfully, retain the durable
   lease until existing reconciliation establishes release safety. A failed native
   batch releases only its leases, never payload still referenced by clean/history.
6. Adopt reusable prepared plans across both paths. Verify streamed input hashes
   even when reusing a recipe; a path/stat match is not content proof. Delete
   duplicated clean/native chunking policy and synthetic identity plumbing once
   crossover and failure tests pass. Do not spool a second complete file to adapt
   clean to a path-only API.

Tests: native→clean→native and clean→native→clean on real 10-GiB files; multiple
small edits; repeated/zero/random chunks; mid-read cancellation; fsync failure;
native Git-index failure; filter termination; old committed version after rollback;
concurrent paths sharing chunks; recipe and chunking-policy identity conflicts.
Exercise both incompressible deterministic content and sparse/repeated content;
prove 10 GiB of logical bytes passed through hashing/reconstruction. A file length
or sparse allocation measurement alone does not qualify the large-file pipeline.
Verify V6/V7 and large-file E2E. Done: switching paths creates no second full local
payload representation; each pointer has durable reconstruction; rollback and
restart preserve published versions. Separately report hash/CDC CPU, local bytes,
fsyncs, planning and upload so add latency reduction is measured, not assumed.

### G. Qualify lifecycle, migration and provider behavior together

Scope: touched readers/writers, `cmd/gc`, fsck/doctor/repack, retained roots,
maintenance checkpoints and dedicated CI/E2E fixtures.

1. GC traces every retained descriptor/page and payload dependency, including
   reader-pinned roots and resumable builds; grace remains enforced. Use existing
   GC protection mechanism, not local Arc lifetime as cloud retention proof.
2. Inject process termination at each durability/publication boundary. Verify
   acknowledged state reconstructs, partial state is not visible, builders resume,
   lock owners release/recover and stale publishers cannot promote readiness.
3. Hold readers while concurrent pushes, ref rewind, compaction and GC execute.
   Confirm stable captured results; new readers observe new complete state.
4. Audit exact released contracts. Rebuild derived browse data; migrate any changed
   authoritative catalog shape explicitly. Test representative tagged repositories
   and interrupted/resumed migration. No indefinite full-scan runtime reader.
5. Run RustFS correctness and representative S3 latency/conditional-write cells.
   Missing cloud credentials or infrastructure are a named open gate, never a pass.

Verify full changed-crate CI, native HTTP/process oracles, provider fault tests and
the matrix below. Done: receipts bind source, binary, provider, fixtures and failed
negative controls; all required cells pass without ignored credential-dependent proof.

### H. Extend blame semantics after efficient history is stable

Scope: remote Git blame types/algorithm/cache identity, HTTP attribution consumers
and native line-porcelain fixtures. Preserve the performance contract from D.

Implement in separate PRs: source path/source line positions; multiple-parent line
ownership; whole-file rename following; each explicitly supported whitespace and
ignored-revision option. Account for rename candidate work; expose defined budget
errors rather than silently dropping origins. Move/copy detection is a separately
scoped capability. Version semantics and caches whenever results can change.

Verify: native `git blame --line-porcelain` under identical options on merges,
renames, repeated lines and delete/recreate fixtures. Compare origin OID, original
path, source line, final line and displayed author data. Done: supported semantics
have exact oracle parity and documented unsupported options; no blanket Git parity
claim from first-parent tests.

## Commands and verification gates

Set the external target on EVERY compiling invocation. Before long compilation:

```sh
test -d "$HOME/Workspace" && test -w "$HOME/Workspace"
mkdir -p "$HOME/Workspace/crabbuild-target/crab-ios-abf8fc00-push-audit"
export CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-abf8fc00-push-audit"
```

The export applies only to this shell. New tools/shells require it again. Another
worktree requires its own directory. Cargo here is `/Users/haipingfu/.cargo/bin/cargo`.

| Gate | Command, after setting external target | Required result |
| --- | --- | --- |
| V1 | `cargo test -p crab-metadata --features storage --locked` | Relevant codec/page/property tests execute and pass |
| V2 | `cargo clippy -p crab-metadata --features storage --lib --locked -- -D warnings` | No new warnings; payload-only feature compilation also passes |
| V3 | `cargo test -p crab-remote-git --locked` | Visibility, traversal, history, blame and cleanup regressions execute and pass |
| V4 | `cargo test -p crab-read -p crab-remote --features crab-remote/publication --locked` | Capture/opening/publication/dependency tests pass |
| V5 | `cargo test -p crab-write --locked` | Catalog/checkpoint/compaction tests pass |
| V6 | `cargo test -p crab-staging --locked` | Recipe/lease/rollback/durability tests pass |
| V7 | `cargo test -p crab --locked` | Native add, clean/filter and push regressions pass |
| Build | `cargo build -p crab-remote --features publication --release --examples --locked` | Qualifier examples build |
| Install CLI | `make -C crab CARGO=/Users/haipingfu/.cargo/bin/cargo install` | Builds/installs via external target; inspect Makefile artifact lookup first |
| Format | `cargo fmt --all -- --check` | Exit zero for touched source before commit |

Use the absolute Cargo executable if PATH lacks cargo. Relevant small tests locally;
full/broad suites in CI/dedicated environment. Do not count a filter running zero
tests as proof. Record unrelated baseline lint failures separately; do not suppress
source lints to obtain green output. Python verifier comparisons remain active under
`python -O`. Existing HTTP oracles: `crates/crab-http-server/tests/verify_live.py`
and `verify_process.py`; inspect their CLI before running dedicated-server fixtures.

Read performance, existing harness (replace dedicated bucket/run placeholders):

```sh
python3 crab/scripts/e2e/run_remote_read_scaling_rustfs.py \
  --root "$HOME/Workspace/CrabData/indexed-read-abf8fc00" \
  --endpoint-url http://127.0.0.1:19090 \
  --bucket <dedicated-bucket> --run-id <fresh-run> \
  --crab-bin "$CARGO_TARGET_DIR/release/crab" \
  --qualifier "$CARGO_TARGET_DIR/release/examples/qualify_remote" \
  --history 100 1000 10000 --repeats 20

python3 crab/scripts/e2e/run_large_file_edits_rustfs.py \
  --root "$HOME/Workspace/CrabData/indexed-read-abf8fc00" \
  --endpoint-url http://127.0.0.1:19090 \
  --bucket <dedicated-bucket> --run-id <fresh-run> \
  --crab-bin "$CARGO_TARGET_DIR/release/crab" --file-mib 10240
```

Do not reuse another chat's server/dataset or overwrite a receipt. Source external
qualification repositories are read-only. Keep large fixtures, build artifacts and
raw receipts on the workspace volume. Never print ambient credentials.

## Final qualification matrix

| Fixed workload | Grow independently | Acceptance |
| --- | --- | --- |
| One commit/object batch | Graph commits and repository objects | Bounded pages, exact Git object hash |
| 100-entry directory attribution | Unrelated commits/paths | ≤512 KiB decoded and ≤1 MiB query bytes at 10k fixture |
| One selected ref | Unrelated ref count | No unchecked sibling-ref/control capture |
| Arbitrary-OID visibility | Visible refs and disconnected history | Exact membership; report remaining ref-count cost |
| File with 20 edits | Unrelated commits | Change-point work, exact selected blame semantics |
| Same text size | Actual content edits/merges | Report unavoidable comparison/build growth |
| Fixed 10-GiB recipe/edit | Distinct historical chunks | No proportional historical catalog decode |
| Point reads | Uncompacted frontier/runs | Enforced lookup amplification bound |
| Captured reader | Concurrent push/rebuild/compaction/GC | No mixed snapshot; no deleted required dependency |
| Native/clean crossover | Number of edits and shared versions | No duplicate full payload; safe lease/rollback |

Measure opening, query and maintenance separately. Include cold/warm p50/p95,
HTTP request/bytes by object class, page decodes, CPU, RSS, local write amplification,
index build/compaction time and storage amplification. At least 20 independent
samples per final latency cell. For the frozen local fixture, largest-history
attribution/blame cold p95 must be ≤3× smallest-history p95 +20 ms. Establish cloud
targets from cloud baseline first; local RustFS timing is not cloud proof.

## Stop conditions and review checklist

- If a new index changes visibility/ref transaction authority, isolate the protocol
  decision and prove publication ordering before implementation proceeds.
- If membership sets or compaction exceed measured build/storage/admission budgets,
  revise the representation; do not move unlimited work into opening or hide it.
- If a shipped format is found, define and test its migration boundary before removal.
- If a common staging engine needs a second full-file spool or weakens rollback,
  revise the source/sink and ownership boundary before switching callers.
- If a proof needs secrets, cloud access or destructive external mutation unavailable
  to this task, record the precise missing gate and continue independent work.

Before each PR verdict ask: **Is this the best fix, or only a plausible fix?**
Evidence map must include entry point, owner, caller, callee, sibling consumers,
tests, released contract and base behavior. Explain non-test LOC growth. No stage
is complete from codec tests alone; require user operation → actual store/Git side
effect → independently verified visible result. The full initiative is complete
only after all applicable slices and qualification cells are proven.
