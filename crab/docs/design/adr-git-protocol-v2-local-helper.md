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
before its bytes are read. On the terminal wire path, standard Git owns
promisor pack installation and configuration on the local repository.

The layered cold-clone optimization uses classic helper fetch to
reuse authenticated pack bodies and indexes. Git sends capabilities before filter
options, so that optimization cannot assume an unfiltered request. Classic
capsule fetch retains the same parsed filter AST and invokes the canonical
upload-pack planner for filtered, shallow, and hidden-ref-constrained requests. The helper installs
filtered packs with `.promisor` markers; Git still owns partial-clone remote
configuration. An unsupported filter fails closed instead of returning a
complete pack. Footer-only discovery must load visibility before planning and
reject changed captured ref positions rather than mixing generations.

Direct installation stages every member before exposing any pack, verifies
the downloaded index union against the authenticated visible-object proof,
and checks captured ref and peeled tips. Repeated objects and identical packs
are deduplicated for proof and installation, respectively. A checkpoint-only
candidate is ineligible when newer ref transactions exist. Native connectivity
remains required when physical packs retain extra unreachable objects. Git's
[single keep-file contract](https://github.com/git/git/blob/v2.50.1/connected.c)
selects an installed pack containing every requested
tip; tips spanning packs require only a small tip pack, never a full repack.

Classic fetch dispatch owns one renewable shared-reader ticket across full,
constrained and raw-object reads, with release on success and failure. The
direct installer cannot bypass admission or acquire separate tickets per layer.

The helper session owns one Git runtime across wire and classic fetch. On EOF,
protocol error, or cancellation it finishes or drops active operation contexts,
then awaits runtime shutdown before exiting Tokio. Shared response-pack
producers may outlive a cancelled waiter, but not their helper process. A lost
producer or cache-reader lease cancels its child work and drains cleanup before
holder-checked release; lease release must not race an unfinished writer.

An unfiltered fresh fetch of exact visible ref targets may plan directly from
the proof's complete per-ref closure. Negotiated, shallow, filtered, tag-expanded,
and arbitrary-object requests retain bounded traversal. Pack generation batches
the default operation object bound for range coalescing while preserving the
existing aggregate byte budgets. Locator batches use bounded read-ahead scans
when their request density or active SST fan-out makes one ordered pass cheaper
than independent SlateDB point lookups. Sparse batches retain the exact-key
path, and every scan falls back to exact gets if it would read beyond the
pinned catalog row bound.

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
