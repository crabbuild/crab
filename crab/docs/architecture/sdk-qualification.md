# SDK qualification inputs and evidence

This record accompanies `crab-sdk.md`. The delivery inventory in
`sdk-capabilities.json` covers all 48 section-8 cells: 45 implemented contracts
and three explicit 1.0 exclusions. Its generated Markdown view is
`sdk-capabilities.md`. Inventory validation proves API and test discovery, not
execution or backend qualification. The chronological implementation evidence
below records intermediate states; current-head CI artifacts supersede those
states for release decisions.

## PR 160 CI failure diagnosis

The SDK branch is rebased onto `f63e03fab05`. Conflict resolution retains main's
concurrent OnceCell lookup initialization and exclusive close ownership while
opening the SDK's selected immutable shard root and draining parser tasks.
All 26 lookup tests pass (`sdk-rebase-metadata.log`); SDK write/content
all-target compilation passes (`sdk-rebase-check.log`).

Main's storage stream framing validation now rejects short/long bodies and
wrong response ranges before LFS validation, preserving `StorageError::CorruptObject`.
The first LFS run reports one stale assertion expecting only the LFS hash-error
variant (`sdk-rebase-lfs.log`). The regression now checks the exact responsible
error variant and object identity: hash mismatches remain LFS errors, and framing
failures retain the storage path. All 45 object-store tests pass
(`sdk-rebase-lfs-final.log`). Range pinning also reuses main's validator predicate,
rejecting empty ETags and versions; full reads still verify delivered bytes.

The completed protocol job on `cc06501e35d` fails its direct CRC-error assertion.
Shared-flight completion can retain an Arc of the original error, returning
`SharedRead` depending on scheduling. Main already checks the nested CRC cause
and exact object ID in both sibling CRC regressions; the rebase retains that fix.
The original job log is `ci-protocol-cc065.log`. This is not evidence that the
new head has passed CI.
Both focused CRC regressions pass (`sdk-rebase-crc.log`,
`sdk-rebase-crc-sibling.log`). Strict LFS/metadata/SDK library-and-test Clippy
with SDK write/content features passes (`sdk-rebase-clippy.log`).

The completed CI failures at `9a2e9ef1d21` have three independent causes:

- Protocol and workspace tests lose the scoped telemetry subscriber through
  process-global tracing callsite registration during concurrent tests. The
  full 79-test reader suite reproduces the `(1 origin read, 0 events)` failure;
  probes show the producer and test on the same thread with TRACE enabled but
  the storage callsite disabled. Propagating dispatch into the producer did not
  fix it and was reverted. The test now re-executes alone with a global
  subscriber, matching CLI/HTTP setup, retaining its exact origin/event and
  warm-cache assertions. All 79 reader tests pass (`ci-telemetry-after.log`).
- NFS publication's freeze guard relied on closing its file descriptor to end
  the OS lease. An inherited descriptor can retain the same open file
  description until child exec. Both overlay write/freeze guards now explicitly
  unlock before releasing their in-process guards. A duplicated-descriptor
  regression reproduces the failure before the fix; all 469 NFS-feature tests
  pass afterward (`ci-overlay-short-tmp.log`). This bug is also present on
  `58447c6eae5` main. The first local broad run hit macOS's Unix-socket path
  limit from the long external TMPDIR; rerunning with a shorter external TMPDIR
  passes without changing the socket test.
- Architecture gates find missing SDK/remote dependency registrations, the
  metadata reader's required parser-task tracking feature, and a fresh-process
  fixture inside production-scanned metadata source. The fixture moves to
  `crates/crab-metadata/tests/plan_receipt_recovery.rs` and preserves its fresh
  process, compaction, historical attribution and no-repair assertions; it
  passes (`ci-receipt-after.log`). The reviewed architecture registration patch
  is held outside the checkout pending explicit inventory approval. Its full
  gate run passes (`ci-architecture-proposed.log`); the existing gate's four
  regression tests also pass. No gate is disabled or baseline auto-regenerated.

These are local diagnosis/proof records under the external task target. New
head CI completion is still required; this does not complete SDK qualification.
Strict reader/metadata library and test lint passes (`ci-reader-metadata-clippy.log`).
An additional strict VFS/NFS library-plus-tests diagnostic reports 417 lint
errors (`ci-fixes-clippy.log`); that broader diagnostic is not a passed gate.

## Mutation readiness limits and catalog kind evidence

Tree rebuilding now belongs to `crab-remote::objects::TreeEdits`. HTTP keeps
create/update/delete policy, expected-blob checks and unchanged-file outcomes.
The shared owner rejects root, duplicate and overlapping byte paths, groups
edits by ancestor, creates missing directories and prunes empty directories after
deletion while retaining an empty root. It accepts mode/OID values rather than
file bodies, so it is not yet the streamed file preparation boundary.
The native HTTP integration passes (`sdk-tree-edit-http.log`), including fetch-based
checks that both the deleted file and its empty parent tree are absent. Strict
remote/HTTP library-and-test Clippy passes (`sdk-tree-edit-clippy.log`). Executable
mode selection is retained by the HTTP adapter; no new dedicated mode test is
claimed by that original run. The extended native HTTP fixture now uploads an
executable through Git, edits it through HTTP on a proposal branch, fetches into
a fresh bare repository and verifies both mode `100755` and the edited bytes
(`sdk-tree-mode-http.log`). That integration passes. The Rust 1.91 object tests
pass (`sdk-tree-batch-tests-final.log`). The execution
fixture applies four edits under two byte-distinct prefixes, reads only the root
and one existing child tree, retains an untouched executable entry, and verifies
the resulting child entries and OIDs. Insertion tests cover duplicate,
overlapping, root and non-UTF-8 byte paths. Multi-file HTTP upload still uses its
older recursive builder. The shared implementation adds about 30 non-test lines
over the former single-path owner; that growth pays for one-pass batching, which
avoids rereading shared ancestors and never needs to fetch newly encoded trees.
Strict remote/HTTP library-and-test Clippy passes after the batch change
(`sdk-tree-batch-clippy.log`).
The native HTTP integration passes again on the final source
(`sdk-tree-batch-http-final.log`).

Two CI jobs on `7d8818e66e7` fail while compiling the same CLI regression:
`StorageError::Throttled` gained an optional provider source on main, but the
new uncertain-commit test constructed it with only `retry_after`. The fixture now
sets `source: None`, preserving its intended local synthetic failure. Logs:
`ci-cache-7d881.log`, `ci-offline-7d881.log`, and
`ci-throttled-source-test.log`. This source fix does not alter retry behavior.

Shared `crab-git::pack_writer` now constructs full-object SHA-1 packs from sized
readers through a 64 KiB buffer, preserving collision-detecting checksums and
typed backend errors. Output limits include the trailer; cancellation is checked
between input chunks. HTTP generated-object publication consumes the writer
inside its existing drained blocking worker, using the existing 2 GiB limit.
This removes the additional compressed-pack allocation; HTTP input objects
remain in memory, and SDK `prepare_commit` is still absent.
All six focused Rust 1.91 tests pass (`sdk-stream-pack-tests.log`), including
native Git index/verification, exact limits, length mismatches, cancellation,
short output writes and original I/O errors. Native HTTP push/file edits and
all merge methods pass (`sdk-stream-pack-http.log`, `sdk-stream-pack-merge.log`).
Strict Git/HTTP library-and-test Clippy passes (`sdk-stream-pack-clippy.log`).
These are correctness checks, not whole-process memory or throughput evidence.

The HTTP content editor and pull-request merge path now consume shared remote
tree reading, tree encoding and object hashing from `crab-remote::objects`.
The former server implementations are removed; HTTP actor normalization and
synthetic-email policy remain in the server. Reads still use the caller's
budgeted operation, and callers still validate and bound tree entries before
encoding. No new dependency or storage/publication path is introduced. This
extracts existing mechanics for SDK commit preparation; it does not implement
the public `prepare_commit` API or streamed content edits.
The HTTP merge-method regression and native-push/file-edit integration pass
(`sdk-object-build-http.log`, `sdk-object-build-content.log`); the frontend
build and strict remote/HTTP library-and-test Clippy also pass. These exercise
the existing HTTP caller, not a new SDK commit operation.

Shared commit encoding now accepts independent author/committer signatures,
ordered parents and exact message bytes. It validates header delimiters and
four-digit timezone representability; timestamps are signed Git seconds.
The existing HTTP actor normalization and trailing-newline policy remain above
the shared encoder. Gitoxide's locked timestamp writer permits two hour digits
and valid minutes, which supplies the offset bound. Two Rust 1.91 tests verify
distinct identity/time round trips, exact parent/message preservation, native
Git hash agreement and rejection of header injection (`sdk-commit-encoding.log`).
HTTP merge and file-edit/publication integration tests also pass
(`sdk-commit-encoding-http.log`, `sdk-commit-encoding-content.log`).
This is encoding infrastructure; SDK commit request validation, bounded streamed
file edits, content artifacts and recovery binding are still required.

`Client::resume_ref_update` now resumes saved ref-only tokens through the same
executor as prepared mutations. It revalidates placement with current credentials,
then checks prior attempts under the operation lease before opening the read
generation or creating preparation files. Recorded attempts reconcile without
replay. Both entry points retain the ref leases, GC fences, journal binding,
readiness handling and draining cleanup of the existing shared owners.

The same-token concurrency regression executes a prepared request concurrently
with the public resume API using one serialized recovery identity. A caller
that encounters the held operation lease receives a pre-commit conflict; any
successful outcomes and later read-only reconciliation identify the same
transaction. No retry loop is added. A missing historical transaction previously
surfaced as NotFound; it now preserves the token as Indeterminate. The regression
seeds an unresolved intent and proves resume neither recreates scratch nor
advances its ref. Other malformed or inaccessible evidence retains typed errors.

All 40 SDK unit tests pass on Rust 1.91 (`sdk-resume-tests.log`), and strict
write/content library and test Clippy passes (`sdk-resume-clippy.log`). The
RustFS smoke passes in 5.00 seconds (`sdk-resume-rustfs.log`): separate processes
initialize, prepare/save/drop, resume execution, recover while indexing, advance
refs, and recover historical commitment. Child PATH and GIT_EXEC_PATH contain
no tools. This is a small S3-compatible fixture, not full provider/performance
qualification. New commit/content preparation and its resumption remain absent.

Mutation execution and reconciliation now pass the caller's selected read limits
to the readiness reopen. Previously those reads silently used defaults. A
regression applies a one-request limit independently to execution and recovery:
both preserve the committed receipt and report pending readiness, then ordinary
reconciliation proves readiness with the same transaction identity. The test
fails before the fix and passes afterward on Rust 1.91; strict SDK write/content
library and test Clippy also passes. This does not establish aggregate mutation
accounting: receipt reads and catalog maintenance still need separate proof.

The earlier claim that SDK catalog repair invokes Git for missing kind sidecars
was incorrect. Both callers of the private collector always disabled kind
population; the Git-spawning branch was unreachable. That branch and its unused
boolean are removed. Publication continues to accept absent kind sidecars and
leaves them absent; the canonical remote reader resolves object metadata. The
catalog integration fixture now exercises both present and absent kind sidecars,
checks their preservation, and checks exact kinds, sizes and byte-identical
commit/tree/blob reads after publication. Both catalog cases and all eight
generation tests pass (`sdk-catalog-optional-kinds-final.log` and
`sdk-catalog-optional-kinds.log`). This closes a fixture coverage gap,
not a previously reachable executable dependency. Full SDK no-Git qualification
still requires the plan's remaining public mutation and workflow surfaces.

## Public direct initialization progress — incomplete SDK

The `write` feature exposes lazy `Client::initialize_remote(locator, head)`
requests. The canonical `crab-write::initialize` owner validates fully qualified
branch HEADs, creates metadata roots conditionally and adopts valid existing
roots without changing refs or HEAD. The SDK applies a finite owner deadline
and tracks cancellation/shutdown. Initialization owns no leases or database
sessions; an interrupted conditional root create can complete remotely and a
later initialization safely adopts it. It never creates a bucket or invokes Git.
Filesystem development stores support these conditional creates, while ref
publication remains unsupported there.

The shared owner's missing-layout HEAD and nonempty LIST could straddle a
concurrent initializer. It now rechecks and validates canonical ownership before
rejecting that prefix. A deterministic store hook reproduces the false corruption
on the preceding implementation (`sdk-initialize-race-before.log`); all five
owner tests pass after the fix. The original delayed-list fixture did not expose
the race because InMemory captures its listing before the delay; it was replaced.

| Evidence boundary | Verified source and behavior |
| --- | --- |
| Entry and public owner | `crab-sdk::Client::initialize_remote` delegates through tracked operation admission; invalid input maps to InvalidInput and incompatible roots to Corruption with sources retained |
| Shared callee and siblings | `crab-write::initialize_repository` calls metadata conditional-create/layout owners; CLI `initialize_remote_repository_store` and HTTP `initialize_repositories` already use that same owner |
| Current main | `58447c6eae5` has the shared initializer and both callers, but no SDK crate; its owner rejects the concurrent-layout interleaving |
| Dependency contract | `Store::create_strict` uses object_store conditional Create; metadata creation adopts only a valid existing descriptor/manifest; no unconditional replacement or bucket creation is introduced |
| Scope and fix choice | Keeping the ownership recheck in the shared initializer fixes all three callers, with an extra HEAD/read only on the previously rejected nonempty-prefix path |

`sdk-initialize-final-tests.log` records 35 SDK and five owner tests passing on
Rust 1.91, including filesystem initialization, laziness, unchanged incompatible
prefixes, adoption after descriptor-only interruption and bounded timeout/close.
`sdk-initialize-rustfs.log` records the five-process smoke passing: the first
process initializes and reads an empty remote with Git absent from PATH, then
the existing four processes verify ref mutation and historical recovery. The Git
pack fixture still seeds commit content through shared owners; public streamed
commit preparation is not implemented. `sdk-initialize-clippy.log` records strict
SDK/write library and test lint passing with write/content features. The lockfile
adds only the existing async-trait dependency for the owner's test store hook.
`sdk-initialize-cli.log` records all 30 CLI initialization regressions passing
through the updated shared owner.
Full aggregate initialization accounting and backend/platform qualification
remain open, alongside the other unfinished SDK phases.

## Public direct ref mutation progress — incomplete

The `write` feature now exposes validated atomic ref batches, owned preparation,
execution outcomes, versioned recovery tokens and read-only reconciliation.
Tokens bind physical placement, repository prefix, ordered edits and policy,
request digest, a UUIDv7 operation nonce and the derived plan ID. Explicit
credential cache namespaces are not used as durable placement. Filesystem
stores retain canonical placement for reconciliation but reject publication
before lease admission because `object_store` 0.14.1 rejects `PutMode::Update`.

`Client::reconcile` closes a plan-level recovery gap: a restarted process can
read the receipt even while `open_remote` returns `Indexing`. Repository-level
reconciliation additionally verifies the selected prefix. Neither repairs
receipts/catalogs nor treats missing proof or current ref equality as a result.
Execution uses the shared plan/ref/GC lease owners, opaque prepared artifacts,
nonce-bound journal attribution and generation maintenance. Public read workers
keep their existing deadline behavior; mutation workers retain terminal evidence
after cancellation and await cleanup.

`sdk-public-ref-write-unit.log` records 29 Rust 1.91 unit tests passing. New tests
cover token tampering and deserialization, deadline outcome retention, exact
remote bytes, atomic rejection after a competing writer, and historical recovery
after refs change, plus rejection of a rewind without explicit force-with-lease.
Delayed receipt lookup previously ignored cancellation, preventing timely close
(`sdk-ref-reconcile-cancel-before.log`). Reconciliation now cancels that read-only
future. Shared plan admission had the same gap while holding an operation lease;
its regression fails before the fix (`sdk-plan-admission-cancel-before.log`) and
all nine publication tests pass afterward (`sdk-plan-admission-cancel-after.log`),
including reacquiring the released lease. Mutation snapshot reads use the same
pre-publication cancellation boundary inside the lease scopes. Mutation workers
and actual writes still drain rather than being dropped.
`sdk-public-ref-write-rustfs.log` records a passing live test:
an independent Git fixture seeds a fresh isolated prefix, then four processes
with Git absent from PATH execute a tag create, recover its committed/pending
outcome while a generation owner blocks catalog maintenance, delete the tag,
and recover the original transaction after compaction. The client persists its
token before execution. This exercises public SDK methods without a source
checkout; initial seeding still uses shared owners, not public initialization.

`sdk-public-ref-read-siblings.log` records four lifecycle, six remote-read and one
filesystem/native-read test passing with `write,content`; three ignored
fixture/live tests are not counted. Strict SDK library/test lint passes for
`write` and `write,content` (`sdk-public-ref-write-clippy.log` and
`sdk-public-ref-write-content-clippy.log`). The normal dependency gate now checks
the write profile and requires its shared owners without CLI/server dependencies;
all four profiles pass (`sdk-public-ref-feature-boundaries.log`). Cargo.lock only
adds SDK edges to existing workspace owners and pinned serde/UUID packages.
Locked Rust 1.91 checks for default/remote and warnings-denied write rustdoc also
pass (`sdk-public-ref-profiles-doc.log`).

The first default-stack test reproduced an overflow in nested catalog readiness
futures. A diagnostic enlarged-stack run measured a 98,304-byte readiness future
and 108,544-byte mutation worker. Boxing the catalog-maintenance future reduces
the worker to 36,352 bytes; default-stack tests pass without an environment
override. Temporary probes were removed. Diagnostic logs are
`sdk-public-ref-stack-size-large.log` and `sdk-public-ref-stack-after.log`.

Streamed commit/content preparation, reconstruction of
executable preparations from tokens, local/managed workflows, full fault/GC
qualification and aggregate mutation accounting remain unfinished. The feature
and capability declaration do not qualify a backend or complete phases 2/3.

## Public read regression after publication extraction

At `8d966255225`, the locked Rust 1.91 `content` profile passes four lifecycle
tests, six remote-read tests and the filesystem-backed native-read fixture
after its source checkout is removed. Evidence:
`sdk-public-read-post-publication.log` under the external task target directory.
Three explicitly ignored fixture/live tests were not executed. The capability
validator still reports 48 cells with phase 0 activated; neither this regression
run nor inventory validity promotes unqualified capabilities.

## Publication extraction boundary audit

Manifest CAS retains both fields of the storage `UpdateVersion` through its
existing read and update. Previously it reconstructed an ETag-only token,
discarding the version required by GCS. A private manifest reader now preserves
the complete token; the public ETag return and persisted snapshot format are
unchanged, with no additional storage read. Ref-journal heads and push locks
already preserve both fields and require no parallel implementation change.

`sdk-manifest-version-before.log` records a failed conditional update against a
test backend requiring the version returned by its read. After the fix, all 23
manifest-store tests pass on Rust 1.91 (`sdk-manifest-version-after.log`). The
backend requirement matches `object_store` 0.14.1's GCS conditional-update
implementation; S3/Azure consume the ETag instead. This synthetic backend proof
does not replace live GCS qualification.

Manifest CAS now retains `ManifestCommitUncertain` for failures after the
conditional update attempt, except an explicit storage precondition conflict.
The diagnostic binds the candidate's serialized digest and preserves the
storage source. It is not a historical receipt or automatic replay permission.
The direct CLI reports this case as non-retryable `indeterminate`, alongside
uncertain journal commits. Pre-CAS validation/history failures stay distinct.

`sdk-manifest-uncertainty-owner.log` records all 22 manifest-store tests passing
on Rust 1.91, including a backend that stores the update then loses its reply;
the test independently reads the committed candidate. The CLI retry regression
covers both marker and manifest errors (`sdk-manifest-uncertainty-cli.log`).
The `object_store` 0.14.1 S3/GCS/Azure conditional-update paths keep their default
non-idempotent request setting, unlike unconditional overwrites. Live-provider
manifest lost-response qualification remains open.

The shared journal outcome boundary now distinguishes proven commitment from
an indeterminate marker attempt, retaining transaction identity and the source
error. HTTP maps indeterminate outcomes to transport failure without per-ref
rejection. Direct CLI journal publication consumes the same classification.
The CLI previously labeled uncertain markers `transient`, allowing its optional
automatic integration retry to replay them; it now emits `indeterminate` with
`retryable: false` and calls for durable evidence reconciliation.

`sdk-journal-outcomes-http.log` records three local receive/fault tests passing;
`sdk-journal-outcomes-live.log` records the real RustFS fault matrix passing.
`sdk-journal-outcomes-cli.log` records three uncertainty/source-boundary tests,
including the command retry selector, passing. Its debug link emits a macOS
unwind-table size warning. The sibling ordinary-transient retry test passes in
`sdk-journal-outcomes-retry-siblings.log`, and strict publication/HTTP library
and test lint passes in `sdk-journal-outcomes-clippy.log`.
This boundary is not the public SDK mutation/recovery-token API.

Source inspected at `2aac0781e763509f402b3968488e7f47846e97c1`.
This is implementation evidence, not a completed phase-2 qualification.

| Surface | Existing owner and caller | Remaining shared boundary |
| --- | --- | --- |
| Expected refs and graph validity | HTTP `receive/publish.rs::publish` calls `receive/validate.rs::prepare`, then `crab-git::receive_plan::validate`; the latter checks expected old values, duplicate destinations, final namespace, incoming objects, connectivity and ancestry | Reuse these mechanisms without moving HTTP policy defaults into the SDK |
| CLI receive policy | `crab/src/git/push.rs` resolves force, expected-ref leases and configured receive policy before selecting proceeding destinations | Preserve per-ref CLI outcomes and atomic-batch selection before shared publication admission |
| Connectivity input | HTTP validates a quarantined incoming pack against committed visibility; CLI `verify_connectivity` walks proceeding tips through its local Git object database and committed frontier | Keep local object acquisition behind the local feature; a filesystem-free SDK write cannot use the CLI executable path |
| Content dependencies | HTTP calls `crab-read::dependency_proof::verify_dependencies` before uploading the prepared pack; CLI revalidates or replans base-bound dependencies when its snapshot changes | Bind proof to the admitted snapshot and immutable payload; an earlier successful proof cannot authorize a changed base |
| Commit and readiness | Both callers reach `crab-write::journal::commit_edits`; mirror supplies `CommitOptions::with_plan`. HTTP then calls `finish_committed`; CLI releases ref leases after the marker before derived index repair | Extract the lifecycle with explicit commitment attribution and readiness, preserving cleanup ordering and caller authorization |

