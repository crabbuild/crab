# Git protocol v2 and partial-clone boundary

Crab's Git wire protocol v2 implementation is client-side. Git starts the
local `git-remote-crab` process, sends the terminal
`stateless-connect git-upload-pack` command, and then speaks protocol v2 over
the same stdio. The helper reads the pinned repository directly from the
configured object store. Crab does not deploy an upload-pack listener,
receive-pack service, HTTP smart endpoint, callback, queue, or protocol
gateway.

This document describes the implemented profile on the current development
line. RustFS qualification is green, but provider and release qualification
remain before this becomes a released support claim.

The ownership decision is recorded in
[ADR: Keep Git protocol v2 inside the local helper](../design/adr-git-protocol-v2-local-helper.md).

## Implemented profile

The helper advertises `stateless-connect` only after it can open a single
manifest generation with matching pack-index, locator, and all-object
visibility coverage. The session supports:

- protocol-v2 capability advertisement;
- `ls-refs` with ref prefixes, symrefs, peeled tags, unborn HEAD, and hidden
  refs;
- `fetch` with wants, haves, done, tags, sideband/progress, shallow/deepen,
  and `deepen-relative`;
- the bounded filter matrix: `blob:none`; `blob:limit=<n>[kmg]`; `tree:<depth>`;
  `object:type={tag,commit,tree,blob}`; `sparse:oid=<full SHA-1>`; and
  repeated filters or percent-encoded `combine:` intersections;
- `thin-pack` and `ofs-delta` request options are accepted, but the first
  producer emits a self-contained, non-delta pack because no external base is
  required or assumed;
- standard local Git pack installation on the v2 path.

The filter forms are parsed and planned before object bytes are read.
`blob:limit` uses Git's binary `k`, `m`, and `g` suffixes and retains blobs
strictly smaller than the limit. `tree:<depth>` retains tree/blob entries
whose tree-relative depth is smaller than the requested depth. `sparse:oid`
accepts only a full 40-hex SHA-1 for a visible blob containing the
sparse-checkout specification. Repeated `filter` arguments and `combine:` use
intersection semantics; parser input is bounded to 4 KiB, 16 members, and
eight nesting levels.

The support matrix is:

| Filter specification | Status |
| --- | --- |
| `blob:none` | Accepted and qualified on the development line; ordinary blobs are omitted |
| `blob:limit=<n>[kmg]` | Accepted; blobs with size at least `n` are omitted |
| `tree:<depth>` | Accepted; depth is a bounded non-negative decimal |
| `object:type={tag,commit,tree,blob}` | Accepted |
| `sparse:oid=<full SHA-1>` | Accepted when the specification blob is visible and valid |
| repeated `filter` / `combine:` | Accepted as bounded intersections |
| `sparse:path`, `blob:depth`, and other unlisted forms | Rejected before object I/O |

A client request for an unsupported form receives a protocol error; Crab never
acknowledges it and downloads a complete pack as a substitute.

Fresh, unfiltered fetches of exact visible ref targets use the visibility
proof's complete per-ref closure directly. Requests with haves, shallow or
depth semantics, filters, tag expansion, or a want that is not an exact ref
target use the bounded traversal planner. Pack generation reads up to the
operation's default 10,000-object bound as one locator batch so adjacent pack
ranges can be coalesced; fetched-byte and inflated-byte budgets remain the
memory and I/O bounds. Locator batches spanning at least one exact-read wave
and at least half of the pinned pack inventory use one ordered SlateDB scan,
clipped to the requested SHA-1 range. The scan abandons itself and returns to
exact reads if stale rows would make it examine more than twice the requested
object count, so sparse and stale-heavy repositories remain bounded.

Failures detected before the `packfile` response section use Git's terminal
`ERR` packet. Failures after that section begins use sideband channel 3. This
keeps request rejections distinguishable from truncated pack generation.

Every requested OID is admitted from the immutable visibility proof before the
remote reader obtains object bytes. The proof and locator are tied to the same
manifest generation and pack-index hash. Invalid or missing proof suppresses
v2 advertisement; it never triggers a silent complete filtered fetch.

Git owns the local promisor lifecycle: the Git version in use records the
remote's promisor/filter configuration and marks received promisor packs with
`.promisor` sidecars. Crab's helper does not invent a second local repository
configuration or pack-installation protocol.

Rollback qualification accepts one of two explicit outcomes from the
immediately prior Crab binary: it services an authorized promised raw OID with
byte identity, or it rejects the request before writing response bytes,
`.pack`, or `.promisor` files. A downgrade that takes the refusal path must
hydrate or unfilter the repository while a compatible binary is still
available; it must never silently install a complete fallback pack.

The existing line-oriented helper remains the compatibility path for older Git
and for ordinary complete fetches when v2 is unavailable. Push continues to
use Crab's existing helper and manifest-CAS pipeline; there is no receive-pack
takeover.

## Explicitly unsupported

The first profile rejects stateful `connect`, `packfile-uris`, `object-info`,
`ref-in-want`, date/ref-exclusion shallow selectors, and other capabilities not
listed above. A v2 session cannot fall back to the line protocol after the
terminal handoff; it reports the protocol failure instead.

This wire protocol is distinct from Crab's long-running clean/smudge
`filter-process` protocol v2. It is also distinct from Crab's lazy pointer
checkout: `crab clone` does not request a Git partial clone, while ordinary Git
can request one of the supported filter forms directly.

## Operations and repair

Direct pushes and protected/service publication paths rebuild the visibility
proof for each committed generation. Repack, history recovery, and
`crab metadb rebuild` do the same. `crab doctor --metadb` reports locator and
visibility coverage separately. `crab fsck --store` checks current and
historical proof roots; `crab fsck --store --repair` verifies the historical
pack closure and idempotently backfills missing roots. Until repair completes,
v2 stays disabled for that repository.
