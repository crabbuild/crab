# Local secondary index read contract

## Current gate

BeyondDB admits local secondary indexes with `ProjectionType=ALL`. KEYS_ONLY
and INCLUDE remain explicitly unsupported at CreateTable. The objective remains
all DynamoDB projection modes; admitting those modes before fixing the engine
boundary would silently return incorrect attributes or omit valid base fetches.

The pinned ExtendDB revision is
`bdb7b3df4ace3b80a6e928f144036d056aec0327`. No dependency change is applied here.

## Reproduction at the contract boundary

Create a table with HASH `pk`, RANGE `sk`, and an LSI on `pk`/`score`, projection
KEYS_ONLY. Write `{pk: "a", sk: 1, score: 2, payload: "value"}`.

| Query/Scan request | Required returned attributes |
| --- | --- |
| IndexName only | pk, sk, score |
| Select=ALL_PROJECTED_ATTRIBUTES | pk, sk, score |
| Select=ALL_ATTRIBUTES | pk, sk, score, payload |
| ProjectionExpression=payload | payload |

For a nonprojected attribute, LSI reads may fetch the base item. A GSI cannot do
so. Read capacity must account for the index read and any required base fetch.
References: [Query](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_Query.html),
[Scan](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_Scan.html),
[LSI attribute fetches](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/LSI.html).

Pinned source evidence:

- `crates/engine/src/query.rs` resolves metadata and Select, but only passes
  effective key schema, key predicate, maps, direction, limit, start key, and
  index name to `DataEngine::query`.
- `crates/engine/src/scan.rs` has the same missing selection boundary.
- `crates/storage/src/lib.rs` returns `(Vec<Item>, Option<Item>)`; there is no
  index/base read-byte accounting in that result.
- `crates/storage-sqlite/src/data/index.rs::project_item_for_index` persists
  only projected attributes. `query_scan.rs` reads those stored images.
- Query post-processing applies index projection only for explicit
  ALL_PROJECTED_ATTRIBUTES. Neither full-image nor projected-image responses
  alone implement all four cases without an engine change.

## Proposed upstream change

Make the selection decision in ExtendDB's engine, shared by Query and Scan.
Carry an explicit read plan to storage, with a base-table fetch flag for LSIs.
The engine derives it from Select, ProjectionExpression/AttributesToGet,
FilterExpression, index projection, and the base/index key union. Default index
selection must be ALL_PROJECTED_ATTRIBUTES. GSI validation still forbids base
fetches.

The storage response must carry separately measured logical index and base
read bytes along with items and continuation. Fetches happen in the same
storage read context as the index page, preserving each item's committed
visibility. The engine uses those measures for table/index capacity arms and
keeps filtering, projection, Count, and evaluated-key pagination in their
current owner. No protocol parsing or request-local hidden state belongs in
BeyondDB's backend.

Update every DataEngine implementation and mock in the same upstream change:
SQLite, PostgreSQL, MongoDB, DynamoDB forwarding, and tests found by a complete
trait-consumer search. Add protocol cases for all four selections, INCLUDE,
COUNT, nonprojected filters, strong reads, empty pages, continuation after
filtering, and ReturnConsumedCapacity=INDEXES. Compare against DynamoDB Local
and the documented cloud contract; retain any cloud-only qualification gaps.

After approval: prepare/test the upstream implementation in an isolated ExtendDB
checkout, open its PR, then update BeyondDB's pin and adapt the backend to the
new read plan/result. Remove the non-ALL CreateTable gate only with signed SDK
projection/capacity tests and owner-restart proof. Do not add a parallel HTTP
adapter or edit the Cargo dependency cache.

## Additional qualification

The initial ALL path stores ordered index keys and reads the base image in the
same Cell query. Index updates share base write/transaction/import commands.
The [logical 400-KiB item limit](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/Constraints.html#limits-items)
includes base and corresponding ALL projections, even though physical images
are not duplicated. Five LSI B-tree pairs add to
prepare-time capacity reservations. A 512-MiB Cell can still throttle an item
collection below DynamoDB's 10-GiB LSI limit; fleet scale and larger hot item
collections remain unqualified.