HTTP currently supplies `allow_non_fast_forward: false` and prevents deleting
the default branch. CLI can permit force or an expected-ref lease, subject to
administrator policy. Moving the HTTP validator wholesale would change CLI
semantics. The shared operation must receive validated caller policy, while
Git validation remains in `crab-git` and durable journal mechanics remain in
`crab-write`. The existing shared lease helpers alone do not satisfy this
boundary or the phase-2 requirement to remove duplicated lifecycle code.

The next implementation must also account for the CLI's initial-manifest
publication branch before its journal path. A journal-only extraction cannot
claim all CLI publication uses the shared lifecycle. Tests of the extracted
path must enter both callers, cover initial and existing repositories, and
exercise policy rejection, changed-base dependency validation, marker
uncertainty and post-commit repair failure. No new runtime or test result is
claimed by this audit.

The first follow-through extracts direct plan admission into
`crab-remote::publication::with_plan`: its renewing operation lease encloses
both the durable-attempt check and the execution callback. Native mirror push
now calls that owner instead of assembling the check and lease itself. The
existing managed-authority branch remains outside direct admission. This adds
the metadata `storage` feature to publication; it does not add the local-index
or remote-index feature, a Git executable dependency, or a new lockfile package.

`sdk-plan-owner.log` records seven shared publication tests passing on Rust
1.91.0, including a new test that writes an unresolved intent through the first
callback, refuses a second callback, and reacquires the operation lease after
rejection. This proves admission and cleanup, not committed same-token
concurrency or the remaining complete publication lifecycle.
`sdk-plan-owner-cli.log` records all 26 native-push unit tests passing through
the updated caller, including unresolved-plan refusal before ref admission.

`sdk-plan-concurrency.log` extends shared-owner evidence to eight passing tests
on Rust 1.91.0. `concurrent_same_plan_commits_once` enters `with_plan`, holds the
first callback open while a competing execution attempts admission, and then
commits through `with_leases` and `crab-write::journal::commit_edits` with plan attribution.
The competitor receives lease contention without entering its callback; a
later retry receives `PlanAlreadyAttempted`. Read-only receipt lookup binds
the resulting transaction, the current snapshot contains exactly one journal
transaction and the expected ref, and ref/global/repository GC leases can be
reacquired. Channels establish overlap without timing sleeps.

This test uses conditional in-memory storage, a canonical layout and an empty
manifest with synthetic Git object IDs. It does not prove graph validation,
referenced-content retention, real backend races, process termination, payload
mismatch, or public SDK execution. `crab-write` is a test-only dependency of
`crab-remote`; the production publication feature keeps its existing closure.

## Shared incoming preparation extraction

`crab-remote::prepare` now owns incoming-pack quarantine, committed-visibility
graph access, ref/graph validation, visibility planning, pack preparation and
reader cleanup. HTTP's `receive/validate.rs` supplies its existing graph/pack
bounds and deletion/non-fast-forward policy, and maps typed errors back to the
existing receive protocol categories. No second graph implementation remains
in the HTTP caller. The shared path drains its cancellation observer after
the blocking reader work completes.

This moves preparation mechanics, not HTTP authorization or branch protections.
CLI preparation and the complete shared commit/recovery lifecycle remain open.
The added normal dependencies are existing Git/remote-reader owners and their
hash/object types; the lockfile adds four existing workspace edges without
changing package versions. Default `crab-remote` features remain empty. The
publication feature now includes remote-index support for committed visibility.
The new error/options boundary increases code size while removing the HTTP
implementation; it makes caller policy explicit rather than inheriting HTTP's
force-push prohibition in future SDK writes.

Evidence retained under the task's external target directory:

- `sdk-shared-prepare-check.log`: Rust 1.91 publication compilation.
- `sdk-shared-prepare-minimal.log`: locked Rust 1.91 default-feature compilation.
- `sdk-shared-prepare-native-live.log`: native HTTP push through real RustFS,
  exact objects and atomic rewrite rejection, one passing test.
- `sdk-shared-prepare-faults-live.log`: real RustFS receive fault matrix,
  one passing test using a separate fresh prefix.
- `sdk-shared-prepare-pull-merge.log`: pull-request merge methods through the
  shared preparation caller, one passing test.
- `sdk-shared-prepare-clippy.log`: strict shared-owner and HTTP library/test lint.

The first combined live invocation incorrectly reused one prefix across tests:
fault-case child repositories made the subsequent root initialization nonempty.
`sdk-shared-prepare-http-live.log` retains that failure (four tests passed,
one failed). The initialization guard correctly rejected it. Separate fresh
prefixes resolved the test setup error; no guard or assertion was weakened.

## Shared prepared-artifact upload

`Prepared::upload` now owns the immutable pack, index, reverse-index, kind
metadata and visibility-evidence upload sequence. Preparation retains its exact
validated ref updates privately; callers cannot replace the plan, pack or
visibility map, and upload accepts no second ref-update list. HTTP consumes the
resulting journal edits and pack entries through this owner. Dependency proof
now runs inside `Prepared::upload` before any immutable upload, using the bound
layout, validated pointer set, admitted snapshot and caller-selected limits.
Authorization/lifecycle revalidation, snapshot checks and journal commit
remain in the caller. Upload alone never changes refs and may leave unreferenced
immutable objects if interrupted.

The move replaces the HTTP upload implementation rather than keeping two
paths. It adds typed artifact storage/metadata errors and explicit Tokio file
I/O feature forwarding, with no new dependency or lockfile entry. The complete
shared publication lifecycle and SDK recovery-token binding remain unfinished.

Scoped proof: `sdk-shared-artifact-http.log` (three receive/fault tests passed;
two live tests excluded), `sdk-shared-artifact-live.log` (real RustFS native push
passed with a fresh isolated prefix), `sdk-shared-artifact-pulls.log` (all merge
methods through the shared caller), `sdk-shared-artifact-clippy.log` (strict
owner/HTTP library and test lint), and `sdk-shared-artifact-msrv.log` (locked
Rust 1.91 publication compilation). These logs are under the external task
target directory; they do not prove public SDK write wiring.

The dependency step adds the existing `crab-read` owner to the publication
feature without introducing new package versions. The HTTP push regression
now first sends a valid Git graph containing an LFS pointer with absent content,
requires rejection with no refs or uploaded pack objects, and then successfully
publishes ordinary Git content. `sdk-shared-dependencies-http.log` records three
local receive/fault tests passing. `sdk-shared-dependencies-live.log` records the
same native push fixture passing against a fresh RustFS prefix. Five dependency
owner tests pass on Rust 1.91 (`sdk-shared-dependencies-owner.log`), including
missing Crab/LFS content, corruption, invalid batches, and cancellation.

## Prepared upload placement binding

Preparation now checks that its supplied layout uses the reader's exact
in-process transport and repository/global prefixes before reading graph proof.
The prepared result retains that layout privately; upload no longer accepts a
replacement store or destination. `RemoteGitRepository::matches_store_layout`
compares transport handles and both prefixes, without exposing raw storage or
treating an application namespace string as proof of physical placement.
HTTP supplies its admitted layout when preparing the request.

This is an in-process invariant, not authorization, durable token validation,
or provider identity comparison across independently constructed clients.
Ref/snapshot and authorization revalidation before commitment remain required.
The owner regression covers matching clones and mismatched repository prefix,
global prefix and transport (`sdk-prepared-placement-owner.log`, Rust 1.91).
`sdk-prepared-placement-http.log` records three local receive/fault tests
passing; two live tests were not executed in that run.
`sdk-prepared-placement-pulls.log` records the merge-method regression passing,
and `sdk-prepared-placement-clippy.log` records strict owner/HTTP library and
test lint passing.

Preparation also retains its reader version. Before dependency proof or uploads,
`Prepared::upload` rejects a snapshot with a different manifest ETag or generation,
or pending journal commits. This check assumes a snapshot read from the bound
store; it does not authenticate caller-mutated snapshot fields or replace the
current-state and authorization checks before commitment.

`sdk-prepared-snapshot-owner.log` records the matching/mismatched ETag regression
on Rust 1.91. The fixture creates the canonical layout before reading a snapshot.
`sdk-prepared-snapshot-http.log` records three local receive/fault tests passing
(two live tests excluded), and `sdk-prepared-snapshot-pulls.log` records the
merge-method regression passing. These checks do not qualify SDK writes.

## Shared artifact commitment

The uploaded artifact result is opaque and borrows its validated preparation
and admitted snapshot. Its consuming `commit` method performs the reader
freshness check and calls the shared journal owner using the retained placement,
edits and packs. HTTP can no longer replace those values between upload and
commit. Authorization, archive status, HEAD policy, leases and post-commit
readiness remain above this boundary; complete lifecycle extraction is open.

This promotes the existing `crab-write` dev dependency to an optional publication
dependency without changing package versions or the lockfile. The default
feature remains empty. The additional ownership fields keep the prepared files
and snapshot alive through commitment rather than adding another upload path.

Scoped evidence under the external task target directory:
`sdk-shared-artifact-commit-http.log` records three local receive/fault tests;
`sdk-shared-artifact-commit-live.log` records the real RustFS fault matrix;
`sdk-shared-artifact-commit-pulls.log` records the merge-method regression;
all pass. `sdk-shared-artifact-commit-msrv.log` records the locked Rust 1.91
publication check. No public SDK mutation is exercised by these tests.

## Canonical journal attribution

Opaque uploaded artifacts now accept the journal owner's `CommitOptions`, so
planned publication preserves attribution rather than constructing unplanned
options at the final boundary. HTTP supplies its existing TTL/cancellation;
direct mirror and CLI callers already supply this owner type. Reusing it keeps
one commit path, with no extra storage request, dependency or persistent format.
Planned callers still hold `with_plan` outside their ref leases and GC fences.

`sdk-artifact-plan.log` records a real Git pack passing shared preparation,
dependency checks, upload and attributed commitment. HTTP generation maintenance
compacts its journal; the remote reader verifies the exact file bytes and receipt
lookup retains the transaction identity. Same-plan admission then rejects replay.
The existing local HTTP fault matrix also passes. This proves the owner binding,
not the still-missing public SDK token or fresh-process/scoped-GC qualification
with referenced content.
`sdk-artifact-plan-native.log` records the native HTTP push regression passing;
`sdk-artifact-plan-clippy.log` records strict write/publication/HTTP lint, and
`sdk-artifact-plan-msrv.log` records the locked Rust 1.91 publication check.

Journal attribution now uses one `commit_edits` entry point. Direct mirror
publication selects `CommitOptions::with_plan`; ordinary CLI and HTTP commits
use the same validation and namespace path without attribution. The separate
plan entry point and context wrapper are removed. No release tag contains the
removed API's introducing commit, and the owner crate is unpublished.
Version-one persisted intent and receipt contracts are unchanged.
`sdk-canonical-journal-owner.log` records ten journal and eight publication tests
passing on Rust 1.91, including same-plan serialization and receipt proof.
The stable CLI test target compiles (`sdk-canonical-journal-cli.log`); all 26
native-push module tests pass (`sdk-canonical-journal-native-module.log`).
`sdk-canonical-journal-clippy.log` records strict write/publication/HTTP library
and test lint passing. First-import manifest CAS and complete lifecycle
extraction remain separate work.

## Initial-import publication boundary

The direct CLI first-import path now calls
`crab-write::initialize::publish_initial_manifest` after uploading its segmented
bulk metadata and complete Git visibility proof. The shared owner holds the
namespace lease across the fresh snapshot check and manifest CAS. A changed
manifest ETag or newly visible journal create declines initial publication;
normal caller reconciliation remains responsible for the next action.

The CLI still owns eligibility, authorization, ref/GC admission, proof
preparation and its post-commit bookkeeping. Planned mirrors remain excluded
from this path and use the attributed journal. A lost manifest CAS response
remains an error requiring outcome recovery; this extraction does not claim
public SDK mutation outcomes or complete phase-two delivery.

`sdk-initial-publication-cli.log` records the competing journal-create regression
passing through the CLI caller. `sdk-initial-publication-success.log` records a
successful first push with its ref in the manifest and no pending journal work.
These are local object-store fixtures, not live-provider qualification.
`sdk-initial-publication-msrv.log` records the locked Rust 1.91 owner check.

## Initialization input validation

The shared initializer rejects invalid or non-branch HEAD input before storage
access. It uses the shared Git ref-name validator and preserves that error as
`WriteError::InitialHead`'s source. HTTP already validates its configured short
branch; direct owner callers now receive the same protection. The CLI retains
the typed owner failure in its source chain.

`sdk-initialize-invalid-before.log` records the regression accepting an empty
HEAD before the fix. `sdk-initialize-invalid-after.log` records all four
initialization tests passing on Rust 1.91: invalid input leaves an empty prefix,
canonical roots are created/adopted, and incompatible prefixes/layouts remain
unchanged. This is prerequisite owner proof, not a public SDK initializer.
`sdk-initialize-validation-consumers.log` records successful CLI and HTTP test
target compilation with `gix-transport`; existing fixture warnings remain.

That broader check exposed a missed mirror schema fixture: it still used format
one and omitted the required nonce. The fixture now constructs format two, and
the committed JSON schema requires `operation_nonce` and matches the current
Rust descriptions. The original schema validation failure is retained in
`sdk-mirror-nonce-schema.log`; the corrected validation passes in
`sdk-mirror-nonce-schema-after.log`. All committed schemas match their generated
Rust definitions (`sdk-mirror-nonce-schema-drift.log`). No assertions were relaxed.

## Release profiles

Every profile runs with Git 2.30.9 and the exact current Git version recorded by
the runner. Record exact Rust versions, target triples, source SHA and features
in each report. Floating toolchain labels alone are insufficient evidence.

| OS / architecture | Direct storage | Managed | Native HTTP local |
| --- | --- | --- | --- |
| Linux x86_64 | RustFS, live S3, live GCS, live Azure | Full service qualification | Actual Crab HTTP server |
| macOS arm64 | RustFS | Local workflow smoke | Actual Crab HTTP server |
| Windows x86_64 | RustFS | Local workflow smoke | Actual Crab HTTP server |

Missing infrastructure fails qualification. It does not remove a cell or turn
an ignored test into proof. Other architectures have no initial release claim.

## Fixtures and isolation

Use an explicitly configured private bucket/container and a new prefix
`sdk-qualification/v1/<source-sha>/<run-uuid>/<case>/`. Validate SHA and UUID
components before constructing the prefix. Never adopt an existing prefix.
Record created objects for cleanup; restrict cleanup to that exact owned case
prefix. Never run bucket-wide GC. Keep external Git repositories read-only.
Generated checkouts, spool files and build artifacts live on the external
Workspace volume in directories owned by the current run.

`sdk-qualification-fixtures.json` defines byte sizes, deterministic generation
and SHA-256 digests. Generate one block at a time; do not allocate the whole
large file. Fixture kinds:

- Empty repository: unborn symbolic HEAD `refs/heads/main`, no commits or refs.
- Namespace conflicts: independently test `refs/heads/topic` against
  `refs/heads/topic/child`, and the equivalent tag pair, including concurrent
  atomic batches. A losing batch must change no refs.
- Ordinary tree: 10,000 files `ordinary/00000.txt` through
  `ordinary/09999.txt`, each with the `ordinary` fixture bytes. This deliberately
  isolates path/tree scaling from content diversity; it is not an upload
  throughput benchmark.
- Crab content: `large/data.bin`, exactly 1 GiB of `crab` fixture bytes,
  ingested through canonical Crab staging. Verify raw pointer and independently
  reconstructed content separately.
- LFS content: `lfs/data.bin`, exactly 32 MiB of `lfs` fixture bytes, with a
  canonical LFS pointer whose SHA-256 and size match the fixture record.
- Unix paths: raw path bytes `b"names/invalid-\\xff.bin"`; verify bytes through
  tree pagination, blob reads and archives. Windows does not claim this case.

Use explicit author and committer `SDK Fixture <sdk-fixture@example.invalid>`,
timestamp 1700000000, timezone +0000, and message `SDK qualification fixture`.
Record generated Git OIDs and pointer bytes with each report. Their values
depend on the completed fixture tree and canonical staging output; do not
invent expected OIDs before building it.

Remove the source checkout before the remote-read case. Verify writes through
an independent clone, `git fsck`, and independent SHA-256 comparisons. A test
entering an owner crate directly does not establish public SDK wiring.

## Current toolchain investigation

