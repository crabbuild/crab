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
| 4 | Lazy graph lookup and bounded opening | Paged OID/ordinal/parent/summary queries; reachable versus retained proof; tag/shallow/cancellation/cleanup checks | Foreground graph/path integration qualified on RustFS; bounded control opening, broader shallow/lifecycle and release/cloud qualification remain pending |
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
queries together; they are not per-query RSS. Catalog qualifier totals also
include the complete inspection oracle, so they cannot establish selected-query
memory bounds. Index construction uses an explicit
`build-indexes` operation, outside measured reads, with its own requests, bytes,
CPU and peak RSS. Read-only qualifications do not trigger maintenance.

The verifier checks complete cold/warm measurements, unfinished backend work,
Git object content hashes, page order/continuation, first-parent history and every
blame line. Tree attribution is compared with native Git path history. Verification
remains enabled under `python -O`.

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
This remains a CI qualification gap rather than a passing image result.

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


## Paged run admission: format work in progress

The frontier fixture opens from capsule runs, not a checkpoint. Its 10k transport
receipts contain capsule-control ranges and no `v2/checkpoints` reads. Paging only
checkpoint visibility would therefore miss the measured source of linear opening.

Run footer/admission version 2 commits two existing-engine page trees: OID-to-member
lists and dense per-member object counts. Full readers authenticate the entire
sidecar, cross-check its page descriptor against the footer and reconstruct canonical
bytes, including counts. Member lists retain ordering, aliases and bounds; zero-count
members remain represented. Explicit page-kind discriminants prevent Cargo feature
selection from changing wire bytes. Pure page decoding remains payload-only; the
existing qualifier now counts capsule-admission page decoding separately.

Twenty-one focused run/admission tests pass with default and storage/remote-index
features. Payload-only compilation, strict metadata Clippy, two metadata-view tests
and qualifier observer tests pass. Re-signing incorrect member counts remains an
error; removing canonical body/count comparison makes the exact negative-control
test fail, and restored source passes. The obsolete format-1 positive assertion is
removed; the unsupported-version test now explicitly rejects version 1.

The introducing capsule commit has no containing release tag, consistent with the
recorded upstream release audit. Released v1 manifest/catalog formats are unchanged.
Retired run prefixes must be republished; no compatibility reader is added. An
isolated `make install` passes for FUSE/NFS/cache-server binaries without changing
global installations. Fresh-prefix RustFS proof passes 63 harness checks and 45
commands: Crab pushes 100/1,000-commit fixtures, two index-maintenance processes
finish, and all 20 native-Git query processes pass. Source/binary hashes and receipts
remain external. This establishes a real publication/read smoke for the new format.
The slice still performs complete admission decoding and does not satisfy bounded
opening or finish the initiative.

## Git control opening with cold OID admission

Git read consumers now use `open_git_view_from_root`. Metadata authenticates the
run footer against its captured pointer and validates paged member counts against
the pack directory. It keeps the OID admission body cold and omits eager frontier
placement-hint dictionaries. Complete maintenance/publication readers still verify
the entire admission sidecar and its canonical trees. Reference reconciliation,
visibility edits, catalog precedence and source/member ranges retain their owners.
The wire format is unchanged from the preceding version-2 format slice.

`CapsuleRunAdmissionReader` provides duplicate-preserving batched exact hints and
authenticated misses. Both directories share one encoded-page budget; warm pages
remain charged. The shared cache now accepts embedded body ranges and keys pages
by absolute source offset. Graph, path-state and physical-index consumers retain
their zero-based body ranges. Page and object authentication remain mandatory;
placement membership never authorizes reference visibility.

Five metadata-store tests, 21 payload-only run tests and the Git/complete view
equivalence test pass. The shared pager's 21 path-state, 27 graph and 15 physical
index tests pass. Metadata/read strict Clippy, frontend build, CLI/HTTP/S3 consumer
checks and qualifier builds pass. Fault injection leaves unrequested OID damage
cold during opening; exact lookup and complete inspection reject that damage.

The final-source RustFS replay reuses the preceding format slice's immutable
100/1,000-commit fixtures. It passes 41 qualifier processes: all ten supported
queries at 100 commits and 20 cold/warm resolve samples per history. Native Git
oracles pass, warm resolve reads fetch zero backend bytes, and every measurement
ends without active backend work. Opening reads 78,142/471,334 backend bytes and
decodes one admission count page at 100/1,000 commits. Source/binary hashes,
transport receipts and results remain external; frozen fixtures are unchanged.

This removes full run-OID decoding from these read opens, not all linear opening
work. Checkpoint visibility/catalog decoding, ref/source inventories and transaction
reconciliation remain eager. The unindexed pack-index path remains existing product
behavior. HTTP/S3 service E2E, 10k version-2 scaling and representative-provider
latency qualification are still missing for this slice. Stages 5–8 and unified
native/Git staging remain separate work. The PR stays draft and is not merged.

### Shared visibility checks and 10k qualification

Full-capsule and control-only readiness/application now share borrowed checks.
The old four implementations cloned entire added/removed OID lists to remove ref
keys from a temporary map. Canonical transaction encoding/decoding already proves
unique refs, so counting matched borrowed rows preserves rejection of uncommitted
evidence. Ref-tip matching, deletion checks, cross-ref base readiness and prior
closure validation remain unchanged. The production refactor removes 74 lines;
complete JSON visibility decoding remains eager.

All 36 capsule-read tests and strict all-target read Clippy pass. A new paired
full/control regression rejects extra/missing evidence, deleted-ref evidence and
wrong old/new tips. Removing the unmatched-row guard makes its exact test fail;
restoring the source passes the suite. Both qualifier builds pass. Final-source
RustFS replay passes 121 qualifier processes: all ten queries at 100 commits,
plus 20 cold/warm resolve and attribution samples at each of 100/1,000/10,000
commits. Warm query backend bytes are zero; source hashes match the receipts.
Opening bytes are 78,142/471,334/1,341,201, with one admission count page decoded
at every history size. These shared-host debug results do not establish a
representative-provider latency target or bounded total opening.

The fresh 10k fixture was published through the format slice's isolated installed
CLI; the read qualifier contains the current lazy-opening source. The complete
ten-query 10k qualification **fails** at path history: tree traversal charges
100,010 entries against the unchanged 100,000-entry budget. The failed report is
retained outside the checkout. Resolve/attribution proof does not override that
failure. Path history still visits unrelated commits/trees and needs the planned
change-point traversal; absent/deleted paths require explicit change records
because the current path-state trie drops deleted children. Limits were not raised
and no baseline or expected-failure file was changed.


## Paged visibility delta format and lookup work in progress

Capsule visibility delta version 3 replaces flat JSON OID lists with a paged
ref directory and separately authenticated added/removed OID sets. The shared
pager uses explicit kind codes 14/15. Empty sets have no root, and identical
sets may share immutable pages. Existing visibility byte/ref/object bounds
admit the format; complete decoding also charges logical OID references so
shared roots cannot expand into unbounded repeated output. Complete inspection
authenticates the body, validates edit invariants and reconstructs canonical
bytes. Retired delta JSON has no serialization alias or fallback reader.

Run footer version 3 commits each visibility descriptor. Visibility pages always
remain in their capsule section; the previous small-table footer copy is removed.
Full control readers compare descriptors against the original sections before
exposing edits. Catalog's existing bounded inline/detached policy is unchanged.
The stored visibility snapshot, released v1 proof formats, root pointers and
manifest/catalog contracts are unchanged. Upstream release refs still match the
recorded audit and none contains capsule-protocol code. Retired run prefixes
must be republished; no automatic migration is provided.

`CapsuleRunControl::visibility_reader` captures the descriptor and embedded body
span for one authenticated capsule. Batched OID changes preserve duplicates and
order. Absent refs, absent changes and missing/corrupt pages remain distinct.
Replacement edits must admit their new tip; an OID in both loaded sets is an
error. Warm pages retain their encoded-work charge, and the smallest budget
fails before I/O. Visibility changes alone never authorize a want: prior closure
and ref reconciliation remain separate required proofs.

Focused proof passes 86 payload-only capsule tests, 94 storage/remote-index
capsule tests, all 36 read-side capsule tests, strict metadata Clippy, both
qualifier builds, observer tests and HTTP/S3 consumer checks after the frontend
build. A 10k-OID lookup opens its footer below 4 KiB, stays below a 128 KiB
query budget, returns verified misses, reads zero warm backend bytes and rejects
corruption. Separate tests cover removed/unchanged OIDs, empty edits, shared
roots, wide ref directories, normalized edit properties and re-signed overlap.
Removing canonical byte comparison makes the exact authenticated-unclaimed-byte
negative control fail; restored source passes. The retired footer-version test
now rejects version 2 and allows the new version 3; the existing detached-control
assertion is unchanged.

Final-source isolated `make install` passes for CLI/FUSE/NFS/cache-server and the
Git helper. Fresh-prefix RustFS qualification passes 63 checks and 45 commands:
real Crab pushes publish 100/1,000-commit fixtures, both index-maintenance processes
finish, and all 20 supported-query/native-Git comparisons pass. Source/binary
hashes match the receipts and remain external. One sample per query establishes
publication/read smoke, not the final 20-sample performance cells. Complete
repository opening still materializes visibility and catalog state. This format/lookup work does not yet satisfy bounded
opening, the remaining stage-4 gates, the 10k path-history gate or stages 5–8.


## Lazy frontier visibility reconciliation

Git control capture now keeps explicit paged visibility evidence. Scalar ref edit
headers are authenticated in batches and matched to the unique transaction edits;
unloaded evidence is never treated as absence. Complete and paged controls share
one ref-ordering loop, retaining expected-old links, activation and predecessor
ordering, exact final refs and peeled refs, and uncommitted/deleted-ref checks.

