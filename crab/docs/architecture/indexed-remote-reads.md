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
| 4 | Lazy graph lookup and bounded opening | Paged OID/ordinal/parent/summary queries; reachable versus retained proof; tag/shallow/cancellation/cleanup checks | Pending |
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
verified pages; the graph still counts layers. Do not relabel decoded bytes as
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
