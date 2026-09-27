# BeyondDB metadata ownership

Status: base and GSI serving paths use independently owned directory leaves.
The account retains one fixed-size publication anchor per base/index generation
and the table catalog. Public routing, splitting, statistics, recovery and deletion
share the tree protocol. The base cutover is under verification: focused signed
SDK checks and all-target compilation pass; native runtime verification and
broader CI remain incomplete.
Catalog sharding and 10,000-Cell/multi-TB qualification remain open.

## Current atomic boundaries

| Boundary | Current owner and consumers | Contract that must survive |
| --- | --- | --- |
| Table name and generation | `table.rs`, `backend.rs`, account `ddb_tables` | Account-unique names; fresh generation after recreation; generation checks must precede mutations of an old table. |
| Initial publication | `backend.rs`, `routing.rs::ActivateTableRoute`, `global_index/routing.rs::ActivateGlobalIndexRoute` | Publish all GSI owners before admitting base writes. A durable catalog row can precede all of these publications. |
| Point routing | `backend/data/routed.rs::routed_partition`, `backend/admission.rs` | A full primary-key lookup resolves one current owner. An existing transaction token resolves its original coordinator and participants before reading current routes. |
| Scan/maintenance pages | `read_route_page`; routed Scan, TTL, projection, capacity, statistics and recovery | Ordered coverage, bounded pages and explicit detection of conflicting directory changes; no full-directory read on the request path. |
| Base split | `directory/transfer.rs`, `split.rs`, `provision/capacity.rs` | Source/child reservations and route replacement share one leaf commit. Keep the intent until both children open. Unrelated splits may publish independently. |
| GSI split | `directory/transfer.rs`, `global_index/transfer.rs`, `provision/global_indexes.rs` | Transfer versions and tombstones as well as items; retain unfinished source/child reservations through opening. |
| Delete | `table/deletion.rs`, account lifecycle marker and cleanup worker | Fence the exact generation before bounded metadata cleanup. Prepared account transactions prevent deletion; name reuse waits for catalog removal. |
| Statistics | `statistics.rs::PublishStatistics`, `backend/statistics.rs` | Base and GSI sampling validate each leaf before leaving its immutable interval; final publication checks the live table and index generation set. |
| Residency/recovery | `provision.rs`, `provision/residency.rs`, `provision/transactions.rs` | Metadata absence is authoritative only at the current owner and for the exact generation. Original transaction participants remain recoverable after directory changes. |

Moving SQL tables alone would break these contracts. Increasing the account
budget or creating one directory Cell per table would retain an eventual
single-writer, single-Cell limit for a large table.

## Required topology

Keep IAM/bootstrap metadata outside data-directory writers. Table-name lookup
uses an ordered, splittable account catalog. Each immutable table generation has
a stable lifecycle authority and separate ordered directories for base and GSI
ranges. Catalog and range-directory growth use bounded nodes rather than a
fixed hash modulus or an account-owned list of every metadata Cell.

A directory node is either a writable leaf or a branch containing bounded child
references. Its identity survives conversion from leaf to branch. Splitting a
leaf freezes its membership, copies bounded pages into separately owned children,
verifies their completion and then publishes the child references in the parent.
The parent publication is the routing cutover. Children are not writable merely
because their roots exist. Retries must compare the immutable split identity,
child bounds and generation; stale leaf writers must receive a route-change
outcome rather than recreate entries behind the new branch.

This avoids rewriting a global root for each ordinary range split: leaves own
independent mutations, and existing branch pointers stay stable when a descendant
splits. A point request follows one path. Listing and Scan resume from logical
keys/bounds and reread the path after a redirect; a cursor cannot pin a physical
leaf forever. Depth, fanout, encoded page size and per-request traversal work
need explicit bounds and adversarial split-history tests.

## Publication and lifecycle

Creation needs a durable generation and recoverable progress before independent
owners are installed. `CREATING` must remain visible until all initial index
routes and the base route are published. The existing account-scoped worker can
repair an incomplete creation from the stored table record; later directory
extraction must retain that behavior without relying on the original request.
The creation placement/count policy is persisted in the table generation before
any initial owner is installed. Recovery uses that policy after configuration
changes. UpdateTable rejects a routed generation until base-route publication
inside the account command, keeping the installed specification immutable during
creation. Metadata extraction must retain that atomic lifecycle guard.

