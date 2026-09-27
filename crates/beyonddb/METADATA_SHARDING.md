# BeyondDB metadata ownership

Status: range-directory protocol implemented; serving-path extraction incomplete.
The current account Cell still owns the table catalog, base/GSI directories and
their split plans. The new directory module is compiled and has a tested native
Cell controller, but public DynamoDB requests do not use it yet. This document
does not claim that the account metadata limit is removed or that 10,000 active
Cells have been qualified.

## Current atomic boundaries

| Boundary | Current owner and consumers | Contract that must survive |
| --- | --- | --- |
| Table name and generation | `table.rs`, `backend.rs`, account `ddb_tables` | Account-unique names; fresh generation after recreation; generation checks must precede mutations of an old table. |
| Initial publication | `backend.rs`, `routing.rs::ActivateTableRoute`, `global_index/routing.rs::ActivateGlobalIndexRoute` | Publish all GSI owners before admitting base writes. A durable catalog row can precede all of these publications. |
| Point routing | `backend/data/routed.rs::routed_partition`, `backend/admission.rs` | A full primary-key lookup resolves one current owner. An existing transaction token resolves its original coordinator and participants before reading current routes. |
| Scan/maintenance pages | `ReadRoutePage`, `ReadGlobalIndexRoutePage`; routed Scan, TTL, projection, capacity, statistics and recovery | Ordered coverage, bounded pages and explicit detection of conflicting directory changes; no full-directory read on the request path. |
| Base split | `routing/split_state.rs`, `split.rs`, `provision/capacity.rs` | Source/child reservations and route replacement currently share one account commit. Keep the intent until both children open. Unrelated splits may publish independently. |
| GSI split | `global_index/split_routing.rs`, `global_index/transfer.rs`, `provision/global_indexes.rs` | Transfer versions and tombstones as well as items; retain unfinished source/child reservations through opening. |
| Delete | `table/deletion.rs`, account lifecycle marker and cleanup worker | Fence the exact generation before bounded metadata cleanup. Prepared account transactions prevent deletion; name reuse waits for catalog removal. |
| Statistics | `statistics.rs::PublishStatistics`, `backend/statistics.rs` | The current final publication checks every sampled base/GSI directory epoch in the same account commit, preventing a split from double-counting source and children. |
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
owners, including explicit completion acknowledgements before name reuse. The
current cleanup is account-local and does not implement that distributed protocol.
An unavailable metadata owner must never be treated as deletion evidence.

Directory split and data split are distinct operations. A metadata leaf cannot
move an unfinished data-split reservation without transferring its recovery
ownership. Either fence directory splitting while those reservations exist or
supply a checked migration protocol. The current membership lookup by random
partition ID also needs replacement: hash-directed metadata lookup requires the
partition's logical boundary or a separately sharded identity index. An account
Cell must not retain an ever-growing reverse-lookup table.

The single numeric table-wide route epoch must also change. Keeping an account
write for every directory mutation would preserve the current writer bottleneck.
Leaf generations and route versions must support Scan continuity and safe split
replay. Statistics need publication at a boundary that can atomically validate
the sample's membership, then bounded aggregation; checking remote versions and
later writing an account total is not equivalent to today's atomic guard.

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

## Activation comparison prerequisite

Base and GSI activation replay now compare compact indexed pages of at most 64
ranges in one Cell snapshot. Base replay compares its epoch and table snapshot
once; neither replay path reconstructs a full directory result. This removes the
GSI SQL-result overflow above 1,000 rows and bounds additional comparison memory
independently of directory growth. A mismatch stops comparison immediately.

The metadata regression installs 1,024 directory entries through native commands,
restores the account owner, replays both base and GSI activation and rejects
changed entries across page boundaries. These are directory entries, not 1,024
active data Cells; public initial placement remains capped at 256. This is a
prerequisite cleanup, not implementation of the sharded topology above.


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
The directory namespace is accepted by the signed peer scope and local resolver;
fleet placement and discovery integration still remain.

### Evidence and remaining integration

`tests/elastic_cells/directory_tree.rs` runs the native client through the real
Cell app, host, SQL runtime and object-store recovery. It installs 1,024 route
entries, rejects a full-leaf reservation and a corrupt child copy, then replaces
the owner before any child copy, after one copy, and after parent publication.
The controller restores every case. Tests also check stale-parent writes,
leaf-version conflicts, reservations blocking metadata movement, independent
child writers, install replay after mutation, recursive splitting and an unchanged
ancestor. These are metadata entries, not 1,024 active data owners. Focused test:
passed in 2.60 seconds. This is native protocol proof, not DynamoDB SDK cutover.

Public creation/routing, data and GSI split controllers, deletion, statistics,
TTL/projection traversal, distributed residency, and background discovery still
use account-owned directories. They must switch together with generation/lifecycle
fences; the new tree must not become a second authoritative copy. Ordered table
catalog splitting remains separate work. Retained native command receipts also
still need general history retirement. No account-limit or fleet-scale claim is
justified until those integration gates pass.


The sibling transactional-read cleanup/restart regression also passed (5.05 s)
with the compiled-limit provisioning path, covering existing 512-MiB account,
data and coordinator types alongside the directory test's 16-MiB nodes. Strict
all-target Clippy passed. The new native directory regression is included in the
SDK/process CI workflow; this protocol revision still needs that CI run.
