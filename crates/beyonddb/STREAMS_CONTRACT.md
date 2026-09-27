# Streams implementation: closed-shard contract proposal

Status: **proposal only**. `extenddb-stream-completion.proposed.patch` is an
unapplied diff against ExtendDB `bdb7b3df4ace3b80a6e928f144036d056aec0327`, the
revision currently pinned by BeyondDB. It does not change Cargo resolution or
install a dependency override. It has been parsed/formatted with Rustfmt and
passes `git apply --check` against that revision. Compilation, its proposed
tests, and SDK qualification remain pending approval and execution.

## Why this dependency change is necessary

BeyondDB's full DynamoDB objective includes Streams and online Cell splits.
A stream shard belongs with the Cell that commits its item mutations. A split
closes the source shard and starts child shards. Consumers need an unambiguous
end to the parent before processing its children; AWS documents shard lineage
and ordering in [DynamoDB Streams](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/Streams.html).
`GetRecords` must stop returning an iterator when a closed shard is exhausted;
see the [NextShardIterator response contract](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_streams_GetRecords.html).

The pinned ExtendDB contract cannot express this:

- `crates/storage/src/lib.rs:159` returns `(records, Option<String>)`. The
  implementation uses that option for the last sequence returned, despite its
  misleading iterator doc comment. An empty open page returns `None`.
- `crates/engine/src/streams.rs:227` calls the backend, preserves the incoming
  sequence when that option is `None`, and always constructs `Some(iterator)`.
- SQLite, PostgreSQL, and MongoDB all implement those last-sequence semantics.
  Their shard metadata already has an ending sequence, but reads do not use it
  to signal completion.
- Current upstream main `998c12b72856dbaba8f3d508c62a4c4302be3229` was inspected
  too. It still always produces another iterator; a pin update alone does not
  resolve this boundary.

Returning an empty page, a guessed cursor, or a storage error from BeyondDB
cannot correctly terminate the iterator. A separate BeyondDB Streams HTTP
handler would duplicate ExtendDB's protocol ownership. The backend must be
able to report exhaustion through the shared storage contract.

## Concrete proposed change

The patch covers every producer and consumer found in the pinned source:

| Surface | Proposed change |
| --- | --- |
| Shared storage contract | Add `StreamContinuation::More(Option<String>)` and `End`; keep records in the same result. `More(None)` preserves the incoming position on an empty open shard. |
| Streams handler | Return no next iterator for `End`, while preserving the final page's records. Continue renewing the iterator timestamp for `More`. |
| SQLite | Read the ending sequence in the existing account-ownership join, fetch one lookahead record, and report `End` only when closed and exhausted. |
| PostgreSQL | Read ending sequence with the shard's table ID, retain the catalog ownership check, and use the same lookahead rule. |
| MongoDB | Read ending sequence from the already-fetched shard document, retain account validation, and use the same lookahead rule. |
| Page limits | Validate 1–1000 records before lookahead; fetch at most limit + 1 and return at most limit. |
| BeyondDB | Its current unsupported `StreamEngine` methods already use the result alias. Implement Cell-backed pages against the new explicit continuation after the dependency is validated. |

Two proposed handler tests cover closed-shard termination and open-page cursor
preservation/advancement with timestamp renewal. Two proposed SQLite tests
cover an empty open/closed shard, a closed shard with multiple pages including
an exactly-full final page, and account isolation. These tests are **not yet
compiled or run**. PostgreSQL/MongoDB require their backend integration gates;
compiling their adapters alone will not establish their runtime behavior.

**Is this the best fix?** An explicit continuation state removes the ambiguity
at the owning contract. It preserves both valid empty-open polling and final
nonempty pages, without sentinel sequence numbers, error matching, or a second
HTTP implementation. All three sibling backend producers must change with the
engine consumer. The closing writer must publish its final records before the
ending marker and never append afterward; BeyondDB must enforce that ordering
in the source Cell's seal command.

## Approval boundary

Root `AGENTS.md` requires explicit approval for dependency patches, overrides,
or vendoring. Applying this proposal to ExtendDB and changing BeyondDB's
resolved dependency therefore requires that approval. The proposal is reviewable
before approval; the Cargo manifests, lockfile, and cached dependency source
remain unchanged. No upstream pull request or message has been sent.

After approval: prepare an isolated ExtendDB checkout on the workspace volume,
apply and compile the change, run its handler and backend tests, fix any issues,
then update BeyondDB to the tested immutable revision and rerun its signed SDK
and process recovery gates. An upstream PR requires authorization to publish it;
this proposal does not assume that authorization.

## Remaining BeyondDB implementation

This dependency fix is necessary but does not implement Streams by itself.
BeyondDB still explicitly rejects streamed writes and Streams API operations.
The implementation must cover all of these boundaries before support is claimed:

1. Store stream identity, view type, generation, shard lineage, and lifetime
   independently from the current table route. Disabled/deleted tables and
   sealed split sources must retain readable history until retention permits
   collection.
2. Commit records with item changes for Put, Update, Delete, batch writes,
   TTL deletion, and committed transaction resolution. Conditions, ABORT,
   no-op writes, and split import must not emit change records. Retried apply
   must not duplicate them. Include stream images in prepare's capacity budget.
3. Fence stream enable/disable and view-type transitions across all participant
   Cells. A cached table description cannot allow a write to skip the active
   generation. Persist and recover the transition after owner loss.
4. Close the parent with its final records before activating child writers;
   publish retained shard discovery alongside the route switch. Preserve
   per-item ordering through the lineage without imposing one global writer.
5. Implement account-scoped ListStreams, paginated DescribeStream, iterator
   validation, sequence lookups, and bounded GetRecords. Enforce the response
   byte budget as well as record count; this proposal only changes completion.
6. Add bounded retention and recovery for old stream generations, then qualify
   all view types, no-op/conditional writes, TTL identity, cross-Cell COMMIT and
   ABORT, split lineage, disable/re-enable, delete/recreate, and hard restart
   through signed clients. The current dependency's other Streams gaps, such
   as byte bounds and shard-filter support, also need qualification.

The full API and 10,000-Cell/multi-TB objectives remain open. No Streams support
or production-scale claim follows from this proposal.
