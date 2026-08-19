# ADR: Keep Git protocol v2 inside the local helper

- Status: Accepted for the development-line profile
- Date: 2026-08-16
- Scope: Git wire protocol v2 fetch and the bounded partial-clone filter matrix

## Context

After Git selects remote-helper `stateless-connect git-upload-pack`, Git is
already the protocol client. The `git-remote-crab` child must provide the
upload-pack protocol role for that invocation while reading the repository
from object storage. The existing gitoxide transport APIs model the opposite
client role and cannot own this exchange.

## Decision

The local Crab helper owns the bounded protocol-v2 pkt-line state machine,
generation-pinned admission, remote range reads, traversal, and pack
production. It writes only protocol bytes to stdout and terminates with the
Git invocation. Crab deploys no upload-pack listener, smart-HTTP endpoint,
protocol gateway, callback, queue, or service database for this path.

The first profile advertises only the proof-gated
`stateless-connect git-upload-pack` path and supports `ls-refs`, `fetch`,
shallow/deepen, tags, sideband, and the bounded filters `blob:none`,
`blob:limit=<n>[kmg]`, `tree:<depth>`, `object:type={tag,commit,tree,blob}`,
full-SHA-1 `sparse:oid`, and repeated/combine intersections. It accepts Git's
`thin-pack` and `ofs-delta` request options but emits a self-contained,
non-delta pack until an external-base proof is implemented. Stateful `connect`,
receive-pack takeover, `packfile-uris`, `object-info`, `ref-in-want`,
`sparse:path`, `blob:depth`, and other unlisted filter forms remain
unsupported. A failed terminal session does not fall back to the line parser.

Filter parsing is bounded and follows Git's intersection semantics. `blob:limit`
retains blobs strictly smaller than the limit, `tree:<depth>` uses a
non-negative decimal depth, and `sparse:oid` requires a visible blob identified
by a full SHA-1.

One session binds refs, peeled refs, pack inventory, locator coverage, commit
graph data, and the all-object visibility proof to one manifest generation and
pack-index hash. Every want, traversal child, and lazy raw OID is admitted
before its bytes are read. The standard Git process owns promisor pack
installation and configuration on the local repository.

An unfiltered fresh fetch of exact visible ref targets may plan directly from
the proof's complete per-ref closure. Negotiated, shallow, filtered, tag-expanded,
and arbitrary-object requests retain bounded traversal. Pack generation batches
the default operation object bound for range coalescing while preserving the
existing aggregate byte budgets. Dense locator batches use a read-ahead range
scan instead of one SlateDB point lookup per object. The scan is selected only
when the request covers at least half of the pinned pack inventory and falls
back to point lookups after two rows examined per requested object; smaller and
sparse batches keep the exact-key path.

## Consequences

- Direct object-store operation remains the canonical and complete topology.
- Older Git versions retain the existing line-oriented helper path.
- Provider and release qualification are required before this development-line
  capability can be described as released support.
- Once a released binary creates promisor repositories, later binaries must
  continue servicing authorized promised-object wants or refuse downgrade
  before installing a repository that would be stranded. The retained release
  smoke accepts either byte-identical raw-OID service or a zero-byte refusal
  with unchanged pack and `.promisor` state, and records which mode ran.

## Rejected alternatives

- A hosted upload-pack service violates the client-only deployment boundary.
- `gix-protocol`/`gix-transport` client APIs invert the roles after Git has
  already become the protocol client.
- Acknowledge unsupported filters and download complete packs would be a
  false partial-clone contract.