The read owner represents closures as immutable parent/edit layers. A new ref
selects the same first visible inherited base as complete reconciliation. Moving
the source ref later cannot change that captured parent. Prior tips and removals
are checked through membership lookup; removed OIDs cannot also be added. The
historical dictionary retains removed objects without making them visible.
An initial authenticated added root supplies its exact dictionary count; equal
roots add no objects. Differing actual additions are read and checked against
the retained dictionary, preserving its existing maximum. Actual edit work can
still be large; unrelated historical closures remain cold in the initial case.

Bulk `git_visibility_index`, `candidate_git_visibility` and `tip_bound_transitions`
are asynchronous and materialize the same captured controls only on request.
Immutable section hashes, descriptors and canonical bytes are checked before
bulk visibility is exposed. Tip-bound planning retains its complete captured
transition source, independent of the controls still needed for current refs.
CLI, HTTP, protected-service and checkpoint callers await that proof. Diagnostic
formatting excludes the captured credential-bearing transport. The wire format
is unchanged from the preceding visibility-format slice.

Focused proof passes 46 capsule-read tests, 38 checkpoint integration tests,
86 payload-only and 94 storage/remote-index capsule tests, strict metadata/read
Clippy and CLI/HTTP/S3/protected-service consumer checks after a frontend build.
The 10k-OID regression opens below 64 KiB while leaving complete visibility
unloaded. Layer tests match complete edits, preserve inherited bases and reject
removals outside the prior closure. Replacing ref membership with dictionary
presence makes the exact retained-invisible-object test fail; restored source
passes. Final-source RustFS replay passes 41 qualifier processes: all ten queries
at 100 commits and 20 cold/warm resolve samples per 100/1,000-commit history.
Warm resolve backend bytes remain zero, and receipts match the qualified source.
Opening bytes change from 31,888/121,857 to 31,292/79,065 at 100/1,000 commits.
These are local debug observations, not a representative-provider latency target.

Final-source isolated `make install` passes for CLI/FUSE/NFS/cache-server and the
Git helper. A fresh-prefix smoke using that CLI passes 39 checks and 26 commands:
real Crab push, index maintenance and all ten supported-query/native-Git checks
at 100 commits. Shared-consumer tests pass 21 path-state, 27 graph and 78 object-
locator tests (one existing ignored test), plus 27 cross-surface paged tests.

The fresh 10,000-commit corpus publishes successfully, but its full query run
fails path-history admission: 100,010 entries exceed the 100,000-entry maximum.
That failed report is retained. Separate final-source resolve and attribution
qualification passes 20 samples each against the same corpus; opening reads
81,743 bytes and warm query backend bytes remain zero. Source hashes match both
replay receipts. These targeted results do not turn the full-query failure into
a pass. Receipts remain on the mounted qualification volume, outside the PR.

Parent PR #547 CI remains failed after attempt 2. The original effect-delivery
cancellation reservation failure passes on retry; an activity-lease-expiry test
then leaves 74,168 local-disk bytes reserved after shutdown. The cleanup cause
remains unresolved. No test, baseline or expected-failure assertion is weakened.
Checkpoint visibility/catalog decoding, ref/source capture and broader frontier
amplification remain open. The 10k path-history budget failure, stages 5–8 and
unified staging remain unresolved; this slice does not complete the initiative.


## Path-history and blame change points in progress

The 10,000-commit path-history failure comes from walking every first-parent
commit despite available path-state attribution. The entry maximum remains
100,000. First-parent path history now looks up candidate changes, verifies raw
candidate/parent commits and tree entries, and resumes pagination at the parent.
Blame uses the same lookup to skip unchanged versions while retaining existing
line comparisons, mode-only behavior and deletion boundaries. Historical paths
below a blob ancestor are absent; direct entry lookup retains its component-kind
error. Root-path and all-parent history retain their traversal policy.

Path-state descriptor version 6 retains deletion/type-reset ordinals and old
trie descendants. Reset markers mask older visible values without rewriting old
roots or expanding every deleted subtree. Missing descendants can have covering
reset candidates, so raw entry comparison remains required before reporting a
change. The paged reader shares visible attribution and candidate lookup through
one interpreter. A missing/corrupt advertised page remains an error. Complete
loads, compaction, resumable prefixes and maintenance scans retain the markers.
HTTP projection keeps visible-node attribution: its sole caller supplies paths
from the captured raw directory listing (`crates/crab-http-server/src/api.rs`).

Upstream release tags were rechecked at their peeled commits: v1.0.1 and v1.2.0
through v1.2.4 contain no path-state source. Unreleased version 5 is rejected;
existing generation/browse maintenance handles its corrupt/retired descriptor by
rebuilding, without a second reader codec. Broader lifecycle proof remains open.
The read-scaling harness now captures source and manifests for metadata, read,
remote, remote-Git, storage and write owners, alongside binary and fixture proofs.

Proof passes 23 storage/remote-index path-state and seven payload-only tests,
native-Git history/continuation comparisons, cold indexed/unindexed blame
comparisons with separate runtimes, every native blame line for mode/content/
unchanged-file cases, ordinary semantic and four blame regressions, and strict
metadata/remote-Git Clippy. Four harness tests, frontend build and CLI/HTTP/S3/
protected-service consumer checks pass. The separate runtimes prevent a shared
result cache from substituting for indexed execution.

Fresh-prefix RustFS publication, maintenance and all ten queries pass at 100,
1,000 and 10,000 commits: 87 checks / 64 commands. The original failed 10k report
remains retained. Final-source replay passes 123 processes: all ten queries per
history and twenty cold/warm path-history plus twenty blame samples per history.
Receipts match the current Rust source hashes, native Git results and completed
backend operations. Warm query origin reads remain zero. Cold read bytes are:

| Commits | Path history | Blame |
| --- | ---: | ---: |
| 100 | 133,757 | 133,772 |
| 1,000 | 728,954 | 728,968 |
| 10,000 | 4,691,860 | 4,708,972 |