Deletion now records a generation-scoped marker in the account Cell before
bounded cleanup. Live-table lookups fence new account writes/prepares, route and
index publication, split admission/publication and statistics publication. The
capacity worker discovers deleting generations and resumes cleanup after owner
restore. Name reuse waits for catalog removal; delayed delete/cleanup commands
carry the original generation. See [deletion proof](SCALING.md#bounded-generation-scoped-table-deletion).

Metadata extraction must preserve these lifecycle semantics across independent
owners, including explicit completion acknowledgements before name reuse. Base and GSI cleanup retain each anchor until its whole directory tree acknowledges
retirement. An unavailable metadata owner leaves deletion pending and cannot be
treated as deletion evidence. Account cleanup waits for base retirement as well as every index retirement.

Directory split and data split are distinct operations. A metadata leaf cannot
move an unfinished data-split reservation without transferring its recovery
ownership. Either fence directory splitting while those reservations exist or
supply a checked migration protocol. Base/GSI split and residency callers supply the partition's logical lower bound
to find the leaf, then check its exact partition identity. The account no longer
keeps base or GSI participant/range rows.

Base and GSI range epochs advance from their own source; leaf membership versions
advance independently. Pagination validates the leaf containing its prior logical
lower bound before crossing to a neighbour. Child versions start above the
frozen parent version, so resolving an old cursor into a descendant detects the
change. Statistics accumulate one leaf at a time, validate that leaf version, then
leave its immutable interval permanently. Subsequent splits behind that bound
cannot count both old and new owners. Statistics remain asynchronous samples,
not a cross-Cell snapshot; final publication checks the live generation set.

## Transaction and recovery requirements

Admitted transactions keep their original coordinator identity, target Cells,
partition epochs and digests. Directory movement changes future admission only.
A successful token replay must still find the old decision after catalog, table
and directory owners have all changed. Delayed prepares remain fenced by the
participant's retained terminal record.

Metadata nodes use product admission, signed peer scope, configured and
expired-owner recovery, and capacity reclamation. Restoration and metadata
admission prefer releasing settled data/GSI owners, then allow settled directory
owners to yield. This lets a lookup path outlive its local residency without
requiring every directory node to remain active. Generation checks and fresh
runtime settlement preflight precede worker close and authoritative release;
membership and unfinished transfers stay in the durable root. New data/GSI
bootstrap still requires free capacity or proven retirement. Metadata must
not be silently bootstrapped when authority is missing. General fleet ownership
and discovery must cover metadata nodes without centralizing all recovery work
in one account writer.

The signed SDK regression
`sdk_reads_restore_data_and_live_directory_with_one_available_slot` fills four
resident slots with account, credentials and a live directory after draining
its data owner. Before directory reclamation, GetItem fails with no eligible
placement destination. After the change, repeated GetItem calls restore the
data owner, and subsequent directory reads restore exactly the previous metadata
state. The directory authority retains its root while idle. The four focused
reclamation tests pass in 7.59 seconds, including account-independent participant
restoration and historical base/GSI source recovery. This small fixture does not
qualify deep-tree churn, concurrent movement or fleet-scale recovery.

## Integration gates

- Failed signed CreateTable requests recover from cataloged progress without a
  client retry, including published GSIs and account-owner replacement.
- Signed create/describe/list/update/delete and recreation cross catalog splits;
  tags, TTL configuration and statistics retain their intended generations.
- One table's base and GSI directories outgrow one metadata Cell. Point requests,
  forward/reverse Query, Scan, projection, TTL and recovery use bounded traversal.
- Faults before/after child preparation, metadata cutover and data-range cutover
  preserve coverage and reject stale writers. Parent/child owner loss must be
  recoverable from durable state alone.
- Delayed transaction phases and token replay cross both metadata and data splits;
  no replay is admitted against replacement participants.
- Deleting generations, pending splits and unavailable metadata cannot authorize
  premature residency release or object-store reclamation.
- Run real multi-node SDK/process proof, then 1,000 and 10,000 active Cells with
  multi-TB data. Record metadata write concurrency, lookup latency, recovery lag,
  admission backpressure and storage cost. Local page-count tests do not prove
  those fleet gates.

The extraction should replace the account routing path when integration passes.
Keeping two authoritative route directories or a reader fallback would require a
separate migration contract. No such compatibility requirement has been adopted
for these unreleased schemas.

## Initial directory publication

Creation records an index intent with the table generation before installing any
independent owners. Its nullable fingerprint distinguishes an unpublished intent
from a serving anchor. Creation installs the initial GSI owners and root directory
before publishing that fingerprint. The anchor stores no growing range inventory. Activation replay compares that fingerprint, preserving later
leaf mutations. Base activation now verifies a directory copy receipt through
the same publication contract.
The 1,024-entry native activation regression checks replay and corrupted input;
these are metadata entries, not 1,024 active owners. Public initial placement
remains capped at 256 ranges per table/index.

## Native range-directory protocol

`src/directory.rs` now registers an entity-scoped directory Cell type. Each node
stores at most 1,024 compact range records, serves at most 64 records per page,
and has a 16-MiB database / 4-MiB capture ceiling. A node's table generation,
identity and interval are immutable. Root and child installation verify contiguous
coverage, distinct local range identities and nonzero range epochs. An immutable
installation fingerprint permits a delayed install to acknowledge the original
copy without overwriting later membership.

`directory/changes.rs` reserves a source and both data/GSI children in one leaf
before a range transfer. Publication replaces exactly that source and advances
only that leaf's version. Reservations remain through child opening. A metadata
split cannot freeze a leaf with an unfinished range plan, so migration cannot
lose that plan's recovery owner. Independent leaves can publish independently;
there is no ancestor write for a descendant's data-range change.

`directory/transfer.rs` retains a typed base or GSI split plan beside the compact
reservation in the same commit. Recovery through the source or either child
returns the exact immutable table/index contract, including after membership
publication removes the source. Publication compares that full plan; completion
removes it with the participant reservations. The native base-transfer regression
restarts the owner before and after publication, rejects a changed contract and
wrong generation, and checks that metadata splitting remains fenced until finish.
Base and GSI controllers use this shared protocol. Base serving now resolves
the same directory tree before admitting a data request.

`directory/split.rs` freezes an exact leaf version and records deterministic child
identities, bounds and copy fingerprints. Children install in a non-serving state.
The controller captures their durable installation receipts before publishing
child references in the parent. That commit removes the parent's bounded copy;
its immutable split remains available to finish opening children after restart.
A former leaf now returns redirects and rejects stale writes. Opening children
is an authenticated controller operation, like data-range opening: the controller
must observe parent publication first. A participant command does not perform a
remote read of the parent.

`provision/directory.rs::split_directory` resumes frozen, partially copied and
published splits. It restores published children from existing authority and
never creates an empty child behind an already-published branch. A later split
of a child uses the same protocol without rewriting its ancestors. The current
depth ceiling is 127; adversarial depth/rebalancing qualification remains open.
This is a bounded primitive, not a literal unlimited-capacity promise.

Provisioning now constructs replica limits from the compiled Cell type instead
of assuming every type uses 512/64 MiB. Initial bootstrap, idle restoration and
expired-owner restoration use the same declared limits. Existing account, data,
index, coordinator and credential declarations retain their existing ceilings.
The directory namespace uses signed peer admission and placement for child copies
and idle restoration. Split and retirement controllers follow live remote owners
and use the existing expired-session takeover path when required. Published paths
require cataloged identity and a durable root before restoration; a missing child
cannot be replaced with an empty node. Account, credential and coordinator targets
remain outside this admission capability. Background GSI discovery restores at most the current and next metadata path
for each page. Terminal directory nodes can release residency under the runtime
generation and settled-work checks; their durable roots remain. Live directory
rebalancing and general branch/leaf residency still need qualification.

### Directory retirement

`directory/retirement.rs` fences an exact node generation before acknowledging
retirement. Branches and frozen nodes retain both child addresses, their
publication status and an acknowledgement bitmap until both descendants finish.
Leaves and importing copies become terminal immediately. A frozen parent can no
longer publish, but its unfinished copies still require durable retirement fences. Installation, opening, membership changes and split discovery all respect
the terminal fence. Retained SQL rows and command receipts are not garbage-collected.

`provision/directory.rs::retire_directory_step` advances one bounded path and
records a child's durable retirement before its parent can finish. Published
children require existing authority. For unpublished children, the retained
parent intent authorizes installing a terminal fence even if copying never began. A lost receipt is recovered from the child's terminal state. Missing
or unavailable published children leave retirement pending. Public DeleteTable first fences the table generation. The capacity worker runs
this protocol for every index intent, records its terminal receipt against that
generation, then continues bounded account cleanup. An unpublished creation intent
also authorizes a missing root's terminal fence, preventing delayed installation
after deletion. These native commands trust the authenticated lifecycle controller
to verify the account or parent intent; they do not read other Cells. Name reuse waits for every
index acknowledgement; delayed receipts cannot affect a replacement generation.

### Evidence and remaining integration

`tests/elastic_cells/directory_tree.rs` runs the native client through the real
Cell app, host, SQL runtime and object-store recovery. It installs 1,024 route
entries, rejects a full-leaf reservation and a corrupt child copy, then replaces
the owner before any child copy, after one copy, and after parent publication.
The controller restores every case. Tests also check stale-parent writes,
leaf-version conflicts, reservations blocking metadata movement, independent
child writers, install replay after mutation, recursive splitting and an unchanged
ancestor. These are metadata entries, not 1,024 active data owners. Focused test:
passed in 5.33 seconds with retirement coverage. It also interrupts retirement
after a grandchild commit but before its parent acknowledgement, leaves another
published child owned by an unavailable host, and verifies eventual completion
after that owner releases it. Wrong-generation retirement and delayed install,
open, split and range-publication attempts are rejected. This is native protocol
proof, not DynamoDB SDK cutover.

`tests/peer_network/residency/directories.rs` runs split/copy/open over signed mTLS
between two leased hosts, verifies remote ownership, restores a released child
through peer activation, retires a live remote child, then recovers a lost child
retirement acknowledgement after its owner stops and its advertisement expires.
That focused test passed in 18.71 seconds. It exercises native metadata commands
over the real peer transport; it does not claim public DynamoDB directory routing.

## Serving cutover verification

The GSI cutover replaces account-owned range and split-plan SQL with one canonical
directory path. Full transfer plans stay beside compact leaf reservations until
both data children open; unfinished transfers prevent metadata splitting. The
controller splits a full metadata leaf before reserving another data split.
Published-parent traversal can finish opening a copied child after a lost
controller reply, using the parent's retained immutable split.

Focused GSI transfer/restart and projection recovery tests passed (3 tests,
31.46 s on the admission revision). All three directory activation, split/restart
and residency regressions passed together (12.88 s). The terminal-directory
residency regression also checks that full
admission creates no ownership claim, waits for the runtime's settled gate, then
reclaims and restores a retired node without losing its fence (2.07 s).

The signed SDK scenario creates a GSI, splits its metadata root, splits a data
range in one child, releases the metadata owners, then checks paginated Scan and
Query after restoration. DeleteTable waits for directory retirement before the
catalog generation disappears. Three repetitions passed (9.35, 6.27 and 5.29 s).
Two runs exercised rejection by a full peer followed by successful placement.
The original scenario exposed stale signed capacity: new admission previously
claimed ownership before rejecting capacity, and converted the capacity error
into a generic transport failure. Admission now checks capacity before a new
claim and preserves its native error; placement excludes each exhausted candidate
once, and retries only while authority confirms no owner was claimed. A claimed
owner still requires the existing recovery protocol.

Strict all-target Clippy passed after the final lint fixes (16.74 s).
The preceding protocol/lifecycle revision passed SDK/process CI (run
36323580772). This serving cutover still needs broader creation, deletion,
statistics, recovery and CI qualification; focused SDK proof is not that gate.

That GSI qualification preceded the base cutover below. Table-name catalog
splitting, live metadata rebalancing, coordinator expansion and general retained
history retirement remain open. No fleet-scale claim follows from either cutover.

### Cutover lifecycle and admission qualification

Focused signed SDK regressions passed for deletion before and after an unpublished
root copy (9.62 s), sparse-index statistics (2.83 s), partial-creation recovery
(2.14 s), and split-source capacity reclamation (3.62 s). Deletion coverage checks
rejected delayed installation, a fresh generation on recreation, and subsequent
SDK writes and reads. These tests precede the unpublished child-retirement change.

The index split/owner-restore regression exposed metadata starvation when live
ranges occupied a released account owner's slot. Under admission pressure,
account, credential and directory restoration can now release one settled data
or GSI owner. The runtime rechecks generation, queued work and publication state;
release preserves its durable root, items and transaction intents. New data-owner
bootstrap does not use this priority; restoration of existing published roots
now shares the residency allowance described below. The SDK regression passed after this change
(16.49 s); fleet throughput and admission fairness remain unqualified.

The native directory-tree regression now also retires a frozen descendant with
zero, one and two unpublished child copies, restarts during retirement and
rejects delayed install/open commands for every child. It passed in 10.46 s.
Published children still require recovery of existing authority; the unavailable
published-child case remains pending until its owner releases it.

The full-node metadata restoration regression passed in 2.55 s: it fills all
slots, releases the account, occupies that slot, then restores the account by
releasing exactly one settled data owner. The released root remains durable and
SDK reads recover every original item. All five focused creation regressions
passed together in 10.91 s after reclamation learned to stop waiting when another
sweep has already freed capacity or released all of its observed retired owners.
The preceding run reproduced a five-second stale-candidate timeout on immediate
recreation; the successful rerun alone does not establish fleet admission fairness.

Both signed directory lifecycle regressions passed together after these changes
(20.56 s): metadata splitting with SDK pagination and deletion, plus remote child
retirement and expired-owner recovery. One run exercised full-peer rejection and
successful subsequent placement. Broader SDK/process CI still predates this cutover.

Strict all-target Clippy passed on the lifecycle revision (14.51 s), with format
and diff checks clean. No dependency pins or lockfiles changed. These focused results do not replace broader SDK/process CI qualification.

### Metadata outage and projection scheduling

The three focused owner-recovery regressions passed together (3.58 s). A new
signed SDK test injects an unavailable first index directory while leaving the
second available. Capacity maintenance advances and publishes the healthy index
split. The same test exposed journal head-of-line blocking: the healthy index
received the first image but newer writes timed out behind retained failed work.

Projection attempts now durably defer failed entries behind work already queued,
using source commit sequences for both enqueue and retry order. Payload identity
and projection versions remain immutable; an entry is deleted only after all
required index changes succeed. Both account and routed source commands use the
same implementation. Account-local public GSI creation remains unsupported; the
public GSI path uses routed data owners. The unreleased outbox schema and delivery
command codecs change together; no released-root migration is claimed.

The regression passed after the fix (4.59 s), then passed with source-owner release
and restoration during the outage, recovery of the failed directory, eventual
acknowledgement of all retained work, and both indexes retaining the newest image
(7.12 s). This is bounded fault injection through the production projection loop,
not throughput or fleet fairness qualification.

After durable retry scheduling, all three native GSI regressions passed together
(24.36 s): journal replay/version fences, serving owner recovery with independent
indexes, and range transfer preserving versions and tombstones across restart.
The implementation replaces account-owned GSI range/plan rows with bounded leaf
ownership and adds traversal, creation-intent and retirement recovery. Those
distributed lifecycle responsibilities account for the production code growth;
there is one authoritative GSI route path, with no account-route fallback.

Final pre-push gates passed: strict all-target Clippy (23.69 s), the BeyondDB
server build (45.21 s), formatting and diff checks. The branch already includes
latest fetched `origin/main` (`311105eb864`); broader SDK/process CI must verify
the pushed cutover before it is considered ready to land.

## Base-range epoch independence

Base split plans now carry only their exact source and children. Each child uses
`source.epoch + 1`; the controller no longer reads a table-wide route epoch to
construct a data-owner version. Publication still compares exact source/child
identities and increments the current directory epoch for Scan invalidation.
Completed replay reconstructs the immutable plan from the sealed source without
inventing a historical directory version. Changed native operation codecs reject
old plan/result shapes. No released-data compatibility is claimed.

This removes a dependency that would couple independently writable base-directory
leaves through one global counter. At this intermediate revision base route storage remained account-owned;
the serving cutover below removes those range and plan rows. GSI split plans already
use source-relative child epochs. Transaction participants retain their admitted
partition identity and epoch, so unrelated branch splits do not change them.

The new regression first failed against the prior implementation: after two
splits on another branch, an untouched sibling received child epoch 4 instead
of its source-relative epoch 2. The revised case passed (1.81 s). It now also
selects the descendant containing a stored item, executes and replays an SDK
transaction across the deeper branch and untouched sibling, then checks both
committed values. All four focused signed split regressions passed together
(7.20 s), including transaction-blocked capacity progress, independent split
replay and recovery after publication before child opening.

## Recovery through bounded residency

CI run 36328796541 passed 32 peer SDK tests and all three server-process tests,
but exposed two incomplete integration paths. Recovery of nine durable owners
into eight resident slots failed with native admission exhaustion. The remote
account recreation fixture also omitted the lifecycle worker and attempted
CreateTable immediately after the asynchronous DeleteTable response.

Admission now permits restoration of an existing published root to release one
settled base/GSI owner, using the same runtime generation, queue, publication and
lease checks as metadata admission. The released owner retains its exact root,
items, locks and journals. Fresh range bootstrap still requires free capacity;
this policy does not turn stale placement measurements into new ownership claims.
The SDK recovery test revisits the original data ranges and the GSI after peer
loss, exercising reuse of the eight slots across all nine durable owners.
That extension caught a second gate: ownerless-root placement rejected the full
pool before activation could reclaim it. Peer resolution and explicit range
provisioning now apply the same admission policy before sampling local capacity.
This changes real residency before selection rather than overstating advertised
headroom; ordinary planner resource and ownership checks remain in force.

The recreation fixture now runs the production lifecycle loop and waits for
DescribeTable to return ResourceNotFound before reusing a name. This follows the
[asynchronous DeleteTable contract](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_DeleteTable.html).
The HTTP state constructor documents that lifecycle-worker requirement.

Native participant recovery also exposed default-stack exhaustion in the nested
admission/restore path. BeyondDB's shared published-root restoration future now
lives on the heap; it retains ordinary cancellation and admission-lock ownership.
The original local scenario passed without a fixture wrapper. Later Linux CI
(run 36336572025) still overflowed the default test-thread stack. Qualification
now runs the native capability suite with a 16 MiB test-thread stack; peer and
server process checks use their normal stacks. No runtime, LTX or dependency
contract changed.

Pre-rebase focused verification: the nine-owner/eight-slot SDK test passed
in 24.78 s; three claimed-owner recovery/metadata-admission scenarios passed in
3.13 s; four signed split/replay/transaction scenarios passed in 6.10 s. Native
participant recovery passed on the default stack in 6.49 s. Numeric Query paging
passed in 4.14 s, and the bounded route-page case passed in 9.30 s. Strict
all-target Clippy passed (11.76 s), and the server binary built (17.69 s).
The remote-account recreation fixture and full peer/process suite still require
CI on the follow-up head. Earlier process success does not qualify these changes.

### Concurrent owner admission

Signed GSI Scan after a drain exposed a placement race: the ingress selected a
remote destination, then another admission claimed the same Cell locally before
the remote activation arrived. The peer correctly refused that stale activation;
the ingress previously propagated its refusal as an SDK 503.

The serving resolver now reloads ownership once when an activation reports
CellNotActive and authority proves a changed ownership epoch. It resumes the
winning claim through the normal admission path before dispatching application
work. Stable refusals and ambiguous transport outcomes still propagate. The
peer receiver's prohibition against acquiring Cells during ordinary invocation
is unchanged; explicit provisioning and rebalancing retain their existing policy.

A deterministic signed SDK regression pauses the selected peer's activation,
commits the competing local claim, then releases the peer request. It failed
with 503 before the fix and passed with SDK retries disabled afterward (0.57 s).
The original index split/tombstone/owner-restore scenario then passed five times
(6.56–11.37 s). Claimed-owner recovery and full-pool metadata restoration passed
all three focused scenarios (1.20 s). These checks establish this admission race's
behavior, not arbitrary churn tolerance or fleet-scale qualification.

### Ownership changes during lookup and claim

Repeated SDK GSI qualification reproduced a 503 after owner drain (the ninth
unmodified run failed). Instrumented repetition also captured an idle-root CAS
loss: a remote activation read Idle, another node claimed the Cell, and the
losing storage conflict became a generic peer rejection.

Placement and published-root restoration now share one bounded admission
re-resolution. A CellNotActive or typed storage StateConflict triggers one
fresh attempt only when authority proves a different ownership epoch. This
runs before application dispatch and does not replay accepted item mutations.
Stable refusals, unrelated storage errors and ambiguous transport errors still
propagate. The runtime's CAS, exact-root checks and rollback remain unchanged.

A separate stale observation can name a local Serving owner that has drained
before local handle lookup. Published-root restoration now reloads that local
observation under the admission lock; an Idle successor is restored normally.
Foreign Serving owners still route remotely without taking the local admission
lock. Base, GSI, directory, credential and coordinator requests use this shared
resolver; ordinary peer invocation still cannot acquire ownership.

Deterministic signed SDK tests pause the selected remote activation both before
admission and inside its conditional claim, then install a competing winner.
A second test pauses an authority response, drains its local owner, and resumes
the stale read. Both new interleavings returned SDK 503 before the fix, with
client retries disabled. After the fix, both tests passed (6.47 s), the original
GSI workload passed ten consecutive runs (9.23–18.15 s), and all five reclamation
and four recovery siblings passed (9.77 s and 7.67 s). Strict all-target Clippy
and the standalone server build passed. Full peer/process qualification remains
a separate CI gate; these tests do not establish arbitrary churn or fleet-scale
readiness.

## Base serving cutover

Base creation installs a bounded directory root and publishes its durable copy
receipt through `ActivateTableRoute`. The account stores the initial fingerprint
in `ddb_directory_roots`, with the same lifecycle ownership as GSI anchors.
Account-owned base route rows, split tables and whole-route native queries are
removed. `read_route_page` and `ReadRouteDirectory` serve both range kinds.
Point reads, Scan, TTL, projection discovery, capacity and owner recovery consume
that path. Existing admitted transaction tokens retain their original targets.

Base split selection reads its immutable source contract from the data owner and
compares the compact identity in the selected leaf. Begin/publish/finish operate
on that leaf's full transfer record; reservations survive publication until both
children open. Full metadata leaves use the existing freeze/copy/publish/open
controller before another range split is reserved. Base statistics use the same
leaf interval validation as GSI statistics, without an account-wide range epoch.

Five focused signed SDK split tests passed together (3.38 s). They cover disjoint
plans, transaction-blocked progress, recovery after publication before child open,
source-relative epochs, and a new base metadata tree scenario. The latter splits
the root into two leaves, changes one leaf independently, drains metadata and data
owners, then verifies Scan pagination, GetItem, committed transaction-token replay
without a second increment, and sampled item counts. These tests use the serving
resolver with SDK retries disabled for the restoration checks.

Native fixtures now use directory publication and leaf-owned transfers; retired
account-routing operations have no remaining source/test consumers. All-target
compilation passes. The large data-range/transaction owner-restart fixture passes
(11.35 s) with its documented 16-MiB test-thread stack; broader runtime gates
remain incomplete.
The focused SDK proof does not qualify all lifecycle paths, fleet throughput,
metadata admission fairness, or the 10,000-Cell/multi-TB target. Account table
catalogs, anchors and lifecycle writes still have one writer and a finite budget.

Additional focused checks pass on the base cutover: two signed statistics tests
(4.83 s) cover sparse base/LSI/GSI counters and rejection of a sample interrupted
by a base split; remote directory split/retirement and expired-owner recovery pass
(17.55 s) using the actual base root created by SDK CreateTable.

The earlier CI run [36330738484](https://github.com/crabbuild/crab/actions/runs/36330738484)
finished with 33 peer tests passing and two failures on its pre-cutover head:
partial-creation projection hit admission exhaustion, and split-source reclamation
left an expected historical owner resident. Both focused scenarios now pass
on the base cutover, but the broader CI gate still needs a new run. The
subsequent [run 36332205297](https://github.com/crabbuild/crab/actions/runs/36332205297)
on `666295a8ad9` finished with 35 peer tests passing, the same historical-owner
reclamation failure, and all three server-process tests passing. Neither run
contains this base cutover.

### Capacity and participant restoration

Base and index directory failures now advance the capacity cursor using the same
policy. An unavailable directory is revisited next pass, allowing healthy tables
and indexes to continue. The new base-outage regression failed with an unchanged
cursor before the fix. Both base/index outage tests pass (8.30 s), including SDK
writes/reads after splitting and index projection recovery.

Restoration admission prefers settled sealed base/GSI owners before serving
ranges. It reads local state and preserves every released durable root. Account
and directory metadata are not prerequisites for restoring original transaction
participants. A regression reproduced that unwanted dependency at full capacity
with the account unloaded. It now passes together with the split-source
reclamation/SDK restoration scenario (10.92 s). Runtime release still rechecks
the exact residency generation and settled work. New range creation retains its
metadata-qualified retirement policy and capacity refusal.

Creation verification currently passes original-count recovery, incomplete-table
update rejection, and worker completion after account restoration. Concurrent
delete/recreation checks are still under investigation; this is not an all-green
lifecycle or fleet qualification.

The four native directory activation/transfer/tree/retirement checks pass together
(25.12 s). Bounded traversal across 1,025 metadata ranges passes (19.42 s); this
fixture does not instantiate 1,025 data owners. Deleted-table residency and
historical-root restoration pass (15.80 s). Strict all-target Clippy and the standalone server build pass after
fixture migration.

Two intermittent observations remain open: concurrent delete/recreation can
report admission exhaustion or a released local owner, and the mixed-participant
large-payload scenario reported an unavailable Cell once before passing alone
(42.31 s). Neither successful rerun establishes that the failures are resolved.

A diagnostic mixed-participant run and three consecutive repeats passed
(42.31, 52.45, 29.12 and 23.40 s). The diagnostic did not observe a missing
peer owner on those runs. Temporary probes were removed; the earlier failure
remains unexplained and must not be counted as fixed.

### Reclamation snapshot ordering

A deterministic regression reproduces one creation failure: releasing two local
ranges during the first account lookup made reclamation read the second range
from its stale discovery list. It failed with `target Cell is not locally owned`
in 0.62 s. Reclamation now gathers base/GSI state under the admission gate before
looking up account/directory metadata. It drops that gate before metadata reads,
which may themselves need owner restoration. Final release still requires a
fresh runtime residency generation and settled-work check.

All three reclamation SDK scenarios pass together (7.65 s), including the new
interleaving, historical-source recovery and original-participant restoration
without resident account metadata. The five creation SDK scenarios pass together
(15.69 s). Native capacity backpressure passes (1.50 s), preserving
`LimitExceeded` classification, worker readiness and the retained split plan;
that classification is already present in `origin/main`'s `provision_error`.
Numeric Query/Scan and owner restoration pass (3.86 s).

CI now runs the entire native capability suite alongside signed peer SDK and
server-process suites, serially and with `--no-fail-fast`. Native fixture
migration affects more paths than the previous filtered CI checks covered.

### First transaction admission at full residency

Coordinator admission now shares metadata's allowance to release a settled
base, GSI or directory owner. Previously, a first coordinator without a
published root was classified as new range growth and refused when every slot
was occupied. The signed SDK regression reproduced `LimitExceededException`
with account, credential, directory and two data owners filling five slots.

`first_cross_cell_transaction_at_capacity_survives_coordinator_restoration`
now commits increments across the two data Cells, drains the coordinator,
replays the same client token through owner restoration, and verifies each
increment was applied once. SDK retries are disabled. All five reclamation
scenarios pass together (4.69 s). The fixture uses in-memory object storage;
it establishes owner restoration, not process-loss or fleet-scale durability.

The shared admission gate and runtime `release_idle_cell` still require the
exact residency generation, settled-work preflight and authoritative release.
New base/GSI owners retain capacity refusal and peer placement; this change
does not alter transaction decisions, coordinator registration or wire formats.
The native capacity-sweep regression still refuses new split children with
`LimitExceeded` while preserving readiness and the unfinished split (0.56 s).

The earlier Linux CI run `36338473488` subsequently exposed a timing gap in
first-coordinator admission. The same SDK transaction failed locally with
`LimitExceededException` in 0.28 s, with client retries disabled. Runtime commands
invalidate idle inventory; its background inspection can finish after the next
admission request. A diagnostic snapshot contained no eligible data/directory
owners, while those same owners became eligible after 150 ms.

Recovery admission now waits up to five seconds for an eligible resident
data/GSI/directory owner, checking every 50 ms and stopping if capacity becomes
available. This uses the existing retired-range settlement budget. Empty pools
return immediately; fresh data/GSI creation retains its capacity refusal. The
admission lock excludes competing local reclamation, and runtime release still
rechecks the residency generation, settled work and authority. No command retry,
dependency change or runtime contract change is introduced.

The unchanged first-transaction/restoration/replay SDK regression passes ten
consecutive runs (1.58–2.90 s). Five reclamation tests pass (4.59 s), four recovery
tests pass (4.18 s), both owner-race tests pass (0.82 s), and the GSI
split/tombstone/restoration test passes (8.05 s). Native new-range capacity refusal
still passes (0.60 s). Full Linux qualification remains a CI requirement.

### Abandoned transactions under residency pressure

`abandoned_begin_finishes_with_one_participant_residency_slot` publishes BEGIN
for two data participants, prepares one without recording its receipt, then
drains the participant and coordinator owners. Account and three credential
owners occupy four of six slots. The background recovery worker must restore
the coordinator and alternate the two participants through the final slot.
Local handle observations cannot restore the coordinator or drive the decision;
the worker reaches COMMIT with both resolutions, then signed SDK strong reads
verify both images. Transient capacity deferrals are observed before completion.
All four recovery scenarios pass together (6.49 s).

The claimed-owner recovery fixture now budgets nine resident slots: account,
credential, three directory owners and the four data/index/coordinator owners
it explicitly interrupts. Its old eight-slot budget let coordinator admission
release a data owner before the interruption, leaving only three observed targets.
The expected four-owner coverage is retained. These are local owner-restoration
tests with in-memory object storage, not process-loss or fleet qualification.

### Expanded qualification follow-up

CI run `36336572025` on the earlier `abeb98ca53f` failed: six capacity library
tests passed, native tests aborted on a test-thread stack overflow, peer tests
passed 34/40 and process tests passed 2/3. Qualification now supplies the measured
16-MiB stack only to the native Cargo command. Peer/server tests and their child
processes retain their normal stack environment. The shell still runs both
commands after a failure and returns failure if either fails; stub-command
probes verified both failure cases and stack isolation.

Fixture corrections preserve the new ownership contract:

- GSI capacity refusal still requires a full node, now eleven owners including
  both base directories and the index directory. The separate directory-growth
  scenario provides space for its 21-owner peak across two nodes.
- Single-leaf fixture inspection takes its membership version from its first
  route page, then checks subsequent pages. A prior shape read is a separate
  snapshot and can precede the capacity worker's publication.
- Peer startup recovery uses the restoring peer client to traverse directories.
- Process recreation waits for the SDK's table-not-exists waiter. DeleteTable
  acknowledges DELETING; name reuse follows durable directory retirement.

Directory-growth and cold-placement SDK checks pass (6.71 s and 20.46 s).
The GSI capacity test progressed past creation but later observed one SDK Scan
503 while maintenance was active; its diagnostic run passed (11.98 s) without
observing the failure. That transient is not claimed fixed. The temporary probe
was removed. Full peer startup and process recreation require the next CI run.

The previously overflowing native owner-restart scenario passes with the scoped
16-MiB test stack (6.01 s). Strict all-target Clippy and format checks pass.

After rebasing onto `322ba3ed4f0` (including node-session retirement), four
recovery tests pass in 8.03 s, five reclamation tests in 7.84 s and the native
deletion/recreation test in 11.14 s. Strict all-target Clippy and the standalone
server build pass. The rebase retains main's supervised lifecycle worker and
RustFS GA container; native coverage includes its node-session tests.

### Concurrent split completion

Linux qualification `36341489813` on `89d9c2b0299` passed 42/44 native tests,
41/44 peer SDK tests and all three server-process tests. Two peer failures
reproduced locally: cold-placement split recovery rejected a late publication,
and unpublished-root recovery expected an owner that capacity admission had
already released.

The split trace identified `PublishDirectoryTransfer`: a competing controller
had finished and removed the reservation. The directory correctly rejects
publication without that full plan. Base and GSI controllers now share a
publication boundary that accepts this completed state only when no transfer
remains and both exact replacement ranges are published. A remaining plan,
changed route or unavailable owner still prevents success. Durable command
semantics, child fingerprint verification and the finish ordering are unchanged.

The unpublished-root fixture now budgets eleven resident Cells: account,
credential, and three tables with two data ranges and one directory each. It
retains its takeover-owner, incarnation and SDK data assertions; bounded-slot
recovery is exercised by separate fixtures. This corrects the fixture's ownership
budget after the directory cutover, not a production capacity limit.

Cold-placement SDK recovery passes five consecutive repeats (15.39–18.46 s);
five base-split SDK scenarios pass (2.62 s), GSI split/restoration passes
(6.19 s), and unpublished-root SDK recovery passes (15.73 s). The native
directory-transfer rejection contract passes (0.25 s), as do three native GSI
projection/transfer/restart checks (9.83 s). Strict all-target Clippy and the
standalone server build pass. These are focused results on the follow-up tree;
full Linux qualification remains required.

The same CI run also exposed coordinator-history admission exhaustion, an old
directory test's refusal expectation, and a generation-four deletion timeout.
They are not attributed to the split-publication fix. The coordinator/directory
follow-up below addresses the first two; the deletion timeout remains unproven.

### Coordinator inventory and fresh range admission

The coordinator-history failure reproduced locally in 3.93 s. A diagnostic run
failed in 0.23 s with an empty idle-candidate snapshot despite two tracked
coordinators. Coordinator reclamation had bypassed the inventory-settlement wait
used for data owners. Both paths now share that bounded wait and least-recently
used ordering. Coordinator selection intersects the recovery registry with actual
resident targets, so historical registry entries do not cause pointless waits.
Pending transaction checks and exact released-root verification still gate
coordinator retirement; an admitted target is never its own release candidate.

The unchanged twelve-shard/three-slot history scenario now passes (20.35 s),
including concurrent token replay and abandoned-read recovery. All three native
coordinator-residency tests pass together (20.90 s), including fresh data growth
and movement backpressure. The checkpoint/restart/later-BEGIN test passes (0.87 s).

The directory-retirement fixture now exercises fresh data admission, which must
refuse a full pool of live directories before publishing an ownership claim.
After retirement it reclaims exactly one retired directory, preserves the live
directory, restores the retired root and verifies its stale-install fence
(0.61 s). Its former account-admission refusal contradicted the live-directory
recovery policy; metadata recovery remains covered by signed SDK residency tests.

### Request recovery after range-owner expiry

Ordinary routing previously restored Idle owners but delegated Serving owners
to peer transport even after their lease expired. Transaction recovery restores
its recorded participants; index projection skips tables without indexes. Neither
guarantees recovery of a plain table's directory before its next SDK read.

A focused signed GetItem regression moves the directory and data owner to a peer,
verifies both are Serving there, stops the peer and waits for lease expiry. It
failed with HTTP 503 in 15.28 s without SDK retries. Request admission now checks
expired Serving/Recovering owners for cataloged data, index and directory targets
with published roots. It selects capacity through existing placement and uses
the existing authenticated range admission and runtime fenced takeover path.
Live remote owners stay in place; account and credential takeover remains owned
by configured recovery. No accepted application command is replayed.

The runtime contract remains `claim_expired_for_takeover` plus `takeover_restored`:
fence the exact expired session, require published recovery coverage for active
logs, reserve activation capacity, and compare-and-swap Cell authority before
restoring the verified root. Missing/invalid membership and competing authority
still fail closed. Receiver application invocation has no provisioner and cannot
acquire ownership.

The first fixed regression passes (16.29 s). With an additional live-lease
refusal check, it passes in 15.45 s: an unreachable live peer retains both owners,
then the same signed read succeeds after expiry. Two owner-race tests (0.96 s), four
recovery tests (4.38 s), and five reclamation tests (4.71 s) also pass. This does
not establish the full restart scenario: its latest run failed earlier, before
node loss, on `BeginCrossCellTransaction` with peer HTTP admission exhaustion
(134.80 s). A separate diagnostic run hit the same exhaustion during route
inspection (202.44 s). Those codec-admission failures remain open.