Baseline inspected: `4b8b36b1870963ad6bb5e4dadc491cafb9352aad`.
`.github/workflows/rust.yml` selects `dtolnay/rust-toolchain@stable`, not an
exact release. The latest completed baseline run,
[34083635231](https://github.com/crabbuild/crab/actions/runs/34083635231),
resolved Rust 1.98.1 (`48a229cea`, 2026-09-01), target
`x86_64-unknown-linux-gnu`. That run qualified baseline
`ebd0e40d14ca862cefa5366c1f856847e2401660`; the current documentation-only
commit's run was pending when inspected.

Local compiler observed: Rust 1.97.0, commit
`2d8144b7880597b6e6d3dfd63a9a9efae3f533d3`, host
`aarch64-apple-darwin`, LLVM 22.1.6. Local Git: 2.50.1 (Apple Git-155).
These observations are not an MSRV claim.

The user authorized `/Volumes/Workspace` because the usual home-directory
Workspace path is absent. This worktree uses the dedicated
`crabbuild-target/crab-1abf-sdk` directory on that mounted volume. The default
Cargo registry source/cache symlinks point to a deleted external checkout;
dependency inspection therefore uses an isolated Cargo home under this target.

The disposable consumer imports `crab-remote-git`, `crab-read`, `crab-write`,
`crab-staging`, `crab-auth-store` with `managed-service`, and `crab-git` with
`facade`, all with defaults disabled. This covers the planned current owner
closure without CLI/server composition or optional active-active coordinators.
Repeat the closure check when extraction chooses additional owner features.

Its 572 resolved packages match workspace-locked names, versions, sources and
checksums. Workspace lock SHA-256:
`14c6683d4b064736045f74069c02c5da568a8e659eaf38715b95ec661ee95932`.
The highest declared requirement is Rust 1.89.0: `konst` 0.4.3,
`konst_proc_macros` 0.4.1, and `redb` 3.1.3. A workspace-wide unified graph
incorrectly includes unrelated AWS features and suggests 1.91.1; use the
isolated consumer graph for the SDK decision.

The 1.89.0 external-consumer compile probe failed with E0658 in
`xet-core-structures` 1.6.0, `src/merklehash/error.rs:59`: its undeclared
`str::floor_char_boundary` dependency requires Rust 1.91.0 according to the
[standard-library stability contract](https://doc.rust-lang.org/std/primitive.str.html#method.floor_char_boundary).
Rust 1.90 cannot satisfy that API either. Probe 1.91.0 next without modifying
dependency sources, versions or the workspace lockfile.

An isolated `rustc 1.90.0` probe of that exact method also failed with E0658.
The full consumer probe passed on Rust 1.91.0
(`f8297e351`, 2025-10-28), exit status 0. Select **1.91.0** as the SDK MSRV.

The probe uses `--locked --offline`, four jobs and a distinct
`crab-1abf-sdk-msrv` target directory. Metadata, dependency comparisons and
compiler output are retained under this worktree's external build directory.
The recorded terminal output is:

```text
Checking crab-sdk-msrv-probe v0.0.0
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 28s
```

Phase-0 validation: 48 capability cells and all mandatory phase-1–6 test
references accounted for; five validator regression tests passed; Markdown
generation and whitespace checks passed. Fixtures, profile definitions, exact
baseline CI compiler and passing external-consumer MSRV probe are recorded.
No production owner changed and no duplicate production path was introduced.
Phase 1 can now begin; later phases still require their own acceptance proof.

## Phase 1 progress — incomplete

`crates/crab-sdk` now contains validated byte paths, SHA-1 identities,
unambiguous revision selectors and source-preserving errors. Its three
value-contract tests pass on Rust 1.91.0. It remains unpublished; no backend
cell is promoted.

The `remote` feature now contains an internal operation tracker. Its two tests
pass on Rust 1.91.0: dropping a request signals cancellation while close waits
for worker cleanup, closed admission rejects new work, and typed error results
survive the worker channel. The three value tests also pass with that feature.
The tracker is now wired into an incomplete public filesystem client:
`open_remote`, pinned refs, snapshot creation, raw blob reads, refresh and client close.
Commit metadata, paginated trees and history now use the same tracked owner
operations. SDK cursors bind repository identity, generation and commit, with
owner traversal checks retained. The native-Git fixture verifies tree/history
continuation and rejects a cursor after a generation change even when the
commit is unchanged. The Unix non-UTF-8 path extension also passes: Git's index
creates the raw-byte tree entry without requiring the host filesystem to
represent that name, and SDK tree/blob reads preserve it after source removal.
Cursor serialization is not implemented.
Owner operations finish before their worker returns; refresh creates a separate
repository handle. All eight SDK tests pass on Rust 1.91.0, including a real
filesystem-backed empty repository open and closed admission through retained
handles. A native-Git pack fixture now proves raw bytes after deleting the
source checkout and preserves old snapshot bytes after a ref/catalog update.
That test exposed an owner gap: new operations reopened the latest catalog.
Repository handles now capture the immutable catalog identity and operations
reopen that exact checkpoint. The existing catalog-free publication path is
unchanged. Expanded ref/reachability assertions and six owner operation
regressions pass. All 17 owner repository-open tests pass after replacing the
old rejection assertion with the required immutable-catalog success contract.
Cloud provider
configuration, remaining read methods, full operation options/progress,
operation identity, dropped-worker error reporting and SDK stream lifecycle
remain incomplete. Builders currently require `remote`; default-feature
builder availability still needs implementation. This is not SDK lifecycle
qualification.

The locked normal-dependency tree has 35 distinct package names for default
features and 337 for `remote` on this host. Default features include no other
Crab crate. Neither tree includes `crab`, `crab-read`, `crab-lfs`, `crab-vfs`,
`crab-http-server`, `crab-cache` or `crab-cache-store`. These are dependency-tree
observations, not final packaging or cross-platform feature-closure proof.

`crab-remote-git` now supports per-operation aggregate limits on the same
pinned repository/snapshot. The operation owns its limits for deadlines,
budgets, accessors and batch admission. Default callers and caller-pinned
publication snapshots still use their configured limits through the same
constructor. Six focused operation tests pass, including warm-cache limits,
deadline expiry, cancellation and shutdown cleanup. Formatting and whitespace
checks pass. HTTP/CLI consumer proof and SDK-level lifecycle tests remain due.

The LFS owner now has a focused-test-verified read-path change: full streams hash
delivered bytes in one body request, partial streams bind verification to a
strong object version, and reads do not publish verification receipts. Final
bytes are withheld until framing and full-file integrity are proven, so an
HTTP Content-Length consumer cannot finish before observing corruption.
Seven focused fault-injection tests pass on Rust 1.91.0, including one-body
full reads, zero receipt writes, version-bound ranges, invalid bounds,
corruption and incomplete framing. All 29 existing object-store regressions
also pass, including selected-replica fallback and multipart integrity checks.
All five focused HTTP LFS handler tests pass, including read-only downloads,
corrupt-body failure, admission cleanup and existing upload/batch behavior.
The native Git LFS push/clone smoke also passes with Git LFS 3.7.1 against the
loopback HTTP server: a 10 MiB file uploads, clones and matches byte-for-byte.
Generated repositories are isolated on the workspace volume, with the test
helper disabling global/system Git configuration. This qualifies that shared
HTTP/LFS client path, not SDK content support or cloud-provider behavior.

The Crab hydration owner has an unqualified exact-range writer API with
caller-owned cancellation and lookup lifetime. It shares full reconstruction's
output-size and destination-lifetime checks, while retaining chunk-only
verification for ranges. The VFS-facing buffer API preserves clamping and now
delegates to that path. The first hydration run passed 34 tests and failed the
new pending-source cancellation test: Xet's writer join waited on a source
future that did not observe cancellation. The read adapter now receives the
operation token and cancels download admission, availability and chunk reads.
All 35 hydration tests pass after that fix. The expanded pending-source test
also passes for cancellation and drop through both range and full-file callers.
The SDK's bounded stream bridge remains unimplemented.

To reproduce, create an external standalone Cargo workspace with this manifest,
replacing each `REPO` with the checkout location. Copy the workspace lockfile
before resolving; compare every resolved dependency's name, version, source
and checksum with that lockfile before compiling. Only the consumer root may
be new. Run Cargo metadata without `--locked` once to remove unused workspace
members, then use `--locked` for the probe. Do not update the source lockfile.

```toml
[workspace]
[package]
name = "crab-sdk-msrv-probe"
version = "0.0.0"
edition = "2024"
[dependencies]
crab-remote-git = { path = "REPO/crates/crab-remote-git", default-features = false }
crab-read = { path = "REPO/crates/crab-read", default-features = false }
crab-write = { path = "REPO/crates/crab-write", default-features = false }
crab-staging = { path = "REPO/crates/crab-staging", default-features = false }
crab-auth-store = { path = "REPO/crates/crab-auth-store", default-features = false, features = ["managed-service"] }
crab-git = { path = "REPO/crates/crab-git", default-features = false, features = ["facade"] }
```

The consumer binary imports each owner with `use crab_read as _;` (substituting
each crate's Rust name), then defines `fn main() {}`. Dependency compilation is
the probe; this binary makes no runtime behavior claim.

### SDK diff and blame wiring

The source-deleted filesystem fixture now exercises public SDK diff and blame
against two real Git commits. It checks exact text replacement bytes and line
coordinates, binary classification, complete first-parent line attribution,
binary blame rejection, and rejection of a diff base outside the old snapshot's
pinned roots. Both operations finish their shared owner operation before
returning SDK-owned values. This establishes local wiring only; cloud,
resource-limit, cancellation and performance qualification remain outstanding.

`cargo +1.91.0 test -p crab-sdk --features remote --locked --offline -j 4`
passes all eight current tests on the external volume. Evidence:
`sdk-diff-blame.log` in the worktree's external target directory. No capability
cell is promoted by this focused proof.

### Incremental Git archives

The SDK's Git-mode archive now uses the shared owner's explicitly closable
`ArchiveReader`. The existing owner stream delegates to that same reader;
HTTP ZIP consumers retain their current stream interface and traversal semantics.
The SDK worker retains one queued entry plus one producer entry, bounded by the
owner's object/aggregate limits, and observes the owner cancellation/deadline
token while backpressured. It is registered with the same admission/tracker
path as ordinary request workers. Tokio's locked channel contracts establish
that cancelling a borrowed receive does not consume a message; terminal results
remain on a separate oneshot until observed.

Focused owner archive tests pass (3 tests): exact traversal semantics, cancelled
stream finalization, and explicit close before/after the first entry without
additional pack reads. The SDK suite passes (9 tests), including source-deleted
archive bytes with Unix raw paths, EOF, early close, drop, client shutdown with
an unread stream, and a deadline expiring while the consumer is idle. Logs are
`archive-reader.log` and `sdk-archive.log` in the external target directory.

Hydrated archive mode rejects explicitly; content support is unfinished.
Injected close-failure propagation through the public stream, cleanup-error
reporting after dropped handles, large-archive memory measurements, RustFS/cloud
execution and cross-platform qualification remain outstanding. These checks do
not activate a capability cell.

The HTTP consumer rebuilt successfully and its four focused archive tests pass
(`http-archive.log`), including encoded output limits, drop cancellation and ZIP
cleanup. SDK errors now expose secondary cleanup failures separately while
preserving both typed diagnostic sources; the owner compound-failure mapping
has a regression test. This does not replace injected session-close failure
coverage through the complete public stream.

### Ordinary file stream delivery

`Snapshot::open_file` now streams verified ordinary Git bytes through the same
tracked receiver and terminal-result path as Git archives. Delivery uses 64 KiB
chunks and a one-entry channel; the owner still decodes/cache-bounds whole Git
objects. This is not proof of bounded 1 GiB pointer reconstruction.

The native Git fixture now contains a 192 KiB binary object and valid Crab/LFS
pointers. With the source checkout removed, public SDK tests verify exact
ordinary bytes, delivery chunk bounds, early close before/after consumption,
deadline expiry with an idle receiver, client shutdown with unread archive and
content streams, exact raw pointer reads, and explicit hydration rejection.
The nine-test SDK remote suite passes (`sdk-content.log`). The rejection is an
unfinished capability boundary, not completion of the planned content feature.

Remaining content work includes the cancellable blocking-writer bridge, explicit
cache placement and budget integration, Crab/LFS reconstruction, exact ranges,
hydrated archives, aggregate-budget qualification, and the planned large-file
end-to-end performance evidence. No delivery phase or capability is promoted.

### Exact ordinary-file ranges and caller failures

`ReadOptions::with_range` now selects exact half-open bounds for `read_blob` and
ordinary `open_file`. Reversed bounds fail at construction; resolved file bounds
reject overflow/past-EOF requests while allowing empty ranges through EOF.
Commit, tree, history, diff, blame and archive reject range-bearing options.
The source-deleted fixture compares varied binary bytes for whole, edge, empty,
interior and chunk-boundary ranges. Both raw and streaming entry points reject
invalid ranges, including `u64::MAX`. All ten SDK tests pass (`sdk-ranges.log`).

Caller validation now enters the shared finalizer through `Error::Consumer`.
This replaces the previous successful-owner-result marker used for unsupported
content and preserves SDK error categories and typed sources alongside close
failures. A real owner-session test proves that the consumer error records a
failed operation and retains its source (`consumer-finalization.log`). The SDK
compound-error mapping has a corresponding regression test.

Ordinary range reads still decode and charge the complete bounded Git object;
this evidence does not establish reduced origin bytes or pointer-range
reconstruction. Crab/LFS content ranges and performance qualification remain
unfinished. No capability cell is activated.

The HTTP consumer rebuilt against the added owner error variant and its four
focused archive tests pass (`http-consumer-errors.log`). Formatting, whitespace
and the five inventory-verifier tests also pass for this increment.

### Initial LFS content feature

The incomplete `content` feature now connects `Snapshot::open_file` to
`crab-lfs::LfsObjectStore::get_stream`. Standard LFS pointers produce logical
bytes, while metadata-only builds still reject hydration. The same owner
operation holds the Git session through content EOF/error, charges declared
logical output to its response budget before opening LFS storage, observes its
cancellation/deadline during opening, delivery and backpressure, and preserves
typed LFS failures through finalization. LFS extension processing is explicitly
unsupported for remote reads; its broader SDK contract remains to be resolved.

The source-deleted fixture publishes a valid 192 KiB LFS object without a
verification receipt. Public tests check exact full/range/empty-range bytes,
invalid bounds, early close, idle deadline, client shutdown with unread LFS,
same-size corruption, corrupt partial reads, truncated storage, missing objects,
and absence of newly created verification receipts. All ten SDK tests pass with
`remote,content` (`sdk-lfs.log`). This is focused filesystem wiring evidence,
not the planned 32 MiB RustFS/live-cloud qualification.

Normal dependency trees are recorded in `sdk-remote-tree.txt` and
`sdk-content-tree.txt`. The metadata-only tree excludes `crab-lfs`; content adds
it. Both exclude `crab`, `crab-http-server`, `crab-vfs` and `crab-read`. The last
exclusion reflects unfinished Crab hydration, not completion of that feature.
The lockfile adds SDK edges to existing packages; no upstream version or
checksum changes were introduced.

Exact LFS transport request/fetched-byte accounting, broad lifecycle
qualification, fault injection during verification/delivery replacement,
large-file memory/throughput measurements, Crab reconstruction and hydrated
archives remain outstanding. In particular, logical response-byte admission is
not proof of the full aggregate transport budget contract. Phase 1 and all
capability cells remain unqualified.

The metadata-only remote suite also passes all ten tests
(`sdk-metadata-after-lfs.log`), and default-feature tests pass (three value
tests). Formatting, whitespace, inventory validation and its five tests pass.

### LFS verification-job drain

LFS stream reads now accept an explicit `LfsReadSession`. The SDK holds this
session around opening and delivery, drops their futures/streams on every
terminal path, and then awaits the session before finishing its Git operation.
Both full delivery hashing and receipt-miss partial-read preverification use
the session's blocking-task tracker. Admission closure and job registration
share one lock; no new hash job can race past session close.

The locked Tokio-util `TaskTracker::spawn_blocking` implementation acquires its
tracking token before spawning and drops it after the job returns. A gated
worker test proves session close remains pending after its join handle is
dropped, rejects further work, and finishes only after the worker is released
(`lfs-read-session.log`). An owner streaming test verifies that both full and
partial verification paths route jobs through the session. All 40 focused LFS
object-store tests pass (`lfs-session-regressions.log`), including writes,
receipts, multipart transfers, corruption and version-bound ranges.

The existing non-session LFS entry point retains its global hashing semaphore
and ordinary task-spawn behavior. Write/maintenance callers continue through
that path; receipt policy and transport semantics are unchanged. This closes
the SDK's identified detached-hashing lifetime gap without claiming broad
performance or cross-platform lifecycle qualification.

The ten-test SDK content suite passes with the drain wired
(`sdk-lfs-session.log`). All six HTTP LFS tests also pass
(`http-lfs-session.log`), including a real native Git LFS push and fresh clone
of a 10 MiB object through the loopback server. Formatting, whitespace,
inventory validation and its five tests pass. This is focused lifecycle and
sibling-consumer proof; full SDK release qualification remains open.

### Initial Crab reconstruction wiring

The `content` feature now connects Crab pointers to `crab-read::ShardHydrator`.
`ContentCache` requires an existing absolute directory and finite nonzero
retention budget; the SDK projects these into the existing cache owner rather
than calling its ambient cache-root resolver. Direct storage and repository
identity namespace the cache. A repository handle lazily initializes and reuses
one hydrator; blocking initialization is awaited by its tracked SDK worker.

A blocking writer bridges Xet output to the one-entry SDK byte channel, copying
at most 64 KiB per write. The locked Xet `SequentialWriter` runs this writer via
its runtime's `spawn_blocking`. Cancellation breaks channel backpressure while
the receiver remains alive, and reconstruction is awaited through writer and
cache completion. Whole-file requests use full BLAKE3 verification; partial
requests call the owner's exact verified-range writer API.

The real fixture publishes canonical xorb/shard records and a pointer with a
shard hint, then removes its source checkout. Public tests verify exact whole,
partial, chunk-boundary and empty-range bytes, early close, idle deadline,
shutdown with unread Crab/LFS/archive streams, and corrupt-xorb rejection with
a fresh cache. A gated writer test checks cancellation with a full channel.
All eleven content-feature SDK tests pass (`sdk-crab.log`).

This fixture found an error-boundary bug: `std::io::Error::source` skips its
boxed payload, hiding the bridge's typed cancellation marker. The SDK now
inspects that payload when classifying reconstruction errors, while retaining
ordinary writer errors. Focused post-fix evidence is in `sdk-crab-cancel.log` and the final content
test log.

The updated normal dependency trees keep reconstruction/cache crates out of
metadata-only `remote`; `content` now includes them. Neither tree includes the
CLI, HTTP server or VFS. The SDK's new dependency edges resolve existing locked
packages without upstream version/checksum changes.

Still unqualified: 1 GiB memory/throughput behavior, aggregate origin budgets,
concurrency/cache lifecycle at scale, pointers without shard hints through a
published file index, cloud/RustFS execution, and hydrated archives. Later SDK
phases remain untouched. No capability cell is activated by this increment.

Metadata-only remote tests pass all ten tests (`sdk-metadata-after-crab.log`),
and default-feature tests pass all three value tests. Formatting, whitespace,
inventory validation and its five tests pass for this increment.

### Configurable aggregate read limits

Added SDK-owned `ReadLimits` and validated `ReadOptions::with_limits`.
All fourteen aggregate dimensions map to the canonical Git operation budget;
per-object safety bounds remain independent. Numeric defaults are documented
in public field rustdoc and checked against explicit expected values and the
owner defaults. Zero fields, zero duration, and overflowing duration are rejected
before operation admission. Existing timeout/range selection remains independent.
The source-deleted filesystem fixture now proves that warmed raw/blob file reads
reject a one-byte response budget and that a later default-budget read succeeds.

Proof: Rust 1.91.0, locked `cargo test -p crab-sdk --features content`:
14 tests passed, including the native Git/Crab/LFS fixture (3.26 seconds).
External log: `sdk-read-limits.log` in the worktree's designated target directory.
The metadata-only `--features remote` run also passed all 13 tests
(`sdk-read-limits-remote.log`). Formatting, diff whitespace, the 48-cell
inventory validator and its five regression tests passed.

Transport admission remains unresolved. Source audit found that
`crab-remote-git/src/reader.rs::charge_origin_range` and pack downloads charge
before `Store` methods; `Store` retries below those charges. Cache-aware
ObjectStore paths sometimes call `origin().inner()` directly. Locked
`object_store` 0.14.1 additionally retries inside its HTTP client
(`client/retry.rs`, `RetryConfig` and `send_retry`). A facade observer or a single
ObjectStore call counter cannot prove a bound on physical HTTP attempts.
The eventual transport boundary must count retries, preserve typed rejection,
prevent body overrun, and share the operation budget across Git and hydration
without charging one request twice. No transport coverage is claimed here.

### Hydration origin admission

Added storage-owned `ReadAdmission` for GET/HEAD attempts. The wrapper sits
under `Store` retries and on configured read routes, including raw `inner()`
access. It reserves the advertised body length before payload polling, checks
exact response framing, and preserves coalesced range reads. Reservations are
conservative and are not refunded after failure or cancellation. Typed
`ReadRejected` errors remain fatal through storage, cache and I/O wrappers;
auth-message heuristics cannot relabel them. Writes and listings are unchanged.

SDK Crab/LFS reads now share `OperationContext` request/fetched-byte counters
with Git pointer resolution. Hydrator clones retain shared cache, download and
buffer controls. Shard-hint and bloom-prefilter admission failures stop rather
than falling through to another lookup. SDK cancellation classification retains
the owner's deadline/finalization precedence. CLI direct and nested read-error
conversions retain the typed source and fatal retry classification.

Proof on Rust 1.91.0 with the locked dependency graph:

- SDK `--features content`: 15 tests passed, including source-deleted native
  Git/Crab/LFS fixture (3.97 seconds). Tiny fetched-byte/request budgets reject
  hydration after the identical budget admits the cached Git pointer. A later
  ordinary-budget reconstruction succeeds. Log: `sdk-admission.log`.
- Storage admission: seven tests passed, covering retry cutoff before provider
  access, raw/routed shared accounting, zero payload polls on byte rejection,
  short/oversized body rejection, nested failure sources, auth-classification
  isolation and charging a coalesced transport span once. Log:
  `storage-admission.log`.
- Existing storage error-map and retry slices: 15 tests each passed. Logs:
  `storage-admission-errors.log`, `storage-admission-retries.log`.
- SDK metadata-only `--features remote`: 14 tests passed
  (`sdk-admission-remote.log`); existing hydrator slice: 35 passed, including
  cancellation, writer cleanup, cache completion and concurrent failure
  isolation (`read-admission-siblings.log`). Formatting, whitespace and the
  capability inventory checks also passed.

Affected CLI read-error tests passed (two tests, including real reconstruction)
on installed stable Rust 1.97.0. Log: `crab-admission-errors-stable.log`.
The initial 1.91.0 CLI command was rejected because locked AWS dependencies
require 1.91.1; this does not alter the proven SDK-only MSRV. No dependency
versions/checksums changed; `async-trait` moved from dev to normal dependencies
in the Git owner for its admission implementation.

Remaining transport work: Git's existing charges still sit above its facade
retries; listings, remote cache-service traffic and provider-internal HTTP
retries are not covered by this hook. Full transport qualification is not
claimed. The new production surface centralizes admission/framing at the
storage boundary and shares existing owners; it does not add a second
reconstruction or retry implementation.

The audit also exposed an unqualified no-shard-hint path:
`StoreClient::resolve_file_index` opens the current `FileIndexLookupSession`
and logs rather than returns close failures. An unscoped reader may write
SlateDB checkpoints. SDK hydration must inject the existing bounded,
snapshot-bound, write-free `FileIndexLookupSession::for_snapshot` path and
prove no-hint reads against the pinned repository inventory, including
failures and zero write attempts. Current fixture proof covers shard hints;
it does not establish that missing-hint behavior is safe or complete.

### Pinned lookup for missing and stale Crab hints

Repository handles now retain the compacted manifest's immutable shard-index
root. The SDK captures that root and generation with each handle; refresh
creates a separate capture. Metadata-only opens do not fetch shard metadata.
Crab reconstruction always receives a caller-owned lazy lookup, including the
fallback from stale hints, and explicitly closes it before Git finalization.

`SharedFileIndexLookup::for_shard_index` selects the existing canonical shard
search without opening SlateDB or reading a latest manifest. Initialization is
lazy so valid hints avoid an index fetch. It enforces shard cardinality before
reading index segments. The normal segment-reading implementation is shared;
only the caller-specific limit error is checked above it. Hydrator composition
retains its existing metadata cache, transport admission and shared resource
controls. This replaces the SDK's use of the current-state file-index opener;
other owners that deliberately request current-state acceleration keep their
existing contract.

Missing reconstruction recipes now preserve `ReadError::NotFound` through the
Xet adapter. An indexed shard lacking its file is a typed corruption failure.
Locked Xet 1.6.0 `retrieve_file_term_block` propagates adapter errors with `?`
and treats `Ok(None)` as a past-EOF request; missing recipes must remain errors
to avoid silent empty output. The owner regression now checks the typed cause
instead of a diagnostic substring. Invalid pinned hash parsing also retains
its source as invalid-data I/O, classified as corruption by the SDK.

Proof on Rust 1.91.0, locked dependencies:

- `sdk-pinned-content.log`: 15 SDK tests passed. The source-deleted native
  fixture (3.42 seconds) includes valid, absent and stale hints. Whole and
  partial reads from the old handle succeed after a newer manifest clears its
  shard inventory; the refreshed handle reports typed not-found for missing
  recipes. Git archive assertions include the exact new pointer fixtures.
- `metadata-pinned-content.log`: six snapshot-lookup tests passed. A real
  pre-existing file-index database plus a write-rejecting/counting store proves
  zero write attempts on successful lookup, miss, close, cardinality failure,
  and invalid-root failure. Closed clones reject subsequent nonempty lookups.
- `read-pinned-content.log`: 19 read-adapter tests passed, including the typed
  missing-file regression and existing hint/recipe behavior.
- `sdk-pinned-content-remote.log`: 14 metadata-only SDK tests passed. Its normal
  dependency tree still excludes hydration, cache, LFS, HTTP and VFS crates.
- The existing manifest round-trip and malformed pack-entry tests each passed
  (`metadata-pinned-content-roundtrip.log`, `metadata-pinned-content-pack.log`).
  The attempted `segmented_store` filter selected zero tests and is not counted
  as proof; those two callers exercise the shared segment reader instead.

The metadata-only test feature combination emits an existing dead-code warning
for `GitCatalogVisibilityRead` in the untouched Git-visibility test module.
Formatting, diff whitespace and capability-inventory validation passed.
Production LOC growth adds the pinned source policy and lazy initialization;
lookup, cache, reconstruction and segment parsing remain in their existing
owners. No dependency versions or lockfile contents changed in this increment.

Still unqualified: cancellation while an origin read or metadata parser is
pending, complete physical transport/retry accounting, 1 GiB resource and
performance gates, cloud backends and the remaining SDK phases. This increment
does not establish the complete phase-one lifecycle or release gates.


## Pending origin cancellation and metadata parser drain

The content admission adapter now observes its operation token while waiting
for request admission, origin response headers, byte admission and each body
item. Cancellation remains a typed fatal `ReadRejected` carrying
`StorageError::Cancelled`, so cache wrappers cannot retry it. The remote owner
supplies the same token used by caller cancellation and its deadline; SDK source
traversal preserves cancellation through the storage wrapper.

The storage regression first failed with the old adapter while blocked on
headers (`storage-pending-before.log`). Both pending headers and a pending body
now terminate, drop the blocked read and consume only one request admission
(`storage-pending-after.log`, eight admission tests). A published remote fixture
also exercises pending pack reads with caller cancellation and an operation
deadline (`owner-pending-cancellation.log`). This verifies the operation-to-store
contract, not a cloud-provider cancellation qualification.

Metadata sessions now track their canonical blocking shard parsers. Explicit
close waits for detached parsers before closing the reader or returning; the
SDK already awaits this close before operation finalization. The parser
regression occupies the only blocking worker, times out the parser's join,
verifies close remains pending, then releases the worker and observes completion
(`metadata-parser-drain.log`). The locked Tokio utility implementation acquires
its task token before scheduling and releases it after the blocking closure
returns. The added optional metadata dependency uses the existing workspace
version; no dependency versions changed.

Complete physical transport/retry accounting, cloud-provider behavior, large
fixture resource/performance gates and remaining SDK phases are still open.

Surrounding validation passed: all 24 file-index lookup tests
(`metadata-parser-drain-all.log`), all 15 SDK content-feature tests including
the source-deleted native repository fixture (`sdk-parser-drain.log`), and both
cancellation/deadline cases in the owner fixture. Formatting, whitespace, all
48 inventory cells and five inventory-verifier tests passed. The file-index-only
configuration retains the previously recorded unrelated dead-code warning.
The small production increase owns cancellation at the existing transport
boundary and parser lifetime at the existing lookup session; neither introduces
an alternative reconstruction or lookup implementation.


## Unobserved SDK worker failures at client close

`Client::close` now returns `Result<()>`. After worker and Git-runtime drain it
reports the first unobserved worker failure since the previous completed close.
The retained diagnostic is bounded to one error; this is not an exhaustive error
history. Ordinary drop cancellation is ignored only when it has no secondary
cleanup error. Errors already received by an operation/stream future are not
reported again. The diagnostic is consumed after all asynchronous cleanup so
cancelling close does not lose it at a later await.

The previous worker discarded a failed oneshot send. It also lost an error when
send succeeded immediately before an unread receiver was dropped. Workers now
retain failed deliveries, while receiver drop closes admission to the oneshot
then takes any completed result. Locked Tokio documentation and implementation
establish that close prevents later successful sends and `try_recv` observes a
preceding send/close without spurious failure, including the already-consumed
case. SDK callers were updated to observe close's result; no released SDK API
exists to preserve.

`sdk-unobserved-close.log` records 18 passing SDK
content-feature tests, including the native source-deleted repository fixture.
New runtime tests cover both delivery orderings with a typed permission-denied
cleanup source, expected cancellation, bounded first-error retention and no
repeat after observation. The existing drain test still holds cleanup pending
until its explicit gate is released. This is lifecycle-unit plus existing native
read regression evidence, not injected cloud cleanup-failure qualification.

The metadata-only remote configuration also passed all 17 tests
(`sdk-unobserved-close-remote.log`). Formatting, whitespace, capability inventory
and its five verifier tests passed. The SDK directory remains untracked in this
uncommitted implementation, so Git numstat does not include it; the runtime is
305 lines including tests. The production increase is the bounded diagnostic
slot and the two delivery paths that feed it. Public operation cancellation,
progress, identity and the remaining phase-one/full-plan gates remain open.


## Public cancellation and absolute deadlines

SDK-owned `Cancellation` and `OperationOptions` now expose shared caller
cancellation, an absolute monotonic deadline and validated aggregate limits.
Repository open, refs, refresh and snapshot take operation options directly;
file/metadata/archive reads use `ReadOptions::with_operation`. `ReadOptions` is
now Clone rather than Copy because it owns the shared control handle. Existing
callers explicitly clone reused options. Ref results check their returned names,
SHA-1 IDs and entry count against response/entry limits before copying entries.

All entry points use the same tracked worker admission. A pre-cancelled or
already-expired call does not invoke its worker. In-flight interruption signals
the worker token and awaits its result, preserving cleanup rather than dropping
the worker future. A deadline maps cancelled results to Timeout without removing
secondary cleanup diagnostics; other owner errors remain their original kind.
Successful worker cancellation outcomes retain their owner-defined meaning,
including explicit stream close. Dropping one operation does not cancel the
shared caller handle. Absolute deadlines cover stream delivery/backpressure;
validated owner-duration defaults remain independently active.

Validation: `sdk-operation-controls.log` records 19 passing content-feature
tests; `sdk-operation-controls-remote.log` records 18 metadata-only tests. Native
fixture checks cover pre-cancelled/expired commit, blob, content and archive
calls, cancellation of an opened content stream, and a subsequent unaffected
read. Public repository tests cover open cancellation, expired refs/refresh/
snapshot and a cached-ref response limit. A gated runtime test proves both caller
cancellation and deadline wait for cleanup and preserve its typed secondary
error. The previous abandoned-result fixture now waits for actual worker entry
before dropping the task: cancellation before entry correctly performs no work
and therefore produces no injected cleanup error.

A separate final runtime run adds shared-caller-scope isolation. Progress events,
operation identity, default-feature builders, full transport accounting, cloud
and large-file/performance qualification, and later SDK phases remain open.

The final targeted runtime run passed seven tests
(`sdk-operation-controls-runtime.log`). It exposed an ignored nested close
result in the existing native fixture's timeout wrapper. That caller now checks
both timeout and close results; the native fixture passed again without the
warning (`sdk-operation-controls-close.log`). Formatting, whitespace, all 48
inventory cells and five verifier tests passed. No dependency changes were
needed. Production growth adds one operation-options module and extends the
existing worker lifecycle; read/reconstruction owners remain canonical.


## SDK operation identity

Every SDK worker-admission attempt now allocates an opaque process-local
`OperationId`. Allocation uses checked atomic increment, so exhausted identity
space fails instead of reusing an ID. Error debug output contains the ID and
existing static context without exposing the dependency source. Standalone
validation before admission has no operation identity.

The tracked runtime attaches identity to worker errors, admission failures and
oneshot receive failures. Deferred client-close reporting retains the original
identity, including the secondary cleanup error. Content/archive streams expose
their ID after opening and retain it after EOF; stream-synthesized cancellation
and early-opening failures use the producing task's identity. Fallible commit,
tree, history and blame result conversion now stays inside the tracked worker,
so its errors cannot escape correlation. Read-owner algorithms are unchanged.

Tests verify distinct identities across worker/admission failure, preservation
through unobserved cleanup, and identity equality between a real cancelled native
content stream and its returned error. IDs provide diagnostic correlation only;
write recovery requires the separately planned durable receipts/tokens.

Validation passed: 21 content-feature tests (`sdk-operation-identity.log`),
20 metadata-only tests (`sdk-operation-identity-remote.log`) and three default
value tests (`sdk-operation-identity-values.log`), with no compiler warnings.
Formatting, whitespace, all 48 inventory cells and five verifier tests passed.
The small production increase supplies the identity value and propagation at
existing error boundaries; no dependencies or alternate execution paths were
added. Progress, cloud configuration, default-feature builders, complete
transport accounting, large-file/performance qualification and later phases
remain incomplete.


## Bounded typed progress

`Progress::channel` and `OperationOptions::with_progress` connect a single-slot
watch channel to the existing SDK worker. Events carry operation identity and
report Started, Cancelling and cumulative consumer-delivered bytes/items.
Content chunks and archive entries update counts only when returned by public
`next`; no payloads, paths or credential context enter events. Shared destinations
coalesce across operations; independent channels retain independent latest
values. Channels close when all destination handles are dropped, not when an
operation succeeds. Terminal results remain on operation/stream futures.

The locked Tokio watch contract retains only the latest value. The receiver
copies `borrow_and_update` after `changed`, avoiding the documented duplicate
observation race and never retaining a watch borrow across an await. Producers
do not await receiver consumption. Progress is optional; absent destinations
skip delivery counters. Buffered delivery after cancellation remains possible
and does not claim whole-file integrity or physical transport byte counts.

The coalescing regression publishes 10,000 updates without a receiver read and
then verifies only the latest remains, including after sender drop. Native
content and archive fixtures leave progress unread until actual EOF, then
compare reported cumulative bytes/items and identity with delivered results.
Gated lifecycle tests observe Started/Cancelling for caller cancellation and
deadline; dropping the progress receiver leaves typed operation errors intact.
`sdk-progress.log` records 22 passing content-feature tests. A final targeted
runtime run (`sdk-progress-runtime.log`) covers the strengthened lifecycle tests.

The final runtime run passed eight tests; metadata-only remote passed 21 tests
(`sdk-progress-remote.log`). No compiler warnings. Formatting, whitespace,
48 capability cells and five verifier tests passed. No dependencies changed;
the new progress module is 95 lines including its coalescing regression. Its
single-slot ownership is shared by the existing operation and stream paths.
This establishes lifecycle/consumer-delivery reporting, not per-operation
network metrics: owner metrics lack SDK operation correlation. Cloud support,
complete transport accounting, default-feature builders, large-file/performance
qualification and later phases remain unfinished.


## Direct cloud environment-chain selection

`DirectStoreOptions` now selects S3, GCS and Azure through `s3_from_env`,
`gcs_from_env` and `azure_from_env`. The SDK validates that account/bucket/
container arguments are bare names before provider construction. Provider
naming rules remain delegated. Client build calls the existing
`crab-storage::build_static_env_target_store`; no credential or endpoint fallback
logic is duplicated. The same storage owner is used by CLI replication and auth
composition. Those sibling paths and provider implementations are unchanged.

Cloud cache namespaces use the owner's resolved transport identity, covering
provider, account/bucket and endpoint addressing. A missing identity fails
closed rather than falling back to bucket-only isolation. Filesystem placement
retains its canonical-path identity. Store selection moved into its own module
so the public repository client retains operation composition responsibility.

Locked object_store 0.14.1 source was checked for all three `from_env` builders
and construction paths. Environment-chain construction uses their existing
semantics; this does not promise the full separate AWS SDK credential chain or
add explicit refresh-provider injection. A subprocess test clears its inherited
environment, uses an isolated home and synthetic construction credentials, then
builds all three stores and compares the SDK namespace to the owner's identity.
No remote requests are made. A separate table rejects URL/path/credential-shaped
names before credential resolution. Never count construction as backend E2E.

Validation passed: 24 content-feature tests (`sdk-cloud-selection.log`) and
23 metadata-only tests (`sdk-cloud-selection-remote.log`), including the native
filesystem fixture. The nested subprocess reports its single identity test too;
that duplicate execution is not an additional test in these counts. No compiler
warnings. Live RustFS/cloud, explicit provider injection, full transport
accounting, default-feature builders, large-file/performance gates and later
phases remain outstanding.

Formatting, whitespace, all 48 capability cells and five verifier tests passed.
No dependencies changed. The store-options module is 204 lines including tests;
client composition is 600 lines. Production growth adds provider selection and
uses the canonical owner constructor; it does not add another provider stack.

## First live RustFS SDK read

The existing local RustFS service was reused without restarting or changing its
configuration. Image: `rustfs/rustfs:latest`, resolved image ID
`sha256:67f06d4b3479fd9d323d8a99c86aac411e665ac0981d06b52a2b790c66db359e`.
Endpoint: local port 9000. Dedicated bucket: `crab-sdk-1abf-qualification`.

The opt-in `s3_read` integration test publishes an empty manifest through the
storage owner into a unique process/time prefix, then builds the public SDK S3
client, opens the repository, verifies symbolic HEAD and empty refs, and checks
that resolving the unborn branch returns typed NotFound. After client close,
object keys, sizes and ETags equal the pre-read inventory. The test deletes only
objects it created. A subsequent bucket listing returned zero objects.

`sdk-rustfs-empty-read.log` records the successful remote-feature test on
Rust 1.91.0: one passed, 0.17 seconds of test execution. The test is ignored by
default because it requires an explicitly configured dedicated bucket; the
recorded run used `--test s3_read -- --ignored`. No container, other repository
prefix or unrelated data was removed. The initial `KeyCount` listing field was
absent in RustFS's response; the subsequent query counted the actual Contents
array instead.

This is real S3-compatible empty-repository read evidence, not merely provider
construction. An unchanged inventory does not prove zero attempted writes;
read-only policy/audit evidence, pack-backed reads, Crab/LFS reconstruction,
source-deleted large fixtures, cloud providers and performance gates remain
unqualified. Formatting, whitespace, all 48 inventory cells and five verifier
tests passed. No production code or dependency changes in this increment.

## Source-deleted Git pack reads on RustFS

The native Git fixture now has an opt-in S3 destination. It requires an empty
dedicated bucket, copies closed published objects there and selects the public
SDK S3 client. The original Git source and pack file are removed before reading.
It reuses the fixture's generation-pinning, commit/tree/history/diff/blame,
raw/file ranges, Git archive, cancellation/deadline, limits and progress checks.
This run enables `remote` without `content`: cloud pointer hydration remains
separate work.

The first live attempt exposed a fixture mistake: Store::put is create-only and
correctly rejects changed mutable manifest bytes. The snapshot copier now uses
the owner's explicit overwrite method; this test does not exercise production
CAS publication. A second run completed its read checks but left two earlier
catalog checkpoint files during cleanup. Cleanup had used the publisher's final
inventory, which no longer included every copied generation. The fixture now
records every copied key, rejects any final destination key not in that ledger,
and removes its complete publication set. This verifies fixture ownership of
retained checkpoints; it is not proof of zero attempted SDK writes.

`sdk-rustfs-pack-read.log` records a passing live test in 4.04 seconds on the
same local RustFS image/endpoint and dedicated bucket as the previous smoke.
The bucket-empty assertion passed after cleanup. The initial create-only failure
is retained in `sdk-rustfs-pack-read-before.log`; inventories of failed-run
objects and retained checkpoints are stored alongside the logs. Only this
fixture's objects were removed during recovery. No production code changed.

The local content-feature native fixture also passed after the shared fixture
change (`sdk-rustfs-fixture-local-regression.log`, 3.65 seconds), retaining Crab
and LFS regression coverage. Formatting, whitespace, 48 capability cells and
five verifier tests passed. Cloud hydration, read-only policy/audit proof,
large-file/performance tests, full transport accounting and remaining phases
are still unqualified; no capability cell was promoted by this narrow smoke.


## Live RustFS Crab and LFS hydration

The S3 variant of the source-deleted native fixture now runs with `content`.
Corruption and missing-content fixture writes use the storage owner's explicit
mutation methods rather than filesystem-only operations. Auxiliary cold-cache
clients use the selected DirectStoreOptions, so limit/corruption reads target
the same S3 store. The filesystem fixture retains the same checks through a
local Store. No production read or provider implementation changed.

`sdk-rustfs-content-read.log` records a passing content-enabled RustFS run in
8.41 seconds. The 192 KiB ordinary/Crab/LFS fixture exercises exact full/range
bytes, chunk-size bounds, idle deadlines, early close, transport admission
limits, corrupt/truncated/missing LFS, absence of LFS receipts, corrupt xorb,
and missing/stale Crab hint lookup pinned across generation advancement.
The inherited Git read, archive and lifecycle checks also run. Fixture objects
are restored for subsequent reads and its complete copied-object ledger is
removed at the end; the final bucket-empty assertion passed.

Local content regression passed in 3.40 seconds
(`sdk-content-fixture-storage-regression.log`); local metadata-only regression
passed in 1.47 seconds (`sdk-content-fixture-remote-regression.log`). No compiler
warnings. Formatting, whitespace, 48 capability cells and five verifier tests
passed. These are small-fixture correctness smokes, not the specified 1 GiB Crab
and 32 MiB LFS memory/performance qualification. Read-only policy/audit proof,
complete transport accounting, cross-provider/cross-OS gates and later SDK
phases remain open. No capability cell was promoted.

## Executable public read examples and remaining API gaps

Added the required `examples/remote_read.rs` and `examples/remote_archive.rs`.
Both are feature-gated example targets requiring `remote`, accept explicit S3
bucket/repository/branch arguments and use only the public SDK read surface.
The read example reports raw bytes' commit, length and BLAKE3; the archive
example consumes Git-mode entries through EOF. Invalid argument shape/encoding
returns errors. Output failures propagate through a result scope that still
awaits client close; simultaneous primary/cleanup failures report both.

Rust 1.91.0 built both executables (`sdk-read-examples.log`). The live RustFS
fixture invoked them after removing its source repository and after publication
advancement. It checked the read command's exact commit/length/hash output and
the archive command's expected byte-path/size entry. The combined fixture passed
in 4.51 seconds (`sdk-read-examples-rustfs.log`) and cleaned its published keys.
Both commands also rejected missing arguments with nonzero exit and usage text.
Formatting, whitespace, 48 capability cells and five verifier tests passed.

The API audit identified two unresolved contract items. Hydrated archives are
still rejected in Snapshot::archive; the current ArchiveEntry owns whole-entry
Bytes, so merely enabling hydration would violate large-file streaming intent.
Its replacement must preserve one archive operation's aggregate budget and
cleanup lifetime while delivering bounded content chunks. Also, current read
methods require explicit options arguments; the plan's default read-builder
ergonomics remain missing, independently of the already-recorded default-feature
builder gap. These findings are not waived by passing examples or smokes.


## Default read request builders

Public repository and snapshot read calls now return lazy, directly awaitable
Request builders. Defaults require no options argument; `with_options` supplies
OperationOptions for repository calls and ReadOptions for snapshot reads. The
old signatures were removed, and SDK tests plus both executable examples were
updated. Private execution methods still use the same tracked worker path,
limits, operation identity, cancellation and cleanup. No compatibility path was
retained for this unpublished API.

The request test drops a builder and a converted-but-unpolled future without
entering its operation, then verifies the last selected options are used exactly
once on await. Existing public tests cover default and configured calls across
all methods, including cancellation/deadline and range errors. The implementation
uses bounded boxed factory/future storage; this introduces fixed per-call
allocation overhead that remains subject to the planned performance gates.
It does not buffer content or create another runtime.

The content suite passed 24 tests (`sdk-read-builders.log`); metadata-only remote
passed 24 including the new laziness test (`sdk-read-builders-remote.log`). The
initial content build found unused example imports, which were removed. Scoped
Rust 1.91 Clippy (`--lib --no-deps -- -D warnings`) passed after simplifying the
repeated future type and removing a redundant must_use attribute
(`sdk-read-builders-clippy.log`). The toolchain's Clippy component was installed
because it was absent; no project dependency changed.

This resolves default read-call ergonomics, not the distinct requirement to
expose builders with the crate's default feature set. Hydrated archive streaming,
large-file/performance qualification, full transport accounting, read-only audit
proof and later phases remain incomplete.

Both examples rebuilt successfully (`sdk-read-builders-examples.log`). The
content-enabled RustFS fixture then invoked them and passed its full small-file
Git/Crab/LFS checks in 8.50 seconds (`sdk-read-builders-rustfs.log`), including
scoped cleanup. Formatting, whitespace, 48 inventory cells and five verifier
tests passed. Production growth supplies the requested lazy configuration
boundary; implementation methods and owner algorithms remain canonical.

## Default-feature configuration builders

ClientBuilder, DirectStoreOptions and RepositoryLocator are now available with
`default = []`. They retain SDK-owned configuration values; constructing provider
stores and Client read APIs still requires `remote`. There is no placeholder
client or runtime fallback. Cloud target validation moved into the selector
constructors, which now return Result before credential resolution. Existing
callers and examples observe those results. Actual provider construction still
uses the canonical storage owner when the feature is enabled.

The configuration test constructs filesystem/S3/GCS/Azure builder values without
a Tokio runtime and checks validated repository prefixes. Invalid cloud names
fail before client building. The default normal dependency tree
(`sdk-default-builder-tree.txt`) excludes Tokio, tokio-util, object_store,
crab-storage, crab-remote-git and crab-read. Configuration fields intentionally
retained for feature-enabled construction have narrowly scoped default-only
expected-dead-code annotations; no runtime capability is enabled to silence them.

Validation passed: six default-feature tests (`sdk-default-builders.log`),
27 content-feature tests (`sdk-default-builders-content.log`) and 26 metadata-only
tests (`sdk-default-builders-remote.log`), excluding nested subprocess duplicate
counts. The existing provider construction/identity test and native filesystem
fixture remain green. Default-feature scoped Clippy with warnings denied passed
(`sdk-default-builders-clippy.log`), as did formatting, whitespace, 48 capability
cells and five verifier tests. No dependency changes were required. The moved
builder/value code now sits at its feature boundary; provider mechanics remain
unchanged. Hydrated archives, large-file/performance and read-only audit gates,
complete transport accounting and later SDK phases remain open.

### Bounded archive framing implementation checkpoint

SDK archives now emit Entry metadata (including representation size), Data frames
of at most 64 KiB, and EndEntry only after content delivery succeeds. With the
content feature, the existing Crab and LFS readers hydrate pointer entries under
one archive operation and aggregate logical-byte accounting. Symlink targets
remain literal Git bytes. The shared Git archive reader retains its default raw
representation for existing consumers. Reconstruction and forwarding are joined
so consumer disconnect drops the internal receiver and drains reader cleanup.

Local Rust 1.91.0 evidence in the dedicated worktree target:
- `sdk-hydrated-archive.log`: content SDK tests pass, including byte-identical
  Crab, missing/stale-hint Crab, and LFS archive contents with strict framing.
- `sdk-archive-limits.log`: native source-removal fixture passes; aggregate
  logical limit permits two ordinary entries and rejects the next hydrated entry.
- `sdk-archive-symlink.log`: pointer-shaped symlink remains literal; regular file
  containing the same bytes remains classified as an LFS pointer.
- `sdk-archive-owner.log`: three existing Git archive traversal, incremental
  cancellation, and early-close tests pass.

This supersedes the earlier unsupported hydrated-archive checkpoint. Archive
corruption/early-close under hydration, live S3 rerun, HTTP consumer checks,
large-file memory/performance, and full qualification remain open. No backend
capability is promoted by this checkpoint.

### Hydrated archive failure and RustFS evidence

The shared native fixture now checks that corrupted Crab xorbs, corrupted LFS
objects, and missing LFS objects return their typed error while the affected
archive entry remains unfinished. Closing after receiving hydrated Data from
either content type drains within the fixture's two-second bound. These tests
exercise the same archive producer and hydration readers used by SDK consumers.

- `sdk-archive-failures.log`: local content fixture passes (3.70 seconds).
- `sdk-archive-rustfs.log`: the same fixture passes against the existing RustFS
  service (8.17 seconds), including rebuilt remote read/archive examples.
- The dedicated `crab-sdk-1abf-qualification` bucket had zero objects before and
  after the successful run. This is cleanup evidence, not a zero-write-attempt
  proof or a read-only-policy qualification.
- `sdk-archive-clippy.log`: content SDK library Clippy passes with warnings denied
  on Rust 1.91.0.

HTTP archive source inspection confirms its writer consumes `ArchiveEntry.bytes`
from the unchanged default Git representation. An HTTP consumer build remains
outstanding. Explicit external cancellation/deadline during hydration, large-file
resource measurements, and transport-attempt accounting still need qualification.

### Archive cancellation and HTTP consumer build

- `sdk-archive-cancellation.log`: local source-removal fixture passes (3.24
  seconds). Caller-owned cancellation after the first hydrated Data frame for
  both Crab and LFS returns `Cancelled`; explicit close then drains successfully.
  The same test retains early-close coverage without caller cancellation.
- `sdk-archive-http-check.log`: `cargo +stable check -p crab-http-server --locked`
  passes for the affected consumer and shared owners. This closes the consumer
  compile gap above; it is not an HTTP archive end-to-end qualification.

The scope remains phase-one implementation/qualification. No capability status
is promoted. Archive deadline and drop/drain evidence, large-file resource
measurements, read-only request auditing, and later delivery phases remain open.

### Reproducible large-file qualification inputs

`crab/scripts/generate-sdk-fixtures.py OUTPUT_DIRECTORY` now generates the exact
version-one fixture manifest with 1 MiB SHAKE-256 blocks, checks each declared
SHA-256, and publishes without replacing existing files. The output directory
must already exist on the external qualification volume. Memory use during
input generation is bounded by blocks rather than file size; this makes no
claim about SDK reconstruction memory.

For this worktree, generation completed under
`/Volumes/Workspace/crabbuild-target/crab-1abf-sdk/qualification-inputs`:
4 KiB ordinary, 1 GiB Crab, and 32 MiB LFS. `sdk-fixture-generation.log` records
matching manifest hashes. Independent `shasum -a 256` of the resulting files
also matches all three hashes (`sdk-fixture-shasum.log`). A repeated invocation
rejects replacement with exit status 2 (`sdk-fixture-no-overwrite.log`).

These are qualification inputs, not yet a published large-file repository.
The existing 192 KiB native fixture uses a single chunk; it cannot establish
realistic multi-xorb reconstruction memory or the controlled Linux RSS gate.
Next qualification work must publish these inputs through realistic chunk/xorb
layout, remove the source checkout, and measure SDK streaming in a separate
process so fixture construction does not contaminate peak RSS.

### Standalone hydrated reader example

`remote_read` now accepts an explicit `git` or `hydrated` representation after
its path argument. Hydrated mode uses `open_file`, hashes each returned chunk,
and observes verified EOF and explicit close. It never collects logical file
bytes. An optional existing absolute cache directory enables Crab reconstruction
with a 512 MiB retention budget; that budget is not a process RSS bound.

The source-removal RustFS fixture executes the example in raw mode for an
ordinary file and hydrated mode for both Crab and LFS pointer files. All three
results match expected commit, byte length and BLAKE3 (`sdk-logical-example-rustfs.log`,
8.70 seconds). The dedicated bucket is empty after cleanup. Content examples
build on Rust 1.91.0 (`sdk-logical-example-build.log`).

This supplies a separate reader process for subsequent large-file measurements;
it does not yet demonstrate the 1 GiB reconstruction/RSS acceptance gate.

### First real 1 GiB reconstruction measurement: memory failure

The ignored `large::publish_large_qualification_repository` fixture publishes
verified deterministic inputs to an empty dedicated S3 bucket. It packs 64 KiB
chunks through the normal XorbBuilder, drains completed xorbs while building,
writes multi-xorb shard terms and native Git pack/catalog metadata, and removes
the temporary source checkout before returning. The persistent task-owned bucket
`crab-sdk-1abf-large` is retained for repeated isolated reader measurements.
This focused large-file fixture does not replace the planned 10,000-file matrix.

`sdk-large-publish.log`: publication passed in 123.61 seconds. Captured commit
`d1e36d8b869f5d32fd111f9f3575480a1ac03e64`; Crab BLAKE3
`de9706caf2109b9a8d76e62e091383f1b8f3c11c7dd1674d8d17414664de515a`.
The standalone `remote_read` example explicitly admits 4 GiB of fetched bytes
and a 120-second duration for logical reads; SDK defaults remain unchanged.

Separate macOS debug-reader processes, measured with `/usr/bin/time -l`:
- `sdk-large-read-cold.log`: exact 1,073,741,824 bytes and matching BLAKE3;
  19.92 seconds, maximum RSS 1,127,120,896 bytes.
- `sdk-large-read-warm.log`: same exact result; 18.74 seconds, maximum RSS
  1,127,432,192 bytes. This reused the 512 MiB retention cache; the full 1 GiB
  working set cannot be assumed resident in that cache.

Correctness succeeds for this fixture, but memory is approximately 1.05 GiB.
This does not meet the intended 512 MiB gate and is not controlled Linux proof.
Diagnosis is active: distinguish concurrent fetch/decode buffers, asynchronous
cache retention, and reconstruction-wide retention before choosing a fix.
No memory/performance capability is qualified.

### Controlled memory probes (diagnostic settings reverted)

Using the same persistent 1 GiB fixture, cache directory and macOS debug reader:

| Probe | Time | Peak RSS | Content |
| --- | ---: | ---: | --- |
| Baseline: concurrency 64, decoded budget 256 MiB | 18.74 s | 1,127,432,192 B | Exact |
| Concurrency 2, decoded budget unchanged | 24.15 s | 652,312,576 B | Exact |
| Concurrency restored, decoded budget 64 MiB | 55.69 s | 520,142,848 B | Exact |

Evidence: `sdk-rss-concurrency-two.log`, `sdk-rss-buffer-64.log`.
The first probe supports concurrent fetch/decode allocations as a contributor.
The second reaches roughly 496 MiB RSS here, but nearly triples elapsed time.
Neither is accepted as the final performance fix. SDK configuration and the
reader executable are restored to baseline (`sdk-rss-restored-build.log`);
no diagnostic configuration or logging is retained.

Dependency inspection: Xet FileReconstructor acquires the custom semaphore by
logical term size; AdjustableSemaphore clamps oversized requests to total
permits. StoreClient holds its download permit while CachingStore reads a
complete compressed xorb, verifies its payload and decodes selected ranges.
The semaphore therefore cannot be interpreted as a total physical-memory cap.
Next diagnosis should inspect simultaneous compressed/decoded allocations and
cache retention, preserving throughput as well as correctness. Linux and
five-run cold/warm qualification remain outstanding.

### Decoded-cache retention probe

A further one-variable diagnostic disabled the decoded chunk cache while
retaining baseline download concurrency and decoded-buffer admission.
`sdk-rss-no-cache.log`: exact 1 GiB result, 19.38 seconds, peak RSS
650,035,200 bytes (about 620 MiB). Baseline was 18.74 seconds and about
1.05 GiB RSS. Disabling cache is not accepted as the product fix; both source
and executable are restored (`sdk-rss-cache-restored-build.log`).

Source evidence narrows the next fix: Xet `XorbBlock::get_data` clones decoded
Bytes into a detached cache-put task after StoreClient returns. Reconstruction's
logical buffer permit can be released independently of that clone. The owner
cache-completion wrapper waits for cache Arcs at EOF but does not constrain
bytes retained by those writes. Cache absence materially reduces observed peak
without comparable throughput loss. This supports investigating cache-write
admission/lifetime coupling, while the remaining 620 MiB also requires analysis
of compressed and decoded buffers. No dependency modification was made.

### Cache-admission regression captured (currently failing)

`cache_fill_finishes_before_decoded_bytes_leave_admission` exercises the real
hydrator with a controlled pending cache put and an observed writer. It releases
the cache and drains the operation before asserting, so failure does not leave
its test task running. Current code writes 131,072 bytes while the cache fill is
still pending; expected zero under the intended coupled-admission ordering.
`sdk-cache-admission-regression.log` records the failing test. This regression
is intentionally unresolved pending the owner fix; this worktree is not ready
to land with that failure.

Upstream source also explicitly notes that XorbBlock's cache key uses only the
first ChunkRange. Moving cache ownership must preserve correct multi-range
behavior, not repeat that limitation. The proposed boundary is StoreClient's
term read, which already owns the download permit and cancellation scope, with
cache reads/writes completed before returning decoded bytes to Xet. No upstream
dependency patch or production cache behavior has been changed yet.

### Decoded-cache ownership fix

StoreClient now owns decoded-cache lookup and publication inside its cancellable
term read. Cache fills complete before decoded bytes return, retaining both
download and reconstruction admission. Each disjoint range gets its own cache
entry and relative offsets. Xet receives no cache, eliminating its detached
cache tasks; the previous cache-completion wrapper and its obsolete internal
tracking tests are removed. Existing real-hydrator lifecycle tests remain.

Proof:
- `sdk-cache-admission-fix.log`: all three real-hydrator cache tests pass,
  including the previously failing output-before-fill regression and pending
  cache cancellation/drop/error behavior.
- `sdk-term-cache-tests.log`: disjoint-range cache offsets and combined reads pass.
- `sdk-owned-cache-native.log`: SDK source-removal fixture passes (3.46 seconds).
- `sdk-cache-owned-large.log`: exact 1 GiB result, 24.00 seconds, peak RSS
  854,441,984 bytes (about 815 MiB) with baseline concurrency/buffer settings.

This resolves the cache lifetime regression, but does not qualify performance:
RSS remains above 512 MiB and throughput requires further work. Shared-consumer
and additional cache-hit/corruption verification remain outstanding. No dependency
patch, configuration fallback, or diagnostic override is retained.

### Cache ownership: reuse and consumer verification

`reopened_decoded_cache_reconstructs_without_origin_xorb` fills the real disk
cache, drops the hydrator, creates a fresh runtime/origin containing only shard
metadata, and reconstructs byte-identical content. It rules out the in-memory
xorb-read cache or origin as the source of success (`sdk-cache-origin-absent.log`,
0.53 seconds).

Further focused proof after the ownership change:
- `sdk-cache-reuse.log`: disk-cache root, reopen and capacity contract passes.
- `sdk-owned-cache-ranges.log`: four range writer tests pass, covering exact
  ranges, invalid bounds, cancellation/drop, and corrupt origin.
- `sdk-owned-cache-http.log`: affected HTTP consumer check passes.

These checks support cache functionality/lifecycle; they do not qualify the
remaining 815 MiB RSS result, throughput, full cloud matrix or later phases.

### Post-fix decoded-budget probe and optimized-build preparation

With cache fills owned by StoreClient, a temporary 128 MiB decoded-buffer
budget returned exact 1 GiB bytes in 38.32 seconds at 449,200,128 bytes peak
RSS (about 428 MiB). Evidence: `sdk-owned-cache-buffer128.log`. This is a
macOS debug diagnostic, not controlled Linux qualification. The SDK-only
budget override is removed; the shared-core default remains 256 MiB.

The qualification contract compares SDK overhead against the same shared-core
baseline across five cold/warm runs, not against these earlier diagnostic
variants. An optimized Rust 1.91.0 reader build was started with the restored
source (`sdk-optimized-reader-build.log`); its completion and measurements
must be checked before citing optimized results. The existing debug example
binary still contains the last diagnostic budget and must be rebuilt before
using it as a baseline. No diagnostic source override remains.

### Optimized reader and shared admission default

Rust 1.91.0 release build completed (`sdk-optimized-reader-build.log`). With
cache ownership fixed and the previous 256 MiB decoded budget, the optimized
1 GiB read returned exact bytes in 19.90 seconds at 845,725,696 bytes peak RSS
(`sdk-optimized-large-read.log`). A temporary 128 MiB probe returned exact bytes
in 17.48 seconds at 439,058,432 bytes peak RSS (`sdk-optimized-128-read.log`).

The shared ReadRuntimeBuilder default is now 128 MiB, leaving headroom for
compressed fetch buffers, decoding and cache I/O. The SDK-only override is
removed. This admission count is not a universal physical-memory guarantee.
CLI read/hydrate callers supply their resolved explicit budget; SDK and auth
view construction use the shared default. Auth-view consumer proof remains
required before closeout of this change.

Final shared-default release build and cache lifecycle tests pass
(`sdk-default-budget-release.log`, `sdk-default-budget-tests.log`). Its separate
reader process returns the same exact bytes in 18.20 seconds at 505,446,400
bytes peak RSS, about 482 MiB (`sdk-default-budget-large.log`). This is improved
macOS evidence, not the required controlled Linux five-run comparison. No
performance capability is promoted. Debug executables must be rebuilt before
further baseline use; release/example now reflects the shared default.

### Shared-default consumer and 32 MiB LFS proof

The optimized SDK reader returned 33,554,432 LFS bytes from the persistent
large fixture with BLAKE3
`505731360cbf1627bb27de3a32c0ae2d6f1e8c2de97adbe5413acbe6795dbf7c`, matching
publication (`sdk-large-lfs-read.log`). This run proves content, not timing;
a consumer build was running concurrently.

The auth-server consumer checks pass:
- `sdk-default-budget-auth.log`: locked build check with stable Rust.
- `sdk-default-budget-auth-view.log`: native filtered-view construction and
  view-local Crab reconstruction pass (2.05 seconds). This exercises the shared
  runtime default through the existing real-dependency consumer fixture.

A local Linux ARM64 qualification image is available without downloading a new
image: `rust:1.91-bookworm`, pinned image ID
`sha256:feaf6897eff114cb7db2d204f8444d8133c1c9bc2b9d6e1b07f9f5726232d1a4`.
Availability is not a Linux build or measurement result; that work remains open.

### Linux qualification environment prepared

Started task-owned container `crab-sdk-1abf-linux` from pinned image
`sha256:feaf6897eff114cb7db2d204f8444d8133c1c9bc2b9d6e1b07f9f5726232d1a4`.
Actual tools are Rust/Cargo 1.91.1 (the image's tag alone is not a version proof).
Architecture is Linux ARM64, with four CPUs and a 4 GiB container memory limit.
The source checkout is read-only at `/workspace`; Cargo's writable Linux target
is `/Volumes/Workspace/crabbuild-target/crab-1abf-sdk/linux-target`. The native
linker is selected as `cc` for the container; repository config is unchanged.
RustFS is reachable at `host.docker.internal:9000`.

The release SDK example build is in progress (`sdk-linux-release-build.log`).
Do not count it as a completed Linux build until its terminal result is checked.
`crab/scripts/measure-sdk-read.py` prepares one fresh-process Linux measurement:
it launches only the SDK reader, checks exact expected stdout/exit status, and
reports child peak RSS, CPU and elapsed time. CLI help runs inside the container.
No Linux SDK read or memory result is claimed by this setup checkpoint.

### Linux cold-read SIGBUS: qualification failed

Linux release build completed in 2m59s (`sdk-linux-release-build.log`). The
measurement-runner test passes on Linux, including rejection of mismatched
output (`sdk-linux-measure-tests.log`). The first cold SDK read failed with
signal 7 before emitting a result: `sdk-linux-large-cold.json` has exit -7 and
verified=false. Its RSS is not successful-read evidence.

Installed GDB in the task container (`sdk-linux-debugger-install.log`, including
package updates recorded there). A debugger run reusing that cache completed
with exact bytes (`sdk-linux-sigbus-gdb.log`), but a new empty cache reproduced
SIGBUS (`sdk-linux-sigbus-cold-gdb.log`). The fault is BUS_ADRERR at
0xfffff686bfc0, inside a 32 KiB shared mapping of `.catalog.sqlite-shm` on the
host VirtioFS bind mount. GDB showed two mappings of that file. Its size after
termination was 32,768 bytes. `/dev/shm` was empty, ruling out simple exhaustion
of the container's 64 MiB shared-memory mount.

The crash is unresolved. Investigate SQLite shared-memory initialization,
concurrent mapping lifetime and filesystem coherence before choosing a fix.
No native-Linux read, memory, or backend capability is qualified by this run.

### SIGBUS narrowed to broken filesystem lock semantics

The same Linux binary and pinned base image succeed with a new native tmpfs
cache: exact 1 GiB result, 6.47 seconds, peak RSS 372,764,672 bytes
(`sdk-linux-native-cache.json`). This is a diagnostic filesystem comparison,
not disk-cache performance qualification.

An independent two-descriptor OFD-lock probe on the host bind mount confirms a
broken prerequisite (`sdk-linux-ofd-probe.log`): after descriptor A acquires a
shared lock at the SQLite dead-man byte, descriptor B's F_OFD_GETLK reports
F_UNLCK and its exclusive F_OFD_SETLK succeeds. Both should conflict. Thus the
filesystem can let another opener reset an existing mapped SHM file, explaining
the cold-start SIGBUS mechanism. Native SQLite's upstream initialization also
requires this lock exclusion before truncating abandoned SHM to three bytes.

The initial main-database exclusion guard passed host database tests but did not
close the Linux crash: `sdk-linux-lock-guard-cold.json` records exit -7, and
`sdk-linux-guard-gdb.log` again locates BUS_ADRERR inside catalog SHM. Do not infer
filesystem-wide safety from an exclusion probe on one file.

`sdk-linux-guard-strace.log` explains the distinction: independently reopened
main database descriptors report the expected read lock and reject an exclusive
probe with EAGAIN. Newly created SHM descriptors can instead report no conflict.
The standalone probe uses newly created files, explaining its different result.
This traced read happened to complete; debugger and untraced cold runs still
reproduce the crash. Tracing changes timing and is not a regression pass.

The private database owner now also probes the actual SHM file through an
independent descriptor before truncation or mapping, verifying inode identity.
Strace installation is recorded in
`sdk-linux-strace-install.log`; the diagnostic container includes those tooling
changes in addition to its pinned base image.

The updated trace (`sdk-linux-shm-guard-strace.log`, lines 28–40) verifies the
guard's failure path: the first SHM probe reports F_UNLCK, releases its probe
lock, and performs no dead-man reset. A later independently reopened descriptor
proves exclusion before proceeding with SQLite's normal initialization. The
traced SDK read returns the exact expected 1 GiB hash.

Untraced cold reads also succeed: `sdk-linux-shm-guard-cold.json` records exact
bytes, exit 0, 6.36 seconds and peak RSS 356,352,000 bytes. Native tmpfs with the
same executable succeeds in 6.42 seconds at 429,633,536 bytes
(`sdk-linux-native-shm-guard.json`). These are focused crash-regression evidence,
not the five-run disk-cache/shared-core performance qualification. Both cache
features together pass all 24 private-database tests and owner-library clippy
with warnings denied (`sdk-shm-guard-tests.log`, `sdk-shm-guard-clippy.log`).
Three additional independently empty bind-mount caches also return the exact
1 GiB result without a signal (`sdk-linux-shm-guard-cold-2.json` through
`sdk-linux-shm-guard-cold-4.json`). Formatting and diff whitespace checks pass.

### Measurement artifact identity

The Linux measurement runner now records SHA-256 of the executed binary and
the expected commit, byte count and BLAKE3 digest in each report. Hashing occurs
before timing and does not launch a child that could contaminate child RSS.
The Linux runner regression checks both correct and deliberately mismatched
output, including artifact identity and expected-result fields.

`sdk-linux-fingerprinted-warm.json` verifies the existing 1 GiB fixture in
6.36 seconds with peak RSS 357,683,200 bytes. Its executable SHA-256 is
`21696fad90dd2d20b46678fe7c5423a3212074c95c0150d58419f7d97e4b9f96`,
independently recomputed from the executable after the run. This identifies the
binary; it is not a source-to-build attestation. Five cold/warm samples against
the same shared-core baseline, physical request/byte accounting and immutable
source/build provenance remain required before performance qualification.

### Repository capability discovery

Implemented the required `RemoteRepository::capabilities` query as synchronous
metadata. It returns `ReadGit` and conditionally `ReadContent`; it advertises no
unimplemented mutation family. Public documentation distinguishes mechanism
availability from authorization and backend qualification. The enum is
non-exhaustive so later operation families can be represented without forcing
callers to assume these two exhaust the contract.

The filesystem-backed public-client test checks the exact reported families
and metadata access after client close. Both remote-only and remote/content
profiles pass their two public-client tests on Rust 1.91.0
(`sdk-capabilities-remote.log`, `sdk-capabilities-content.log`). No dependency
or feature-closure changes are involved.
SDK-library clippy with `--no-deps` and warnings denied passes
(`sdk-capabilities-clippy-scoped.log`), as do formatting and diff checks.
Dependency-wide clippy stops on `nonminimal_bool` in unchanged
`crates/crab-metadata/src/git_visibility.rs:164`
(`sdk-capabilities-clippy.log`); that broader gate is not claimed passing.

### Independent read-only HTTP guard

`crab/scripts/verify-sdk-read-only.py` runs the real read example through a local
HTTP proxy that forwards GET/HEAD and rejects/counts mutation methods. Unknown
verbs also invalidate qualification. It verifies exact expected example output,
requires observed read traffic for each case, records per-case request counts,
and fingerprints both the executable and case inputs. It never logs HTTP
headers or signed request targets. Proxy transport failures invalidate the run;
request threads drain before final counters are reported.

The guard regression first performs a successful GET, then attempts PUT or
TRACE while the fake reader still exits successfully with expected output.
Both attempted methods fail qualification; GET-only passes
(`sdk-read-only-guard-tests.log`). Thus output success and an unchanged backend
cannot hide a forbidden attempt.

The existing RustFS fixture passes all three cases in
`sdk-read-only-live-final.json`, with inputs in `sdk-read-only-cases.json`:

| Public example mode | Exact bytes | Observed GETs | Write attempts |
| --- | ---: | ---: | ---: |
| Raw ordinary Git blob | 4,096 | 45 | 0 |
| Hydrated Crab file | 1,073,741,824 | 67 | 0 |
| Hydrated LFS file | 33,554,432 | 46 | 0 |

All expected commit IDs and BLAKE3 digests match. Total: 158 GETs, no HEADs,
zero write attempts and zero proxy failures. The ordinary fixture digest was
independently computed with `b3sum`. Crab uses a fresh explicit local cache;
local cache writes are permitted and do not pass through the remote guard.
The first input omitted that mandatory cache and correctly failed InvalidInput;
the failed report is retained as `sdk-read-only-live-diagnostic.json`.

This proves successful raw/Crab/LFS read behavior for this RustFS fixture and
the recorded executable, not every error path or cloud backend. The mandatory
read/lifecycle contract targets, lagging-catalog write-attempt proof, and the
full backend matrix still remain to be completed.

### Public lagging-catalog read contract

Added the required `crates/crab-sdk/tests/remote_read.rs` target with
`read_only_open_never_repairs`. It creates generation-2 manifest/inventory
metadata with either no locator or generation-1 locator coverage. Public
`Client::open_remote` returns typed `Indexing`; client cleanup succeeds and the
complete sorted object-name/byte inventory is unchanged afterward. The fixture
intentionally needs no pack payload: readiness must be diagnosed before Git
object reads. This follows the owner's existing absent/stale coverage contract
while verifying its SDK error translation and lifecycle.

Both remote-only and content-enabled tests pass on Rust 1.91.0
(`sdk-read-only-indexing.log`, `sdk-read-only-indexing-content.log`). The test
target's scoped clippy, formatting and diff checks pass as well
(`sdk-read-only-indexing-clippy.log`). This
checks storage effects, not rejected write attempts; HTTP-guard evidence follows
below. Other mandatory cases in this target and the lifecycle target
are not yet complete.

### Lagging-catalog write-attempt proof

The read-only guard now accepts an explicit expected exit code and exact stderr,
in addition to exact stdout. Expected failures must still make observed read
requests and attempt zero writes. Regression cases reject both a wrong exit
code and a wrong error diagnostic, as well as PUT/unknown-method attempts
(`sdk-read-only-error-guard-tests.log`). A nonzero process exit alone cannot
qualify an expected SDK error.

The opt-in `publish_indexing_qualification_fixtures` test generates absent/stale
fixtures through the same helper as the public read regression. It requires an
existing explicit output directory and refuses to overwrite either fixture
subdirectory. Both fixture generation and the read regression passed
(`sdk-indexing-fixtures.log`).

Each fixture was uploaded in turn into the empty task qualification bucket,
then opened by the real SDK read example through the HTTP guard:

| Locator coverage | Result | GET requests | Write attempts | Proxy failures |
| --- | --- | ---: | ---: | ---: |
| Absent | Exact `Indexing` diagnostic, exit 1 | 8 | 0 | 0 |
| Stale, generation 1 versus manifest 2 | Exact `Indexing` diagnostic, exit 1 | 17 | 0 | 0 |

Reports: `sdk-indexing-absent-guard.json`, `sdk-indexing-stale-guard.json`;
matching case files record the exact expected diagnostic. Both guard processes
exit successfully. Object key/size/ETag inventories were unchanged after each
open, then only uploaded fixture keys were deleted. The qualification bucket
was verified empty after cleanup. These reports identify the same read-example
binary as the successful-read guard record; they do not qualify other backends
or complete the remaining phase-1 contracts.
The fixture target's scoped clippy passes (`sdk-indexing-fixture-clippy.log`),
as do formatting and diff whitespace checks.

### Observed response-byte accounting

The HTTP guard now reports `response_body_bytes`: bytes forwarded from origin
response bodies, excluding headers, transfer framing and locally generated
rejection responses. Request threads drain before the total is reported.
The guard tests check exact totals for successful reads and for reads followed
by denied methods (`sdk-read-guard-byte-tests.log`). This supplies independent
traffic evidence; it does not change or complete SDK operation-budget charging.

A dedicated Linux SDK suite was started with `cargo test -p crab-sdk --locked
--features content`, external per-worktree target and fixture directories, in
`crab-sdk-1abf-linux`. `sdk-linux-suite.log` records a link failure with SIGKILL;
the container memory controller reported `oom_kill 2` under its 4 GiB limit.
The run exited 101 before tests. A retry of the same suite with `-j 1` passed
(`sdk-linux-suite-serial.log`): 28 top-level tests passed, four opt-in tests
ignored, zero failures. The unit suite also launches a one-test subprocess;
that nested result is not counted twice. Doc tests contain zero cases. Build
time was 9m20s; the container OOM counter did not increase during the retry.
Opt-in live tests skipped by this command must be accounted for separately.

The two read tests were then executed explicitly from the built Linux test
binaries, using the dedicated RustFS bucket:
`s3_pack_reads_survive_source_removal_and_keep_snapshots_pinned` passed one test
with zero ignored in 6.87 seconds (`sdk-linux-live-native.log`), and
`sdk_s3_read_preserves_empty_manifest` passed one test with zero ignored
(`sdk-linux-live-empty.log`). The qualification bucket was verified empty after
both tests. The two fixture-publication helpers remain intentionally excluded
from normal suite counts; their separate publication evidence is recorded above.
This is Linux SDK read/lifecycle integration evidence, not a performance baseline
comparison, cross-OS matrix, or completion of the write/local/managed phases.

### Automated SDK dependency boundaries

`crab/scripts/check-sdk-features.py` checks Cargo normal dependency trees for
all target platforms. It uses actual defaults for the default profile, and
explicit features with defaults disabled for remote and remote/content. The
gate rejects runtime/storage owners in the default profile, hydration owners
in remote-only, and Crab CLI/server/VFS packages in every read profile. Required
read owners must also be present, preventing empty/incomplete trees from passing.

The Rust 1.91.0 check passes: 38 default, 411 remote and 438 content packages,
with no forbidden or missing packages (`sdk-feature-boundaries.json`). Negative
regressions pass for forbidden dependencies and missing owners. The architecture
workflow runs the check and regressions; both push and PR path filters include
the new scripts. Workflow YAML parsing and diff checks pass. This is source
wiring and local gate evidence, not a completed GitHub Actions run or cross-OS
compilation result.

### Explicit provider configuration audit

Before the explicit-S3 increment, `DirectStoreOptions` exposed environment-chain
cloud selection only.
The storage owner already supplies `build_object_store_with_endpoint` for
explicit credentials and `build_s3_object_store_with_provider` for refreshable
S3 credentials. Neither is a complete environment-independent SDK configuration
path as-is: the refreshable constructor starts from `AmazonS3Builder::from_env`,
and the shared `default_client_options` reads `AWS_ALLOW_HTTP` even for explicit
credential construction. SDK-owned validated inputs must compose the owner
while making that transport policy explicit. Raw object-store handles must stay
internal, as the plan requires. The following increment addresses static S3
credentials; caller-supplied refresh and explicit GCS/Azure SDK inputs remain gaps.

### Explicit S3 configuration

Added SDK-owned `S3Options` with private fields, validated nonempty credentials,
optional session token and endpoint, and redacted Debug. The storage owner's
`build_explicit_store` consumes explicit transport policy, preserving signing,
multipart and transport identity. Its common construction path remains shared
with existing provider callers, whose environment policies are unchanged.
Explicit SDK cache namespaces bind access-key/session identity in addition to
transport placement; the provider transport identity remains credential-free.

Proof on Rust 1.91.0:

- Default-feature configuration: three tests pass (`sdk-explicit-s3-default.log`).
- Remote configuration and endpoint-secret rejection: four tests pass
  (`sdk-explicit-s3-configuration.log`).
- Credential cache separation with stable transport identity passes
  (`sdk-explicit-s3-cache-scope.log`).
- Provider-owner construction/identity regressions: 30 tests pass
  (`sdk-explicit-provider-owner-tests.log`).
- Both environment-selected and explicit S3 live reads pass, two executed and
  zero ignored (`sdk-explicit-s3-both-live.log`). The explicit test spawns a child
  with conflicting AWS endpoint, credentials, token, region, addressing and HTTP
  settings; the public SDK still opens the intended RustFS repository. Only the
  child environment is changed. Fixture objects are removed after the check.
- Scoped SDK/storage library clippy passes (`sdk-explicit-s3-clippy.log`).
- The existing auth-store consumer builds on installed stable
  (`sdk-explicit-s3-auth-consumer.log`).
- All-target dependency boundaries retain the same 38/411/438 package counts
  and pass (`sdk-explicit-s3-features.json`). Formatting and diff checks pass;
  the live qualification bucket is verified empty after cleanup.

No dependency or lockfile change is required. The earlier Linux suite predates
this increment; these new live checks ran on the host against RustFS. Full
cross-platform/provider qualification and automatic explicit-credential refresh
are not claimed.

### Explicit GCS configuration

Added SDK-owned `GcsOptions` for a bucket and static bearer token. Construction
uses the existing explicit storage owner; no dependency or owner behavior
changes. The locked object_store GCS builder skips ADC loading when credentials
are supplied. The SDK keeps token scopes separate in cache namespaces while
preserving the credential-free transport identity. Debug output is redacted.

Rust 1.91.0 focused proof:

- `sdk-explicit-gcs-tests.log`: GCS validation/redaction passes; isolated child
  construction with a malformed ADC file passes and distinct tokens produce
  distinct cache namespaces with the same transport identity. Two top-level
  tests pass; the subprocess repeats the construction test internally.
- `sdk-explicit-gcs-default.log`: all four default configuration tests pass,
  including pure GCS configuration without a runtime or provider resolution.
- `sdk-explicit-gcs-clippy.log`: scoped remote SDK library clippy passes.

Live GCS and cross-platform qualification remain outstanding. This is
construction proof, not a qualified GCS read capability. The plan's previous
added statement requiring a caller-defined credential refresh API was removed:
the original plan requires selected default chains and managed grant refresh
through its auth owner, not a new refresh abstraction.

### Azure SAS transport correction

While auditing explicit Azure prerequisites, the existing owner path was found
to pass percent-encoded SAS values to `with_sas_authorization`, which expects
query-pair values and encodes them for transport. A local HTTP server regression
demonstrated double encoding of a signature containing plus, slash and equals.
The unchanged test fails before the fix (`sdk-azure-sas-before.log`) and passes
after selecting the dependency's `AzureConfigKey::SasKey` string parser.

The locked object_store 0.14.1 source supplies the contract: Azure builder
`resolve_sas_token` calls `split_sas` for the string form; request authorization
passes the resulting pairs to the form serializer. No dependency changes.
Auth-store's `azure_authorization` passes SAS strings unchanged to this common
owner, so it receives the same correction. S3/GCS and Azure bearer branches are
unchanged. The previously exported `parse_sas_query_pairs` remains untouched;
it exists in release tag v1.1.0 but is no longer used by provider construction.

`sdk-azure-sas-after.log`: all 31 provider-store tests pass on Rust 1.91.0.
`sdk-azure-sas-auth-consumer.log`: auth-store consumer check passes on installed
stable. `sdk-azure-sas-clippy.log`: scoped storage/remote SDK library lint passes.
Formatting and diff checks pass. The wire regression proves local HTTP encoding,
not Azure authorization or backend qualification. Explicit Azure SDK options
remain outstanding. Its bearer implementation also requires input validation:
the dependency constructs the authorization header with `HeaderValue::from_str`
followed by `unwrap`, so invalid header bytes must not reach that path.

### Explicit Azure SDK configuration

Added `AzureOptions::bearer` and `AzureOptions::sas`, selected through
`DirectStoreOptions::azure`. Values remain private, Debug is redacted, and bearer
validation rejects invalid header characters before provider construction. SAS
query strings are parsed by the corrected owner path. Optional endpoints follow
the S3 validation and explicit HTTP policy. Authorization kind and token bind
SDK cache scope independently of credential-free transport identity.

The three explicit providers now share one SDK helper for owner construction,
transport error conversion and cache identity finalization. This removes the
duplicated S3/GCS construction branches; no dependency changes are needed.

Rust 1.91.0 proof:

- `sdk-explicit-azure-tests.log`: seven configuration tests and one local HTTP
  transport test pass. The latter runs both bearer and SAS children with
  conflicting Azure environment settings, serves an empty ref-transaction list
  followed by a missing manifest, and checks actual request authorization,
  correctly encoded SAS signatures, and the public `NotFound` outcome.
- `sdk-explicit-azure-scope.log`: five store-option tests pass, including Azure
  authorization-kind/token separation and existing S3/GCS identity invariants.
- `sdk-explicit-azure-default.log`: all five pure configuration tests pass.
- `sdk-explicit-azure-clippy.log`: scoped remote SDK library and changed
  integration targets pass lint.
- `sdk-explicit-azure-s3-regression.log`: both existing RustFS S3 tests execute
  and pass, zero ignored, after sharing the explicit constructor. The dedicated
  qualification bucket has no objects afterward.
- `sdk-explicit-azure-features.json`: all-target feature-boundary checks pass
  with unchanged default/remote/content package counts of 38/411/438. Formatting
  and diff checks pass.

The initial transport fixture incorrectly returned 404 for the first request,
which was a container list rather than a manifest read. The dependency source
confirmed the list response schema; the corrected fixture returns an empty
listing first. No product error classification was weakened to pass the test.
Temporary source-chain diagnostics were removed. Full live Azure/GCS and
cross-platform qualification remain outstanding; this is not backend acceptance.

### Listing and repository-open admission

Storage admission now covers list, offset-list and delimiter-list invocations.
Streaming listings defer admission and backend construction until first poll;
each invocation charges once, and cancellation drops a pending provider stream
or delimiter request. This is not physical pagination accounting: backend page
requests, listing response bytes and provider-internal retries remain opaque.

Repository opening now creates an independent aggregate budget and temporarily
wraps its store for metadata, listing and locator reads. The returned handle
retains the original store, so subsequent operations do not inherit the opening
budget. Operation budgets allocate unique IDs centrally; the existing semantic
context and content owners share the same budget admission implementation.

Optional commit-graph and shallow-closure loading propagate admission failures.
Regression testing exposed transparent metadata/storage error wrappers hiding
the admission marker from ordinary source traversal. The repository boundary
inspects those typed variants before permitting optional-index fallback.

Focused evidence on Rust 1.91.0:

- `sdk-list-admission-before.log`: both new listing-budget tests fail before the
  implementation. `sdk-list-admission-after.log`: all 11 admission tests pass,
  including pending-work cancellation for all three listing forms.
- `sdk-open-admission-before.log`: public opening incorrectly succeeds with a
  one-request budget. `sdk-open-admission-public.log`: all five executed tests
  pass across remote-client, read-only indexing and Azure request targets;
  one fixture publisher remains intentionally ignored. The open regression
  rejects one-request and one-byte budgets, then opens successfully using a
  fresh operation on the same client.
- `sdk-open-index-admission-diagnostic.log`: the optional commit-graph test
  reproduces swallowed admission through transparent errors.
  `sdk-open-admission-owner.log`: all 18 repository tests pass after the typed
  fix, including both optional-index rejection cases and existing generation,
  cancellation and framing checks.
- `sdk-open-admission-content.log`: the native Git/content fixture passes after
  opening admission was added. Its real source checkout is removed before reads.
- `sdk-open-admission-clippy.log`: scoped storage, remote-git and remote SDK
  library lint passes. Formatting and diff checks pass.

Remaining accounting work includes Git-reader storage retries and operation-time
locator acquisition, provider pagination and HTTP retry/body traffic. Repository
opening's full deadline/runtime-shutdown coverage also needs audit beyond the
existing phase-specific cancellation tests. These checks do not establish the
whole SDK performance or lifecycle acceptance gate.

The existing HTTP consumer also builds on installed stable
(`sdk-open-admission-http-consumer.log`). New production code pays for the
previously bypassed listing boundary and per-open budget ownership; shared
budget admission moved out of the semantic context instead of adding a second
implementation. No dependencies or lockfile entries changed in this increment.

### Repository-opening deadlines and public lifecycle target

The public owner now wraps repository opening in one scoped deadline/cancellation
driver. It cancels and awaits the handshake on caller cancellation, runtime
shutdown or configured duration expiry, including pending transaction listings.
The scope uses no detached timer task. Its temporary cancellation token is not
retained in successful repository handles. Error normalization recognizes typed
and wrapped cancellation while retaining real failures and typed close errors;
semantic-operation finishing uses the same normalization for expired deadlines.

Added the required `crates/crab-sdk/tests/lifecycle.rs` target. Its
`close_drains_dropped_operations` case polls a public open until a local HTTP
server receives the request, drops the operation, and checks client close and
connection release. `open_timeout_drains_pending_transport` verifies public
Timeout and transport release before client close. The target still needs its
named integrity-close and warm-cache cases; this is not full lifecycle acceptance.

Rust 1.91.0 proof:

- `sdk-open-deadline-before.log`: a transaction listing ignores a 50 ms owner
  duration and hits the test's 2 s outer timeout before the fix.
- `sdk-open-deadline-after.log`: all 19 repository tests pass. Deadline coverage
  blocks listing, manifest and locator acquisition; caller cancellation includes
  listing, and runtime shutdown now exercises both listing and manifest phases.
- `sdk-deadline-errors-retry.log`: all seven error tests pass, including wrapped
  cancellation, retained close errors and preserved non-cancellation failures.
- `sdk-open-lifecycle.log`: both public TCP lifecycle tests pass, zero ignored.
- `sdk-open-deadline-clippy.log`: scoped remote-git and remote SDK library lint
  passes. Formatting and diff checks pass.

One compiler process failed writing its incremental dependency graph with
ENOENT (`sdk-deadline-errors.log`). The process was confirmed terminal, the
mounted volume remained writable, queued builds completed, and the retry passed
without source workaround, dependency changes or artifact deletion. Initial
lifecycle test compilation was corrected to use the SDK's actual IntoFuture
contract and avoid requiring Debug on repository handles.

`sdk-open-deadline-content.log` passes the native Git/Crab/LFS fixture after
successful opening, proving the opening cancellation scope does not poison
later reads. `sdk-open-deadline-http-consumer.log` records a passing HTTP consumer
build on installed stable. The new lifecycle target also passes scoped lint
(`sdk-open-lifecycle-clippy.log`). Production growth implements the missing
whole-handshake scope and shared typed error normalization; most additional
repository/error lines are focused regressions. No public signature or
dependency changes were required.


### Locator and shallow-closure admission

Operation-scoped locator acquisition and subsequent catalog page reads now use
an admitted store sharing the operation cancellation token and budget. Failed
acquisition cancels that scope; successful acquisition retains it until operation
cleanup. Shallow-closure loading uses the same admission boundary. Synthetic
locator lookup and closure request/byte charges were removed to avoid charging
logical lookups as physical reads.

The public locator limit regression exposed SlateDB 0.15.0's unbounded retry of
`object_store::Error::Generic`. Admission rejection now uses its non-retryable
`NotSupported` envelope while preserving the typed storage source. Storage error
mapping and SDK metadata classification recover the original limit/cancellation
reason. No dependency patch was needed. The regression has an independent
one-second operation deadline and requires `LimitExceeded`, not timeout.

Current scoped evidence under the checkout-specific external target directory:

- `sdk-locator-storage-admission.log`: 11 admission tests pass.
- `sdk-locator-error-map.log`: 15 error mapping tests pass.
- `sdk-locator-budget-regressions.log`: nine remote Git budget regressions pass.
- `sdk-locator-visibility-batching.log`: existing visibility batching test passes
  with its 30-request ceiling unchanged.
- `sdk-locator-content-lifecycle-after.log`: five public SDK tests pass across
  lifecycle, native Git/Crab/LFS content and read-only indexing; three explicit
  publisher/live tests remain ignored in this invocation.
- `sdk-locator-admission-clippy.log`: scoped storage, remote Git and SDK library
  lint passes with content enabled, `--no-deps` and warnings denied.

The aggregate-fetch regression now applies its one-byte limit to the operation
being tested, after opening with default limits; its zero pack-read assertion
remains intact. Hydration tests find the exact minimum request/byte budget that
admits the raw pointer, then require hydration to fail under that same budget.
This accommodates actual catalog overhead without granting content-read headroom.
It is a behavioral budget regression, not a performance baseline.

This closes locator admission gaps only. Other Git reader retries, provider
pagination/internal retries and listing payload accounting remain to be audited.
Framing-corruption retry behavior and constructor cancellation/session cleanup
also remain separate proof obligations. Full SDK phase and performance gates
are not complete.


### Terminal response-framing errors across retry boundaries

The locked SlateDB 0.15.0 `RetryingObjectStore::get_opts` collects ranged bodies
inside its retry closure. Its default retry count is unbounded and `Generic`
errors are retryable. The admitted store previously encoded short/oversized
bodies as `Generic(CorruptObject)`, allowing deterministic malformed range bodies
to loop until an independent deadline or budget stopped them.

Framing errors now use the same terminal object-store envelope as admission
errors, retaining `StorageError::CorruptObject`. The facade maps the source back
and preserves its existing one-retry corruption policy; it does not inherit
SlateDB's unbounded policy. SDK metadata/object-store classification follows
preserved typed storage sources through I/O wrappers, so the envelope is exposed
as `Corruption`, not `UnsupportedCapability`.

`sdk-framing-terminal-before.log` records the failing short/oversized-body
regression before the envelope fix. `sdk-framing-terminal-after.log` records
12 passing storage admission tests, including exact two-attempt request and
advertised-byte accounting for a persistently short body. These tests exercise
the real Store facade and injected object-store payloads; the SlateDB retry
classification claim is supported by the locked dependency source, not a new
end-to-end malformed-catalog fixture. `sdk-framing-error-kind.log` records four
passing SDK error tests, including metadata/I/O corruption preservation and
existing admission/cleanup categories. This is scoped failure-path evidence;
full physical I/O and SDK qualification remain incomplete.

`sdk-framing-terminal-clippy.log` records passing scoped storage/SDK library
lint with content enabled, `--no-deps` and warnings denied. Workspace formatting
and diff whitespace checks pass. The added production source walk replaces the
admission-only lookup and preserves typed corruption without a new public API.


### Pack stream transport admission

Canonical pack and index/reverse-index artifact streams now use the operation's
admitted Store. Request admission runs for each facade header retry, and actual
advertised response bytes are reserved before payload consumption. Pack inventory
size remains an early budget check and an integrity check; it no longer supplies
the physical byte charge. No stream buffering or second download path was added.

`sdk-pack-stream-admission-before.log` shows the regression: an invalid inventory
claiming 32 bytes caused a 64-byte response to be charged as 32. After the fix,
`sdk-pack-stream-admission-after.log` records three passing stream tests, covering
actual response reservation and the existing incremental identity verifier.
`sdk-pack-stream-reuse.log` records five passing native integration tests for
canonical reuse, index/content corruption, response-limit preflight and blocked
download cancellation. Sidecar streams share the admitted Store boundary; their
existing per-artifact length checks and cleanup remain in place. No new dedicated
sidecar retry fixture was run in this increment.

Shared pack-entry and pack-index singleflight reads still use synthetic charges;
these remain an explicit physical-accounting gap. This increment does not prove
all Git read retries, global accounting or the full SDK performance gate.

Scoped remote Git library lint passes in `sdk-pack-stream-clippy-retry.log`
with incremental compilation disabled. The first run terminated with an external
incremental dependency-graph ENOENT; volume writability was confirmed before the
retry, and no artifacts were deleted. Formatting and whitespace checks pass.
Production changes replace manual stream charges with the existing admission
boundary; the main added lines are the response-size regression.


### Operation-owned range admission

Coalesced object ranges and packed-entry metadata reads now attach the existing
operation read-admission policy to their Store clone. The early known-range-size
check remains; request and response reservations occur at the actual storage
boundary, including facade retries. This replaces synthetic request/byte charges
without changing range coalescing, delta verification or decoding.

`sdk-range-admission-before-retry.log` records a failing regression: a missing
64-byte range consumed 64 fetched bytes despite never returning a response body.
`sdk-range-admission-after.log` passes with one request and zero advertised body
bytes charged. The first build attempt (`sdk-range-admission-before.log`) ended
with incremental dependency-graph ENOENT; the mounted target remained writable.
Subsequent compilation used `CARGO_INCREMENTAL=0`, without deleting artifacts.

`sdk-range-admission-batch.log` records three passing native tests: visibility
batch request bounds, selected delta bases sharing a coalesced range, and metadata
concurrency bounded by aggregate byte limits. `sdk-range-admission-budget.log`
records nine passing budget regressions from that newly built integration binary.
These checks preserve the existing assertions and ceilings.

Shared packed-entry and pack-index producers remain separate: their cancellation
scope can outlive an individual waiter, so attaching one waiter's cancellation
policy would break other callers. Their synthetic accounting has not yet been
replaced. Provider-internal retries/pages and full SDK qualification also remain
incomplete.

`sdk-range-admission-clippy.log` records passing scoped remote Git library lint
with warnings denied and incremental compilation disabled. Formatting and diff
whitespace checks pass. Production growth attaches the shared admission policy
and preserves preflight checks; no new public API or retry loop was added.


### Open defect: shared producer inherits the initiating operation budget

The concurrent native regression
`shared_blob_read_does_not_inherit_another_operations_budget_failure` holds one
cold base-blob GET open, starts a second read of the same pinned blob, then
releases the producer. The first operation allows one inflated byte; the second
uses default limits. Both currently receive the first operation's
`LimitExceeded { actual: 65547, maximum: 1 }`.
`sdk-shared-budget-diagnostic.log` records both typed errors. This is an open
correctness defect, not passing qualification or a supported behavior.

The owner is `RemoteGitReader::read_packed_entry`: its shared work closure
captures `flight_budget` from the initiating caller and charges inflation before
decode. `RemoteGitRuntime::read_packed_singleflight` shares that producer's error
with every waiter; its key separates object/decode limits but not independent
aggregate budgets. The source-chain test helper additionally skipped the error
inside `Arc<Error>`; it now traverses that explicit shared wrapper without
weakening the required error category.

The repair must move caller admission out of the single initiating closure.
Shared work must admit each participating operation independently, return budget
rejection only to that participant, and keep work alive for other admitted
waiters. Retry request/byte admission and pre-decode inflation must use the same
ownership model; late joiners must be admitted against work already reserved.
Cancellation must remove only the cancelled participant, while runtime shutdown
still drains the producer. Splitting flights by operation identity would avoid
the symptom by losing cross-operation coalescing and is not sufficient delivery.
The regression deliberately remains failing until this ownership change is
implemented and verified alongside the existing cancellation/coalescing cases.

`sdk-shared-budget-regression.log` confirms failure at the generous caller
assertion after the shared-error helper correction. The producer and both
operations are drained before that assertion. The test uses a blocked origin
and a bounded 100 ms window for the second waiter; it does not yet instrument
waiter registration directly. This failing regression is intentionally retained
as unfinished work and must pass before delivery.


### Shared packed-entry participant admission

The preceding shared-budget defect is repaired for packed-entry producers.
`budget/shared.rs` maintains independent participant budgets and rejection
channels. Request attempts, advertised response bytes and pre-decode allocation
are charged to participating operations; an exhausted participant receives its
own typed failure while admitted participants continue. Joining an existing
producer replays its prior reservations once per operation. A producer with no
admitted participants retires; fresh callers start a replacement, and retirement
checks producer identity so an older task cannot remove that replacement.

The reader no longer captures the initiating operation's inflation budget in
shared work. Its synthetic packed-entry request/byte charge and `charged_budget`
leader marker are removed. Shared origin cancellation remains runtime-owned;
each participant retains its operation cancellation scope. Runtime shutdown
continues to own producer draining. This does not change pack-index producers,
which still require the same admission treatment in a subsequent increment.

Evidence in the external checkout-specific target directory:

- `sdk-shared-admission-tests.log`: four focused library tests and three native
  integration tests pass, including independent rejection, late participants,
  retired admission and cancellation of one shared waiter.
- `sdk-shared-admission-budget.log`: ten budget tests pass. The formerly failing
  concurrent regression now additionally requires exactly one pack GET, proving
  the repair did not separate callers into duplicate origin reads.
- `sdk-shared-admission-flights.log`: five existing flight admission/cache tests
  pass from the current library test binary.
- `sdk-shared-admission-coalescing.log`: the existing 16-caller cold-read and
  warm-cache regression passes with its request assertions unchanged.
- `sdk-shared-admission-content.log`: five public SDK content/read/lifecycle
  tests pass; three explicit publisher/live targets remain ignored in this run.

This adds participant admission state because shared work must enforce distinct
operation limits without losing coalescing. It replaces the leader-only marker
and charges rather than retaining a compatibility path. Full SDK qualification,
pack-index admission, provider-internal request accounting and controlled
performance baselines remain incomplete. The earlier failing logs are retained
as before-fix evidence, not current failures.

`sdk-shared-admission-clippy.log` records passing remote Git and SDK library
lint with content enabled, warnings denied and `--no-deps`. Formatting and diff
whitespace checks pass. This is scoped lint and runtime proof, not a broad
workspace or cross-platform qualification result.


### Unified immutable-read flight admission

Pack-index HEAD and body reads now use participant admission for every facade
attempt and advertised response. The cached source size is a preflight bound,
not a synthetic fetched-byte charge. The before-fix missing-index regression
reserved 64 body bytes without receiving a body (`sdk-index-admission-before.log`);
it now reserves zero (`sdk-index-admission-after.log`).

Packed-entry, index-body and index-size flights now share `runtime/read_flight.rs`.
This removes three separate join/wait/retire loops while retaining independent
participant rejection, runtime-owned producer cancellation, bounded producer
admission and identity-checked retirement. Index wrappers retain their owner
responsibilities: check source-size limits, recheck caches after admission and
publish caches before retiring a producer. Generated-pack publication work keeps
its distinct existing orchestration; this refactor covers immutable reads.

`sdk-index-admission-flights.log` records five passing existing flight/cache
checks. `sdk-index-admission-budget.log` records ten passing native budget
regressions, including independent shared caller budgets with exactly one pack
GET. `sdk-index-admission-pack-reuse.log` records five passing canonical pack
reuse, corruption, preflight-limit and cancellation tests. Test signatures now
supply operation budgets to the private shared-read entry points; existing cache,
request and correctness assertions remain unchanged.

The reader no longer contains manual StorageRequests/FetchedBytes charges;
its network reads pass through operation-owned or shared Store admission.
This establishes the facade boundary, not provider HTTP accounting: provider
internal retries, pagination and listing payloads remain outside that hook.
Global performance qualification and the remaining SDK phases are incomplete.

`sdk-index-admission-content.log` records five passing public SDK tests across
content, read-only indexing and lifecycle cleanup; three explicit live/publisher
tests remain ignored in that invocation. `sdk-index-admission-clippy.log` records
passing scoped remote Git/SDK library lint with content enabled, warnings denied
and `--no-deps`. Formatting and diff whitespace checks pass. No dependencies or
public signatures changed; the new shared flight module replaces duplicated
coordination loops rather than adding another execution path.


### Independently runnable phase-one read contracts

The required `remote_read` target now contains `snapshot_stays_pinned`,
`raw_and_hydrated_bytes_are_distinct` (content feature), and
`byte_paths_round_trip` (Unix), alongside its existing read-only indexing case.
The required `lifecycle` target now contains `warm_cache_respects_limits`.
These are independent public SDK scenarios, not aliases for the large smoke
test. Their shared fixture creates two real Git commits, publishes immutable
packs and locator metadata, removes the source checkout before SDK opening,
and advances publication only through fixture-owned metadata operations.

Git command isolation, catalog publication and copying of closed published
objects were extracted from the existing native smoke fixture into shared test
support. The larger native Git/Crab/LFS smoke still exercises its own broader
behavior and optional real-backend path. The focused hydration case checks LFS
pointer bytes against exact reconstructed content; broader Crab coverage remains
in the native content fixture.

`sdk-named-read-contracts.log` records nine passing tests with content enabled
across `remote_read`, `lifecycle` and the native smoke target, with three explicit
publisher/live tests ignored. `sdk-named-read-remote-only.log` records eight
passing tests without content (two explicit publisher/live tests ignored).
`sdk-named-read-clippy-after.log` records passing scoped lint for all three test
targets with content enabled and warnings denied. Redundant Copy clones and an
invalid-range literal representation were cleaned up without changing test
inputs or assertions; no production behavior or dependency changed.

The required `stream_close_reports_integrity_failure` named case remains open.
Existing EOF-corruption checks and ordinary early-close checks do not by
themselves prove that an unobserved terminal integrity error is returned by
explicit close. A deterministic fault/completion fixture is still required;
a sleep-based assertion or relabeling the broad smoke test is not completion.
Real-backend and full performance qualification remain separate open gates.


### Explicit close preserves an unobserved integrity failure

The required `stream_close_reports_integrity_failure` lifecycle case now runs
through the public SDK against a real Git/LFS filesystem fixture. It replaces
LFS bytes with same-size corrupted content, consumes only nonterminal chunks,
and synchronizes on the owner's structured error-completion span field. The
owner records this field after retaining the semantic result and closing the
locator session. A bounded test barrier holds completion after that record until
the consumer stops polling, so explicit stream close must return `Corruption`.
Client close then succeeds, proving the already-observed failure is not reported
again as abandoned work. No sleep-based completion assumption or production test
hook was added.

`sdk-stream-close-contracts.log` records ten passing tests across lifecycle,
remote reads and the existing native Git/Crab/LFS smoke, with three explicit
publisher/live cases ignored. The initial focused run is recorded in
`sdk-stream-close-integrity.log`; the combined run uses the final span-field
observer rather than a debug event. The test-only tracing dependencies reuse
versions already locked in the workspace. A before/after lockfile comparison
shows only the SDK's two dependency links added, with no version or checksum
changes. `sdk-stream-close-features.json` verifies unchanged normal dependency
counts (38 default, 411 remote, 438 content) and no forbidden dependency edges.

Public close documentation and the plan now distinguish cleanup from full-file
verification: early close cancels unread work and reports unobserved finalization
errors; successful EOF is required to claim full-file integrity. This clarifies
existing behavior rather than changing delivery or cancellation semantics.
All named phase-one read/lifecycle cases now exist on their applicable feature
and platform configurations. This is not full phase-one acceptance: broader
lifecycle, real-backend, memory and performance qualification remain open.

Final span-field observer validation: `sdk-stream-close-clippy-final.log` records
successful scoped lifecycle clippy with warnings denied. Workspace formatting
and `git diff --check` also pass.

The next unresolved lifecycle boundary is the last waiter of an immutable read
flight. Source inspection shows `ReadFlights::run` uses runtime shutdown tokens
for producers, while caller cancellation exits its wait loop. `SharedBudget`
checks participant cancellation when charging subsequent work, but has no
participant-drop notification while origin I/O is pending. The existing
`cancelling_one_cold_waiter_does_not_cancel_shared_origin_work` regression proves
survivor behavior; it does not establish last-waiter cleanup. A deterministic
pending-origin reproduction and a fix preserving survivor coalescing remain
required before claiming the plan's no-owned-tasks acceptance condition.

### Last-waiter ownership of immutable read producers

A deterministic pending-producer regression reproduced the lifecycle gap:
`last_waiter_stops_pending_producer` failed before the ownership change because
dropping its only reader left the producer running until runtime shutdown.
The reproduction is recorded in `sdk-last-waiter-before.log`.

`SharedBudget` now issues a lease for every waiter. An atomic transition closes
admission when the last lease leaves; cancellation uses a child token, preserving
the runtime and unrelated flights. `ReadFlights` retains the initial lease before
spawning, releases it on every exit, and joins terminal cleanup for the last live
caller. A dropped future cancels through the lease guard while the runtime keeps
the task tracked. The producer releases admission before publishing completion.
A concurrently completed semantic failure remains observable during cancellation.

The same owner covers packed entries, pack-index bodies and index-size HEADs.
All three reader closures use its cancellation token for origin admission and
pending storage calls. Generated publication packs retain their separate
cross-process producer-lease contract and do not use this immutable-read owner.
Tokio-util's child-token contract explicitly guarantees cancellation does not
propagate to the parent; Tokio watch sender closure identifies departed budget
participants. No dependency versions or public API types changed.

Departed participants stop accruing work. A per-participant ledger reserves
missed work when an operation rejoins, without double charging prior work.
Aggregate reservations precede awaits on participant locks, so interrupted
admission cannot leave replay totals behind a participant's ledger. Unchanged
reservations avoid taking the operation budget lock.

`sdk-last-waiter-final-contracts.log` records eight unit and three native Git
integration tests passing before the final interrupted-admission ledger check.
The native cases prove zero active pack calls and object flights before runtime
shutdown for the last cancelled reader, successful surviving-reader bytes, and
independent budget rejection with one shared origin read. The test backend's
active-call counter now uses a drop guard so cancellation is measured correctly.
The subsequent `sdk-last-waiter-ledger-contracts.log` records nine unit and three
native integration cases passing, including interrupted admission followed by
rejoining and continued charging. That run began before rebasing onto
`a371fb7d002`; it remains pre-rebase evidence.

`sdk-post-rebase-read-contracts.log` records the public SDK consumer run on the
rebased source: ten passing lifecycle, remote-read and native Git/Crab/LFS cases,
with three explicit publisher/live cases ignored. These results do not establish
complete phase-one, cross-platform or performance qualification.


### Post-rebase lint and LFS extension delivery

`sdk-post-rebase-owner-clippy-after.log` records successful owner lint, including
unit-test bodies, with warnings denied. The initial run identified an
`err_expect` lint in the new opening test; an explicit result match preserves
its rejection assertion without requiring a Debug implementation on successful
repository handles. `sdk-workspace-inheritance-features.json` confirms unchanged
normal dependency counts (38 default, 411 remote, 438 content) after centralizing
the SDK's internal dependencies through workspace declarations.

The public fixture now includes a valid LFS pointer with an extension alongside
ordinary Git and standard LFS files. `sdk-lfs-extension-contracts.log` records
six remote-read and four lifecycle tests passing, with one explicit fixture
publisher ignored. The new case verifies raw pointer access, typed rejection of
full/ranged hydration, and archive failure without Data or EndEntry for the
extension-bearing file. This exercises Git classification, content dispatch,
error mapping and cleanup rather than only the pointer validator. No extension
transform implementation or production fallback was added.

### Post-rebase RustFS and read-only qualification

`sdk-content-all-targets-clippy.log` records successful SDK content lint across
all targets with warnings denied. `sdk-live-read-build.log` and its JSON artifact
stream record a successful build of both public examples and the remote-blob
integration executable from the current worktree using Rust 1.91.0.

`sdk-post-rebase-rustfs-smoke.log` records the real-storage
`s3_pack_reads_survive_source_removal_and_keep_snapshots_pinned` test passing.
It exercises source-checkout removal, pinned snapshots, Git/Crab/LFS reads and
archives, including public example subprocesses. The dedicated qualification
bucket was empty before and after the run; the persistent large fixture was
not removed.

`sdk-pr160-read-only-result.json` records three verified public `remote_read`
invocations against the persistent RustFS fixture: 4 KiB ordinary Git, 1 GiB
hydrated Crab with a fresh local cache, and 32 MiB hydrated LFS. Each returned
the expected pinned commit, byte count and BLAKE3 digest specified in
`sdk-pr160-read-only-cases.json`. The rejecting proxy observed 154 GET requests,
zero write attempts and zero proxy failures, transferring 1,109,411,359 response
body bytes. The executable SHA-256 was
`3052dd923cb846bc9db0667b21e070254c8f23e319f76c744853503d1d151667`.
These are native debug correctness results from the worktree, not a controlled
Linux performance comparison or evidence for GCS/Azure and other OS cells.

### CI cancellation outcome correction

PR 160's Rust suite exposed a failure in
`cancellation_and_drop_release_destination_during_pending_source`. The pinned
Xet 1.6.0 `FileReconstructor::run` explicitly returns `Ok(0)` for cancellation
when its run state contains no error. The shared hydrator previously checked
cancellation only after an upstream error, allowing this successful upstream
return to become a hash or size mismatch. It now checks cancellation after
upstream success and output-owner cleanup, before integrity validation. Existing
writer-error handling retains precedence on the error path. Full and ranged
reconstruction share this adapter, including SDK and VFS consumers.

`sdk-hydrator-cancel-ci-fix.log` records all 36 hydrator tests passing, including
the failing CI case, concurrent failure isolation, source-reported cancellation,
pending cache cleanup and writer/final-flush error preservation. This is focused
local proof; the full CI rerun remains outstanding.

The Git-current compatibility artifact from run 34146375765 fails
`filtered-transfer-smaller`: both full and filtered transfer counters are zero.
Its earlier lifecycle checks pass. This telemetry failure remains under
investigation; it must not be dismissed as an unrelated baseline failure without
source and baseline proof.

The retained Git-current logs contain nonzero `operation_summary` counters:
the full-clone command totals 245,266 fetched bytes and the filtered-clone command
92,851. The protocol collector currently reads only `storage_request` events,
while the large-repository collector also consumes summaries. This establishes
an observer mismatch, not yet a corrected performance result: shared-flight
participant charging and sidecar events need an accounting review before using
summed operation budgets as physical transfer measurements.

`sdk-hydrator-cancel-ci-clippy-after.log` records successful crab-read library
and test lint with warnings denied. The initial lint run found an intentionally
invalid range literal and a complex test cache tuple; explicit range fields and
a local tuple type name preserve both tests' behavior. Workspace formatting and
diff whitespace checks pass.

### Producer transfer observations

Completed range reads now emit byte observations inside the producer, after
the Store facade returns bytes and before caller cancellation or decoding.
Packed-entry and index shared flights emit once; cache hits and budget replay
emit nothing. Completed pack and sidecar streams use the same observation
target. This measures delivered pack/index payload at the reader boundary,
not HTTP framing, hidden provider retries, metadata traffic or partial failed
streams. The read-only proxy remains the independent wire-level check.

`sdk-shared-read-telemetry-test.log` records a real Git fixture regression:
eight concurrent callers perform one backend pack read and emit one observation;
a subsequent warm read emits none. `sdk-shared-read-telemetry-final.log` repeats
that passing regression after the stream-observation edits, and
`sdk-read-telemetry-final-clippy.log` records successful owner lint across library
and test targets. The range regression does not independently prove stream
observation counts.

The large-repository collector enables the scoped storage target and no longer
adds operation budget reservations to transfer totals. It retains semantic work
and timing summaries. `sdk-transfer-collector-tests.log` records 39 passing
qualification tests, including both collectors counting one 123-byte read once
despite eight participant summaries. These changes restore measurement rather
than relaxing the filtered-transfer assertion; a fresh real lifecycle run is
still required before the CI failure is resolved.

### Fresh native protocol lifecycle

`protocol-lifecycle/sdk-pr160-telemetry-4/artifacts/report.json` records all 145
checks passing, including mirror cancellation of a real pack child, receipt
replay, metadata-change rejection, exact Crab/LFS reconstruction and the final
filtered-transfer comparison. Delivered pack/index bytes were 206,754 for the
full clone and 28,095 for the filtered clone. These are reader-boundary transfer
observations with the exclusions described above.

This run used macOS, Apple Git 2.50.1 and task-local AWS CLI 1.46.1, matching the
CLI version in the retained CI report. The debug Crab binary was built with
default features plus `gix-transport` using Rust 1.97.0. Its SHA-256 was
`ae6eba4040b2208f80f773c29adb294f895426b7136f23479cf80ef675068407`.
The source was HEAD `814d74b3827f24213b036c0e520ebed05783a540` plus the frozen
patch recorded in `sdk-protocol-source.patch` and fingerprinted by
`sdk-protocol-source.json`; the report explicitly records a dirty worktree.
`sdk-protocol-cli-build.log` records the successful build and a macOS debug
unwind-table linker warning. This is not release-mode performance qualification.

Earlier attempts remain retained: run 1 used a non-private host cache; run 2's
system AWS CLI lacked conditional PUT support; run 3's runner override disabled
the isolated Git pack hook. Run 4 used a private external cache, the verified
task-local AWS CLI and the fixture's XDG Git hook configuration. Neither product
assertions nor expected results were weakened. A fresh CI run on committed
source, including the other Git versions and release artifact, remains required.

### Receipt lookup identity

Publication-owner auditing found that the persisted-receipt fast path validated
the receipt's own plan ID and history without binding it to the requested key.
Copying a valid receipt between plan keys in the same repository could therefore
return another operation's commit. Mirror's caller compares ref edits but that
does not distinguish different operations with equal edits. The intent-list path
already checks key/body identity; this correction applies the same invariant to
the terminal receipt lookup before following its intent.

`sdk-receipt-key-storage-before.log` records the new regression failing because
the lookup returned the copied receipt. `sdk-receipt-key-after.log` records all
13 receipt tests passing after the fix, including historical recovery after
compaction, managed receipt recovery and repository isolation. The first command
without the storage feature executed zero matching tests and is not proof.
No persisted keys, versions or fields changed. This fix follows the successful
protocol lifecycle build above, so that binary does not contain it; consumer
and CI validation on the combined source remain required.

The storage-only lint run exposed an unused catalog-read type and a nonminimal
visibility condition. The type and its import now use `remote-index`, matching
their only constructor and public export; the condition uses the equivalent
`is_none_or` form. `sdk-receipt-key-clippy-final.log` records successful library
and test lint with warnings denied, and `sdk-visibility-storage-tests.log`
records all 27 storage-feature visibility tests passing. No warning suppression
or expected-result changes were used. `sdk-visibility-remote-index-tests.log`
records all 33 catalog-enabled visibility tests passing, including lazy catalog
readers and journal handoff.

`sdk-combined-owner-read-tests.log` records the public SDK tests after the receipt
and visibility fixes: four lifecycle and six remote-read cases pass; the fixture
publisher remains explicitly ignored. The added production code is limited to
receipt identity rejection, cancellation outcome mapping and producer telemetry;
the larger diff is regression coverage and qualification evidence.

### Read-only historical receipt proof

The metadata owner now separates proof lookup from receipt repair. A single
lookup validates a persisted receipt or proves a committed intent through the
existing history rules. `read_plan_receipt` returns that proof without invoking
the writer; `resolve_plan_receipt` retains its authorized repair behavior and
avoids rewriting an already persisted receipt. Missing evidence is explicitly
documented as insufficient to prove rejection.

`sdk-read-only-receipt-contracts.log` records 13 passing receipt tests. The
compaction case proves read-only recovery leaves the terminal receipt absent,
then the repairing API recreates it. Both APIs reject a copied receipt under a
different plan key and return no proof for an unrelated plan with matching refs.
`sdk-read-only-receipt-clippy.log` records successful storage-feature owner lint
with warnings denied. The additional owner code distinguishes persisted proof
from committed intent so callers share validation without duplicated reads or
implicit writes. This is publication-owner preparation, not a qualified SDK
mutation/reconciliation surface; operation serialization and lifecycle extraction
remain unfinished.

Publication attribution types now use `PlanCommit`, `PlanIntent`, `PlanReceipt`
and journal plan attribution; metadata, journal, CLI/mirror and protected-receive
consumers move together without compatibility aliases. Version-one serialized
fields, authority tags, persistent paths and digest domains are unchanged.
`sdk-publication-types-tests.log` records 14 passing receipt tests, including
explicit direct/manifest serialization contracts, and
`sdk-publication-journal-tests.log` records all ten journal tests passing.
`sdk-publication-consumer-check.log` records a successful combined CLI and
protected-receive test-target compile check with `gix-transport` on Rust 1.97.0.
Existing fixture warnings remain; these changes do not complete the shared
publication lifecycle.

### Publication extraction evidence map

This map tracks current phase-two extraction. A shared implementation or narrow
regression does not establish full backend qualification.

| Boundary | Current evidence | Remaining extraction or proof |
| --- | --- | --- |
| Operation identity | Metadata owns canonical request hashing and nonce-bound plan identity; mirror-plan format 2 uses those primitives. Minimal-feature Rust 1.91 digest tests and all 26 mirror reconciliation tests pass. | Assemble the SDK recovery token from the complete validated request, verify placement on resume, and qualify payload mismatch through execution. |
| Operation admission | Direct mirror plans acquire their operation lease before refs and refuse existing receipts or intents. All 26 native-push tests pass for that admission change. | Complete shared CLI lifecycle extraction and same-token concurrent execution fault qualification. |
| Renewal outcome | Publication and namespace gates share `RenewingPushLock`; their callback outcome survives late renewal or cleanup failure. Six publication and ten journal tests pass. | Complete SDK-owned mutation cleanup and unobserved-error reporting. |
| HTTP publication | Default-branch and ordinary receive share lease/GC admission and preserve known commitment when read readiness is pending. Focused marker/readiness and GC-exclusion regressions pass. | Native acknowledgements do not provide durable direct-plan receipts; complete live-backend fault qualification and SDK transport wiring. |
| GC retention | The combined test proves receipt recovery after compaction, later refs, destructive repository GC and fresh-process reopening. Five adjacent reachability tests pass. | Referenced Git/Crab/LFS content, live-backend and process-kill qualification remain separate requirements. |

The canonical operation order remains authorization, plan admission, sorted ref
leases, global/repository GC fences, dependency publication, journal commitment,
read readiness and drained cleanup. No complete SDK write capability is qualified.

### Shared publication lease owner

`crab-remote` now has an opt-in `publication` feature owning sorted ref leases,
global/repository GC writer fences and awaited cleanup. HTTP receive calls this
owner and retains authorization, request validation, immutable uploads, journal
commit and readiness. The former HTTP ref-lease implementation is removed;
default-branch publication also uses the shared ref-lease owner. Its manifest
lease and readiness outcome handling still need extraction. CLI admission has
additional wait/successor/reclamation policy and has not moved yet.

The owner preserves the callback's recorded result after renewal failure and
cleanup. A scoped cancellation token isolates renewal failure from unrelated
parent work. A Tokio cancellation drop guard stops an abandoned ref owner's
renewal worker; normal operation must still await release. This does not provide
the SDK's complete operation lifetime or typed mutation-outcome contract.

Current local proof, using Rust 1.97.0 and external-volume build artifacts:

- `sdk-publication-lease-tests.log`: four in-memory coordination tests pass,
  covering competing ref/GC admission, partial admission cleanup, renewal loss
  and abandoned ref-owner cleanup.
- `sdk-publication-lease-clippy.log`: all targets/features pass with warnings
  denied.
- `sdk-http-publication-caller-tests.log`: three existing receive tests pass
  through real native Git/HTTP and injected marker faults; two RustFS cases are
  explicitly ignored. The final run puts generated checkouts on the workspace
  volume through `TMPDIR`.
- `sdk-http-branch-publication-tests.log`: the browser branch/default-branch
  publication regression passes, including native Git visibility.
- `sdk-publication-minimal-check.log` and `sdk-publication-msrv-check.log`:
  default and publication features compile on Rust 1.91.0.

The lockfile adds only the internal package and HTTP dependency edge, without
dependency upgrades. Architecture-policy registration remains unapplied; the new
crate and HTTP edge require registration alongside the pending SDK policy work.
No local/process feature or SDK write capability is introduced. Operation-token
identity/serialization, full CLI/HTTP lifecycle unification, truthful readiness
outcomes and combined compaction/GC/fresh-process recovery remain required.

### Default-branch fencing and internal lease ownership

The browser default-branch regression now holds each GC sweep fence while
attempting a HEAD change. Against `4c5d1428d83`, the global-fence case fails:
the response reports failure but journal HEAD has already changed to
`refs/heads/feature/browser`. `sdk-default-branch-gc-before.log` records that
failure. The shared owner path passes the same regression for global and
repository fences, then successfully publishes after their release
(`sdk-default-branch-gc-after.log`). This proves the previous response error did
not imply rejection and that admission must precede the journal mutation.

Default-branch publication now holds sorted ref and GC writer admission through
read-generation reopening. Its manifest lease uses `with_internal_lease` and
releases before generation maintenance reacquires the manifest lock. Both ref
and internal resources use one private renewal worker; the public standalone
`RefLease` API is removed. Internal renewal failure signals the callback's token
without replacing its recorded result during release. This adds 23 net Rust
production lines to share internal-resource ownership and remove caller cleanup.

`sdk-internal-publication-lease-tests.log` records five owner tests passing,
including callback-result preservation after internal lease loss. This is
coordination-level proof, not an actual SDK commit fault test.
`sdk-default-branch-sibling-receive-tests.log` records the three ordinary receive
tests passing, with two RustFS cases explicitly ignored.
`sdk-default-branch-publication-clippy.log` records successful owner and HTTP
library/test lint with warnings denied. The owner minimum-version check is
recorded separately in `sdk-internal-publication-msrv-check.log`.

Readiness errors after known journal commitment still need a typed outcome;
this extraction does not yet resolve that caller contract. Direct operation
identity, replay exclusion, CLI extraction and combined historical GC proof
remain outstanding.

### Accepted publication with pending read readiness

The shared owner now exposes `Readiness<Generation>` and `finish_committed`.
After acknowledged commitment (or a validated no-op), failure or cancellation
of read-generation work produces `Pending`; the original error is handled and
logged at that boundary. Pre-commit validation and uncertain marker failures
continue to propagate. Both ordinary receive and default-branch publication use
the same completion policy while retaining their existing fence scopes.

The dependency contract is [Git's report-status protocol](https://git-scm.com/docs/gitprotocol-pack):
success reports an accepted reference update. Derived Crab read-index readiness
is a separate concern. The local `receive_wire::report` documentation now states
that distinction. HTTP publication results acknowledge refs; read endpoints
independently report unavailable indexing. Browser branch/content responses
already identify the accepted branch and commit rather than promising readiness;
release and merge application workflows retain their own additional work.

The fault adapter now fails manifest reads only after successful visibility-marker
publication. Against `75d6439708c`, the caller test returns 503 instead of the
expected successful acknowledgement (`sdk-committed-readiness-before.log`). With
the shared completion policy, all five fault cases pass: lost marker reply,
rejected marker, cancellation after a prepared head, failed readiness after the
marker, and cancellation after the marker. Known commits are acknowledged;
unproven attempts do not emit per-ref rejection. The fixture checks persisted
refs, released GC admission and exact content after a fresh server starts.

Verification logs under the external target directory:

- `sdk-committed-readiness-after.log`: the five-case fault regression passes.
- `sdk-committed-readiness-receive-tests.log`: three receive tests pass, covering
  native push and browser file mutation; two RustFS cases remain ignored.
- `sdk-committed-readiness-browser-tests.log`: six browser-related tests pass,
  including branch/default-branch and release publication; the manual identity
  provider fixture remains ignored.
- `sdk-committed-readiness-merge-tests.log`: canonical merge publication passes.
- `sdk-committed-readiness-clippy.log`: owner and HTTP library/test lint passes
  with warnings denied. Minimum-version proof is recorded separately in
  `sdk-committed-readiness-msrv-check.log`.

This resolves the audited HTTP post-commit readiness error path. It does not
complete the SDK mutation outcome API, durable direct-operation identity,
same-token replay exclusion, CLI extraction or historical GC qualification.

### CI telemetry qualification gap

On `6f2b02e477e`, the Linux protocol job failed
`storage_telemetry_counts_shared_reads_once_and_excludes_warm_hits`: the backend
recorded one pack read, but the scoped tracing collector observed zero events
instead of one. The other 78 remote-repository tests passed. Earlier local
telemetry success therefore does not establish reliable cross-platform evidence;
the event-delivery failure remains under investigation. See the
[failed protocol job](https://github.com/crabbuild/crab/actions/runs/34155830082/job/101847413227).

The same head also failed the architecture dependency/feature policy check.
The pending inventory approval described above remains required; neither failure
is waived by keeping the PR in draft.

Local reproduction on `16c96b4e859` gives the same 78-pass/one-failure result;
the telemetry test passes when run alone. A separate 19-line reproduction with
no Crab calls also loses the event: install one scoped subscriber, first use a
callsite on another thread without that subscriber, then emit on the subscribed
thread. `tracing-core` 0.1.36's single-dispatcher callsite registration consults
the registering thread's dispatcher and caches its lack of interest. Diagnostics
confirm the Crab producer uses the correct subscriber thread and dispatcher;
thread migration is not the cause of this failure.

The external logs `sdk-telemetry-ci-reproduction.log`,
`sdk-telemetry-isolated-reproduction.log`,
`sdk-telemetry-dispatch-diagnostic.log` and
`sdk-telemetry-minimal-reproduction.log` retain that evidence. Temporary diagnostic
code was removed. A global-subscriber harness correction with thread-filtered
collection and unchanged assertions is prepared for approval; no dependency patch
or production tracing workaround has been applied.

### CLI direct-plan admission

Native mirror pushes now enter the shared internal lease scope for their plan ID
before discovery and ref admission. Protected publication retains its server
authority boundary. A pre-acquired ref-lease handoff with a direct plan is rejected
after draining those leases, because accepting it would invert admission order.
The production CLI caller that hands off ref leases does not supply a mirror plan
ID; the mirror remote-helper caller supplies no pre-acquired leases.

`sdk-cli-plan-lease-siblings.log` records all 25 native-push tests passing, including
real Git multi-ref publication after a competing plan lease is released, rejection
before any ref lease is claimed, early-return cleanup, staging contention and
ordinary publication. `sdk-cli-plan-lease-check.log` records CLI test-target
compilation. The lockfile adds only the internal CLI-to-publication dependency.

This serializes active direct mirror executions. It does not establish durable
SDK request identity or make replay after an uncertain attempt safe; those gates
remain unfinished.

`sdk-publication-parent-cancel-tests.log` records six publication-owner tests
passing. The added parent-cancellation test pauses callback cleanup under nested
plan/ref scopes, verifies the plan and ref leases reject competing acquisition
and both GC domains reject a sweep, then permits cleanup and verifies every
resource is released before `Cancelled` returns. This proves cooperative drain
ordering; it does not simulate process death or prove post-expiry replay safety.

### Fresh-process historical receipt recovery

The receipt compaction test now commits an attributed transaction, removes its
terminal receipt, compacts, advances the same ref and compacts again. It exports
the resulting storage objects and starts a fresh test process to perform the
public read-only lookup. The parent deserializes the child's returned receipt,
checks its original transaction identity, and verifies that the missing receipt
was not repaired. The existing same-process lookup and authorized repair checks
remain in place.

`sdk-receipt-fresh-process-siblings.log` records all 14 receipt tests passing on
Rust 1.91.0 with the storage feature. Publication uses the existing in-memory
conditional-write backend; the fresh reader uses a filesystem copy of the final
objects because `object_store`'s filesystem adapter rejects update-mode writes.
This is fresh-process recovery evidence after two compactions and a ref advance,
not live-backend publication, process-kill fault injection, or scoped GC proof.

### Failed read measurements

The five-trial facade comparison now has an initial shared-core executable,
`crates/crab-sdk/examples/core_crab_read.rs`, gated on `content`. It compiles
with Rust 1.91.0 (`sdk-core-baseline-check.log`) and reconstructs the retained
1 GiB RustFS fixture correctly, but has not been used for the five-trial comparison. It accepts
the measurement runner's arguments and currently requires a Crab pointer.
The tracked `remote_read` example enters `Client::open_remote`, resolves a snapshot,
opens a logical file, consumes its complete stream into a Blake3 hash, closes
the stream, and closes the client. A baseline must include equivalent Git
snapshot/pointer lookup and cleanup, not only time the reconstruction call.
For Crab content the current SDK composes `ReadRuntimeBuilder`, a 512 MiB cache,
the snapshot's shard index and generation, operation-scoped read admission,
file-index lookup, and `reconstruct_to_writer_with_cancel`. Both paths must
use the same bounds and storage/cache conditions; the baseline may deliver
directly to its hashing writer to measure the facade's channel overhead.
Neither the single-run measurement script nor a repetition wrapper alone
proves the required request/byte or wall-time regression threshold. Measure
origin traffic independently for each path and retain both failed and
successful trials before computing separate cold/warm medians.

`sdk-core-baseline-linux-build.log` records its Linux release build.
`sdk-core-baseline-cold.json` records one fresh-process read using a new cache
on the container's native overlay filesystem: 1,073,741,824 verified bytes,
7.964 seconds, and 406,122,496 bytes peak RSS. The report binds the executable
SHA-256 and exact expected commit/content digest. This is Linux ARM64 diagnostic
evidence, not the required Linux x86_64 release profile or a five-trial median;
it does not yet include independently counted origin requests and bytes.

### Paired shared-core and SDK reads on Linux ARM64

Source `7c307c3fdc67afbaf7ee01b526b2496f4ba3af20`, Rust 1.91.1,
Python 3.11.2. `sdk-paired-read-build.log` records both release examples built
together. `paired-read-7wwq_0iy/` retains all 20 individual measurement reports,
stdout/stderr and `summary.json`; `sdk-paired-read-run.log` retains execution
progress. Five trials each run core and SDK in alternating order, with separate
new native-overlay cache directories and an immediate reused-cache read.
One read-only proxy counts origin traffic independently for all runs, with
stable endpoint identity so reopening a cache does not change its namespace.

| Median | Core cold | SDK cold | Core reused cache | SDK reused cache |
| --- | ---: | ---: | ---: | ---: |
| Wall seconds | 7.368 | 7.288 | 7.470 | 7.387 |
| Origin requests | 63 | 63 | 63 | 63 |
| Origin response bytes | 1,075,802,004 | 1,075,802,004 | 1,075,802,004 | 1,075,802,004 |
| Peak RSS bytes | 369,532,928 | 429,973,504 | 429,182,976 | 431,591,424 |

Every run verified the exact 1 GiB result and commit, with no observed writes or
origin failures. Maximum RSS across all runs was 434,032,640 bytes. The observed
wall-time/request/byte ratios are within the 10% facade threshold; cold SDK
median RSS was 16.4% above core, while remaining below the absolute 512 MiB bar.
Reusing the application cache did not reduce origin traffic in this experiment;
do not claim cache effectiveness from these measurements. Host OS caches were
not flushed, and this ARM64 diagnostic does not qualify the required x86_64
release profile or replace the complete qualification runner.

The measurement process completed all 20 reads but its final source-SHA lookup
failed because the container cannot resolve the host worktree's Git pointer.
The aggregate was reconstructed from the retained reports using the host SHA
recorded during the run, without rerunning or dropping trials. A permanent
runner must capture provenance before measurement and accept the host checkout
identity explicitly when its Git directory is unavailable inside the container.

Read-only inspection of the ten retained cache catalogs found 6–8 decoded-range
entries per cache, totaling 336,285,788–470,380,644 bytes, with no outstanding
leases or reservations. Both example paths therefore populate the decoded
cache. `ReadRuntimeBuilder::build` attaches that cache to the reconstruction
client; `CrabRangeCache::put` reserves catalog capacity and maintains the budget
after writes. The 512 MiB budget cannot retain the full 1 GiB working set.
These observations make eviction a plausible cause of unchanged repeated-read
traffic, but do not prove the cause of every miss. A larger-cache control is
still required before changing cache policy or claiming warm-cache efficiency.

The subsequent larger-cache control is retained in
`large-cache-control-tdt7dn7i/` and `sdk-large-cache-control.log`. Both examples
now accept an optional trailing `CACHE_BYTES`, forwarded by the measurement
runner's `--cache-bytes`; omission retains the 512 MiB example budget. This
exercises the existing SDK cache-budget API and changes no product cache policy.
With 2 GiB budgets and new separate caches, all four reads verified. Each cold
read made 63 requests and transferred 1,075,802,004 bytes; each reused-cache read
made 45 requests and transferred only 25,508 bytes. Core times were 8.263/1.785
seconds (cold/reused), SDK times 7.397/1.175 seconds. All four stayed below
512 MiB RSS. This supports capacity-driven eviction as the explanation for the
earlier whole-file misses, rather than an unattached SDK cache. It is one
control pair per implementation, not a five-trial performance claim. Reports
retain executable hashes because this control used the cache-argument changes
before committing them.

The isolated Linux read measurement runner now emits `terminal_state` and retains
timing, RSS and partial output when its child times out. A timeout has no exit
code, remains unverified even if a complete success line was emitted, and makes
the runner exit unsuccessfully. Python's `subprocess.run` kills and waits for the
child before raising `TimeoutExpired`, so usage is sampled after that cleanup.
The timeout regression and real-process success/mismatch test pass in the
dedicated Linux container. This fixes failed-run evidence retention; it does not
implement the five-run cold/warm baseline comparison or complete qualification.

An executable launch failure also produces a failed report: `spawn_failed`, no
exit code, empty child output, and `launch_error` containing the OS errno and
message. The executable hash and requested result remain available for diagnosis.
A real invalid executable image test passes alongside the timeout and
success/mismatch cases in the dedicated Linux container (three tests total).

### Shared renewing push-lease ownership

`crab-coordination::RenewingPushLock` now owns the renewal worker and awaited
release used by both the publication owner and `crab-write`'s namespace gate.
The namespace gate retains acquisition/retry policy and its callback outcome;
the publication owner retains ref and GC admission order. Both callers drain
the same lease implementation after the operation finishes, including when
renewal fails after a known commit. This removes the namespace outcome slot and
its impossible missing-result branch without introducing a dependency edge.

`sdk-shared-renewing-lease-journal.log` records all ten journal tests passing,
including late lease loss after real commitment and concurrent ref-name
conflicts. `sdk-shared-renewing-lease-owner.log` records all six publication-owner
tests passing, including abandonment and cancellation-drain ordering. These
checks cover the shared mechanics; durable request/replay binding and full CLI
publication extraction remain incomplete.


### Direct plan replay admission

The native direct mirror-plan path now checks durable attempt evidence while
holding its operation lease and before acquiring ref leases. The metadata-owned
check refuses both existing receipts and unresolved intents: missing commitment
proof cannot authorize another execution. A typed `PlanAlreadyAttempted` error retains the plan identity and requests
reconciliation; it is not mapped to Git stale-ref or transient-network errors.
The low-level receipt preparation mechanics and managed finalize authority are
unchanged.

`sdk-unresolved-plan-receipts.log` records all 15 receipt tests passing on Rust
1.91.0, including unresolved-intent and committed-plan replay admission. `sdk-unresolved-plan-cli-check.log` records CLI test-target
compilation passing with existing fixture warnings.
`sdk-unresolved-plan-cli-siblings.log` records all 26 native-push tests passing
with the typed replay refusal, including pre-ref admission, plan-lease cleanup
and ordinary multi-ref publication. `sdk-unresolved-plan-metadata-clippy.log`
records strict metadata library/test lint passing. The broader CLI lint command
fails with 529 diagnostics (`sdk-unresolved-plan-clippy.log`); its diagnostic in
`push_native.rs` is the unchanged nine-argument input constructor, outside the
new admission path. This broad gate is not passed or waived.
`sdk-unresolved-plan-auth-check.log` records protected-server error-conversion
compilation passing. Formatting and whitespace checks pass.
This check does not implement the SDK request digest, fresh nonce, public recovery
token or typed mutation outcomes.


### Combined receipt retention qualification

The repository GC receipt fixture now deletes the terminal receipt, compacts the
original attributed transaction, advances the same ref and compacts again. It
runs the production repository-scoped destructive sweep and requires an
unreferenced pack to be deleted. The remaining journal and manifest objects are exported
to a filesystem store and a new process must recover the original transaction
without repairing the receipt. Conditional publication and GC use InMemory;
LocalFileSystem is used only for read-only process reopening because it does not
implement conditional updates. The first run completed deletion but failed when exporting transient lock keys
that overlap as filesystem file/directory names; that log is retained as
`sdk-receipt-compaction-gc-restart-export-failure.log`. The corrected export
contains recovery metadata only. The corrected test passes in
`sdk-receipt-compaction-gc-restart.log`, including explicit checks that the first
active marker is absent and current refs identify the successor. All five sibling
reachability tests pass in `sdk-receipt-gc-sibling-roots.log`, including
history-only pack retention and corrupt-history rejection. This proves
conditional-store receipt retention across the combined sequence, not live
backend, referenced-content or process-kill qualification.


### Canonical publication identity

`crab-metadata::receipts` now owns request hashing and nonce-bound plan identity.
Request JSON object keys are sorted recursively, arrays retain their order, and
encoding streams into Blake3 under the `crab publication request v1` domain.
The plan ID binds that digest and a 16-byte nonce under the separate
`crab publication plan v1` domain. Serialization errors retain their source.

New mirror-plan files use format 2 and a fresh UUIDv7 nonce; apply-time validation
reuses the stored nonce. Existing request-binding regressions use one fixed
nonce so they still prove payload binding instead of passing due to randomness.
After fetching tags, no release tag contains the mirror-plan introducing commit
`500bea2e677dc4dab51b8accdbc88dea7050c8da`. Unexecuted old unreleased plan files must be
regenerated; attempted plans retain their original IDs for receipt lookup. Persistent version-1 intent/receipt fields, paths and dependency
digest domains are unchanged.

`sdk-publication-identity-msrv.log` records the two minimal-feature digest tests
passing on Rust 1.91.0. `sdk-publication-identity-canonical-order.log` records
all 11 receipt/identity tests passing with serde_json object-order preservation
enabled, including fixed encoding/domain checks. All 15 plan-receipt tests pass
in `sdk-publication-identity-receipt-compat.log`; strict metadata library/test lint
passes in `sdk-publication-identity-clippy.log`. All 26 mirror reconciliation
tests pass in `sdk-publication-identity-mirror.log`, including two command
invocations that save distinct nonces and the lost-success receipt recovery path. The SDK recovery-token API and complete write
execution remain unimplemented; these primitives do not advertise SDK writes.