The raw commit walk is eliminated for indexed exact paths. Cold page/range
amplification remains visible in these measurements and needs further tuning;
these are local debug/RustFS observations, not representative-provider targets.
CLI link build passes before publication. This is not complete stage-5
proof: comparison caching and the complete parity matrix remain outstanding,
alongside stage-4 opening/lifecycle/provider gaps and later stages. First-parent
blame remains a bounded supported subset; Git's automatic whole-file rename
following is a separate requirement ([Git blame documentation](https://git-scm.com/docs/git-blame)).

PR #549 CI now also reports a container Compose recovery/cluster qualification
failure. The failed log is retained; cause remains unresolved and no assertion
or baseline has been weakened. This work has not landed and the goal remains
active.


## Captured graph failures and rejected page-size experiment

Captured snapshot graph readiness is required when supplied. Previously the
snapshot opener inherited the manifest opener's suppression of graph failures,
which could silently replace indexed reads with raw traversal. All snapshot
constructors now share required graph loading; missing, corrupt or retired
advertised metadata fails opening. Absent readiness still permits raw reads.

The manifest opener retains its released optional-graph policy. Upstream v1.2.4
(`b4d5fd361f9a1a43fdd468dc604901d2c61006f0`) suppresses optional graph errors
while propagating cancellation and admission rejection. Released snapshot
constructors did not load graph indexes. Explicit graph attachment remains
required. Separate tests cover graph opening failure and lazy path corruption;
maintenance tests prove rejected retired graph/physical descriptors are rebuilt
from authoritative sources before derived reads resume.

A 4 KiB derived-leaf experiment completed 186 source-matched RustFS processes
across 100/1,000/10,000 commits, including twenty cold/warm samples each for
resolve, history and blame. Native Git comparisons passed, operations drained,
and warm queries reread no origin bytes. At 10k, history query bytes fell from
4,691,860 to 1,909,743, but requests rose from 696 to 958. Median cold history
latency was 4926.67 ms versus the prior 1,779.36 ms; blame was 4832.20 ms versus
3,036.64 ms. These local debug timings are not representative-S3 targets.
The page-size policy is rejected: no experimental codec versions or leaf
limits are published. Receipts remain in the dedicated qualification volume.

Checkpoint visibility/catalog opening, comparison caching, provider/lifecycle
proof and later stages remain incomplete. The initiative remains active.


## Checkpoint admission ownership and measured duplicate decoding

The qualifier now exposes `checkpoint`, which uses the real logical publisher
and verifies the advertised pointer and footer afterward. It reports publication
work independently of reads. A successful `metadb compact` no-op is not used
as checkpoint evidence. `checkpoint_visibility` records complete ordinal-proof
decodes separately from graph/path/physical and frontier visibility pages.

The dedicated RustFS fixtures were checkpointed at 100/1,000/10,000 commits,
then their indexes rebuilt. A 189-process baseline passes native Git comparisons
and twenty cold/warm samples each for resolve, path history and blame per cell.
It exposes two full visibility decodes during every indexed opening.

Source/member ordinal bounds now belong to the checkpoint metadata decoder,
which checks its own source inventory before frontier sources are appended.
Previously an out-of-range checkpoint source could alias a newer frontier
source in the combined inventory. The new regression fails against the prior
decoder, then passes with the source fix. Complete view capture still checks
the source-catalog digest and closure proof. With verified physical readiness,
repository opening skips the second decode used only to construct placement
hints. Unindexed placement retains that needed admission map; ordinary fetch
controls still leave the body cold. No format, dependency or config changes.

A source/binary-matched 183-process replay passes the same native oracles and
twenty cold/warm samples per operation/history. Operations drain and warm query
origin bytes remain zero. The measured change is decode work, not I/O:

| Commits | Opening origin bytes | Proof bytes decoded before | After |
| --- | ---: | ---: | ---: |
| 100 | 29,255 | 20,492 | 10,246 |
| 1,000 | 119,259 | 150,768 | 75,384 |
| 10,000 | 771,540 | 1,453,518 | 726,759 |

Proof: 15 checkpoint codec tests with storage and 15 payload-only tests; 46
capsule-read tests; qualifier instrumentation tests; strict selected-owner
Clippy (`--no-deps`, all targets); qualifier and CLI link builds. A broader
dependency-inclusive Clippy probe still fails six pre-existing coordination
lint expectations; this is not a green workspace-lint claim. These expectations
are present on this stack and absent on current `origin/main`; no main-only
unrelated-failure claim is made. CI remains a separate gate.

Opening still reads and decodes the entire checkpoint proof once. The next
implementation must page the retained OID dictionary, source/member bindings,
ref closures and transition histories through authenticated footer roots, then
wire lazy initial reconciliation and explicit bulk materialization. Retained
but invisible OIDs must stay separate from ref membership. Footer/source/ref
inventory costs, historical Xet catalog paging, comparison caching, lifecycle
and provider proof and fuller blame semantics remain incomplete.


## Checkpoint visibility pages in progress

The new canonical checkpoint format is 6 (`CRBCKP06`). A local release-tag audit
checked all 19 tags; none contains the layered checkpoint module. The released
v1 manifest reader is unchanged. Retired v5 checkpoint pointers are rejected
explicitly; existing v5 qualification prefixes must not be used for v6 proof.
No foreground v5 decoder or full-scan codec fallback is added.

The implementation replaces the flat ordinal-body codec with authenticated
dictionary, ref-closure and transition pages. Footer roots bind the body region,
retained-object count and source-catalog digest. Normal Git capture loads the
footer and initial ref headers; reconciliation queries dictionary retention and
actual ref membership separately. Explicit bulk inspection verifies the entire
section against those captured roots and its canonical encoding. Source/member
bounds are checked by both producer and reader against checkpoint sources.

The obsolete flat snapshot type, decoder, builder branch and encoding-ratio test
are removed. Canonical membership and incremental-history behavior remain in
the checkpoint tests. Recovery uses the same builder and pointer format.

CLI hydration also consumes Git views, so the shared view's `pointer_catalog`
API is now asynchronous. It explicitly materializes the captured checkpoint
when required; Git opening does not fetch that body. Publication, hydration,
protected receive, replication and maintenance callers await that canonical API.
Full catalog materialization still exists as the explicit bulk operation;
normal historical Xet lookups remain a later, incomplete gate.

Current proof: targeted capsule suites pass for metadata (96), read (47), and
write (22), including the added captured-catalog regression. Metadata includes
four lazy-reader tests: a 50k-object lookup within
128 KiB despite a body over 2 MiB, retained-but-invisible denial, warm zero origin
reads, full-materialization parity, and corrupted/missing page failure. Shared
read/write/remote compile checks, the CLI link build, protected-receive all-target
compile check and 84 payload-only capsule tests pass. Strict owner-crate
Clippy passes with `--no-deps --all-targets` for metadata/read/write/remote.
Format-specific fixtures use v6 without changing their epoch/visibility
assertions. This local suite is separate from the fresh-prefix end-to-end proof
below.

Fresh-prefix RustFS proof passed under
`checkpoint-pages-fresh-live-r1` outside the checkout. Native push published
unchanged Git fixtures, then the actual checkpoint publisher and index builder
ran before comparison. All three cells (100, 1,000 and 10,000 commits) pass
20 cold/warm samples for
resolve, file history and blame, plus all ten query oracles. The 189 processes
include six maintenance runs and prove 210 native query comparisons and 3,150
blame-line origins. Every measured operation drains backend work; warm history
and blame queries read zero origin bytes.

| Commits | v5 opening bytes | v6 opening bytes | v6 checkpoint page bytes | Complete proof decodes |
| --- | ---: | ---: | ---: | ---: |
| 100 | 29,255 | 20,588 | 504 | 0 |
| 1,000 | 119,259 | 45,456 | 499 | 0 |
| 10,000 | 771,540 | 46,376 | 499 | 0 |

Opening performs 12/13/14 backend operations in these fixed-ref/source fixtures.
These results establish no bound across independently growing refs or source
inventories. Loopback RustFS latency is not representative hosted-S3 latency.

A subsequent commit and native push passes all ten native Git query
comparisons. The first small-text harness invoked `crab add` without a tracking
pattern, so that command correctly skipped the edit; its commit changed only
other staged metadata. This did not qualify a committed text edit. The helper
probe subsequently staged the text with `git add`, checked committed bytes and
pushed the actual edit through the installed Git helper. Removing only the physical-index readiness field also
passes, then the exact readiness record is restored. Tampering with its retained
dictionary page fails capture with a page-integrity error and zero active
backend operations; restoring the object restores native Git parity. A focused
catalog regression passes: a captured Git view materializes its original
catalog after the mutable root changes. Qualification now counts checkpoint
visibility page decoding separately from complete proof decoding.

Before marking this draft ready: finish tagged-ref, force-push, cancellation,
Git-helper fetch/hydration and live lifecycle qualification; independently grow
ref count, source inventory and uncompacted history. The standard Makefile
installer completed into an isolated workspace prefix. Its Git helper passes
clone and strict fsck, actual text-edit push/fetch, annotated tag push/fetch/
delete, force-push/fetch and shallow clone/fsck. A fresh empty client rejects
the removed commit with upload-pack policy error CRAB-E0060; an already-populated
client is not a valid oracle for that denial. The fixture main is restored. Source
inventory/ref capture, comparison caches, catalog indexing, lifecycle/provider
qualification, unified staging and fuller blame remain incomplete.


### Ref and physical inventory qualification

The existing qualifier passed 20 cold/warm samples each for resolve, object point
and object batch reads, plus its full native-oracle set, at 1/10/100/1,000 refs.
All refs point to the same fixed history; selected query output stays unchanged.
The 252 processes prove 280 native query comparisons. Complete repository
opening reads 20,588/34,934/175,073/1,577,466 bytes with 12/22/114/1,028 backend
operations. Its fixed resolve query reads 37,365 bytes at every ref count. This
exposes full ref-head capture cost; it does not establish bounded single-ref
opening. The existing selected-ref API separately passes 20 cold/warm samples per
cell. Its query reads 1,442/1,985/1,989/1,993 bytes in 2/3/3/3 backend operations.
Root loading is measured separately and reads 1,312/2,734/16,955/159,158 bytes;
this is not constant total opening cost. Native push and import already consume
this selected-ref API; the qualifier now exercises it directly.

A second 252-process/280-query matrix holds the final 164 commits and 613 Git
objects fixed while varying publication schedules. Actual pack counts are
1/8/32/64; capsule-run rollup makes actual source counts 1/8/1/1. Logical
checkpointing reads/writes zero pack-body bytes. Opening reads
26,951/45,215/59,169/83,443 bytes, and resolve reads
54,283/55,067/57,755/61,339 bytes. Peak process RSS for resolve stays below
35 MB. These cells expose member-inventory and publication-history growth;
they do not prove independent scaling to 64 source objects.


The direct commit-visibility probe publishes a distinct commit on a temporary
ref and resolves its immutable OID successfully. After deletion and index
rebuild, the same retained OID fails with `Revision { reason: NotReachable }` and
zero active backend operations. This checks the reader's visibility boundary,
separately from Git-helper upload-pack's default want policy.

The existing large-file harness passed against the isolated Makefile-installed
v6 binary in a fresh RustFS bucket. It uses two actual 10 GiB files with
deterministic entropy, 5,000 code files and four versions, alternating native
add and Git clean. It checks pointer staging, incremental upload, fresh-cache
historical hydration and integrity after removing source authority. All 49 checks
pass; earlier v5 or small-file receipts are separate evidence.

The four publications in `large-v6-live-r1` have now passed. These are single
release-binary observations sharing RustFS with other qualification/build work,
not 20-sample performance cells:

| Version / add path | Add seconds | Push seconds | New xorb bytes |
| --- | --- | --- | --- |
| 0 / native | 460.4 | 359.7 | 10,744,916,848 |
| 1 / Git clean | 808.4 | 6.7 | 2,216,724 |
| 2 / native | 156.9 | 6.7 | 2,133,870 |
| 3 / Git clean | 498.5 | 8.9 | 2,215,428 |

After version 1 staging, segments retain 10,739,857,220 bytes alongside
10,744,916,848 bytes in prepared push plans. This reproduces the duplicate local
payload problem when switching staging paths; efficient incremental remote
uploads do not establish unified local staging. Source authority and source
cache were removed before reconstruction. Cold hydration of both 10 GiB files
takes 604.4 / 549.4 / 488.0 / 502.8 seconds for versions 0–3. All eight SHA-256
checks pass. Strict native Git fsck and deep remote integrity pass, with zero
errors and repair failures. The binary fingerprint remains unchanged. Receipts
live under `qualification-large-v6/large-v6-live-r1`; the report records the
binary, source revision, source patch and harness fingerprints. These receipts
qualify the frozen v6 binary, not the new paged pointer-catalog encoding.


### Blame comparison cache

The next branch adds reuse of first-parent Myers line mappings across immutable
blob pairs. Attribution and comparison entries share one bounded LRU and its
existing configuration. Ordered blob IDs, repository identity and an explicit
semantics version identify comparisons; full attribution also includes commit,
raw path and generation. The caller still reads/verifies both blobs, charges the
comparison-cell budget and checks cancellation before using a retained mapping.

The qualifier's `blame-history` case requests ten successive first-parent
snapshots and checks each range against native Git. Hit/miss instrumentation is
separate from graph/path-page decode counts. Two focused cache isolation/capacity
tests, five blame integration tests and two qualifier counter tests pass,
including rejection under a stricter comparison budget. The 11 snapshot-index
contract tests pass, including caller/shutdown/deadline cancellation of graph
and path pages. Strict Clippy passes for both changed owner crates with
`--all-targets --no-deps -D warnings` and the publication feature.

The dedicated RustFS comparison run passes cache-enabled and disabled queries at
100, 1,000 and 10,000 commits, with 20 process samples per cell and native Git
line-origin checks on ten successive snapshots. All 120 processes pass 60,040
native line-origin comparisons. Cold comparison misses fall from 192 to 21 at
100 commits and from 191 to 20 at both larger sizes, with 171 hits per query.
Warm queries read zero origin bytes and all measured backend work drains.

Cold query p50/p95 milliseconds, disabled versus enabled:

| Commits | Disabled | Enabled |
| --- | --- | --- |
| 100 | 693.9 / 1130.4 | 578.0 / 748.4 |
| 1,000 | 2953.3 / 4075.9 | 2925.3 / 4214.2 |
| 10,000 | 2418.9 / 2922.5 | 2235.7 / 2665.1 |

These debug processes share RustFS with the large-file qualification. The
deterministic work reduction is established; the mixed latency results do not
establish a production latency target. External receipts, binary fingerprint
and source hashes live in `blame-comparison-live-r2`. Hosted S3 latency
qualification remains outstanding.

### Historical Xet catalog: page-format foundation in progress

`prepare_delta` in the native push path and `PreparedReceive::upload_capsule`
in shared receive both reconstruct `CapsuleRepositoryView::pointer_catalog`
before selecting a few dependencies. File bindings are applied in publication
order; equal file sizes permit a later shard binding, while shard/xorb identity
conflicts fail. Chunk placement selection follows the lowest xorb hex identity
and then its first matching occurrence. That precedence must survive indexing.

The next branch replaces the monolithic JSON catalog codec with `CRBPCP01`
authenticated page trees for files, shards, xorbs and reverse chunk placements.
Shard closures and positional xorb chunk lists have nested page roots, so a wide
dependency list cannot inflate one row or the bounded top-level descriptor.
Normal lookups will use authenticated misses and loaded dependency closure;
complete inspection retains body-hash, canonical-layout and full closure proof.
This branch only supplies the format foundation through existing real catalog
encode/decode callers. Foreground full scans are not removed yet.

The codec preserves the original hex ordering rather than Xet's raw hash byte
ordering. The locked xet-core-structures 1.6.0 implementation encodes its hex
identities by little-endian u64 words; using raw bytes as ordered keys changes
placement precedence. A wide-catalog regression caught this during development.
All 19 local release tags lack the pointer/run/layered source modules. Retired
catalog JSON is rejected; no fallback or legacy format reader is introduced.
Payload-only capsule tests pass 91 cases; storage-enabled capsule tests pass
103 cases, including the new wide/adversarial catalog cases. The CLI compile
check passes through read, write, remote and VFS consumers. A separate official
Makefile installation completed for fresh-prefix RustFS qualification. The
older installed binary remains pinned to its separate 10 GiB receipts. This
format foundation does not establish the complete stage-6 performance gate.

The Make-generated full-feature artifact now passes the separate
`pointer-pages-live-fuse-r1` RustFS qualification: 96 commands and 71 checks.
Both native add/push and Git-clean/helper push pass initial publication,
fresh-cache checkpoint clone/hydration, alternate-path edits after checkpointing,
a second checkpoint, exact edited/duplicate hashes and deep remote integrity.
The captured checkpoint bodies contain `CRBPCP01`. The test pins the artifact
SHA-256 `10fa04219b2099cc1940f39225500c8e27c7c52b62e424c936c8df77278504b1`
and verifies unchanged Rust/manifest fingerprints. Its version metadata names
the dirty build's base `0ac1c27d4c8`; this is not a clean-head release receipt.
The final installed no-FUSE binary separately passes the same 96-command,
71-check suite in `pointer-pages-live-r1`, with its own binary fingerprint.
This establishes the new format's live integration for these workflows; full
catalog scans, run aggregation and the remaining stage-6 gates remain unfinished.

Run aggregation must retain capsule membership, not just merge every physical
catalog into one map. `assemble_layered_control_view` trims each ref's run history
after its compacted transaction, deduplicates root transactions and applies only
required journal capsules in validated publication order. A selected run can
therefore contain a checkpoint-covered prefix that must not override a newer
checkpoint file binding. A whole-run aggregate without that selection proof
would widen the catalog and change precedence.

The next slice must capture authenticated catalog roots in run/checkpoint
footers, retain the exact admitted capsule positions, and batch requested keys
across those roots. Run-level key aggregation must retain enough occurrence
membership to select only admitted positions and preserve the existing global
capsule order. Shared immutable descriptors may be deduplicated only after
conflict validation; file bindings retain their consistent size and ordered
shard changes. Qualification must include a checkpoint cut through a compacted
run, overlapping multi-ref transactions and incomparable ref histories, besides
the fixed-recipe/increasing-history request and decode bounds. Root frontiers
allow eight runs, ref frontiers 64 and each run 512 capsules; reading each
capsule separately would retain excessive lookup amplification.

### Shared capsule identity across compaction origins

The catalog-source audit found a prerequisite defect in the unmerged capsule
stack. The same multi-ref capsule can be retained as a leaf for one ref and
inside a compacted run for another. Control construction copied the entire
run's pack directory into every capsule and compared visibility readers using
their physical origins. Complete reads deduplicated identical capsule bytes;
metadata and paged Git views instead reported conflicting transaction content.

`compacted-origin-live-r1` reproduces this through real Git and RustFS: publish
one atomic main/feature update, then 31 feature updates to trigger the existing
32-way run compaction. The feature tip publishes and deep remote fsck passes,
but `metadb owner --once` fails with `CRAB-E0020` at ref-head reconciliation.
The focused Rust regression fails with the same conflict before the fix.

The correction assigns only each capsule's own pack members, normalizes their
logical ranges to capsule-relative coordinates and leaves transport ranges on
the run. Control equality retains capsule/transaction/catalog/pack evidence and
the authenticated visibility section hash, length and page roots, excluding
visibility materialization and transport origin. This also removes repeated
whole-run pack-directory clones. No serialized format or migration changes.

The regression passes after the change, checking complete, metadata and paged
Git views, pack counts/bytes and snapshot parity. A contradictory capsule that
claims the same transaction remains rejected in all three views. All 103
storage-enabled metadata capsule tests and 48 read capsule tests pass; strict
owner-crate Clippy passes with storage and remote-index enabled.

`capsule-origin-fixed-live-r1` uses the rebuilt `qualify_remote` executable
(SHA-256 `969b82482c3b1431bce044423e1953624860a33cd57c94d334bebbb13cdc798c`)
against that same real Git/RustFS repository. Canonical browse-index maintenance
now publishes physical and path-attribution readiness. Ten operations, each cold
and warm, match native Git for the captured main tip: revision, commit, tree,
blob, object point/batch, history, path history, tree attribution and every blame
line. All measured backend operations drain before completion. Receipts live
outside the checkout under `capsule-origin-fixed-live-r1/` in the dedicated
qualification directory. The qualifier exercises the shared metadata reader
used by maintenance; a newly installed CLI `metadb owner` invocation and broader
CI remain pending. No indexed-catalog performance claim follows from this fix.


### Selected checkpoint catalog lookup

Checkpoint format 7 (`CRBCKP07`) authenticates `CRBPCP01` catalog roots in its
footer. The catalog payload format stays unchanged. A fresh audit of all 19
local release tags found neither affected codec path; format 6 belongs only to
the unmerged stack and is rejected explicitly rather than retained as a reader
fallback. Root/history format tests now identify 7 as the current contract and
reject 6.

The metadata owner batches requested file hashes, reverse chunk keys and xorb
identities. One page cache and aggregate query byte budget cover file rows,
shard closures, xorb recipes and reverse-placement validation. Missing keys
remain misses; missing pages, invalid ranges, mismatched chunk ordinals and
conflicting immutable descriptors fail. File bindings preserve publication
order and consistent declared sizes. Page cache hits still consume admission.
Pages remain embedded in the existing checkpoint object, so its retention
already retains every catalog page without a new GC object namespace.

The read view constructs lookup from its captured checkpoint and exactly the
same selected/deduplicated ordered capsule deltas used by full materialization.
CLI `prepare_delta` uses the paged Git view and selects outgoing file and chunk
closures, then checks only its candidate xorb descriptors against the base.
The native and Git remote-helper publication path share this preparation.
Protected receive/shared remote preparation, hydration/file-index wiring and
maintenance migration still need their own consumer proof before the gate can
close. Explicit maintenance retains complete inspection.

This is a checkpoint lookup slice, not the complete historical-catalog gate.
Deltas are still materialized and their chunk suffix is still scanned. Run-level
key aggregation must retain exact selected-capsule membership and publication
order; whole-run overlays would replay checkpoint-covered capsules or widen
visible state. Until that is implemented and qualified, unrelated uncheckpointed
history can still increase opening and query work. No end-to-end constant-work
or latency claim is justified yet.

The next run-index slice must batch each requested key once per captured run,
then filter descriptor occurrences by the exact selected capsule set. Immutable
shard/xorb variants need occurrence membership so an unselected conflicting
capsule cannot change the selected catalog; equivalent descriptors can share
one recipe even when their physical page offsets differ. File bindings choose
the last selected occurrence in the view's reconciled publication order. Required
negative fixtures include checkpoint-covered run prefixes, overlapping multi-ref
runs, conflicting selected variants and retained but unselected variants. These
are implementation requirements, not completed qualification.

The first CLI publication test pass reached eight tests: seven passed, while the
old JSON corruption fixture panicked during UTF-8 conversion before calling
production. The fixture also exists on the paged-format parent branch. It is
retired in favor of known-pointer tests that corrupt an actual chunk-page byte
or truncate the catalog trailer inside an otherwise authenticated capsule.
Both must produce a typed catalog corruption error before staging is consulted;
no JSON reader or production validation bypass is introduced.

`catalog-lookup-fixed-live-r1` published format 7 and matched selected file,
shard, xorb and reverse-chunk metadata against complete inspection on RustFS.
Opening decoded no catalog pages. Cold lookup fetched six ranges (9,132 bytes),
while warm lookup fetched zero origin bytes. The run exposed duplicate decode
instrumentation in the cache wrapper; the shared page decoder already emits
that event. `catalog-lookup-fixed-live-r2` passes after removal: exactly six
cold page decodes (9,132 bytes) and zero warm decodes, with the same transport
counts and zero active backend work at completion. The corrected executable's
SHA-256 is `22157c5a665a9a063b1aa81c2fdc1b10c339e9827e66ff524e376aa0a98500ca`.
The raw root/checkpoint footer and object fingerprints confirm format 7 and its
catalog roots; frozen Rust/manifest fingerprints remain unchanged. Receipts are
retained outside the checkout in `catalog-lookup-fixed-live-r2/` under the
dedicated qualification directory. This four-MiB fixture is functional evidence,
not the required 10-GiB/increasing-history performance gate.

Validation passes: 110 storage-enabled metadata capsule tests, 92 payload-only
metadata capsule tests, 48 read capsule tests, 22 writer capsule tests, all eight
CLI publication tests, the six final lookup regressions and both qualification
counter tests. Strict metadata/read/remote Clippy with all targets and publication
features passes, as do CLI compilation, formatting and diff checks. The CLI test
binary emits an Apple linker warning about unwind-table size; the eight tests
still complete successfully. Installed CLI push/hydration and representative S3
latency qualification remain separate required proof.
### Run catalog occurrence index under implementation

Run footer version 4 stores one embedded catalog index after capsule/index-pool
bytes and before admission pages. Its descriptor commits four paged key roots,
the exact capsule source-body extents, byte count and body hash. File, shard,
xorb and reverse-chunk records retain disjoint capsule occurrence lists for each
logical value variant. Equal shard/xorb values share a recipe even when their
original catalog pages occupy different offsets. Distinct values keep separate
membership, including conflicts in unselected capsules.

Full verification reconstructs the canonical index from the original nested
catalogs and checks exact bytes and roots; footer-only validation checks region
coverage and source bindings without fetching index pages. An absent index must
correspond to absent or canonically empty capsule catalogs. Tests cover relocated
equal descriptors, conflicting file bindings, empty sections, compacted control
parity, omitted roots and re-signed occurrence changes. This is a format/producer
slice still under validation. Foreground lookup remains eager for deltas until
selected run sources and their reconciled ordering are wired into the reader.

Producer validation: 117 storage-enabled capsule tests, 98 payload-only capsule
tests, 48 read-side capsule tests and 22 publication tests pass. Strict metadata
Clippy with storage/remote-index features passes. These checks establish encoding,
compaction and existing consumer integration; they do not establish lazy run
lookups or the fixed-edit history-scaling gate. CLI publication qualification is
pending. Its corrupt-catalog fixture currently attempts to publish malformed
catalog bytes, which the new producer validates earlier; that boundary needs
explicit qualification before this format change is ready to land.

The next reader slice captures run catalog roots and original source extents
from already authenticated run footers. Selected queries retain exact logical
capsule order across physical sources. File bindings select the latest visible
occurrence with consistent size; shard/xorb conflicts include every selected
variant; reverse chunks select the lowest xorb and its first occurrence.
All selected pages, including nested original recipes and warm cache hits,
share one query byte budget. Nested shard/xorb values must match the aggregate
semantic commitment after expansion.

The Git metadata view now captures these sources without decoding delta
catalogs. Run-v4 footers no longer inline catalog bodies, and the materialized
delta lookup API is removed. Bulk maintenance retains explicit materialization
of captured sections with their length/hash checks. Complete and paged capsule
controls compare the same catalog section commitment, preserving logical
identity across compaction.

Reader tests cover selected/unselected conflicts, logical order independent of
physical order, equal variants reusing an unselected source, repeated chunks,
page corruption, and fixed requests against 100 versus 20,000 catalog entries.
A read-view integration test holds one file query fixed against 100 versus
10,000 entries and checks bounded opening/query bytes, warm origin silence and
the full maintenance oracle. The storage-enabled metadata capsule suite passes
123 tests, including selected-run queries and corruption. A separately added
regression proves one aggregate page budget applies across two runs even when
the first run is warm. Metadata/read compilation and strict all-target Clippy
with storage/remote-index features pass. The additional control-identity case
also passes from the newly compiled metadata test binary: complete and paged
controls compare equally, explicit materialization matches the catalog, and a
changed catalog commitment is unequal. These establish 125 distinct metadata
checks. The 49 read-side and 22 writer capsule tests pass, including the new
read-view integration. The payload-only suite passes 99 tests; it exposed one
new unused catalog-field warning. That internal field is now storage-gated,
while pure decoding still validates the catalog before discarding it. The
minimal-feature suite passes all 99 tests again after that correction; strict
metadata/read all-target Clippy with storage/remote-index also passes. Four
other payload-only warnings predate this slice. A source guard covers 445 Rust
files in the CLI, metadata and read crates; the feature-gating correction is
the only intentional source change after the initial capture.

The producer-only CLI run completed with seven passing tests and one setup
failure: the corrupt-catalog fixture attempted publication, and the producer
rejected it before `prepare_delta`. Its replacement exercises publication
directly and checks both the root digest and visible refs remain unchanged.
Selected-reader corruption remains covered independently. The replacement CLI
test passes in the fresh CLI run: all eight publication tests pass. The old
producer-only failure remains a rejected receipt, not a passing gate. The fresh
test binary reports the existing Apple unwind-table-size linker warning.

### Installed checkpoint-reader qualification

Normal `make install` completed from frozen revision `8090eb68df1` with a
private installation prefix. It built the no-FUSE CLI, FUSE helper, cache
server and remote-helper symlink. Both installed CLI variants pass the 4-MiB
native/Git managed-file workflows: 74 commands and 58 checks each. Coverage
includes staging/push, clone/hydration, Git/remote fsck, checkpoint publication
and independent cold checkpoint hydration with byte-identical payloads. Raw
stored root/checkpoint inspection confirms format 7 and nonempty catalog roots
for both workflows. All 1,215 guarded Rust/manifest files remain unchanged.

The no-FUSE CLI SHA-256 is
`ce23afdd3054a8c95ac5d117a2a216a9f76baeeedd4709c866246b7ee2f17794`;
the FUSE helper SHA-256 is
`a92a7bcd74322300d4b51f76da73ddceb27936ada8267d60d3e08b616b111b25`.
External receipts are `catalog-lookup-installed-live-r1/receipt.json` and
`catalog-lookup-fuse-managed-live-r1/receipt.json`. These qualify the frozen
checkpoint-reader slice; they do not qualify the newer run-v4 reader, the
10-GiB matrix or representative S3 latency.

The separate default broad harness stopped after 50 commands and 38 checks at
`missing-manifest-list-fails-closed`: it deletes `/manifest`, while the current
initializer publishes only the capsule `/v2/root`. The initializer contract is
explicitly tested by `remote_initialization_publishes_only_the_capsule_root`;
listing still succeeds through the intact v2 authority. The broad run remains
failed and requires canonical-root corruption fixtures. Its expectation has
not been weakened or suppressed, and the focused receipts do not replace it.
A separate canonical-root live probe passes 34 commands and 18 checks. Listing
and clone reject missing roots with `CRAB-E0032` and corrupt roots with
`CRAB-E0020`; restoring each exact root restores the original branch tip. Its
receipt is `catalog-root-failures-live-r2/receipt.json`. The first probe stopped
on a driver assertion against unborn `HEAD`; the corrected probe commits and
pushes a real fixture before fault injection. Neither probe changes the broad
harness's failed status.

### Reject incomplete catalog captures

Fetch-only views retain physical pack/visibility evidence while intentionally
omitting reconciled logical delta controls. Treating their missing controls as
an empty suffix would let either catalog API return a checkpoint-only result.
The guard represents catalog capture as an optional source set:
`None` means incomplete; `Some(empty)` means a complete capture with no indexed
delta contribution. Both selected lookup and full inspection reject incomplete
captures before origin I/O with `ReadError::IncompleteCatalogView`. CLI
conversion retains this typed failure in its standard source chain.

The regression covers unborn and populated fetch-only views, verifies no I/O
on rejection, and contrasts both with an authenticated miss from a complete
Git metadata view. All 50 read-side capsule tests pass, including this guard,
and strict read all-target Clippy passes. The CLI source-chain regression
and CLI compile check also pass. This guard is not included in the frozen run-v4
installation build at commit `2d78b341010`.

### Run-v4 native and Git workflow qualification

The full-feature release binary at `2d78b341010` passes the existing managed
RustFS workflow: 74 commands and 58 checks across native add/push and Git
add/push. Each uses an actual 4 MiB file and duplicate, then verifies lazy
clone, byte-identical hydration, dehydrate/rehydrate, cold checkpoint clone,
Git connectivity and remote integrity. The input is the existing read-only
100-commit fixture; this is correctness smoke, not history-scaling evidence.

Independent inspection confirms both stored publication runs use footer v4
and contain `CRBRCG01` indexes with nonempty roots. Both resulting checkpoints
use format 7 with file-catalog roots. The binary SHA-256 remains
`5171cfee26d393c0121cc3fb02891312f5825b8f22b4434db8abbc7b37cd07c8`,
and all 1,218 guarded source/manifest files are unchanged. External receipt:
`run-catalog-fuse-live-r1/receipt.json`. The incomplete-capture guard above is
not part of this frozen binary. Normal installation, fixed edits against
increasing history, new 10 GiB qualification, consumer/lifecycle integration
and representative S3 latency remain outstanding.

### Published run-reader CI coverage

The protocol workflow at published head `2d78b341010` completed successfully:
[Git protocol qualification run 38669011348](https://github.com/crabbuild/crab/actions/runs/38669011348).
Its unit logs include the catalog-history growth regression, read-view lazy
catalog regression, shared cross-run warm budget regression and complete/paged
control identity regression. The workflow also passes native Git compatibility
for 2.30, 2.40, 2.45 and current Git, macOS/Windows protocol contracts, and the
released-shape RustFS partial-clone lifecycle. Architecture Gates at the same
head also pass. These results cover the published revision; they do not cover
the local incomplete-capture guard or replace the remaining 10 GiB,
history-scaling, consumer and lifecycle acceptance gates. The external log and
revision-bound receipt are `run-catalog-ci-protocol-unit.log` and
`run-catalog-ci-proof.json`.

### Fixed-edit qualification exposed cache close cost

`run-catalog-fixed-edit-r1` holds a 4 MiB file and 4 KiB edit fixed against
16 versus 1,024 unrelated 64 KiB managed files. The 16-file case passes cold
byte-identical reconstruction, strict Git integrity and remote fsck with zero
errors. The larger case fails during its history push at the 900-second
command deadline; it never reaches the fixed edit. This is a failed scaling
qualification, not evidence that the new catalog passes its acceptance gate.

Two stack samples and offline symbolization identify the observed hot path:
`CacheCatalog::reserve_sync` drops its private database, rusqlite closes the
connection, and SQLite checkpoints the last WAL connection. The second sample
records 753 of 788 blocking-worker samples in `fcntl`; disassembly identifies
`File::sync_all` using macOS `F_FULLFSYNC`. The pinned SQLite source documents
this default close/checkpoint behavior. These observations identify cache
close cost during the failure; a bounded session/checkpoint fix still needs a
deterministic regression, security/lifetime proof and a full rerun. No sync
or payload durability policy has been relaxed.

A fresh read after timeout confirms the 1,024-file repository's published ref
still names its initial commit (`run-catalog-timeout-inspection-r1`, eight
commands/fourteen checks). Failed logs, samples, symbols and dependency proof
remain in `run-catalog-fixed-push-diagnostic.json` and the original run report.
The separate `codex/cache-catalog-io` change retains WAL at all three catalog
writer boundaries. Its regression fails before the fix and passes afterward;
46 catalog and 128 private-filesystem/lifecycle/local-cache tests pass, as does
strict local-cache all-target Clippy. The full failed workload still needs a
rerun. The pinned SQLite 3.49.1 is also affected by the upstream WAL-reset
race; a matched Crab/Cellule dependency upgrade is required before release
qualification. Remaining consumer, lifecycle and 10 GiB gates still apply.


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

### Delegated index acceptance during explicit staging maintenance

A follow-up slice freezes delegated batch membership, rejects unfinished recording
or preparation ownership, and requires every completed path to be handed off in
one transaction. It keeps released mutable-open-batch behavior before delegation.
The maintenance listing rejects partial or unverified ownership instead of
publishing a subset of an ambiguous batch. Direct publication consumes delegation
markers in the same transaction that installs canonical path heads.

`crab staging clean` now checks delegated batches under Git's existing immediate
index lock. It reloads immutable lease identities under that lock and retains the
lock through promotions. Every batch path must have a regular-file index entry
whose verified pointer blob matches its file hash and size. Object headers reject
large raw blobs before loading their body; loaded small blobs are checked against
their Git object ID and parsed with the shared pointer contract. Gix 0.83.0's
`find_header` uses gix-odb 0.80.0's fixed loose-header buffer; locked gix-pack 0.70.0
probes delta headers in 32 bytes. Header inspection does not prove full packed
base reconstruction cost, and this is explicit maintenance rather than a new
foreground query. Typed dependency causes are retained. Cancellation drops the
owned Git lock and preserves batches not yet promoted.

Missing/different entries retain their leases, including native `--defer` work
and older unpushed history. Current index absence cannot establish that payloads
are disposable. Full abandoned-handoff reclamation and termination qualification
remain required; this slice does not add a silent age-based expiry or a ref-tip-only
history scan. Normal push keeps its selected published/delegated batch lookup.

Proof: the staging library passes 242 tests with one existing manual RSS test
ignored. The Git-index module passes four tests, including three new real-Git
cases covering acceptance, held index locks, complete multi-path batches, shared
native ownership, invalid small blob identities and oversized raw-blob headers.
The public occurrence regression also passes after the final ownership-query
change. Consumer checks remain in progress at draft publication. The separate
qualification worktree and release artifact remain pinned to #566.

The follow-up occurrence audit reproduced `chunks_for_file` returning an empty
list for a real prepared-only, verified open version: the public readers selected
only published recipes, then consulted segment rows. The negative receipt is
`retained-recipe-visibility-negative-r1.log`. Both writable and shared hash/size
readers now use any retained verified immutable recipe, including native/delegated
leases and push snapshot pins. Recipe conflicts still fail, and publication/push
eligibility remains a separate, unchanged query. Unsealed segment enumeration
keeps its released staging contract. The public regression compares against direct
CDC, retains repeated occurrences and covers before/after delegation on both
handle types. The corrected regression passes with snapshot-only retention after batch rollback
and empty enumeration after the final snapshot release.

The separate #566 release-artifact 10-GiB qualification completed native add and
initial push, then Git-clean add and commit. Its next push failed with
`CRAB-E0080`: the pointer file had no staged chunk recipe in publication assembly
(`crab/src/git/xet_publication.rs`). The failed fixture retains both delegated
leases. This maintenance/occurrence slice does not resolve that push blocker;
full alternating-path and cold-reconstruction qualification remains incomplete.

### Capsule publication uses selected delegated recipes

The preserved 10-GiB failure identifies a verified delegated recipe with 166,469
ordered occurrences and a 10,737,418,240-byte size. Public occurrence enumeration
was fixed in #569, but capsule assembly still loaded prepared plans and remote
chunk authority through published-only queries. That would reconstruct delegated
files and rebuild their xorbs instead of using their ingestion proof.

A minimized real-staging regression also demonstrates capsule assembly accepting
an active native batch before handoff. The negative run completes with the
assertion `unaccepted or inconsistent recipe became publishable` in
`capsule-handoff-negative-r1.log`; its source patch is captured separately.

The follow-up moves the existing published/delegated merge rule into
`crab/src/git/push_staging.rs`, shared by pipeline admission and capsule assembly.
Capsule assembly requires a selected verified recipe with the pointer's exact
size, reads its ordered pages, and batches prepared authority by recipe identity.
It reads remote authority from that same recipe. Physical xorbs still undergo
body, logical-hash and placement verification before they become dependencies.
There is no admission path for an unrelated active native lease.

The new regression covers native rejection, delegated size mismatch, and exact
preservation of a non-default prepared encoding. All nine publication tests pass, including the new three-case regression and
existing protected-publication, catalog-integrity, deduplication and concurrency
cases. Real 10-GiB retry and cold historical reconstruction remain pending.
The historical catalog scan in `prepare_delta` remains stage 6 work.

The caller audit confirms `crab/src/cmd/push.rs` and the remote helper both use
`capsule_push::run`. Neither those success paths nor capsule publication currently
captures or retires staging push snapshots. The legacy pipeline's snapshot tests
therefore do not qualify capsule lifecycle. A separate follow-up must bind exact
selected owners before publication and release only proven committed owners,
including cancellation, partial/ref-CAS failure and concurrent readers. Until that
is implemented and tested, local post-push reclamation is not qualified.

### Capsule snapshot lifecycle follow-up

Capsule push fences retirement before selecting local recipes, pins selected
published/delegated owners, and retains that snapshot through upload and ref
publication. Confirmed remote commit precedes the local committed journal and
exact retirement. Local cleanup errors leave maintenance work and preserve the
successful remote result. Ref rejection or failure retains unpublished leases.

`StagingPushSnapshot` owns marker/snapshot cleanup on drop, including task abort.
Sibling readers defer retirement to exclusive maintenance. It shares the existing
index and physical-payload implementation with the released asynchronous API;
there is no storage-format or dependency change. Guard tests cover drop, task
abort, shared native ownership, sibling deferral and local journal failure.

Native publication-intent recovery now runs in capsule push before snapshot
admission. Recovery uses Git's immediate index lock. If the complete prior index
state is restored, a bounded-memory local Git object walk checks refs and reflogs
before native rollback. Recipes still referenced there acquire canonical history
owners before native leases are released. Cancellation or an incomplete history
walk fails without authorizing rollback. Ordinary pushes without unresolved
intents do not perform this history scan; this does not replace the historical
remote-catalog indexing workstream.

The minimum Git 2.30 dependency contract supports the selected walk flags and
includes other worktrees with `--all --reflog` by default
([revision options](https://github.com/git/git/blob/v2.30.0/Documentation/rev-list-options.txt)).
Its transport allowlist takes precedence over configured transport permissions
([transport implementation](https://github.com/git/git/blob/v2.30.0/transport.c));
the empty allowlist prevents an implicit fetch during local recovery. Windows
startup sets stdout to binary mode, matching the OID parser's LF delimiter
([MinGW startup](https://github.com/git/git/blob/v2.30.0/compat/mingw.c)).

Both exclusive native-add recovery and shared push recovery use this ownership
rule. The shared staging helper validates all requested recipes against the
intent before adding history owners, preserves the original intent across an
interrupted prefix, and permits an idempotent retry. Prepared-only recovery tests
cover subset retention, full reconstruction and physical xorb deletion only after
confirmed snapshot commit.

Both protected finalize clients use the shared auth response validator. A response
with coordinator metadata must attest `Committed` or `Materialized` before it can
permit local retirement. `Pending`, `ObjectsUploaded` and `Aborted` are rejected;
the existing successful response without coordinator metadata remains supported.
The minimal protected-push suite passes 30 tests.

The existing managed-file smoke checks post-push owners, snapshots, raw payloads
and prepared ownership bytes for native and Git helper push. The frozen #566 CLI
baseline completes native add/commit/push and verifies the remote tip, then fails
the retirement assertion with retained owners/prepared payload. Corrected CLI
proof and final-source 10-GiB qualification remain required.

The large-file harness adds `--pattern entropy|repeated|zero`; entropy preserves
its original seed/default. Repeated input uses a deterministic one-MiB block;
zero input writes real blocks. All patterns retain four alternating native/Git
versions, beginning/middle/end edits, two-file identity checks, source removal and
cold historical reconstruction. Syntax checks pass; the new patterns are unrun.

The frozen #570 release build succeeds with `simd-accel,gix-pathmatch` and no
default features. Its binary hash is recorded externally. Resumed workload
`unified-ingestion-10g-r3` passes the formerly failing v1 Git-clean push and the
subsequent v2 native push, with 2,216,109 and 2,133,870 incremental xorb bytes.
The full workload is still running; cold historical reconstruction is unproven.
This artifact precedes the lifecycle and finalize-contract changes above.


The 10-GiB harness also selects `--first-ingestion native|git`, covering both
native→clean→native and clean→native→clean. Receipts include the selected order
and each add's ingestion path. After each add, the harness rejects local
segment/prepared storage (including temporary stream bodies) beyond one initial
representation, cumulative edited
bytes and metadata allowance (10% of source size, at least 32 MiB). This detects
the observed extra full 10-GiB representation; it is not a latency target or a
claim that all allowed overhead is necessary. Existing runs retain their
original harness identity and cannot prove these newly added checks.

The local history dependency probe additionally reproduces exit zero with a
missing reflog commit and an omitted expected pointer. Git reports that partial
walk only through diagnostics; its
[reflog traversal implementation](https://github.com/git/git/blob/v2.30.0/revision.c)
confirms that contract. Recovery therefore requires a diagnostic-free successful
walk before accepting absence. A warning retains the unresolved intent and its
payloads; there is no log-message matching or alternate deletion path. Both a
missing reflog tip and a missing referenced tree are regression cases. Earlier
consumer builds do not qualify this tightened scanner until rerun.

An extracted-source probe compiles the exact helper and canonical subprocess
owner against the frozen release's actual error/dependency types. The old helper
accepts the incomplete walk (negative runtime exits 1); the tightened helper
preserves the healthy reflog result and rejects the missing tip (positive runtime
exits 0). Source hashes are recorded with both probes. This is function-level
runtime proof; the full updated consumer still needs its own Cargo/CLI run.


Reduced crossover qualification passes with the frozen #570 release: Git-first
repeated data and native-first zero data, each with two 128-MiB files, four
versions, source removal, eight cold file digests and remote fsck. Each run records
71 commands and 53 passing checks. An older debug binary fails the new storage
assertion after its first native-to-clean switch: 67,156,162 prepared bytes plus
68,224,532 segment bytes exceed the 102,760,448-byte bound for its 64-MiB input.
These runs qualify the order/storage oracle; they do not qualify the final
lifecycle source or replace the full 10-GiB matrix. The later inclusion of
stream-temporary bytes in the accounting remains part of that final matrix.

The dedicated #572 provider run passes its provider contract and the initial
native cutover push's retirement check, then stops at the old missing-manifest
fixture. It deleted a retired key while `v2/root` remained authoritative. Missing
and corrupt fixtures now initialize a healthy repository, attest the live root,
and verify their deletion/overwrite took effect before requiring list/clone to
fail. Corruption still requires the integrity error; expectations were not
relaxed. Both updated fixtures pass against the unchanged #570 root consumer
(23 commands, 23 checks), and the 11 source-isolation tests pass. The explicit
v1 upgrade/reset fixtures remain separate. Full final-source canary proof is
still pending.

## Forced publication without a local remote tip

A clone can diverge after another writer advances the remote. Its explicit
force push must not require fetching the discarded tip. Capsule admission now
allows an unavailable ancestry result only for an explicitly forced update or
matched ref lease when receive policy permits rewrites. Stale leases and
`receive.denyNonFastForwards` still reject; publication retains its expected-old
conditional transition. Real ancestry-probe errors remain errors.

Proven fast-forwards and rewrites with an available prior tip keep incremental
visibility. Rewrites lacking that tip publish the
existing authenticated replacement-closure representation, walking only the
new tip. This removes old objects from that ref's authorization without reading
discarded history. Pack generation and LFS publication already exclude only
remote tips present in the local object database. Both native CLI and Git
remote-helper publication use this capsule owner.

The regression matrix covers ordinary, forced, matched/stale lease and denied
updates with the old remote commit absent locally. Successful rewrites verify
the remote tip, revoke old-tip visibility, install packs into an independent
repository and run Git connectivity checks. The existing RustFS ref matrix
also exercises native and remote-helper force pushes without fetching first.
These tests remain pending until the updated-source CI run completes.

## Packed-object qualification fault fixture

The Git-current protocol canary stopped before corruption injection because the
LFS pointer blob was packed, while the fixture opened a loose-object pathname.
The same failure reproduces locally after `git repack -ad`.

The fixture now exports its small disposable source history and unpacks it into
an empty object database before injection. Healthy packed duplicates cannot
mask the corrupt loose blob. The original object database is moved aside and
restored in `finally`; the success check compares every original object-file
hash, the pointer, source/remote refs and working bytes. The caller deliberately
packs its source first so the previous failure remains a regression case.
The checksum mismatch and successful restored dry run remain required.

This changes qualification setup only. Full Git-current CI remains required;
a source-layout repair does not establish a product-level compatibility result.

Local proof with the unchanged #572 release: the repaired packed-source mirror
workflow passes 66 commands and 13 checks; deliberate assertion failure after
the verified checksum rejection restores every original object-file hash and
passes strict Git fsck (34 commands, three checks; cleanup scope only). Seventeen
existing log/source-isolation tests pass. These are selected fixture and mirror
receipts, not a full final-source protocol qualification.

## Combined indexed-catalog and staging qualification

The integration branch uses `4ebfc3dcece` as its base: captured catalog lookups,
run/checkpoint pages, persistent cache WAL policy and the fixed SQLite runtime.
It applies the canonical staging, recovery, exact snapshot retirement and force
publication work through `033bc5042cf`. Catalog encoding and visibility owners
remain those of the indexed branch; no hybrid format or alternate reader is
introduced. The two benchmark audit reports remain excluded.

The large-file harness now checks exact staging retirement after every push.
Managed smokes, committed-history smokes and the multi-edit harness share the
same assertion for heads, leases, unpublished batches, reclaimable ownership,
snapshot journals and payload references. Existing check names remain intact.
The stronger oracle passes 83 commands and 65 checks with the frozen #572
release. Its negative control rejects #570 after a successful publication:
two path heads, two leases, 65 payload records and 4,194,304 retained segment
bytes remain. These receipts establish the oracle and staging component; they
do not qualify the new integrated binary or catalog format.

The combined release binary compiled and the focused RustFS smoke passed
163 commands and 84 checks, including exact retirement, committed restaging
before first push, both cold versions, force/ref updates and invalid roots.
The integrated native-first entropy 10-GiB run passes 75 commands and 57 checks:
all four pushes, exact retirement, eight cold historical SHA-256 checks, Git
integrity and remote fsck with zero errors or repairs.
Those receipts bind the earlier integrated source, not subsequent reader changes.

Pinned hydration/mount lookup uses captured catalog pages in PR #581. PR #584
extends this to current-state file lookup and shares ref ordering and captured
visibility with Git readers. Metadata tests pass (458), as do all 47 read capsule-protocol tests after
preserving corruption classification at the read boundary. CLI, runtime and
retained-root lifecycle checks are required before readiness.
A reproduced independent-ref fixture also requires complete inspection to
validate dependencies after assembling the visible set, while publication-time
delta validation remains strict.

`qualify_remote ... <catalog-file-hash> catalog-current` measures the actual
current-state lookup session. The argument uses Xet `MerkleHash::hex()` encoding,
not the pointer wire format’s raw-byte hex. Hydration converts parsed pointer
bytes with `MerkleHash::from(pointer.file_hash)` before lookup; the qualifier
accepts that catalog identity directly. Opening, cold queries and warm queries are separate; the full
inspection oracle runs after measured queries and explicit session close. This
keeps unrelated catalog decoding out of cold-query preparation. Use a quiescent,
dedicated fixture so the post-query full oracle has the same visible authority.
Compare these
receipts across fixed small edits and increasing distinct historical chunks,
ref counts and uncompacted history; checkpoint row bounds alone are insufficient.

Shared/protected preparation remains a separate integration boundary. Complete
inspection remains appropriate for fsck, compaction and GC; foreground lookups
must not inherit those scans. CLI snapshot reads still install all Git packs;
carry forward the range/cancellation and cleanup contracts when removing that
cost. The final combined 10-GiB matrix, history scaling, lifecycle/migration,
fuller blame and representative S3 gates remain open.

The GC regression passes with both existing cleanup tests (three tests total).
It captures a 20,000-file checkpoint catalog without a file query, replaces it,
and sweeps past the grace period. Actual orphan deletion is required; the old
reader then fetches cold pages and resolves duplicated file requests, while a
new session observes the replacement. Both checkpoints have a valid physical
source descriptor/body. This is an in-memory metadata lifecycle proof, separate
from payload hydration and concurrent real-store qualification.

Current-state qualification against the four-version, two-file 10-GiB entropy
history passes with the updated reader. Opening uses 28 backend operations and
149,860 bytes. The duplicate-file cold query uses 497 range reads and 7,434,846
bytes; the warm query repeats all 497 reads. Both results match full inspection,
operations drain and no writes occur. These are one local debug-profile sample,
not cloud latency or independent history-scaling proof. Repeated recipe metadata
reads are a measured remaining performance gap, not a successful warm-cache gate.
The first attempted receipt remains failed: it supplied pointer-wire hex where
the qualifier requires the distinct catalog `MerkleHash::hex()` representation.

A differential run of the same qualifier binary against a real 4-MiB file passes:
five cold ranges / 3,755 bytes and zero warm backend operations. Combined with
the unchanged captured lookup and the existing 256-page / 8-MiB FIFO page cache,
this supports recipe-page eviction as the cause of repeated large-file warm
reads. A bounded cache of already-verified file bindings is the next candidate;
it must preserve the initial complete dependency proof, immutable capture,
ordered duplicates/misses, error retry and shared close/cancellation contracts.

The current reader's minimal CLI/NFS check and debug build pass. The force-push
policy/visibility regression also passes after awaiting the now-async visibility
API in its test caller; all six policy cases retain their assertions. Eleven
Python fixture tests pass. The previously qualified staging-version assertion
from the combined branch is carried forward with exact post-push retirement.

The complete current-source RustFS CLI smoke remains failed (152 commands,
61 checks). Its shallow-since case supplies an ISO date; the helper treats the
remote-helper option as an integer wire timestamp and rejects it before clone.
The harness still expects the earlier unsupported-option diagnostic. Git's
remote-helper boundary passes date text through, while wire fetch uses a
normalized timestamp. Fixing that adapter and qualifying dated boundaries
against native Git are required; changing the expected message alone would
hide the missing behavior. No full CLI E2E pass is claimed from this receipt.


## Capsule chunk authority and exact staged reuse in progress

PR #593 publishes rebuildable global chunk candidates after capsule ref
publication. Each candidate names a source shard in the source repository's
current authenticated catalog and a canonical origin receipt in the caller's
trusted global scope. A physical xorb alone is insufficient: its catalog shard
must retain the payload through the existing GC contract. Direct CLI publication
and protected receive share the metadata encoder and writer. Protected receive's
80 native-Git/in-memory tests pass; this is not real-S3 qualification.

The follow-up stores each placement under its immutable digest as well as the
replaceable chunk head. Push re-reads the exact add-time placement and origin
IDs, validates source membership, and requires current provider identity even
with warm payload caches. Its read-only proof session closes before publication
opens the global writer. Verified source descriptors and original receipts stay
caller-owned, with one retained-metadata byte allowance across source repositories.
Conflicting descriptors or physical assertions fail explicitly.

A regression measured one redundant full xorb GET after otherwise successful
source/origin proof. Adopting the authenticated descriptor removes that GET:
the first before/after pair fails at one versus zero body reads, then passes the
unchanged zero-read bound. Final-source validation passes all 10 focused CLI/Xet tests, including partial
add-time hints and mixed stale/fresh origin assertions. The cold source fixture
uses native Git and real staging with an in-memory object store.
Released v1.2.4 manifest receipts lack these capsule descriptors and still require
full-body verification when imported into capsule publication.

The dedicated cross-repository smoke oracle now requires one complete capsule
source-proof event and its matching adoption event. It checks exact planned,
indexed, matched, source-verified and payload-verified counts, with zero stale,
mismatched or globally reclassified chunks. Five oracle tests pass; the original
substring oracle rejected valid capsule proofs and accepted counters assembled
from different events. This representation correction is not product E2E proof.
Payload-only run tests pass (28), candidate/key-codec/writer tests pass (19),
existing origin/range siblings pass (12), and strict minimal and remote-index
metadata Clippy pass. The production CLI binary build also passes.

The current-source RustFS cross-repository fixture passes 34 commands and 32
checks at 4 MiB (`capsule-exact-proof-r2-c6a202ae`, code head `098859c02f1`).
The source worktree is clean, and its recorded source hashes remain unchanged
throughout the run. Consumer add retains neither segment nor prepared payload;
push revalidates all 58 exact placements with zero stale, mismatched or globally
reclassified chunks and uploads no new xorb. Add and push each read two xorb
ranges totaling 2,658 bytes (footer and chunk metadata), with no full xorb GET.
A separate empty clone cache then performs a real remote payload GET during
hydration and produces the original SHA-256. This is one local debug-profile
correctness fixture, not a latency or history-scaling qualification.

The binary SHA-256 is
`5002c1c59f8e2d6d3fa82abed1c5686c67d46feb93f975c0c1d76c1930f3dc70`;
the report SHA-256 is
`36d6ec1b744a2f5545a9a998710271bc2fa092ceadf756dd56c95acf75eeff2f`.

The shared `crab-remote` preparation consumer, final 10-GiB qualification,
history scaling, provider latency, lifecycle and full blame semantics remain
open gates.


## Shared capsule preparation in progress

The shared `Prepared::upload_capsule` caller in HTTP receive now uses captured
file, shard and xorb lookups instead of materializing unrelated historical
catalogs. An ordinary Git upload with no new Crab pointer performs zero catalog
reads. Selected pointer and prepared-content closures preserve size, membership
and descriptor conflict checks. Capsule-specific implementation lives in
`crates/crab-remote/src/prepare/capsule.rs`; its public API remains under
`crab_remote::prepare`.

Four regressions failed before the change and pass afterward: one redundant
historical-catalog read for a Git-only upload; Xet-word-order formatting of a raw
body digest; acceptance of rewritten xorb/shard scratch bodies after inspection;
and no shared chunk candidate index after a successful native-content commit.
The initial Git-only fixture sent an all-history pack containing an old pointer;
its corrected incremental pack excludes the advertised tip before measuring.
No weakened assertion or production failure is inferred from that fixture issue.

Six public-API integration tests and strict all-target Clippy pass. The tests use
native Git packs, real Xet artifacts, explicit publication leases, exact origin
and source-anchor checks, complete file reconstruction, and a noncanonical index
fault that must leave committed refs successful. The SlateDB test dependency uses
the same pinned 0.15.0 release already used by metadata; the lockfile adds only
that test dependency edge. Production dependency versions do not change.

The dedicated RustFS public-API test passes for both newly created and occupied
xorb keys. It uses native Git preparation, real S3 publication, cold source and
origin-anchor checks, and complete file-hash reconstruction. The historical
pointer integration also accepts the correct declared size and rejects a
conflicting one. The final source passes strict all-target Clippy and the
no-default-feature check.

Eight HTTP receive/fault tests pass. Three dedicated real-RustFS cells also pass:
the public native-content API, native Git push/fetch and browser editing over the
HTTP listener, and the six receive-fault/restart scenarios. Source hashes remain
unchanged throughout these runs. The API test-binary SHA-256 is
`fe8a75004fd4ef7ec0946b999d32a829b83b28f94b51317a9c34e9990e9ebf5d`; the HTTP test-binary SHA-256 is
`92318648278160dd57f96f35dbcffc01f44799a75a43b84442251965c07fef51`. These are small correctness fixtures, separate from the required
large-file, growth and latency cells.

The final combined 10-GiB matrix, unrelated-history/ref/frontier scaling, broader
lifecycle/provider proof and fuller blame semantics still prevent completion.


## Combined catalog/staging qualification and HTTP fault boundaries

The combined revision carries selected publication, verified catalog bindings,
source/browse retention, retained-source fsck, exact capsule chunk reuse and
shared native-content preparation. Focused checks passed: 558 metadata tests,
81 protected-receive tests, six shared-preparation tests, 242 staging tests,
26 CLI integration tests, 57 CLI siblings, 20 origin/lease tests and eight HTTP
receive/fault tests. Explicit live tests remain separately qualified; these
counts do not establish the full release gate.

The first combined RustFS native HTTP fixture returned 202 during its
corrupt-descriptor case, where the corruption oracle required 503. Readiness
can become visible before the publishing repair worker finishes. A diagnostic
assertion with a one-second delayed generation-owner release response proved
that the fixture could inject its next fault while that worker was active.
The shared native HTTP fixture now awaits maintenance completion before each
fault, reloads the current descriptor and checks its snapshot identity. All
original status, error-code, recovery and Git assertions remain intact. The
controlled probe then passed across ten delayed lease releases, and all eight
HTTP unit/fault checks passed. This changes test synchronization, not the valid
production distinction between incomplete readiness and corrupt metadata.

Raw before/after evidence remains outside the checkout in
`final-integration-http-metadata-probe-r4.json`,
`final-integration-http-metadata-probe-r5.json` and
`http-fault-boundary-after-r1.log`. Combined-source 10 GiB qualification,
representative S3 latency, additional history/ref scaling and fuller Git blame
semantics remain open gates. Earlier component receipts retain their original
source identities and are not final combined-source proof.
