# BeyondDB metadata ownership

Status: GSI serving-path cutover under verification. GSI membership and split
intents now live in independently owned directory leaves. The account retains one
fixed-size publication anchor per index, the table catalog, and base-range
directories. Public index Query/Scan, projection, splitting, statistics, recovery
and deletion use the tree. This work does not remove the account metadata limit
or qualify 10,000 active Cells; catalog and base-directory extraction remain open.

## Current atomic boundaries

| Boundary | Current owner and consumers | Contract that must survive |
| --- | --- | --- |
| Table name and generation | `table.rs`, `backend.rs`, account `ddb_tables` | Account-unique names; fresh generation after recreation; generation checks must precede mutations of an old table. |
| Initial publication | `backend.rs`, `routing.rs::ActivateTableRoute`, `global_index/routing.rs::ActivateGlobalIndexRoute` | Publish all GSI owners before admitting base writes. A durable catalog row can precede all of these publications. |
| Point routing | `backend/data/routed.rs::routed_partition`, `backend/admission.rs` | A full primary-key lookup resolves one current owner. An existing transaction token resolves its original coordinator and participants before reading current routes. |
| Scan/maintenance pages | `ReadRoutePage`, `read_global_index_route_page`; routed Scan, TTL, projection, capacity, statistics and recovery | Ordered coverage, bounded pages and explicit detection of conflicting directory changes; no full-directory read on the request path. |
| Base split | `routing/split_state.rs`, `split.rs`, `provision/capacity.rs` | Source/child reservations and route replacement currently share one account commit. Keep the intent until both children open. Unrelated splits may publish independently. |
| GSI split | `global_index/split_routing.rs`, `global_index/transfer.rs`, `provision/global_indexes.rs` | Transfer versions and tombstones as well as items; retain unfinished source/child reservations through opening. |
| Delete | `table/deletion.rs`, account lifecycle marker and cleanup worker | Fence the exact generation before bounded metadata cleanup. Prepared account transactions prevent deletion; name reuse waits for catalog removal. |
| Statistics | `statistics.rs::PublishStatistics`, `backend/statistics.rs` | Base sampling retains its account epoch guard. GSI sampling validates each leaf before leaving its immutable interval; final publication checks the live index generation set. |
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
owners, including explicit completion acknowledgements before name reuse. GSI cleanup now retains each anchor until its whole directory tree acknowledges
retirement. An unavailable metadata owner leaves deletion pending and cannot be
treated as deletion evidence. Base-range metadata cleanup remains account-local.

Directory split and data split are distinct operations. A metadata leaf cannot
move an unfinished data-split reservation without transferring its recovery
ownership. Either fence directory splitting while those reservations exist or
supply a checked migration protocol. GSI split and residency callers now supply the partition's logical lower bound
to find the leaf, then check its exact partition identity. The account no longer
keeps GSI participant or range rows. Base-range identity lookup still needs the
same extraction.

GSI range epochs now advance from their own source; leaf membership versions
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

Metadata nodes require product admission, signed peer scope, configured and
expired-owner recovery, and capacity reclamation. A tree that can be created but
cannot restore its path when a node's pool is full is incomplete. Immutable
branches and inactive leaves need a qualified residency policy; metadata must
not be silently bootstrapped when authority is missing. General fleet ownership
and discovery must cover metadata nodes without centralizing all recovery work
in one account writer.

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
leaf mutations. Base activation still compares indexed pages of at most 64 rows.
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

Base directories, table-name catalog splitting, live metadata rebalancing,
coordinator expansion and general retained-history retirement remain open. Public
base Query/Scan, TTL and transaction admission still use account-owned base
routes. No account-limit removal or fleet-scale claim follows from GSI cutover.

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
leaves through one global counter. Base route storage is still account-owned;
this step alone does not remove that size/writer limit. GSI split plans already
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
The original scenario passes without a larger stack or fixture wrapper, and CI
now includes that scenario. No runtime, LTX or dependency contract changed.

Focused verification on this revision: the nine-owner/eight-slot SDK test passed
in 24.78 s; three claimed-owner recovery/metadata-admission scenarios passed in
3.13 s; four signed split/replay/transaction scenarios passed in 6.10 s. Native
participant recovery passed on the default stack in 6.49 s. Numeric Query paging
passed in 4.14 s, and the bounded route-page case passed in 9.30 s. Strict
all-target Clippy passed (11.76 s), and the server binary built (17.69 s).
The remote-account recreation fixture and full peer/process suite still require
CI on the follow-up head. Earlier process success does not qualify these changes.
