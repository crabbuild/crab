# Indexed remote reads: implementation and qualification

Status: implementation in progress. This document tracks the entire initiative;
passing one delivery gate does not establish completion of the later gates.
Baseline: `bb590b78f58` on `crab-v2`, including the large-file push audit.

## Required behavior

A fixed request must load metadata proportional to its requested objects, paths,
or relevant file changes, with bounded tree-depth and compaction-level overhead.
Increasing unrelated history must not introduce proportional decoding, scanning,
or memory consumption. Requests needing complete output, including enumeration,
archives, full integrity checks and graph traversal, retain output-sized costs.

Repository/ref authority is captured before queries. Index presence never grants
visibility. Derived records bind the root and visible per-ref positions, including
multi-ref activation. Reader snapshots remain immutable across publication.

Keep current first-parent file/tree attribution and ordinary UTF-8 blame semantics
through the performance milestone. Full merge-parent attribution, rename following,
source line positions and supported blame options have a separate correctness gate.
Xet version attribution requires no payload hydration. Logical line blame for large
Xet files and unified local staging are subsequent workstreams.

## Delivery gates and evidence

| Order | Deliverable | Completion evidence | Current state |
| --- | --- | --- | --- |
| 1 | Reproducible cold/warm baseline and contracts | Native Git oracle, isolated query/open measurements, process resources, HTTP bytes/classes, index decode counts, history scaling receipts | Implemented; RustFS history-scaling baseline passed; cloud latency cell pending |
| 2 | Authenticated bounded path-state pages | Equivalence/property tests; malformed hashes, lengths, ordering, routing and allocations rejected; bounded internal directories | Implemented; 23 focused tests, GC retention regression and 1,000/10,000-commit RustFS format qualification passed |
| 3 | Lazy canonical and projected attribution | Fixed directory page reads only needed pages; branch/force-push/recreation/raw-path/restart E2E proof | Canonical scaling/transitions passed; streaming Cell ingestion, projected native Git oracle and two process starts passed on RustFS; broader cloud/lifecycle qualification remains open |
| 4 | Lazy graph lookup and bounded opening | Paged OID/ordinal/parent/summary queries; reachable versus retained proof; tag/shallow/cancellation/cleanup checks | Foreground graph/path and checkpointed opening qualified on RustFS; tag/shallow/retention/cancellation proofs pass; latency, broad CI and release/cloud gates remain open |
| 5 | Change-point history and first-parent blame | Exact native Git origins under matching semantics; fixed file changes with growing unrelated history; comparison/result cache isolation | Pending |
| 6 | Indexed historical Xet catalog | Indexed checkpoints and visible deltas; bounded compaction; 10-GiB edits and byte-identical cold historical hydration; protected receive and sibling consumers | Pending |
| 7 | Lifecycle and migration | Interrupted publication/builds, concurrent owners, GC retained roots/readers, stale caches and released-format migration qualification | Pending; also required in every earlier implementation |
| 8 | Fuller Git blame semantics | Line-porcelain parity across merges/renames/repeated lines/recreation and each supported whitespace/ignored-revision option | Pending |

Each implementation PR owns one delivery gate or a complete vertical slice within
it. Tests alone are insufficient: user operations against a real S3-protocol store
must produce independently verified results. Representative cloud latency remains
a separate cell from local RustFS correctness.

## Measurement contract

`qualify_remote` opens capsule-v2 state, attaches only state-matching published
browse indexes, and executes one named query twice. Each workload has a fresh
runtime; its second query is warm. Opening is reported independently before and
after the query. The cold query includes required snapshot/path setup so those
reads cannot disappear into unreported preparation. It is cold relative to the
captured handle: opening may already load inline pack/index data, which belongs
to the opening measurements.

The backend observer includes reads through `Store::inner()`, including locator
SST/manifest access. It reports logical backend invocations and consumed bytes;
provider HTTP retries and listing pages require the separate HTTP meter. Decoder
observations count verified graph/path-state blocks and their encoded input bytes.
The frozen baseline counted layers. The paged path-state format counts individual
verified pages; paged graphs count verified pages too, while the released eager
graph format counts layers. Do not relabel decoded bytes as
network transfer or resident memory. Decode events belong at the actual decoding
boundary.

The HTTP meter records requests and request/response bytes by bounded object
class. Capsule/run/checkpoint objects can contain both control and Git payload
ranges: report that class as mixed rather than pretending its bytes are entirely
metadata or payload. Standalone index, pack and xorb classes can be reported
separately. Raw request paths are disabled in the scaling harness. Timing and
counts contain no raw paths, OIDs, endpoints, content or credentials; separate
oracle evidence contains the paths/OIDs needed to prove correctness.

`/usr/bin/time` supplies process CPU and peak RSS. Those cover opening and both
queries together; they are not per-query RSS. Index construction uses an explicit
`build-indexes` operation, outside measured reads, with its own requests, bytes,
CPU and peak RSS. Read-only qualifications do not trigger maintenance.

The verifier checks complete cold/warm measurements, unfinished backend work,
Git object content hashes, page order/continuation, first-parent history and every
blame line. Tree attribution is compared with native Git path history. Verification
remains enabled under `python -O`.

`qualify_remote` also emits `kind: "phase"` timing rows inside measured queries.
Admission, snapshot setup and cleanup are timed for every query; tree-based
queries also time directory listing, and attribution times its indexed lookup.
These clocks leave the enclosing storage/decode counters untouched and preserve
typed errors through operation cleanup. Phase rows contain only operation/cache/
phase labels, elapsed time and outcome. Their durations are diagnostic components
of the parent query, not additional queries or a substitute for its total cost.

## Reproducible qualification

Use the existing per-checkout target on the mounted workspace volume; set it on
all compiling invocations. No local `target/` or external-source mutation.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-abf8fc00-push-audit" \
  cargo build -p crab-remote --features publication --release --examples --locked

python3 crab/scripts/e2e/run_remote_read_scaling_rustfs.py \
  --root "$HOME/Workspace/CrabData/indexed-read-abf8fc00" \
  --endpoint-url http://127.0.0.1:19090 \
  --bucket <dedicated-development-bucket> --run-id <fresh-run-id> \
  --crab-bin "$HOME/Workspace/crabbuild-target/crab-ios-abf8fc00-push-audit/release/crab" \
  --qualifier "$HOME/Workspace/crabbuild-target/crab-ios-abf8fc00-push-audit/release/examples/qualify_remote" \
  --history 100 1000 10000 --repeats 3
