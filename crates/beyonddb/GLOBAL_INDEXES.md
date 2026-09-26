# Global secondary indexes

## Current implementation

CreateTable installs independent index range Cells before publishing the base
route. The table and each index have separate HASH distributions and range
identities. An index entry includes the base primary key, so duplicate index
keys do not overwrite one another. Missing index key attributes make an item
sparse. ALL, KEYS_ONLY, and INCLUDE projections are stored in the index Cell;
Query and Scan never fetch unprojected base attributes.

Index Query supports HASH-only indexes, ordered RANGE comparisons, forward and
reverse pagination, and continuation keys containing the union of base and
index keys. Scan uses the index directory and the index HASH distribution for
parallel segments. The pinned ExtendDB engine rejects strongly consistent GSI
reads and ALL_ATTRIBUTES on partial projections.

This is an initial-index implementation. Online Create/Delete/Update index
operations, automatic index range splits, index statistics, tombstone collection,
and fleet-scale recovery qualification remain unfinished. Each index range has
a 512-MiB database budget and a 64-MiB capture budget. Adding initial ranges is
not an unlimited scaling claim.

## Durable asynchronous maintenance

```mermaid
flowchart LR
    Base[Base Cell command] --> Item[Base item and local indexes]
    Base --> Journal[Immutable projection journal]
    Journal --> Worker[Bounded projection worker]
    Worker --> Index[Versioned index range mutations]
    Index --> Ack[Durable success or superseded version]
    Ack --> Retire[Delete source journal entry]
```

A base Put, Update, or Delete inserts its journal entry in the same command as
the data change. Transaction PREPARE stores no projection entry. COMMIT creates
entries while applying staged base images; ABORT discards the staged images
without emitting projection work. Prepare reserves space for journal metadata
and the old/new images in addition to the existing base/local-index claim.
The account and routed participant paths use the same journal representation.
At 4-KiB pages, eight additional worst-case B-tree edits per changed item add
about 137.5 MiB of reservation for a 100-write participant, before image bytes.
This conservative claim consumes the finite Cell budget; it is an admission
cost, not measured physical growth or proof that every maximal request fits.

One entry contains the table/index generation snapshot, old and new base images,
and the source range epoch plus actor sequence. It is shared across the table's
indexes. Payload reads use 128-KiB chunks, so large old/new pairs do not have to
fit one SQL result or peer response. An unchanged projection creates no work.

For each index, the worker removes the old entry when the canonical key changes,
then writes the new projected entry. A same-key replacement writes only the new
image. The source acknowledges the journal entry only after every required
mutation has a durable successful result. A lost reply, failed index owner, or
interrupted worker leaves the entry available for retry. A failed index does
not suppress projection to later healthy indexes; acknowledgement still waits
for all indexes. Concurrent workers may process the same entry safely.

Each index row retains a lexicographically ordered `(source_epoch, sequence)`
version, a content digest, and either its projected image or a tombstone.
Older versions are ignored, equal versions with identical content replay, and
equal versions with different content are rejected. A delayed old image cannot
resurrect a removed entry. Index identities include the base key, so versions
from different base items do not compete. This relies on the base routing
contract: a key has one owner and a split gives its successor a higher epoch.

GSI propagation is eventual. A successful base transaction does not imply that
all of its GSI changes are already visible, or that index readers observe them
atomically. This follows the [AWS propagation contract](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/transaction-apis.html).

## Splits and recovery

Base split sealing rejects pending projection entries as well as transaction
locks. Split imports therefore copy already-projected images and do not emit
new projection work. A later child mutation has a newer source epoch. The
seal check and state transition share one command, preventing a write from
slipping between the journal check and the source fence.

Startup discovers each index's published directory and restores Idle or expired
owners through the same fenced admission machinery as data ranges. The private
peer receiver accepts the index namespace. The supervised projection worker
then resumes the durable source journals.

The worker rotates configured accounts and pages table names. For each indexed
table it visits base route pages to replay journals, then each index's route
pages to restore read availability even without pending writes. Each iteration
visits at most one page (64 ranges), with four range attempts in flight. It
advances before admission so a failed range is revisited on a later sweep.
Sweep cursors are transient; projection versions and journal contents are durable.

Serving discovery uses the existing provisioner's fenced owner recovery for
both sources and index ranges. Idle Cells restore from published roots. Live
remote owners remain in place; expired sessions require NodeDirectory takeover
proof and authority CAS. Local capacity, active node-log recovery, and competing
ownership still gate admission. The same-endpoint restart path may wait up to
30 seconds for its previous session to expire, as in existing recovery.

