# BeyondDB metadata ownership

Status: implementation design. The current account Cell still owns the table
catalog, base/GSI directories and their split plans. This document defines the
ownership changes required to remove that limit; it does not claim that metadata
has already been sharded or that 10,000 active Cells have been qualified.

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