```

The harness uses local development credentials and an existing RustFS service;
cloud qualification uses the same read binaries with authorized credentials and
a representative dedicated dataset. Run the existing large-file harness with
10,240 MiB per file separately. Preserve receipts outside the source checkout.

For checkpointed opening qualification, add `--checkpoint --workloads resolve
object-point object-batch attribution --repeats 20`. Checkpoint publication and
its complete visibility oracle run as measured maintenance before browse-index
construction. Every selected query still has an independent Git oracle and
separate cold/warm opening measurements. Receipts identify checkpointed cases;
omitting the flag retains the original frontier workload. The source manifest
also captures the shared page and lazy visibility implementations used by opening.

The scaling fixture fixes the selected file at 50 lines and 20 content edits,
and the directory page at 100 entries. It changes a separate path in every
commit. Commit timestamps and data generation are deterministic. Fixtures are
published separately; native Git independently verifies every query. Initial
history cells exercise unrelated commit growth; distinct Xet chunk history,
ref-count growth and long uncompacted frontiers remain mandatory later cells.

Latency targets must be established from pre-optimization receipts before
changing readers. Deterministic tests enforce request/page/work bounds; latency
qualification records cold/warm distributions, CPU, memory and maintenance cost.

## Pre-optimization measurements and acceptance targets

Local RustFS baseline `baseline-r1`: 100/1,000/10,000 commits, three isolated
process runs per workload, ten workloads, 90 native Git query comparisons,
450 checked blame lines, and 207 harness checks. All passed. Complete browse/archive oracles also passed at 100 and 10,000
commits, checking 102 entries/blobs and 50 blame lines in each; 1,100 commit
records were checked across those runs. Receipts and exact
binary/fixture hashes remain under the external qualification root; they are not
source files. Timings below are query medians, excluding opening.

| Commits | Cold attribution | Attribution decoded bytes | Cold blame | Attribution process peak RSS (median) |
| --- | --- | --- | --- | --- |
| 100 | 8.001 ms | 137,704 | 5.707 ms | 22,118,400 bytes |
| 1,000 | 29.865 ms | 1,355,056 | 27.127 ms | 34,488,320 bytes |
| 10,000 | 142.636 ms | 13,052,443 | 954.441 ms | 171,622,400 bytes |

Opening also scales: the attribution runs consumed 150,598 / 1,118,721 /
6,695,676 backend bytes. Before a checkpoint the control-opening path retains
complete frontier capsules; lazy graph loading alone will not remove that cost.
Opening qualification must cover both pre-checkpoint frontiers and compacted
sources instead of warming payloads before the measured query.

Acceptance targets, fixed before optimization, for this same fixture/provider:

- At 10,000 commits, the fixed directory attribution query decodes at most
  512 KiB and downloads at most 1 MiB; it never decodes historical layers.
- First-parent blame resolves change points instead of every unrelated commit.
  Its work-count regression must remain bounded by the 20 edits and path depth.
- For attribution and blame, the largest-history cold query p95 is no greater
  than three times the smallest-history p95 plus 20 ms. Use at least 20 samples
  in final performance qualification; three samples establish this baseline,
  not a reliable tail-latency distribution.
- Opening and query decoded metadata/peak memory must not grow proportionally
  with unrelated history. Exact request bounds follow the implemented page
  height, requested paths and compaction-level limit and are tested directly.

Every target also requires identical Git results. Cloud targets are established
from representative S3 measurements before the cloud optimization comparison;
local numbers do not establish cloud latency.

## Release audit and migration boundary

Audit performed against upstream peeled release commits, not local tag names:

| Release | Upstream commit |
| --- | --- |
| v1.0.1 | `0cd19b61721ea07cdb5d997b07a8d55aace88732` |
| v1.2.0 | `773256d06a70dac187e16c3676f54cc629382b90` |
| v1.2.1 | `0f9fec98d77a9f30f3bc5a38af1b355871f2b3ca` |
| v1.2.2 | `30fae43517412f7e4b17bf7f1bab82232d8d986e` |
| v1.2.3 | `49d3791b484450dac4bc8350cdf15d7caf8f3e15` |
| v1.2.4 | `b4d5fd361f9a1a43fdd468dc604901d2c61006f0` |

All listed commits contain `split_commit_graph.rs`; none contain the capsule
protocol module or `path_state.rs`. The local v1.2.4 tag differs from upstream;
upstream was fetched into a separate audit ref without overwriting the local tag.
Paged path-state/capsule formats can replace their unreleased predecessors.
Split graph migration must additionally inspect tagged producers, descriptor
bindings and repair paths before changing its published format. File absence
alone is not proof that every related serialized shape is unshipped.

## Ownership and consumer coverage

The released graph producer/consumer audit also reads CLI push's
`load_base_split_commit_graph`, remote-helper manifest loading and the published
metadata crate manifest at upstream v1.2.4. Both consumers bind the graph to the
manifest generation, pack index, validation digest and committed roots. Manifest
maintenance retains its version-1 encoding. Capsule browse has no released graph
contract and now writes paged descriptor version 2 through its generation owner.
Public Rust descriptor structs gain optional identity/page fields; workspace
literal consumers are updated, while version-1 serialization omits the additions.
Published Rust API versioning remains part of the release gate.

- `crab-metadata`: canonical codecs, index formats, validation and visibility.
- `crab-storage`: range I/O, retries and backend observation.
- `crab-read`: captured capsule views, Xet dependency resolution and hydration.
- `crab-write`: page durability, incremental builders, compaction and checkpoints.
- `crab-remote-git`: bounded queries, operation admission and explicit cleanup.
- `crab-remote`: maintenance coordination and state-fenced derived publication.
- `crab-http-server`: canonical reads, projection ingestion and readiness behavior.
- CLI push/protected receive/SDK/VFS/fsck/GC: relevant sibling contract consumers.

Every format/read change records caller, callee, sibling, error and lifecycle
proof before publication. Retain maintenance streaming scans for integrity and
compaction; remove eager assembly from normal point queries. Do not introduce a
second authority database or an indefinite old-format runtime fallback.

## Current validation limitation

Strict example Clippy reaches pre-existing unfulfilled `double_must_use` lint
expectations in six coordinator trait annotations on the current `crab-v2` base.
Those files are unchanged by this work. A diagnostic run allowing only
`unfulfilled-lint-expectations` passes; this is not a strict Clippy pass and no
source suppression was added. Record the baseline issue separately from new
format/read correctness gates.

## Paged path-state format

Path-state descriptor version 5 replaces the unreleased whole-layer predecessor.
Each layer remains one immutable content-addressed object; its descriptor commits
to record, node and component-name page roots. Pages use a canonical binary
format with typed keys, authenticated byte ranges, exact key bounds and subtree
counts. Internal directories are paged as well. Typical pages target 32 KiB;
a single admitted long component or summary may use a page up to the hard 2 MiB
bound. Directory keys remain fixed-width regardless of component length.

Commit records and nodes use dense ordinal keys. Wide child collections use
sorted component ordinals; a paged hash-to-component directory preserves exact
raw names. Repeated names are encoded once per layer and shared by decoded nodes.
Writer page deduplication preserves repeated immutable child collections. Unchanged
trie nodes still refer to retained layers. Existing incremental construction and
compaction retain their graph/ordinal binding and verified checkpoint behavior.

Full maintenance decoding verifies the complete layer content hash, every page,
component use and contiguous physical byte coverage. It rejects overlapping,
conflicting or unreferenced page regions. Lazy reads verify their authenticated
page chain and selected records against the captured graph. They do not claim
full validation of unrelated payload entries; maintenance/fsck retain that role.
A missing required page or malformed routing is an error, never a deduplication
or attribution miss. Root pages are verified before accepting out-of-range keys.

`PathStateReader` loads the bounded descriptor, groups node/record/name/child
queries, and reads only dependency pages through the caller's admitted clone of
the same origin store. It caches verified parsed pages with an 8 MiB accounted-data
bound and at most 256 cached pages; cache identity includes layer, offset, kind and complete routing metadata.
Each query admits unique encoded page bytes before consulting cache or launching
bounded concurrent I/O. Cache locks do not cover network waits. The caller keeps
its captured graph and authorization boundary; index hits add no ref visibility.
Graph layer identities also remain pinned: a rebuilt graph with matching scalar
generation/count/digest fields cannot substitute different positional records.

Pages are ranges within the layer object, so existing GC marking of every retained
layer object also retains all referenced pages, including resumable construction
checkpoints. Publication verifies the complete immutable object before publishing
its descriptor/readiness pointer. No separate page-key scan, new authority object,
or old-format reader is introduced. Before GC or readiness accepts an index, its
version and descriptor roots must validate; retired derived indexes are rebuilt
under existing ownership and CAS rules.

## Canonical attribution qualification

The first lazy foreground consumer uses the same captured graph and origin, with
descriptor/page reads admitted by the active operation. Cached metadata never
retains a finished operation's budget or cancellation token. Entry, depth,
logical-object and response work remains charged for warm queries. Missing
required pages are corruption, while absent readiness retains indexing status.
HTTP Cell projection ingestion uses the streaming maintenance consumer below.

Projection ingestion now has a streaming implementation. Its full
maintenance scan verifies layer hashes, authenticated pages and physical coverage
before EOF, retaining a current-layer dictionary/audit rather than the complete
index. The importer consumes verified summaries instead of resolving retained
ordinals through foreground Git visibility. An append-only application migration
preserves summaries and epoch membership while removing unconsumed commit fields.
Focused proof: 26 path-state tests, 30 HTTP maintenance tests, 17 projection tests
and 73 Cell/router/scheduler/release tests pass; three credential-dependent Cell
tests remain ignored. These slices overlap and their counts must not be summed.
A cancellation
negative control reproduced a skipped record in the old scan cursor; the fixed
cursor advances only after page I/O succeeds. Rewind proof confirms retained
graph records remain unavailable to foreground attribution after force-push.

Live `stream-projection-r1` published the 1,000-commit fixture through native HTTP
receive into a dedicated RustFS bucket. An initial attribution response succeeded,
but that alone does not prove the configured Cell projection was ready: canonical
attribution is an existing readiness path. Process restart qualification failed
twice at the verifier's attribution timeout. Instrumentation showed the state
query finished and then waited to drain a cold activation while background
ingestion held the Cell operation guard. The second run's metrics showed 30,120
attribution rows and 192 summaries submitted, zero completed reconciliations and
projection readiness zero. The fix gives projection work the same runtime-owned
Cell lifetime as ordinary collaboration requests. Scheduler cleanup releases a
cold activation only when exclusive operation access is immediately available;
an active borrower leaves ownership with the bounded runtime. Explicit migration
draining and node shutdown retain their close/publication barriers.

The subsequent single-repository run completed one reconciliation with all 1,000
summaries and 105,121 attribution rows. Its selected-source readiness gauge was
one before and after independent native Git verification of 102 attribution
entries, author/time/message fields, file bytes, 50 blame lines and 30 sampled
historical commits. One hundred foreground attribution/metrics polls continued
during ingestion; the largest combined poll took 2.084 seconds. This includes a
private metrics subprocess and is not a standalone HTTP latency percentile.
`process-proof-r3` then passed the same native oracle on two process starts, with
clean shutdowns. Failed r1/r2 receipts remain preserved. These runs use a local
debug binary, not a qualified release image; broader CI and cloud gates remain
separate from this configured-projection correctness proof.

RustFS `lazy-attribution-r1` reused the frozen baseline repositories and native
Git oracle. Twenty process-isolated samples per history size passed: 60 cold/warm
attribution comparisons, with complete browse/archive verification also passing
at 10,000 commits. The latter checked 102 entries/file bodies, 1,000 requested
history records and all 50 blame lines.

| Commits | Cold median | Cold p95 | Maximum path-state decoded bytes | Maximum query backend bytes | Median whole-process RSS |
| --- | --- | --- | --- | --- | --- |
| 100 | 5.197 ms | 5.713 ms | 13,895 | 18,704 | 21,839,872 |
| 1,000 | 4.885 ms | 5.787 ms | 14,425 | 50,741 | 28,426,240 |
| 10,000 | 7.732 ms | 8.305 ms | 77,406 | 105,899 | 107,487,232 |

Query decode/fetch and latency satisfy the frozen attribution bounds. Process
RSS includes opening and still grows with unrelated history; this does not
establish bounded opening or graph access. Provider HTTP evidence covers each
whole process, separately from its backend query counters. Cloud latency,
opening and the later history/catalog/lifecycle gates remain open.

The transition fixture passed both all ten read workloads and complete
browse/archive verification after deletion/recreation, file/tree replacement,
force-push to a side branch and rewind to the original root. It includes raw-byte
names in Git objects, independent of filesystem Unicode support, and checked
300 blame-line origins across six reader runs. Publication first exposed an
existing UTF-8 failure in visibility enumeration; [PR #525](https://github.com/crabbuild/crab/pull/525)
requests OIDs only, matching candidate enumeration. The new regression reproduced
the old failure before the fix, and all 18 visibility tests plus the original live
push passed afterward. These transitions qualify canonical reads; configured
Cell projection has the separate ingestion/restart proof above and owner tests
cover the same rewind visibility boundary. Concurrent retained readers during
compaction/GC and broader interrupted-build qualification remain later gates.

## Paged graph producer and component qualification

Capsule browse construction writes authenticated ordinal-record and OID-directory
pages, with the existing positional digest in its descriptor. Maintenance full
loading checks that digest, OID/record agreement, parent ordering and corrected
generations, layer hashes and contiguous physical page coverage. Unchanged layers
are reused; carries also enforce the 32-layer directory bound for decreasing
append sizes. Removing this carry reproduces a rejected 33-layer descriptor in
the regression test; restoring it passes. The source was restored before final
qualification. Lazy graph loading rejects the released eager format rather than
falling back to it. The full loader retains that format for the shipped manifest
protocol and for owner-controlled rebuilding of unshipped derived browse data.

Graph and path-state readers share the page codec and bounded cache. Point and
batch graph queries retain input duplicates, validate selected OID/ordinal
agreement and parent generations, and charge unique page bytes before I/O or
cache lookup. Cancellation may discard a query without advancing persistent
reader state. Record presence supplies no visibility proof.

Focused proof: 19 graph tests, 21 path-state tests and five shared-codec properties
pass; a ten-test page filter overlaps those slices. Capsule browse publication
and non-commit-tag maintenance pass the existing integration test. Metadata
payload-only compilation and strict storage-feature library Clippy pass; the
remote qualifier diagnostic Clippy run permits only the existing coordinator
`unfulfilled-lint-expectations` baseline. Remote-Git test targets compile, Python
scaling tests pass and architecture guardrails pass. The native verifier rejects
a deliberately wrong graph tree with Python optimization enabled.

Final-source RustFS `paged-graph-r2` reuses the immutable baseline source fixtures.
At 100/1,000/10,000 commits, twenty isolated processes per size each compare cold
and warm sampled ordinal/parent records and duplicate OID batches with native Git.
All 60 comparisons and separate attribution comparisons pass. Opening captures
complete current controls and is reported separately; these component measurements
are not a bounded-opening or release-image qualification.

| Commits | Cold graph component median | Cold p95 | Maximum query backend/decoded bytes | Maximum warm query backend bytes |
| --- | --- | --- | --- | --- |
| 100 | 1.916 ms | 6.994 ms | 10,024 | 0 |
| 1,000 | 18.653 ms | 55.610 ms | 100,237 | 0 |
| 10,000 | 8.698 ms | 24.208 ms | 166,656 | 0 |

This debug-host run has substantial latency variation, including the 1,000-commit
cell; deterministic page/work limits are the format proof. Final foreground,
process-resource, release-image and representative S3 latency gates remain open.
Receipts, failed negative controls, source hashes and binary hashes remain outside
the checkout. Stage 4 still requires explicit visible ancestry proof, lazy
foreground conversion, removal of full positional-digest work and bounded control
opening. Stages 5–8 remain incomplete.

## Authenticated ancestor-set component

Unreleased capsule graph version 3 replaces version 2 with a dense commit-to-set
root directory and a content-addressed node directory in each existing layer
object. Persistent binary interval trees share immutable children, collapse full
ranges and omit unary branches. Exact DAG unions preserve both merge parents and
disconnected history. Full readback recomputes every set from verified positional
records, verifies canonical node bytes and first-use layer placement, and rejects
extra or missing nodes. Removing recurrence validation makes a re-signed forged
set pass; restoring it rejects that set. This proves why authenticated pages alone
are insufficient for visibility. Released manifest graph version 1 is unchanged.

`CommitGraphReader::ancestry_membership` reads the selected root and batched node
dependencies, preserves duplicates, checks self membership and rejects missing
nodes and malformed interval routing. Its caller must supply roots authorized by
the captured ref view. It does not authorize a root from record presence. The
foreground repository still uses the materialized graph and pinned bitmap; their
replacement and lazy path-state graph binding are the next implementation slice.

`paged-ancestry-r1` passes 20 isolated cold/warm native Git comparisons at each of
100, 1,000 and 10,000 commits, plus separate attribution checks. HTTP observation
and whole-process CPU/RSS accompany opening and query measurements. At 10,000
commits, the sampled record/OID/membership component reads at most 411,872 bytes;
all warm component queries read zero backend bytes. Its cold p95 is 60.388 ms on
the debug host, so these receipts do not establish a final foreground latency pass.
Maintenance writes 47,483 / 649,672 / 8,238,957 backend bytes, with 0.15 / 1.03 /
13.49 process CPU seconds; these include migration/build work, not just set union.
The prototype's encoded membership storage test stays below 3x on each doubling
from 1k to 8k for linear, eight-branch, criss-cross and disconnected shapes. This
is fixture qualification, not a worst-case complexity claim.

`ancestry-transitions-r1` uses a separate task-created native Git repository and
RustFS prefix. The main tip excludes side descendants, a merge includes both
parents, and force-push rewind plus side-ref deletion excludes physically retained
descendants. Native Git independently verifies all membership answers. The source
fixtures and raw receipts remain outside the checkout. Full stage-4 opening,
ref-count/frontier scaling, release-image/cloud performance and stages 5–8 remain
open.

The final rebuilt binary also passes `paged-ancestry-r2` (60 cold/warm native Git
comparisons and attribution checks) and `ancestry-transitions-r2`. Focused final
checks pass 27 graph/ancestry tests, 21 path-state tests, capsule browse publication
integration, payload-only compilation, strict metadata library Clippy, qualifier
diagnostic Clippy and architecture guardrails. The negative-control source is
restored; final source/binary hashes and receipts are retained externally.

## Foreground graph integration

Capsule foreground handles now retain the paged graph reader and captured ref
roots. OID resolution, ordered parent validation, generation lookup and membership
are asynchronous and use the operation's admitted store and cancellation. Path-state
binds the paged descriptor's positional identity and validates consumed records
through batched graph reads, without recomputing the complete ordinal digest.
The released manifest format still receives full graph validation; its membership
work charges metadata entries rather than the raw-history budget.

The final-source repository-index run passes nine tests, including rejection of full
graph-layer body reads, admission on warm results and dependency-page cancellation.
The retained-OID public resolution check passes. The shallow-fetch caller is now
async, and the native Git tag fixture passes for both formats.
The resolve qualifier now checks full-OID visibility
after obtaining the selected ref's commit; older resolve timings therefore measure
less work and are not direct comparisons for this expanded workload. Four Python
oracle regressions and architecture guardrails pass. Real-store foreground scaling,
build-cost and consumer qualification are recorded below. Broad CI remains
required; this is not a completed stage-4 gate. Outer control opening and later
history/catalog/lifecycle/blame stages remain open.

Release audit: the synchronous `commits_reachable_from` introduced at
`acf0408de89` is reachable from v1.0.1 and v1.2.0 through v1.2.4. Its conversion
to asynchronous I/O is a Rust source API change; callers now await the proof.
The current crate still declares 0.1.0. A breaking crate-version transition and
consumer migration remain required before publication; the passing architecture
version-alignment check is not proof of that migration. Version-1 stored graphs
retain complete validation and native protocol consumers use the async proof.

`foreground-graph-r1` passes 20 isolated cold/warm attribution comparisons at
each of 100/1k/10k commits and a full-OID resolution comparison at each size.
Native Git verifies all results. At 10k, query reads reach at most 138,597 bytes
and decoded graph/path pages reach at most 110,104 bytes. Graph opening decodes
at most 35,764 bytes; the outer opening still downloads up to 6,094,562 bytes.
Cold attribution p95 is 45.792 / 31.149 / 54.412 ms on the shared debug host.
These pass the frozen local fixture bounds but are not release/cloud latency
qualification or direct comparisons with earlier release-binary receipts.
Final-source checks pass nine repository-index tests, the native Git tag/retention
fixture for both formats, 22 path-state tests, four Python oracle tests and
architecture guardrails. The qualifier builds with real consumer dependencies.
Strict metadata/remote-Git library Clippy and the CLI binary consumer check pass.
Build-qualifier Clippy also passes. No dependency or lockfile changes were needed.

`graph-build-cpu-r1` records 20 samples for each of four graph shapes at
1k/2k/4k/8k commits: 320 component builds. At the two largest sizes, process CPU
growth stays at most 2.358x and complete encoded index growth at most 2.179x per
doubling. The 8k criss-cross fixture uses 1.29 median CPU seconds, 7,228,939 encoded
bytes and 62,357,504 median RSS bytes. Synthetic OIDs isolate the builder; these
are debug component receipts, not native Git integrity, release-image performance
or whole-publication measurements. Release/provider and broader lifecycle gates
remain open. Fixtures, source/binary hashes and raw receipts remain external.

## Metadata-first opening qualification

The existing complete-metadata checkpoint capture is now named
`open_view_from_root_with_metadata`. HTTP browsing, S3 read views, CLI remote
reads, index builders and the foreground qualifier use it before and after
checkpoints. It verifies full visibility, pointer catalogs and transaction/ref
ordering, rather than substituting the narrower ordinary-fetch footer view.
Complete body views remain available for embedded-pack and candidate installers.
Origin-backed installation now handles uncheckpointed metadata source ranges;
resident complete views retain their existing body installation path.

Final-source metadata checks pass five tests, including snapshot/catalog and
activation/visibility equivalence, absence of whole-run body GETs and preservation
of typed missing-dependency errors. Native delta-chain installation passes both
complete and metadata-only views, including repeated installation and native Git
byte comparisons. Architecture guardrails, strict read/remote-Git library Clippy,
CLI, S3 gateway and HTTP consumer checks pass; the frontend build ran before the
HTTP check. The initial PR image qualification failed because ECR rejected all
five attempts to pull its pinned AWS CLI image with a registry data-limit error.
Final head `4a7b039c3d0` passes every non-skipped CI check, including both image
jobs, four native Git versions, macOS/Windows protocol and RustFS partial-clone
lifecycle. Kubernetes and scheduled-fleet cells were skipped and remain unproved.

`metadata-queries-r1` passes 20 cold/warm attribution comparisons at both 100 and
1,000 commits. At 10,000 commits opening reads 2,182,079 bytes, compared with
6,094,562 in `foreground-graph-r1` on the same fixed RustFS fixture. Full-OID
resolution returns the correct native Git result but reads 1,057,066 cold-query
bytes, exceeding the unchanged 1-MiB qualification bound. An independent replay
reproduces this failure: its pack-index range is 844,460 bytes, its commit entry
148 bytes, and graph pages account for another 212,458 bytes. Cold attribution
independently passes at 985,949 backend bytes and 110,104 decoded graph/path bytes;
20-sample largest-history qualification remains pending while resolution is fixed.
The terminal failure and diagnostic receipts remain outside the checkout.

The pre-fix captured-tip regression fails with two entry charges against a
one-entry budget. Its implementation now reuses the root verified during opening,
charging one entry and checking cancellation without rereading ancestor pages.
The focused suite passes ten tests, including the new root proof, retained-history
denial, merge parents and dependency-page cancellation. The qualifier rebuild
passes. Other commit IDs keep exact ancestry checks; arbitrary batch roots retain page verification and
in-flight cancellation tests. Pack-index and historical-control work still scales, so
this local optimization cannot establish the bounded-opening gate: directories,
inventories and ref/frontier capture require indexing and further scaling proof.

`metadata-queries-r2` passes all 63 native Git comparisons with the rebuilt binary:
20 cold/warm attribution runs at each of 100/1k/10k commits and one full-OID
resolution run at each size. At 10k, cold resolution reads 844,608 bytes and no
graph pages; cold attribution reads at most 985,949 bytes and decodes 110,104
graph/path bytes. All warm queries read zero backend bytes. The unchanged fixture
byte bounds pass. Cold attribution p95 is 259.152 / 187.268 / 460.314 ms on this
shared debug host, within its frozen relative bound; these timings do not establish
release/provider performance. Full pack-index reads still scale with repository
objects, and control opening still scales with history. Source/binary hashes,
the earlier failure, raw receipts and checked summaries remain external.

## Paged physical object lookup: runtime qualification

The measured 10k cold-resolution query previously read an 844,460-byte pack-index
range to locate its 148-byte commit. Physical lookup now uses authenticated
OID-to-placement, pack-offset-to-OID and pack-inventory pages. Offset lookup is
required for delta reconstruction; paging only OIDs would retain a hidden full
reverse-index read. Every physical alias remains represented. Primary placement
prefers a self-contained encoding; logical facts agree across aliases while
encoding-specific delta-base facts remain attached to their own placement.

The capsule owner constructs complete physical coverage from authenticated source
sidecars, admits encoded output before upload, and verifies full readback before
publishing readiness. Path completion retains that descriptor hash. Maintenance
can repair missing or corrupt derived bytes; provider/authentication failures
propagate. Foreground readers bind the descriptor to captured state, transport and
storage prefixes, use operation-admitted stores, and return definitive misses
without probing whole indexes. Catalog ordinals and visible-ref authority remain
with their existing owners. GC retains the descriptor and its immutable page
body; stale descriptors can be collected while a shared current body survives.

Browse-readiness version 3 replaces the unshipped version 2 record. No release tag
contains the introducing browse-record commit. Normal readers reject retired
records; the elected maintenance owner rebuilds them under the observed CAS token.
The released manifest protocol and its object-catalog lookup remain unchanged.

Focused proof includes 10 metadata component tests, five runtime regressions,
three browse-record tests, two publication-fence tests, the CLI error-source test
and the GC sweep regression. The offset-delta runtime fixture contains no IDX/REV
sidecars and reconstructs exact verified bytes. Removing OID/offset agreement
fails the forged-directory regression; the initial zero-test negative-control run
is excluded. Existing shared consumers pass 21 path-state and 27 graph tests; the
qualification observer passes two tests. Strict Clippy passes for the four touched
orchestration/read crates with `--no-deps`. The broader invocation fails six
pre-existing coordination lint expectations unchanged from the PR base. CLI test
compilation, HTTP/auth-server/SDK checks and both qualification-tool builds pass.
A strict CLI-wide invocation reports 605 lint errors; JSON diagnostics contain no
primary error span on modified CLI lines compared with the stacked base. This is
not a clean CLI-wide lint result. The missing CLI conversion for the new error
variant was found by wider build and CI proof, then corrected with source-chain
coverage.

The local RustFS run completes 121 native-Git query processes and three maintenance
passes. Each fixed resolve/attribution cell contains 20 cold and 20 warm samples;
the 100-commit fixture additionally checks all ten supported qualification queries.
Every measured operation finishes with zero active backend operations. Source and
binary hashes, native oracles, request traces and checked receipts remain external.

| History | Cold resolve bytes | Cold attribution bytes | Resolve p95, ms | Attribution p95, ms | Physical body bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| 100 | 46,175 | 103,428 | 10.205 | 104.636 | 91,522 |
| 1,000 | 68,117 | 483,280 | 12.756 | 110.685 | 676,936 |
| 10,000 | 87,518 | 392,549 | 72.424 | 145.501 | 6,530,830 |

Warm fixed queries read zero backend bytes. At 10k, cold resolution falls from
844,608 to 87,518 bytes and attribution from 985,949 to 392,549 bytes. The fixed
queries stay below the frozen 512 KiB decoded / 1 MiB transferred limits. These are
shared-host debug timings on local RustFS, not representative S3/release latency.
The traversal change from sixteen to four concurrent path-page reads is included
in these measured fixed-directory cells; other workloads remain unqualified.

Maintenance takes 2.345 / 5.080 / 22.581 seconds and peaks at 34.5 / 53.7 / 245.6 MiB
process RSS in these fixtures. The 10k physical body is 7.73 times the old pack-index
range. That storage amplification remains an optimization gate: dictionary/row
compaction and batch-request behavior must be measured before calling the design
fully efficient. Outer control/ref/inventory opening, the complete stage-4 edge
matrix, old-reader lifecycle, representative provider and source-API release gates
remain open. Stages 5–8 and the separate unified-staging workstream are not complete.

## Compact physical-directory format

Unreleased physical descriptor version 2 replaces version 1. OID rows contain a
pack-slot/offset pointer and logical facts. Packed-entry ranges, CRCs and delta
base facts occur only in physical offset rows. Metadata stores presence flags and
only fields that are present; known zero sizes and all-zero base IDs retain their
presence, rather than becoming missing facts. The existing metadata codec validates
flags and object-kind codes after compact decoding.

| Directory | Key bytes | Value |
| --- | ---: | --- |
| OID → placement | 20 | slot (4), offset (8), present logical facts |
| Physical offset → entry | 12 | OID (20), length (8), CRC (4), present physical metadata |
| Pack ID → slot | 32 | slot (4), count (8), size (8) |
| Slot → pack | 4 | pack ID (32), count (8), size (8) |

Slots are canonical in encoded pack-hash order. Both dictionary directions are
authenticated and joined against the caller's pinned inventory; queries do not
scan the entire dictionary to translate a pack ID. Slot pages use the shared dense
positional coverage checks. Full verification reconstructs all four directories
and compares canonical descriptor and body bytes. Point reads cross-check OID
pointers with the physical entry's OID; offset reads also verify the primary
placement and compatible logical facts without moving delta-base facts between
aliases. All nested reads share one encoded-work budget.

Normal readers reject retired physical descriptors; the elected maintenance
owner rebuilds them into version 2 before publishing the new hash. No release tag
contains the version-1 implementation. Readiness remains version 3; graph, path,
released catalog formats and reference authorization retain their existing rules.
The reader and row codec now have separate modules to keep construction, I/O and
wire validation inspectable. Fifteen component tests cover compact-field presence,
truncation/reserved flags, dictionary disagreement, pack/entry permutations and
retired-format rejection alongside the original coverage and binding tests. The
exact forged-pointer negative control fails when OID agreement is removed and
passes after source restoration. The five runtime, 21 path, 27 graph and shared-body
GC regressions pass; metadata Clippy and payload/storage feature checks pass.

Compact-format RustFS proof completes 301 native-Git query processes and three
maintenance passes. Resolve, attribution, object-point, object-batch and blob
cells each have 20 cold and 20 warm samples at 100/1,000/10,000 commits. Warm queries
read zero backend bytes. Maintenance recognizes old physical descriptors and
rebuilds current readiness before these query comparisons. Earlier timing and
storage receipts above describe the prior format.

| History | Prior body bytes | Compact body bytes | Reduction |
| --- | ---: | ---: | ---: |
| 100 | 91,522 | 37,216 | 59.34% |
| 1,000 | 676,936 | 275,663 | 59.28% |
| 10,000 | 6,530,830 | 2,658,423 | 59.29% |

The 10k body is now 3.15 times the old 844,460-byte Git index, down from 7.73 times.
Cold fixed-query traffic is 37,365 / 66,534 / 72,285 bytes for resolve and
61,880 / 273,003 / 377,318 bytes for attribution. Compact dictionaries introduce
an extra authenticated join on larger fixtures: resolve uses 5 / 7 / 7 requests;
attribution uses 61 / 50 / 26. These costs remain part of provider/batch qualification,
not a claim that every workload improves. Construction, storage and layout changes
add verification responsibilities for both dictionary directions; they do not add
new configuration or runtime dependencies.

The correctness run overlapped a local CLI build. An overlapping timing-only attempt
is explicitly excluded; final timing comes from a later run without those tasks.
Source/binary hashes, checked summaries, migration results and traces remain
external. Opening, the full edge/lifecycle matrix, representative S3/release proof
and subsequent initiative stages remain open.

The separate timing run completes 120 additional native-Git comparisons with 20
samples per cold/warm cell. Its cold p95 measurements are:

| History | Resolve p95, ms | Attribution p95, ms |
| --- | ---: | ---: |
| 100 | 15.634 | 297.608 |
| 1,000 | 15.245 | 509.871 |
| 10,000 | 123.526 | 182.518 |

These timings regress against the prior-format local run despite lower transfer
and storage. In particular, the 1k attribution p95 rises from 110.685 to 509.871 ms.
The two runs are on a shared host rather than a controlled paired release build;
the cause is not established. Repeated authenticated dictionary joins and page
traversal need profiling, followed by controlled provider/release qualification.
This is an unresolved performance gate; smaller index bytes do not close it.


## Remove redundant compact-directory traversal

Offset-delta lookup previously read pack ID → slot, then slot → pack, then the
same pack ID → slot again. The first forward row is already authenticated and
checked against captured inventory. Both lookup directions now share one typed
slot-row reader; offset lookup joins its original forward row directly to that
reverse row. The reverse pack identity check remains mandatory. OID lookup still
checks both directions, and all reads retain the same aggregate work admission.
This changes no stored bytes, descriptor version, public API or configuration.

A stronger re-signed-directory test redirects a pack to another existing valid
slot with equal size/count. Point and offset lookup reject it. Removing only the
reverse identity comparison makes offset lookup accept another pack's object;
the exact one-test negative control fails and restored source passes. All 15
physical-index tests, five runtime regressions, metadata strict Clippy and both
qualification-tool builds pass. Temporary probes are removed from source.

Twenty interleaved in-memory samples of 1,000 warm joins compare the actual new
helper with the redundant third traversal. Median times are 35.700 versus
40.805 ms (12.5% lower); the optimized path is faster in 14/20 pairs. Its p95 is
156.667 versus 108.873 ms, so the noisy shared-host experiment does not establish
an improved latency tail or a completed end-to-end performance gate. Other
worktree compilers were observed using CPU during diagnosis. Earlier preliminary
prototype timings are not substituted for this production-helper measurement.

The remaining opening audit identifies a distinct boundary:
`open_view_from_root_with_metadata` → `assemble_layered_control_view` loads the
complete checkpoint, then constructs its visibility index to order ref controls.
`control_is_ready` checks new-branch base membership in that index; the complete
capsule sibling applies the same rule. Footer-only fetch deliberately has a
narrower admission contract. Bounded opening therefore needs authenticated
visibility/catalog lookups and transaction reconciliation, not a silent switch
to the fetch loader. Existing metadata-view tests cover snapshot equivalence,
multi-ref activation and missing dependencies; those invariants must remain in
the next implementation slice. This audit does not close the opening gate.

The dictionary-join binary at `d68a9177915` completes all 121 RustFS native-Git
query processes: all ten queries at 100 commits and 20 cold/warm resolve and
attribution samples at 100/1,000/10,000 commits. Warm queries read zero backend
bytes, and measured operations finish with zero active backend work. Exact source
and binary hashes, native results and independently checked summaries are external.

| History | Resolve p95, ms | Attribution p95, ms | Maximum attribution decoded bytes |
| --- | ---: | ---: | ---: |
| 100 | 18.843 | 398.433 | 51,111 |
| 1,000 | 30.370 | 163.718 | 232,194 |
| 10,000 | 32.646 | 871.628 | 345,933 |

The fixed-query byte bounds pass. These noisy shared-host debug timings do not
close the latency gate. Opening still reads 2,183,801 bytes at 10k and process
memory/CPU grow with history; correctness proof is not bounded-opening proof.


## Indexed opening without redundant placement hints

`git_repository_from_store` previously copied every ordinal admission into an
intermediate OID vector and built another per-object placement dictionary even
when a complete physical index was attached. `lookup_batch_for_read` returns from
that index before consulting any placement hints. Opening now borrows ordinal
admission directly and retains hint dictionaries only for unindexed reads.
Source/member range checks, hash parsing and capsule-run admission/count validation
remain unconditional. Ref reconciliation, visibility proofs, catalog ordinals,
thin-pack client-base authorization and reconstruction checks are unchanged.
The refactor removes nine production lines and adds no format/config/API surface.

Two metadata-view equivalence/activation tests, five runtime regressions and the
unindexed admitted-source lookup test pass. Read-library strict Clippy and both
qualification-tool builds pass. The final-source RustFS replay passes 61 native-Git
processes: all ten queries at 100 commits plus 20 cold/warm opening and resolve
samples at 100/1,000/10,000 commits. Warm resolve queries read zero backend bytes;
measurements finish with no active backend work. Source/binary receipts and checked
summaries remain external.

| History | Cold open median / p95, ms | Opening backend bytes | Process median RSS, bytes | Process median CPU, s |
| --- | --- | ---: | ---: | ---: |
| 100 | 41.672 / 51.923 | 89,134 | 32,931,840 | 0.070 |
| 1,000 | 141.332 / 164.715 | 557,942 | 36,544,512 | 0.225 |
| 10,000 | 675.488 / 747.510 | 2,183,801 | 61,693,952 | 1.050 |

The previous dictionary-join resolve cells used 78,217,216 bytes median RSS and
1.23 s median process CPU at 10k; this replay uses 61,693,952 bytes and 1.05 s.
These are shared-host debug observations, not a controlled release/provider
latency result. Metadata transfer is unchanged. Complete visibility decoding,
transaction reconciliation and source/ref inventory capture still scale; this
cleanup does not satisfy bounded opening or finish stage 4.

## HTTP ref-only discovery

HTTP protocol-v2 `ls-refs` now captures only transaction-consistent ref authority.
It validates that complete map through `RepositoryRefs::from_ref_state`, sharing
the manifest conversion's names, targets, peeling and HEAD policy. The existing
entry limit and cancellation remain at the HTTP boundary. Fetch still opens its
complete verified visibility and payload path; receive validation is unchanged.

The native Git regression first failed with HTTP 502 when a committed capsule
body was unavailable. With the change, `git ls-remote` returns exact HEAD/branch
refs and native fetch still fails with HTTP 502 for that missing dependency.
The sibling native push/fetch exercise passes for main and trunk, including tags,
unborn HEAD, clone, ref changes and atomic rejection. The same existing ignored
qualification passes against a fresh dedicated RustFS prefix. Three HTTP wire
tests, unauthorized-membership rejection, shared ref-validation tests and
architecture guardrails pass. The frontend builds from the unchanged lockfile.
Test scratch uses the mounted workspace volume; earlier fixture-start failures
on the main disk preserve the required 20-GiB Cell capacity check.

Strict remote-Git library Clippy passes. Strict HTTP Clippy reaches the unchanged
six coordinator lint expectations described above; the diagnostic run allowing
only `unfulfilled-lint-expectations` passes without source suppression. The server
binary builds. PR #535 head `b5296e22091` passes every non-skipped CI check,
including native Git versions, Windows/macOS protocol, both image jobs and RustFS
partial-clone lifecycle. Scheduled fleet qualification was skipped and remains unproved.
No format, dependency or configuration changes accompany this discovery slice.
Full ref enumeration remains output-sized, and prefix filtering still captures
the complete map. Targeted/paged ref capture and remaining control/index scaling
are open stage-4 gates. The multi-command stdio helper retains a pre-opened fetch
snapshot; deferring that work and the CLI's native-pack materialization require
separate consumer/lifecycle proof rather than this stateless HTTP change.

## Stdio ref-only discovery

The terminal helper now captures capsule ref authority before advertising
protocol-v2 capabilities. It opens payload and visibility dependencies only
when a `fetch` command arrives. The same captured refs serve every `ls-refs`
round; hidden refs, peeling, unborn HEAD and existing entry limits remain intact.

Deferred capsule fetch admission checks both the ref maps and their visible
transaction positions against discovery. An intervening force-push or
deletion/recreation cannot reuse identical tip OIDs to substitute another
inventory. Changed authority produces a terminal `ERR` requesting a new fetch;
the helper does not silently recapture refs. Promotion from a tip-bound view to
complete visibility applies the same check. Once admitted, negotiation retains
its verified repository, haves and existing generated-pack lease ownership.

All 44 upload-pack session tests pass, including capsule discovery without run
payloads before/after checkpointing, transaction ABA rejection, legacy discovery,
empty repositories, negotiation and generated-pack admission release. Architecture
guardrails and workflow YAML parsing pass. The session module is now included in
Linux, macOS and Windows protocol CI. Final code commit `f98ca6f0880` builds and
passes 92 native Git/RustFS lifecycle checks across 323 commands, including
discovery, clone, incremental fetch, filters, shallow history and hidden-ref
policy. Source/binary identity and a clean checkout are verified in the external
`helper-discovery-native-r2` receipts. Shared debug-host timings are not provider
latency qualification; final CI and packaged-image proof remain required.

Strict CLI Clippy reaches the six unchanged coordinator expectations. Allowing
only that lint category exposes 604 CLI diagnostics on both base `b5296e22091`
and final code. Primary source-token comparison, normalized for line positions,
whitespace and future layout sizes, matches all diagnostics; one moved call's
span differs only by its binding expression. The one newly introduced match-style
diagnostic was fixed. These are baseline comparison results, not a strict lint
pass; no suppression was added to source.

Complete ref capture remains output-sized;
remaining control/catalog/index opening costs are still stage-4 gates. The
earlier helper capability probe still captures control metadata when an empty
local object database needs classic cold-clone pack routing; this slice changes
terminal upload-pack admission and does not remove that separate probe.

## Selective CLI range reads in qualification

Remote `RepositoryReader` snapshots now use the shared verified Git range reader
instead of installing every captured pack into a local Git object database.
Local repositories retain their native ODB path. Remote commit selection uses
current-ref reachability; retained physical objects alone cannot select a commit.
Plain files, Crab pointers and LFS pointers keep their materialization adapters,
with captured catalog lookup for Crab recipes. Explicit reads retain the existing
egress configuration and the shared transfer profile's repository coverage.

Download, export and workflow dependency pulls drain the remote Git runtime on
success and error. Download/export also await already-started file tasks before
returning an error. Pointer reconstruction receives the reader's cancellation
token, and Git operations finalize their semantic result before locator closure.
Remote Git diagnostics retain their source chain while preserving the existing
protocol error classification.

The library check passes. Three focused tests pass: a selected file reads under
256 KiB despite a stored unrelated 2-MiB blob, retained commits are rejected, and
cancellation prevents further storage reads. Listing fails the same byte bound:
the metadata reader fetches the large packed entry to verify CRC when publication
has not supplied its uncompressed size. The failing receipt is retained; the test
bound is unchanged.

The shared reader now consumes already-proven locator kind/size metadata without
fetching payloads. Its focused test passes and confirms that a subsequent content
read still rejects unavailable bytes. Capsule publication currently supplies kind
but no size, so authenticated size publication remains required to fix listing;
skipping entry integrity checks is not an acceptable substitute. Pointer,
publication/concurrency, native command and lifecycle qualification also remain
required. This implementation is not yet claimed as working. Full control/catalog
decoding and native index sidecar scans remain separate stage-4 opening costs.

PR publication checks for this slice: workspace formatting and the locked CLI
binary build pass. Cargo-confirmed focused tests report 26 passed and one failed:
read/selection tests 9 passed plus the existing listing byte-bound failure,
download tests 5 passed, export tests 7 passed, error-source/diagnostic tests
4 passed, and proven locator metadata test 1 passed. The debug linker emits its
existing compact-unwind section-size warning. Broad lint/CI and native remote
CLI end-to-end qualification remain pending. Publish this slice as a draft;
these results do not establish stage-4 completion or readiness to merge.


## Authenticated object-size publication in qualification

Capsule producers now encode sidecar version 3: pack-offset object kinds,
fully resolved uncompressed Git sizes, and sparse cross-pack delta bases. The
existing pack checksum, object-count binding and Blake3 digest cover these facts.
Lengths and counts are checked before decoding size and external-base payloads;
new external-base ordinals must be strictly increasing. Content reads retain
independent packed-entry CRC and reconstructed Git OID checks.

Incoming quarantine supplies verified reconstructed sizes, including internal
and external delta targets. Native Git builders request `objectsize` alongside
`objecttype`, with replacement refs disabled. Git's pack-header size describes
the delta program for deltified entries, so it is not used as the logical size.
See [Git cat-file batch output](https://git-scm.com/docs/git-cat-file#_batch_output)
and [Git's deltified representation](https://git-scm.com/docs/gitformat-pack#_deltified_representation).

Producer coverage includes shared remote preparation, native CLI capsule push,
checkpoint physical repacking, protected receive synthesis, filtered views, and
S3 gateway capsule mutations. Both origin-backed and installed capsule locator
consumers carry size facts into `GitObjectMetadata`; catalog ingestion also
preserves the new field. Opaque `CapsuleGitPack` construction retains its existing
format-only validation boundary.

Release audit: kind sidecars introduced in `800364813bd` are reachable from
`v1.0.1` and `v1.2.0`–`v1.2.4`; prepared pack artifacts introduced in
`3f21529f020` are reachable from `v1.2.0`–`v1.2.4`. Released kind-only APIs and
`PreparedPack::kinds_path()` retain their original shape/encoding. A separately
reserved temporary metadata artifact supplies the unshipped capsule path;
legacy publication remains on the released sidecar. Both artifacts belong to
the same scratch owner and release their disk reservations together. Capsule
repacking produces resolved facts for prior kind-only capsule sources.

The initial mechanics receipt passes 28 focused tests: 12 codec/range tests,
13 quarantine tests and 3 native metadata-query tests. These include malformed
size/count/ordinal payloads, checksum binding, resolved internal/external delta
sizes, replacement-ref isolation and scratch cleanup.

The first CLI rerun still exceeded the unchanged listing byte bound. Tracing
that failure found two additional gaps: control-only sources omitted the metadata
range, and pack-index decoding discarded logical facts. Lazy index sources now
carry the authenticated metadata range separately from the index, without
reading reverse indexes or spanning intervening pack bodies. Batch flights bind
the extra range proof into their identity. One OID-ordered fact vector replaces
the sparse external-base map and participates in cache memory accounting.

After these corrections, all 38 reader tests pass, including batched size facts,
exact source-byte accounting and changed metadata-hash rejection. All four CLI
remote-read tests pass, including the original 256-KiB listing bound beside the
unrelated 2-MiB blob. Affected CLI, remote, read, protected-service and gateway
consumers compile. The expanded RustFS runner now exercises real CLI download
and export for plain/Crab/LFS content and checks temporary-object cleanup; its
live receipt remains pending.

The published draft at `0ea3e637bf9` has two CI failures. The architecture checker
requires the removed pack installer in its delegation inventory; a prepared
replacement additionally checks the new remote adapter and retains raw-layout
bans. Its isolated positive/negative checks pass, but applying the inventory
change awaits explicit approval under `AGENTS.md`. Git 2.40 fails the mirror
pack-child cancellation marker check: the command returns `CRAB-E0090`, while
the hook-stop witness is absent. The same check passes on base PR #537; the
subprocess runner is unchanged in #538, and the cause remains unproven.
These findings do not establish draft readiness, complete lifecycle proof or
stage-4 completion.


The size-publication implementation is committed at `b29565b9422`. Its clean
source checkout matches the qualified CLI binary. The expanded RustFS native
smoke passes 107 checks across 329 commands, including 15 new checks for remote
CLI plain/Crab/LFS download/export byte identity, absent installed Git ODBs and
export temporary-object cleanup. This run uses native Git 2.50.1 and a dedicated
RustFS prefix; it does not resolve the separate Git 2.40 mirror-cancellation CI
failure or establish representative S3 latency.

Focused checks now total 75 passes: the prior 70 plus one physical-repack test
preserving newer refs/captured history and four catalog persistence/recovery
tests. The new catalog test queries pinned locator size facts before object
reads, so payload reconstruction cannot hide a dropped size field. Released
kind-only and absent-sidecar cases retain absent size facts. Strict library
Clippy passes for `crab-git` and `crab-remote-git`; broader CLI/lifecycle gates and
the pending inventory approval remain open. The native receipt stays bound to
`b29565b9422`; subsequent catalog-test/documentation changes do not change its
production code.


CLI Clippy qualification initially reported three new large-future diagnostics.
Boxing individual read/factory calls did not remove them and was discarded.
Boxing only the provider initializer in the shared `OnceCell` removes the new
transfer warnings and 19 previously reported diagnostics. Locked Tokio source
confirms initialized cells return before invoking this closure, so warm lookups
do not allocate the box. No lint suppression was added to source.

The comparison reports 586 CLI diagnostics versus 604 at the earlier baseline,
with large-future byte sizes normalized only for matching. The one additional
match-arm diagnostic concerns a pack-error conversion verified byte-identical
to both the baseline and `origin/main`; it is retained as an existing-source
limitation. The invocation permits only the previously identified dependency
`unfulfilled-lint-expectations` category to reach CLI diagnostics. This is not a
strict full CLI lint pass. The small initializer change still needs current
binary/native qualification before publication.


The initializer change at `6672e9adec3` now has a successful locked CLI binary
build. Replaying the six remote CLI download/export cases against the existing
RustFS fixture with fresh destination/cache directories also passes: plain, Crab
and LFS bytes remain identical, no installed Git ODB is created, and export
leaves no temporary object files. This narrower replay qualifies the changed
initializer; the complete 107-check receipt remains bound to `b29565b9422`.
The debug build retains the existing compact-unwind linker warning. Neither
receipt resolves the Git 2.40 cancellation failure, the pending architecture
inventory approval, nor the remaining stage-4 and release qualification gates.


## Process-group cancellation grace in qualification

A real process-runner regression reproduces premature SIGKILL: the leader exits
and its pipes close after SIGTERM while a descendant with detached stdio is
still completing durable shutdown. The callback marker is absent on the original
runner. Both changed source files match `origin/main` before this fix, so this
is an existing lifecycle defect exposed during the CLI-read qualification.

The shared runner now permits early cancellation completion only after leader
exit, joined pipes and a process-group observation containing no live members.
macOS reuses its existing kernel group observer; Linux checks bounded proc stat
records, excluding zombie/dead members and handling exit during either open or
read. Restricted inspection does not prove completion. The original ten-second
deadline still escalates unresponsive descendants; the leader stays unreaped
until the group signal, preserving its process-group identity. Windows retains
JobObject termination and completion waiting. Other Unix targets consume the
full grace period because they have no group observer here.

Dependency proof: `process-wrap` 10.0.0 sends signals to the group, but its wait
loop uses `waitpid`, which cannot observe non-child descendants. This is why
leader completion cannot substitute for group completion. See
[waitid child selection and WNOWAIT](https://man7.org/linux/man-pages/man2/waitid.2.html),
[Linux proc stat state and group fields](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html),
and [XNU group PID enumeration](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/proc_info.c).

Focused macOS qualification passes all 13 mirror process tests and both platform
cleanup tests. The new descendant fixture proves graceful callback completion
before the deadline and deadline escalation with a stopped heartbeat when the
descendant ignores SIGTERM. An isolated Linux container compiles the exact
SHA-256-bound canonical runner source and passes the same two descendant cases.
That probe has a small error/cancellation boundary driver; it is not full Linux
CLI or Windows qualification. Native CLI cancellation, full cross-platform CI
and the earlier Git 2.40 hook-marker failure still require current proof.


The locked CLI build at `3b1e6426a83` passes. Its clean-source native cancellation
replay passes all three checks across seven commands: cache ownership stays
held during the real upload-pack hook, the stop marker exists before Crab
returns cancellation, and subsequent cache cleaning succeeds without changing
remote refs. Cancellation returns exit 10 in 358 ms. The existing qualification
harness and prior read-only fixture are reused with an isolated Git config;
native Git is 2.50.1. This does not resolve the Git 2.40 CI gate by inference.

CLI library Clippy emits the same 586 existing diagnostics as the preceding
qualified slice, with no additions/removals. Its invocation permits the existing
`unfulfilled_lint_expectations` category; no source suppressions are added.
The isolated Linux image lacks Clippy, so it supplies compilation/runtime proof
only. Full CLI strict lint and cross-platform CI remain open.


## Verified CLI LFS streams in qualification

PR #539 now passes all 15 non-skipped CI checks, including native Git 2.40,
macOS/Windows protocol, RustFS lifecycle and both image jobs. The scheduled
fleet/owner-loss job remains skipped. This closes its process-runner gates,
without claiming complete stage-4 opening or later stages.

The LFS materialization audit finds a separate cancellation gap. The original
CLI path collects the entire LFS body before writing, and its export verifier
does not observe cancellation while waiting for source bytes. A native-Git and
controlled-storage regression times out after cancellation before headers. A
real CLI replay through the existing RustFS HTTP proxy additionally requires
SIGKILL after SIGTERM and leaves a temporary download file. These are retained
pre-fix receipts, not passing qualification.

Both CLI LFS paths now use `LfsObjectStore::get_stream_with_session`, with
cancellation during response setup and each body read. File output stays in a
private sibling temporary path until verified EOF; pending filesystem writes
are flushed before cleanup or publication. A reader-owned `LfsReadSession`
drains admitted blocking hash work before Tokio exits. Materialization futures
must finish/drop before shutdown. No full-body CLI collection or duplicate
SHA-256 verifier remains.

Released v1.2.4 selective reads report LFS integrity through `CorruptObject`;
the adapter retains that diagnostic while shared LFS owns SHA-256, size and
response framing. Storage and worker errors retain their existing typed sources.
SDK delivery already uses the same verified-stream/session contract. HTTP/S3
streams already use the canonical streaming verifier in their longer-lived
server runtime; this CLI ownership change does not alter those consumers.

All 13 read tests pass, including four header/body cancellation cases across
file/writer output, private streaming progress, corrupt-content destination
preservation and unchanged remote range/visibility bounds. CLI library Clippy
reports 585 existing diagnostics versus 586 before this slice, with no additions.
The invocation permits existing `unfulfilled_lint_expectations`; it is not a
strict full CLI lint pass. Native candidate success/fault qualification and
current 10-GiB/release/provider measurements remain pending.


The zero-size export shortcut also bypassed source validation. A native Git
fixture containing an LFS pointer declaring zero size against a valid nonempty
LFS object reproduces empty publication. Export now verifies every zero-size
source through the selected snapshot before its empty PUT. The regression
checks the preserved integrity diagnostic and absence of a target object.
The shared fixture retains the public reader owner for explicit shutdown.

Final narrow qualification passes 36 tests: 13 read, eight export, five download,
nine canonical LFS stream and one detached-hash session test. Stream contracts
cover response framing, digest rejection, validator identity and no receipt
writes. The final CLI Clippy comparison remains 585 versus 586 diagnostics,
with no new warnings. Native candidate proof is still pending; the previous
forced-termination receipt is a baseline failure.


The clean-source locked CLI build at `59304d9d350` passes. Six fresh-cache
healthy download/export commands retain byte-identical plain, Crab and LFS
content without installing a Git ODB. All six native LFS fault cases pass: both
commands observe header/body cancellation without SIGKILL, reject corrupted
content, preserve the original destination and leave no temporary output files.
The first fault run had an export readiness event contaminated by an earlier
request; immutable request-scoped witnesses remove that fixture race, and the
full replay passes. The earlier failed receipt is retained.

The existing qualification runner now includes the same six cases and 18 checks
on plain-HTTP RustFS POSIX runs. Its direct invocation passes all 18 checks
against the qualified binary and the existing read-only fixture. Windows token
contracts and external S3 performance use their separate gates; these signal
injections are not claimed there. Production source still matches the clean
qualified revision; subsequent harness/documentation changes do not alter it.
Full new-head CI, current 10-GiB read qualification, remaining bounded control
opening and stages 5–8 still require work.

## Deferred CLI catalog materialization

Selective Git/LFS snapshots previously materialized the complete Xet catalog and
copied every file binding into a lookup map before selecting content. CLI
snapshots now retain only the authenticated catalog source; first Crab
hydration initializes the shared immutable lookup. Snapshot clones share that
initialization. Both file and writer delivery use the same captured source,
without rereading mutable roots or opening the latest file-index database.

The captured source detaches checkpoint catalog bytes and transfers ordered
control deltas. It releases unrelated visibility, source directories and Git
metadata instead of retaining the complete view. Materialization preserves
canonical dependency validation, descriptor conflicts and file-binding
precedence. Existing complete inspection and mount hydration keep their shared
catalog contracts; mount needs its lookup for ongoing reconstruction.

Blocking catalog decoders belong to the repository reader. Cancellation can
abandon waiting for a decoder, but shutdown drains its tracked work. Admission
stays registered until a decoder is tracked; a closed tracker rejects later
initialization. Finish or drop materialization futures before shutdown.

Focused checks total 147 passing tests: 16 CLI read, 35 capsule reader, 78 minimal
metadata codec, eight export, five download and five fixture-consumer tests.
Strict Clippy passes for the shared reader and minimal metadata crate.
Default-feature CLI Clippy retains the parent's 585 diagnostic identities;
the strict invocation fails on those existing warnings. Numeric sizes and line
references are normalized in that inventory comparison. No new diagnostics or
source suppressions are introduced.
New cases cover concurrent
file/writer hydration, first hydration after later catalog publication,
shutdown draining, unchanged visibility rejection, checkpoint precedence and
catalog conflicts. A synthetic checkpoint initially violated the existing
nonempty-source contract; its fixture now includes a valid source descriptor.
This does not weaken the codec. Native qualification and new-head CI are
recorded separately when complete. CLI lint remains a separate baseline gate.

This removes an unnecessary opening-time reconstruction and map copy. Control
deltas remain decoded during opening, checkpoint catalog bytes remain captured,
and first Crab hydration still materializes the complete catalog. Bounded
visibility/control opening, paged historical Xet lookup, current 10-GiB reads,
provider latency and stages 5–8 remain unqualified. No format or release contract
changes in this slice.

The clean-source locked default-feature CLI build at `1393945ace6` passes all
six fresh-cache RustFS download/export commands with byte identity and no
installed Git ODB. Six LFS header/body cancellation/corruption cases preserve
existing output and leave no temporary files, without forced termination.
The initial functional replay generated Python bytecode; disabling bytecode
writes gives another passing, clean-source replay. Detailed source/binary-bound
receipts remain external. Fixtures are plain 20 B, Crab 16 KiB and LFS 8 MiB;
they do not qualify 10-GiB reads or S3 latency. Subsequent documentation edits
leave the qualified production source unchanged. PR #540 passes all 13
non-skipped CI checks and is ready for review; nothing is merged.


## Physical paging and selective CLI integration

The indexed reader integration combines the physical paging series at
`2d7f7e241da` with logical-size metadata, process cleanup, verified LFS delivery
and captured Xet catalog initialization. The origin-backed locator extractor
uses the canonical v3 metadata decoder, retaining logical size and zero values
when building compact physical pages. Existing CRC/OID verification, catalog
ordinals and ref visibility retain their separate owners.

CLI snapshot opening now loads published browse readiness and attaches it only
to its matching captured state. Previously this caller never loaded readiness,
so maintenance could publish physical pages without the selective CLI using
them. Header/control/readiness waits observe the reader's cancellation token.
The shared record and index APIs remain canonical; no read-side maintenance or
index-error fallback is introduced. HTTP projection and qualification already
use the same captured-view attachment.

Focused integrated checks pass 205 tests: 20 CLI read, 15 physical index, five
runtime index, 35 capsule reader, 22 path, 27 graph, two publication-fence,
78 minimal metadata and one GC shared-body regression. New CLI cases exercise
real index construction/publication, zero/large sizes without unrelated payload
scanning, physically retained commit rejection, deferred Crab hydration and
missing required physical pages. Removing index attachment makes the precise
missing-page test fail; restoring source makes it pass. A featureless publication
test invocation ran zero tests and is excluded; its publication-enabled replay
runs both tests. Shared reader, remote-index metadata and runtime strict Clippy
pass. Default CLI lint retains the parent's diagnostic identities (numeric
sizes/line references normalized), with no additions.

The original RustFS fixture is unchanged. A fresh task namespace receives its
objects for integrated maintenance and selective-read qualification. Native
binary-bound receipts and new-head CI are recorded when complete. Importing an
existing paging implementation does not close its latency, visibility/control
opening, release or lifecycle gates. Full visibility still loads during normal
opening; stages 5–8 and current 10-GiB/provider qualification remain unfinished.


The clean-source locked CLI and qualifier build at `3df42b5782d` passes integrated
RustFS maintenance and six fresh-cache plain/Crab/LFS download/export commands.
All materialized bytes match, without installed Git ODBs or export leftovers.
Twenty cold/warm resolve pairs match the original fixture's native Git main tip;
warm queries read zero backend bytes and all measurement boundaries have no
active backend work. Cold query transfer is 62,092 bytes; cold opening still
transfers 595,293 bytes on this fixture. These numbers expose remaining opening
work, not a latency guarantee or history-scaling qualification.

Deleting only the fresh namespace's physical-page body makes native selective
download fail without overwriting existing output. Restoring the exact saved
body allows a fresh process to return byte-identical content. The original
fixture is untouched. Source/binary hashes and full receipts remain external.
This proof uses 20 B / 16 KiB / 8 MiB content, not current 10-GiB qualification.
Subsequent documentation edits leave the qualified production source unchanged.

## Paged visibility format foundation

The ordinal visibility producer and complete decoder share `CRBVPG03`, replacing
the unreleased monolithic ordinal stream. Its directory is bounded to 16 KiB;
internal dictionary and transition directories use the shared authenticated page
codec. The sorted OID dictionary supplies ordinal rank and optional source/member
admission. Ref records bind names to closure roots and preserve absent versus
present-empty transition/history maps. Closures use 4096-position blocks with
sparse or bitmap encoding; identical pages share storage within one body.

Complete decoding checks hashes, ordering, coverage, source/member width bounds,
transition semantics and canonical re-encoding. Existing consumers still bind
the decoded proof to the captured source catalog and validate actual member
indices. Producer/checkpoint, shared reader, physical selection and history
recovery use this same codec; no alternate visibility policy is introduced.
Visibility page metrics have their own format label.

This is a draft format foundation, not bounded repository opening. The enclosing
format-5 checkpoint still authenticates the complete visibility section. Normal
opening still fetches the section and materializes the visibility index; the
directory alone is not independently trusted for range reads. Publishing page
roots in authenticated control metadata, lazy membership and ref-edit ordering,
range budgets and native E2E qualification remain required before dependent work.

Release-tag inspection finds no tag containing `5e28f2a8beff`, which introduced
the layered-checkpoint module. The retired development `CRBVORD1` stream fails
explicitly; no fallback reader is added. Existing development repositories using
it cannot be opened by the new full reader without an explicit migration.
Migration/recovery qualification remains a release blocker. Released manifest
formats are unchanged. The prior RustFS receipts qualify the parent source,
not this format change; current 10-GiB and provider qualification remain open.

Focused validation passes 142 tests: 85 payload-only capsule metadata tests,
35 shared capsule reader tests and 22 capsule writer tests. These include dense,
sparse and empty closures, member admission, transition/history round trips,
tampering, truncation, authenticated invalid coverage and arbitrary sparse-set
properties. A one-million-position closure encodes below 160 KiB. Strict Clippy
passes for payload-only metadata and the shared reader. These checks establish
codec and consumer integration; they do not establish native E2E, bounded
transport or migration safety.

## Authenticated checkpoint ranges and fetch admission

The checkpoint footer now commits to the visibility directory, including its
source-catalog binding, dictionary count and relative page roots. Control-only
decoding requires exactly one matching ordinal section; conflicting or missing
commitments fail before publication. Complete inspection also compares the
section directory with that footer. The unreleased format-5 outer envelope is
retained; older development ordinal checkpoints without these commitments are
rejected. No released layered-checkpoint contract exists and no fallback reader
is introduced. Development migration remains an explicit upgrade boundary.

`LayeredVisibilityReader` batches selected OID-to-member reads from the captured
immutable checkpoint. It validates actual source/member indices and retains
misses and duplicate occurrences. The shared cache translates relative pages
through the authenticated body offset; graph, path and physical indexes retain
zero base offsets. Query admission charges all required pages, including warm
ones, before issuing each I/O wave. Each reader owns its directory and bounded
cache; it opens no metadata database or background tasks.

Incremental fetch selection uses this reader for both complete and footer-only
views. The requested objects have already passed fetch authorization. Placement
only narrows candidate packs, and the existing installer still proves each
member is self-contained and covered by requested objects or local haves.
Cancellation and missing/corrupt pages return errors; page failures do not become
locator misses. The CLI preserves typed storage, metadata and cancellation
errors through its existing shared conversion.

Focused validation passes 280 tests: 85 payload-only capsule metadata, 95
storage-enabled capsule metadata, 13 physical paging, 27 graph, 21 path,
35 shared reader, two publisher/fetch admission and two measurement tests.
At 10,000 and 100,000 OIDs, footer opening stays below 16 KiB and selected
admission below 128 KiB; repeat reads transfer zero backend bytes. Other cases
cover corruption, missing ranges, actual member bounds, invalid footer roots
and cancellation before I/O. Strict metadata/shared-reader Clippy passes.
Qualifier Clippy with dependencies stops on six existing coordination lint
expectations; those source files are unchanged from the parent. Selected
qualifier Clippy with `--no-deps` passes; this does not establish a clean
dependency-wide lint gate.

The existing qualifier gains `visibility-members`. This elected maintenance
operation publishes a checkpoint and records 20 cold/warm admission pairs against
complete canonical decoding. Run it only in a dedicated qualification namespace.
It does not run maintenance from ordinary reads. Native source/binary receipts
will record the live result when complete.

Normal repository opening still materializes visibility for ref inheritance,
removals and transaction ordering. Lazy closure updates remain required before
switching that path; this change makes no bounded-opening claim. First-parent
change-point blame, historical Xet paging, lifecycle/release qualification, fuller
blame, unified staging and current 10-GiB/provider grids remain unfinished.

The clean-source locked CLI/qualifier build at `d2683fcde6e` passes a fresh
RustFS replay: native pushes, checkpoint publication, clone with content/tip
identity and a later push/fetch. Twenty cold/warm admission pairs match complete
decoding. This fixture transfers 4,424 B for checkpoint footer opening, 322 B for
a cold admission query and zero bytes for a reused-reader warm query; every
measurement boundary has no active backend work. The fixture contains 1 MiB of
ordinary Git data, not current 10-GiB Xet or provider latency qualification.

Default modern Git chooses stateless upload-pack for warm fetches. The first
corrupted-page probe therefore bypassed the classic selector and is not its
failure proof. An explicit classic-protocol replay corrupts a required
dictionary page while keeping the authenticated footer intact. Fetch returns
a page-integrity error without changing installed packs or remote-tracking
refs. The original checkpoint bytes are restored and verified byte-for-byte;
retry reaches the native source tip. Full source/binary receipts remain external.

Rust 1.99 CI caught the parent's constant-size `chunks_exact` lint. The codec
now uses fixed-array chunks, with all 21 codec checks and strict metadata Clippy
passing. This correction is published in parent PR #544 before the reader
rebase. Documentation-only evidence edits leave the qualified source unchanged.

## Lazy closure validation in query opening

Canonical CLI reads and HTTP browse/projection ingestion use
`open_view_from_root_with_paged_visibility`. It captures the authenticated
checkpoint footer and catalog range, reads ref descriptors and validates declared
visibility edits through selected OID ranks and closure blocks. It does not decode
the complete object dictionary or historical transition streams. Complete
metadata inspection stays available to checkpoint maintenance, protected receive
and native Git planning; their callers require materialized visibility.

The shared page traversal derives dictionary rank from authenticated child
counts. Publication builds and verifies canonical pages from the complete
visibility index; maintenance continues full coverage and canonical-byte
validation. Query reads validate loaded routing/counts and closure blocks,
including sparse/bitmap canonical form and tail bounds. Required missing or
corrupt pages remain errors. Ref descriptor enumeration retains its legitimate
ref-count cost; immutable history roots are admitted but not decoded.

`PagedGitVisibility` retains the materialized index's edit rules: first lexical
matching base at ref creation, old-tip membership before replacement, exact
removal membership, new-tip coverage and the global dictionary ceiling. Ref
creation snapshots borrowed changes through immutable sharing; later edits cannot
change that inherited state. Validation completes before state mutation.
Transaction expected-old links, activation and predecessor order retain their
existing owners. Materialized and paged controls share evidence-shape validation.
Absent visibility evidence stays absent; object authorization remains with the
captured-root graph/read boundary rather than physical placement.

Catalog capture checks authenticated length/hash and is admitted before I/O.
Catalog bytes detach without retaining the historical visibility backing object;
first Crab hydration retains canonical catalog precedence and conflict checks.
This does not page the historical Xet catalog: its captured byte cost remains
until that workstream is implemented. Query page admission subtracts captured
control/catalog bytes, while immutable object size alone no longer rejects a
small visibility query. All checkpoint control loaders also enforce the format's
footer-size ceiling before issuing a range request.

Focused comparison covers inheritance, later edits to the borrowed ref, removals,
replacement, deletion/recreation and matching rejection of invalid edits. At
10,000 and 100,000 OIDs, a fixed edit reads below 128 KiB. Dictionary-rank tests
exercise nonzero sibling prefixes and duplicate inputs. Publisher-to-reader
integration preserves refs, transaction positions, Git snapshot and catalog after
checkpointing and a later update, eliminating the complete checkpoint GET.
Pre-checkpoint activation matches complete opening. Full initiative/release gates,
10-GiB qualification, Xet catalog paging and later blame/staging work remain open.

Final-source qualification at `9a08da540463` passes 285 focused metadata/read/
consumer tests, 14 CLI read tests and one HTTP browse test. Locked CLI/qualifier
and HTTP builds pass. The retained CLI was rebuilt after committing so its
embedded Git revision matches the clean source; the earlier retained binary's
parent revision failed the existing provenance check. Qualification does not
count that failed provenance run as clean-source proof.

RustFS query replay preserves byte identity for ordinary text and a 1-MiB binary
after checkpoint and browse-index publication. Canonical resolve opening reads
17,419 B; cold/warm queries read 2,127 B and zero bytes, with backend work drained
at each boundary. These are small-fixture observations, not final tail-latency
or history-scaling qualification.

Required dictionary and closure pages are corrupted independently while keeping
the authenticated footer intact. Download, export and canonical ref resolution
fail with an explicit page-integrity error. Pre-existing output and its file
inventory stay unchanged. Exact checkpoint restoration is verified byte-for-byte;
retry reconstructs the file, matches the native source tip and drains backend
work. A separate replay stalls the required closure range before headers and
inside its body. SIGTERM makes both CLI commands report cancellation and exit
without a forced kill or destination/temp-file changes.

The existing protocol harness publishes a fresh checkpointed fixture with Crab
and LFS pointers. Its read-only clean-source replay passes all six cold CLI
materializations: ordinary Git content, a 16-KiB Crab payload and an 8-MiB LFS
payload through download and export. Bytes match exactly, no local Git object
database is installed, and export temporaries drain. This verifies first hydration
from the detached catalog without establishing historical-catalog or 10-GiB work
bounds.

A dedicated live HTTP process passes the existing native tree/commit/blob/diff,
historical-read and error oracles with attribution checking disabled. Independent
attribution checks compare all three displayed paths' OIDs, authors, timestamps
and first-line summaries against native Git, including after exact-source Cell
projection readiness. The process shuts down successfully. The unchanged live
attribution verifier instead expects complete message previews, including a
trailing newline. That conflicts with `CommitSummary`'s first-line contract and
`path_state_inputs`' canonical producer; its one-line oracle correction awaits
explicit approval. Do not describe the unchanged full attribution verifier as
passing.

Current PR CI confirms the inherited architecture guard still requires the
retired pack-installer delegation in `crab/src/read/mod.rs`. Its inventory
correction remains subject to the earlier explicit-approval request. No guard,
baseline, dependency or lockfile change is included in this proof update.

## Checkpointed opening and visibility qualification

`lazy-opening-scale-r1` uses the existing scaling harness with checkpointing,
four selected workloads and 20 isolated process samples per history/workload.
All 240 cold/warm query pairs match native Git; all 510 correctness, resource and
transport checks pass. The fixtures retain the baseline's 100 displayed entries,
50-line selected file and 20 content edits. Maintenance, opening and query costs
remain separate. Ambient request-path tracing is disabled for this harness so
transfer receipts retain only object classes.

| Commits | Cold opening bytes | Cold point bytes | Cold attribution bytes | Attribution decoded bytes | Cold attribution p95 |
| --- | --- | --- | --- | --- | --- |
| 100 | 20,305 | 50,055 | 68,616 | 57,847 | 383.889 ms |
| 1,000 | 45,180 | 247,501 | 298,071 | 257,262 | 251.661 ms |
| 10,000 | 46,095 | 273,020 | 378,708 | 347,323 | 3,024.794 ms |

Opening graph decoding is 10,024 / 34,837 / 35,764 B; visibility decoding is
390 / 395 / 393 B. These opening bytes plateau once the queried pages fill;
no complete graph or visibility dictionary is decoded. Reused-reader warm query
transfers are zero; re-opening still revalidates authority and is measured again.
The fixed 100-object batch at 10k reads 1,431,973 B and decodes 1,426,791 B of
physical pages. That cost is distinct from a point read and remains a measured
candidate for tuning; no locator-format or page-size change is justified from
local latency alone.

Attribution meets the frozen 512-KiB decode and 1-MiB transfer limits. Its original
p95 **fails** the frozen relative latency target: four final 10k samples have
unchanged bytes/request counts but much higher wall time. Do not describe this
run as passing the full performance gate because its Git checks pass.

`lazy-opening-latency-r1` follows with 80 read-only, paired attribution processes:
20 samples for each 100/10k and direct/metered combination, alternating order.
All results still match native Git. Direct p95 is 481.103 / 784.528 ms; metered
p95 is 893.437 / 671.941 ms. Large delays also occur at 100 commits. In one slow
metered 100-commit sample, query wall time is 1,634.285 ms, process CPU is 250 ms,
and HTTP forwarding across all phases totals 199 ms. Timing records contain
object classes and durations, without request paths. This isolates substantial
waiting outside measured forwarding; it does not identify its cause or erase
the original failed latency gate. Debug binaries and shared-host variance do not
establish release or representative-provider latency.

`lazy-opening-ref-r1` passes ten native checks. Nested annotated tags reconstruct
the expected file. The existing protocol harness verifies depth-one clone,
deepening, unshallowing and strict Git fsck on the unchanged native-fetch sibling.
After checkpointing and index publication, deleting a side ref leaves its commit
in the checkpoint dictionary. A cold direct-commit read rejects it as unreachable
and preserves existing output. Recreating the ref and republishing indexes makes
the same OID readable again; the nested tag remains correct throughout.

The retained reader binaries are qualified against Rust source `9a08da540463`;
the harness commit `d5e7359fc3a` changes Python and documentation only. A Git diff
confirms all Rust sources, Cargo manifests and the lockfile match that reader
revision. Binary hashes, source inputs, CPU/RSS and per-cell receipts remain in
the external qualification root. Provider qualification still needs an authorized
S3 bucket/region/prefix and locally configured credentials; neither checkout has
an `.env` and this session has no ambient AWS credentials/profile. The earlier
oracle/guard approval requests and public async API release boundary remain open.

## Query phase timing and remaining latency work

The TCP screen's apparent tail improvement does not survive a consistent
median comparison. A subsequent 80-process, 20-pair confirmation alternates
ordinary meter sockets and TCP_NODELAY against the same 100/10k fixtures.
Median attribution is 147.319 / 109.7065 ms for the existing meter and
197.262 / 127.6255 ms with TCP_NODELAY. All Git results match. The meter's TCP
policy stays unchanged; these observations do not establish the original delay's
cause. An owned process-sampling probe exceeds its 30-second deadline without
producing stacks; its qualifier and sampler are terminated and the probe is
excluded from qualification.

`read-phase-timing-r1` qualifies source `e876e2dfa30` with 20 direct-transport
processes at each of 100/1k/10k commits. All 60 cold/warm pairs match frozen,
independently verified native Git results. Every phase uses its allowed timing
fields only and occurs in admission/snapshot/directory/attribution/cleanup order.
Phase durations do not overlap or exceed the parent query. Parent request, byte
and decode counters match the earlier frozen fixture exactly; clocks do not hide
work in preparation. A missing-path failure emits error outcomes for cleanup and
the parent measurement, with no active backend work remaining.

| Commits | Admission median | Snapshot median | Directory median | Indexed attribution median | Cleanup median | Parent p95 |
| --- | --- | --- | --- | --- | --- | --- |
| 100 | 0.036 ms | 13.843 ms | 173.3205 ms | 20.6525 ms | 0.290 ms | 383.917 ms |
| 1,000 | 0.0325 ms | 20.0425 ms | 113.9915 ms | 25.590 ms | 0.2215 ms | 293.513 ms |
| 10,000 | 0.039 ms | 21.912 ms | 33.4125 ms | 34.5275 ms | 0.164 ms | 187.177 ms |

These samples place observed cold time primarily in snapshot/tree reads and
indexed lookup, not operation admission or cleanup. The slowest sample spends
348.535 ms in raw directory reading. They do not prove the source of the earlier
three-second tail or an isolated/provider latency guarantee. The original failed
run remains part of the evidence. Locked qualifier build, selected Clippy with
`--no-deps`, formatting and four existing Python oracle regressions pass.


## Shared point and batch delta materialization

Source `7ee346ece2b` removes the separate point delta application policy.
Point reads retain their packed-entry flights and iteratively gather the bounded
dependency chain, then apply that chain through the batch resolver in one tracked
CPU task. Non-delta point entries retain their direct verified return. Compressed
batch entries charge inflation plus reconstruction; already inflated point entries
charge only new reconstruction allocations. Cached bases remain identity-scoped
and verified. Depth rejection now reports the absolute depth consistently with
batch resolution; the rejection threshold is unchanged. Failed chains do not
publish intermediate reconstructed deltas to the cache.

The production reader removes 25 lines overall; the additional lines are focused
regression tests for exact allocation accounting, repeated requests, cached bases,
insufficient budgets, excessive depth, cycles and cancellation. No exported type,
storage format, dependency or storage-layout contract changes.

Proof: 39 reader regressions, the tracked-blocking shutdown test, and 21 real-Git
integration cases covering REF/OFS reads, batch materialization, budget rejection,
cancelled cold waiters, CRC rejection and shutdown pass. Selected strict Clippy,
formatting and the publication-enabled qualifier build pass. The content-metadata
integration case separately checks ordinary Git, Crab and Git LFS representations.

`shared-materializer-native-r2` qualifies the clean source against local RustFS:
20 cold/warm pairs at each of 100/1k/10k commits. Native Git independently verifies
the first result for each history; subsequent results match those frozen results.
Physical request/byte/decode counters match the preceding phase-clock run. All
successful operations drain backend work. The missing-path error preserves its
admission/snapshot/cleanup phase order, error outcomes and zero active backend
operations. The initial run created only a Python bytecode cache; the final run
disables bytecode writes and records a clean checkout.

This establishes shared reconstruction and local correctness, not a causal latency
speedup. The original failed latency run, representative S3 qualification, full
10 GiB history scaling, and the remaining stage acceptance gates stay open.


## Explicit async API and exposed-type version boundary

The known synchronous-to-asynchronous public API break now declares version
0.2.0 for `crab-remote-git`. The tagged `ReadError::RemoteGit`,
`WriteError::RemoteGit` and `prepare::Error::{Remote,Close}` surfaces expose that
dependency's concrete types, so `crab-read`, `crab-write` and `crab-remote` also
declare 0.2.0. Workspace dependencies pin all four versions exactly. The lockfile
changes only these four package versions; dependency sources and resolved
third-party versions stay unchanged. SDK public values and error categories hide
these concrete types, so its package remains 0.1.0.

Release-policy validation allows library API versions to differ from the product
version while requiring every workspace consumer to pin the declared library
version. It still requires published libraries to be publishable, private crates
to remain private/version 0.1.0, and shipped binary packages to match the product.
SDK packaging reads archive versions from Cargo metadata instead of assuming all
libraries are 0.1.0. Migration documentation requires awaiting graph membership;
no blocking shim or extra stored-format reader is introduced.

`remote-git-api-package-r2` qualifies clean source `a1507f2bbaa`: all 18 normalized
library archives are produced, all four external SDK profiles compile, and all
five existing negative probes fail with their required diagnostics. A separate
external probe using those archives rejects the old synchronous membership call
with an expected-result/found-future type error. Its awaited replacement and the
three exposed error conversions compile. The consumer manifest, source and lock
are restored afterward. Ten architecture-policy regressions pass; actual Cargo
metadata passes the revised version policy. The migrated CLI consumer compiles
with its minimal features; that parent surface emits 18 existing feature-related
warnings, so this is compile proof, not a CLI-wide clean-lint claim. Ten repository
index tests retain released version-1 membership, lazy pages, corruption rejection
and cancellation/deadline cleanup behavior.

This versions and packages the known async API boundary; it does not publish any
crate or establish the entire initiative's final release gate. Representative S3,
10-GiB history scaling, broad CI, and the remaining lifecycle/format/semantics
requirements stay open. PR #553's guardrail failure is verified from its completed
job log: the pack-layout inventory still requires the removed direct installer in
`crab/src/read/mod.rs`. That inventory is unchanged and its prior approval request
remains pending.


## Decode admission survives abandoned waiters

Source `4dfb4ed6eba` moves existing blocking Git decode admission into one private
runtime owner. All six admitted call sites use it: point/batch reconstruction,
packed-entry inflation and inspection, individual index parsing for embedded and
standalone sources, and coalesced index windows. Previously each waiter held its
permit outside the blocking task. Dropping that future detached the task while
freeing its slot, allowing later callers to exceed configured decode concurrency.
Queued and running workers now retain their slots until completion or unwind;
runtime task tracking retains the shutdown join obligation. Cancellation before
admission and cooperative kernel cancellation preserve their existing policies.
The generic tracked comparison/sort path and separate response-pack producer
admission are unchanged. Production Rust shrinks by seven lines; the growth is
four lifecycle regressions. No public types, storage formats or config change.

The negative control extracts the existing caller sequence without fixing its
ownership: a started, channel-blocked worker survives its aborted waiter, while
the semaphore incorrectly reports one available slot at a configured limit of
one. The same regression passes after moving the permit into the tracked worker.
Additional cases cover a detached worker still queued behind a one-thread blocking
pool, cancellation while waiting for a slot, and panic cleanup with a preserved
typed join error. The locked Tokio 1.53.1 contracts confirm that dropping a join
handle detaches work, started blocking work cannot be aborted, and dropping an
owned semaphore permit releases admission; tokio-util 0.7.18 retains task-tracker
tokens until blocking jobs finish.

Proof: 18 runtime cases pass with serial harness scheduling, 48 reader/index
cases and 20 real-Git delta/budget/cancellation/shutdown cases pass. The final
abandonment and reader checks are repeated after lint cleanup. Selected strict
Clippy, formatting and publication-enabled qualifier build pass.
`decode-worker-admission-native-r1` qualifies clean source at 100/1k/10k commits,
20 cold/warm pairs each. Native Git validates the results, physical request/byte/
decode counters match the frozen preceding run, and semantic failure cleanup
leaves zero active backend operations.

The broad parallel runtime run and an isolated repeat fail the pre-existing
1-ms negative-cache expiry test before its immediate cache-hit assertion; a
serial runtime run passes. Its test, insertion and lookup functions are byte
identical to freshly fetched `origin/main` (`cc2e4e25f46`). Neither the test nor
cache implementation is modified. Failed receipts are preserved; the serial pass
does not establish that timing test is stable. Broad CI, the pending pack-layout
inventory approval, representative provider latency, 10-GiB history scaling and
later delivery gates remain open.


## Cache expiry is validated after lock acquisition

Source `19295690f83` fixes a deterministic negative-cache expiry violation.
The old lookup captured time before acquiring its mutex. A blocked lookup could
therefore return an exact miss after its TTL had expired, suppressing ordinary
object lookup longer than configured. The lookup now samples time after lock
acquisition. Process-local negative and manifest cache timestamps use Tokio's
monotonic Instant; without paused test time it wraps the standard monotonic clock.
No public type, cache key, default TTL, dependency version or stored format changes.
Controlled-clock support is enabled only on the existing Tokio development
dependency. Cargo.lock is unchanged.

The negative control holds the real cache mutex, polls the lookup into waiting,
advances the clock to its exact deadline, then releases the mutex. The old ordering
reports a hit and fails; the corrected ordering rejects it. The manifest sibling
already sampled expiry after locking, and corresponding lock-wait and exact-boundary
cases preserve that contract. The existing generation/expiry regression keeps its
1-ms TTL and now checks exactly 1 ms instead of sleeping 5 ms. This preserves and
strengthens its property; no expiry expectation or TTL is widened. A real physical
index/pack fixture seeds an older miss and proves that expiration restores verified
OFS reconstruction with the expected bytes and no pack-sidecar fallback.

Proof: 21 runtime, 49 reader/index, 32 repository and 13 real-Git cache integration
cases pass. Twenty default-parallel runtime runs pass all 420 case executions,
including the formerly scheduler-sensitive expiry assertion. Selected strict
Clippy, formatting and publication-enabled qualifier build pass.
`cache-expiry-native-r1` qualifies clean source with 20 cold/warm pairs at each of
100/1k/10k commits. Native Git results and frozen physical request/byte/decode
counters match. Semantic error cleanup retains its error outcome and leaves zero
active backend work. The preceding failed real-clock receipts remain recorded;
this fix does not erase them or establish a provider latency guarantee.

The pending pack-layout inventory approval, representative S3 qualification,
10-GiB history scaling, broader CI and remaining history/catalog/staging/lifecycle
and blame requirements stay open.

## Prepared reuse retains payloads before recipe sealing

The unified-staging inventory exposed a reproducible ownership gap in
`Index::claim_prepared_chunks`. A direct-stream preparation could select an
existing prepared Xorb without retaining it. Rolling back the original recipe
then removed the canonical placement and physical body before the incoming
recipe sealed. The negative control returned `ChunkNotFound` from a real staged
payload read; its receipt is `prepared-reuse-retention-negative.log`.

Selection now inserts the existing `preparation_payloads` pin in the same
SQLite transaction, once per distinct Xorb in the bounded claim batch. Existing
recipe sealing, finalization, abort, replacement and retirement paths already
respect this ownership table. No schema, public API, chunk precedence, format,
dependency version or configuration changes. Production growth is thirteen
lines at the owner boundary; tests cover actual payload survival and release.

The regression streams real bytes through CDC and the prepared-Xorb writer,
selects them for another preparation, and rolls back their original recipe.
Both abort and seal/publish/release branches verify byte-identical reads while
owned and absence after final retirement. A separate transaction regression
injects an insertion failure and a later size conflict; neither leaves pins,
and the insertion error retains its typed SQLite cause. The pinned rusqlite
0.34.0 transaction implementation defaults to rollback on drop.

Proof: 35 focused prepared-payload cases and 29 streaming lifecycle cases pass;
the existing manual 128-MiB RSS case remains ignored. Selected formatting and
all-targets staging Clippy pass with the repository CI exception for unfulfilled
lint expectations. The unrestricted Clippy attempt fails only on two unchanged
`double_must_use` expectations in `add_push_plan.rs`; its failed receipt remains
preserved. No lint annotations or gate inventories are changed in this slice.

This slice is a prerequisite, not completion of unified staging. Segment-backed
reuse needs separate retention, lookup and compaction proof: claims consult
`chunk_payloads`, while ordinary readers still consult committed/pending
occurrence rows. Git-clean retains its separate streaming implementation.
Unifying ingestion requires preserving every occurrence, native rollback and
clean flush-before-pointer ordering, followed by the alternating 10-GiB
multi-edit/push/read qualification. Representative S3 qualification and the
remaining indexed history/catalog/lifecycle/blame gates remain open.

## Segment reuse has independent payload ownership

The sibling audit reproduced `ChunkNotFound` after a direct-stream recording
selected segment-backed chunks and their original recipe rolled back. It also
found stale canonical locators after compaction: the previous compactor moved
file occurrence rows while leaving `chunk_payloads` at the original position.
The negative receipt is `segment-reuse-negative.log`.

Segment point/batch reads, deduplication and registration now share canonical
payload inventory. Cleanup retains inventory while any raw occurrence, recording
term or sealed recipe owns the chunk. Recordings therefore survive original-file
retirement before sealing, without copying segment bytes or fabricating file
occurrences. Batch rollback releases its recording retention. Sealed recipes keep
all ordered occurrences, including repeats, independently of physical deduplication.

Compaction counts and copies each retained physical payload once, moves inventory
and committed/pending locators in one transaction, and retires the old segment
registry entry before unlink. Output allocation is provisional within that
transaction; failures cannot publish a second current segment. Durable output
installation precedes locator transfer. Exclusive-open recovery removes orphan
compaction bodies only when neither registry nor canonical inventory references
them; a referenced unregistered body fails as corruption and remains intact.
Current-segment recovery includes retained recipe payload boundaries. Segment
handles are captured with locators before releasing the owner lock, so compaction
and reader-pool eviction cannot invalidate captured reads. No synchronous guard
crosses an await. Abandoned cleanup preserves writer-to-index lock ordering.

Release inspection confirms staging layout v1 and the old compactor in v1.2.4.
Layout v2 repairs stale inventory from the committed/pending locators released
readers used, with committed precedence, then updates admission atomically.
Both writable and shared handles can perform this metadata-only upgrade under
their existing SQLite serialization contract. Old binaries reject v2 admission.
Tables, index inventories, payload encoding and public Rust APIs are unchanged.
Upgrade failures retain v1 admission and old locators; missing retained segment
dependencies fail explicitly. Normal v2 opening does not repeat the migration
scan. Migration cannot reconstruct bytes already deleted by an older binary.

The released corrupt-prepared-plan repair contract still uses surviving segment
occurrences. Coalesced planning preserves that source preference with batched hash
lookups, while prepared-only source groups stay bounded to one Xorb. Canonical
inventory retention is not confused with the presence of those old occurrence
rows. The stricter recipe-page occurrence proof used by product plan repair is
unchanged; it is not a new permissive fallback or a claim of unified ingestion.

Real file cases cover recording versus sealed reuse, current versus compacted
segments, original rollback, ordered repeated chunks, captured reads after pool
eviction/unlink, close/reopen and final release. SQLite cases cover two recording
owners, incomplete transfer rollback, writable/shared released-shape upgrade and
early/late upgrade failure with typed causes. A real failed-compaction case checks
one current segment, byte-identical recovery and orphan cleanup. The existing
compaction property fixture follows the changed private transaction API; its
read assertions are preserved. Failed receipts remain recorded.

Proof: the final staging run passes 233 cases with the existing manual RSS case
ignored. Selected formatting and all-targets Clippy pass with the repository's
existing unfulfilled-expectation exception. Seven product push-plan cases pass,
including corrupt-body segment repair, stale remote placements, global dedup and
prepared reuse without rereading. The minimal CLI consumer profile emits its
existing disabled-feature warnings; this is not a clean CLI-wide lint claim.
Production code before test modules shrinks by 146 lines across the three changed
Rust modules. Public dependency manifests, lockfiles and gate inventories are
unchanged. Broader CI and real-provider performance qualification remain required.

Unified byte ingestion, native/Git-clean crossover publication policy and the
10-GiB alternating multi-edit/push/read grid remain required. Representative S3
qualification, pending pack-layout gate approval and the remaining indexed
history/catalog/lifecycle/blame gates also remain open.

### Shared byte-ingestion foundation

Native descriptor staging now delegates to an `AsyncRead + Unpin` ingestion
engine. `stage_reader_streaming` exposes that same engine for non-seeking byte
sources; it does not open or stat its logical lease path. Native wrappers retain
descriptor/path identity validation and changed-file detection. CDC, hashing,
ordered recipes, preparation ownership and failure cleanup stay in one engine.
Pending segment authority flushes before the returned version lease is recorded.
The result retains an open batch; callers still own publication or rollback.

A bounded pipe test covers both segment and prepared-Xorb modes, closes no
staging handle before recovery, and verifies reconstructed bytes, file identity
and absent source metadata. Cancellation of a pending read previously timed out;
the retained negative receipt is `unified-stream-cancel-negative.log`. A token
branch now wakes that read and runs attempt cleanup. Tokio 1.53.1 documents
`read_buf` as cancellation-safe when another `select!` branch wins. The focused
stream run passes 31 cases with the existing manual RSS case ignored.

This is a draft foundation, not completed native/Git-clean unification. Packet
transport wiring, exact prepared-recipe replay, deferred Git-index publication
and crossover failure/10-GiB qualification remain required. The separate
10-GiB run uses the preceding segment-lifecycle binary; it is not evidence for
this extraction. Per-file flush cost and short-read performance remain unmeasured.

### Git-index publication recovery serialization

The unified-staging audit reproduced recovery returning success while another
writer still held Git's index lock. Recovery previously inspected the prior
index without serialization and could roll back a still-active publication.
The negative receipt is `index-recovery-lock-negative.log`; its source patch is
retained separately. Git 2.50.1's [add implementation](https://github.com/git/git/blob/v2.50.1/builtin/add.c)
holds that lock during cleaning, then installs the index after conversion.
Filter success therefore cannot attest to index publication.

Publication recovery now belongs to `crab/src/git/index.rs`, shared by native
add and push. It uses gix-lock 23.0.1's non-writing Marker to acquire the exact
index lock, reads the index and fresh intent list under that lock, and keeps the
marker until staging publication/rollback finishes. Lock acquisition fails
immediately to avoid waiting in the inverse Git-index/staging order used by
Git and its filter child. The typed lock error remains an I/O source. Drop
releases only the marker acquired by recovery; no index replacement is written.
Existing exact-new, exact-prior and ambiguous-state decisions are preserved.
No schema, path encoding, intent format or dependency changes are required.

The regressions cover both exclusive and shared staging handles. A real Git
child pauses inside a controlled clean filter: recovery must retain the recipe
and payload until Git finishes installing its pointer, then publish on retry.
The test releases its filter and reaps Git on exit. Two publication-focused
runs each pass 26 tests, including both handle variants and the real Git child.
The production Clippy contract from `.github/workflows/rust.yml` passes with
the minimal consumer profile and existing warnings. The broader all-test
invocation fails on existing test lint debt; new test `expect` calls were
corrected without changing assertions or adding suppressions. Both successful
and failed receipts remain retained. This fixes the recovery boundary; routing
Crab's clean stream through shared ingestion and deferring its recipe
publication remain required.

### Unified production byte ingestion and external publication handoffs

Production Git-clean now routes ordinary bytes and changed prepared streams through
`crab-staging::stream::stage_reader_streaming`, the same engine native add uses.
Packet adaptation consumes exactly one filter request and preserves its terminal
flush. It restores typed framing errors after ingestion cleanup. The synchronous
filter runs on its existing blocking thread; this does not promise interruption
of a stalled underlying blocking stdin read. Short reads coalesce before CDC to
avoid retaining a full read allocation per small packet. Exact prepared inputs
are verified without CDC or rewriting payloads. Changed inputs replay bounded
verified chunks from a pinned recipe, then continue with the supplied stream.
Late edits still pay prefix reread/CDC cost; no speedup is claimed without measurement.

Git's filter success precedes its index installation. A new durable handoff owner
therefore distinguishes externally delegated leases from active native batches
and published path heads. Git-clean and native `--defer` delegate only complete,
verified recipes with durable payloads. Native atomic index publication retains
its existing intent and rollback protocol. Pointer discovery precedes batched
published/delegated recipe selection in push. Snapshots capture exact eligible
leases and retire only captured owners after remote publication, preserving active
native owners and handoffs created after capture. Both direct and intent publication
supersede only published leases; another caller's open lease remains its property.
The old test requiring deletion of an open prepared owner was removed because its
behavior violated this invariant; the new regression covers both publication paths.

Layout v3 adds the handoff table with a cascading composite lease foreign key.
Released v1 schema creation, locator repair and admission update commit together;
upgrade failure preserves the released shape. The unreleased v2 prototype is
rejected rather than receiving a compatibility reader. The released v1.2.4 public
custom `ChunkStager` API retains its adapter until a breaking library release;
production local ingestion uses the canonical engine. No dependency or lockfile
changes are required.

This remains draft work. New-path 10-GiB multi-edit/push/reconstruction, representative
S3 performance, Git termination and abandoned-handoff reconciliation/reclamation,
full failure injection and broad CI remain required. The concurrent 10-GiB run is
pinned to the older segment-lifecycle binary and cannot qualify these changes.

The final expanded local clean consumer run passes 57 tests. The six-version
crossover exercises both native/Git orders, an unchanged transition, middle/tail
edits, append and truncation. It compares physical prepared-body names and sizes
on the unchanged transition and bounds retained physical growth for small edits,
besides reconstructing every ordered recipe exactly. Staging passes 238 tests
with one existing manual RSS test ignored. Packet/push/publication and lint
receipts remain pending; these local cases do not establish the 10-GiB gate.

CI on `ffcc26edd0d` fails the existing pack-layout architecture assertion in
`crab/src/read/mod.rs`, requiring the retired direct installer symbol. The failing
job is `116161095860` in run `38654549030`. The previously proposed gate update
remains pending approval; no baseline or gate inventory has been changed.

The follow-up real-staging malformed-input case passes: a complete changed payload
without Git's terminal flush returns the original typed EOF error, retains the
prior prepared recipe and leaves exactly its original batch/lease ownership.
Both the temporary prepared-prefix pin and reader attempt are released. This
adds failure proof through the production adapter, rather than only a mocked
stager or packet reader. Two packet tests, one delegated push-admission test,
seven prepared-plan tests and 26 publication tests also pass. Staging all-targets
Clippy passes with the existing unfulfilled-expectation exception. The minimal
product consumer passes the CI correctness/suspicious lint profile with existing
warning debt; strict full-CLI lint cleanliness is not claimed. No production Rust
changes accompany this follow-up test.