There is no independent index placement scheduler or measured fleet RTO. A
sustained write rate above projection capacity eventually consumes the finite
base Cell budget; steady-state throughput and journal admission need further
qualification.

## Evidence map

| Boundary | Source |
| --- | --- |
| API metadata and provisioning | `src/backend.rs`, `src/table.rs`, `src/provision.rs` |
| Base mutation and transaction apply | `src/items.rs`, `src/items/transaction.rs`, `src/partition.rs`, `src/partition/transaction.rs` |
| Durable source journal | `src/global_index/outbox.rs`, `src/global_index_outbox_schema.sql`, `src/item_storage.rs` |
| Routed replay and acknowledgement | `src/backend/global_index.rs` |
| Serving owner recovery | `src/provision.rs` → existing `recover_discovered_owner`, NodeDirectory proof, and runtime authority CAS |
| Versioned index storage and reads | `src/global_index.rs`, `src/global_index/read.rs`, `src/global_index_schema.sql` |
| Index directory | `src/global_index/routing.rs`, `src/routing.rs`, `src/schema.sql` |
| Server worker and peer reachability | `src/bin/beyonddb.rs`, `src/server/peer_receiver.rs` |
| Focused recovery fixture | `tests/elastic_cells/global_indexes.rs` |
| Signed peer-owner recovery | `tests/peer_network/global_indexes.rs`, `tests/peer_network.rs` |
| Signed SDK process fixture | `tests/server_binary/global_indexes.rs` |

The focused fixture interrupts a key move after the old index entry is removed,
restores owners, and checks journal completion, duplicate apply, stale-image
suppression, split fencing, and chunked old/new images larger than 700 KiB.
The process fixture covers creation of all three projections, duplicate index
keys, sparse entries, Query/parallel Scan pagination, transaction key moves and
deletes, rejected strong reads, and replay after owner restart.

The signed SDK/RustFS process fixture passed in 330.55 seconds before the
runtime read-replica rebase. The expanded elastic suite passed all 28 tests in
86.28 seconds, including partial-projection restart and binary prefix bounds.
The account and peer-owner tests passed in 3.41 and 99.12 seconds. These are
selected-path results; fleet throughput and recovery remain unqualified.
After rebasing onto `396e0ab1b40` and fixing optional replica-query stack growth,
the account, elastic, and peer suites again passed all 30 tests (3.47, 92.80,
and 101.82 seconds). The runtime snapshot-read/fencing test, strict BeyondDB
Clippy, HTTP-server all-target check, and Cell/LTX layout checks also passed.
The final rebased signed SDK/RustFS server-process smoke passed in 339.11
seconds, including the added wrong-sort-operand validation check, GSI state
recovery, and transaction-token replay after hard restart.

The new index metadata, journal schemas, and Cell namespace change the unreleased
storage format. Development roots from earlier builds require reprovisioning.
No dependency pin, override, or lockfile changes are required.

## Serving recovery regression

The focused fixture releases the first index owner, checks that a healthy
second index advances while the source journal remains, then releases the
source too. The supervised serving worker must restore both and acknowledge
the completed projection. It releases the index again with empty journals to
prove index discovery restores reads independently of new writes. This fixture
passed in 1.70 seconds.

The signed peer fixture creates an indexed table on a live remote owner and
checks that the already-serving recovery node leaves it there. After the old
node stops renewing its lease, the worker must restore the base and index,
preserve the old projected image, and propagate a subsequent signed SDK write
without restarting the recovery node. It passed in 92.47 seconds. The initial
attempt failed because the new fixture table was absent from its IAM policy;
the fixture now grants only that table and index alongside its existing tables.

**Is this the best fix?** Use the existing projection sweep to discover index
owners and the existing provisioner to recover them. The server supplies its
provisioner and NodeDirectory; transaction recovery and startup retain their
current authority boundary. This adds no second ownership or lease mechanism.
Before this change, serving projection visited only base routes and stopped at
the first failed index. Startup could restore indexes, but an empty journal
provided no serving-time discovery. `origin/main` has no BeyondDB subtree;
these comparisons are against the preceding draft PR revision.

The account and elastic suites passed all 30 tests (3.39 and 82.11 seconds,
two test threads), and strict all-target BeyondDB Clippy passed in 8.84 seconds.
The compiled-server signed SDK/RustFS smoke passed in 317.32 seconds, including
unclean restart, recovered index state, and transaction-token replay.
These results cover selected recovery schedules, not a fleet-scale bound.
