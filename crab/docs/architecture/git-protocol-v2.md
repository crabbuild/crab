# Git protocol v2 and partial-clone boundary

Crab's Git wire protocol v2 implementation is client-side. Git starts the
local `git-remote-crab` process, sends the terminal
`stateless-connect git-upload-pack` command, and then speaks protocol v2 over
the same stdio. The helper reads the pinned repository directly from the
configured object store. Crab does not deploy an upload-pack listener,
receive-pack service, HTTP smart endpoint, callback, queue, or protocol
gateway.

This document describes the implemented profile on the current development
line. The machine-readable support authority is
[`git-capability-matrix.json`](git-capability-matrix.json), with the generated
human view in
[`git-capability-matrix.md`](git-capability-matrix.md). Only matrix cells marked
`supported` are compatibility promises; provider and platform rows marked
`preview` remain qualification work rather than inferred support.

The ownership decision is recorded in
[ADR: Keep Git protocol v2 inside the local helper](../design/adr-git-protocol-v2-local-helper.md).

## Implemented profile

### Bucket-only user contract

The deployment target is Git plus the installed `git-remote-crab` helper,
object-store credentials, and a qualified bucket. No Crab data server is
required. "S3-compatible" alone is insufficient: the provider must preserve
the conditional-create/CAS, range-read, listing, and durability contracts in
the [storage layer](storage-layer.md). Provider and operating-system support
still follow the evidence matrix above.

Git owns local operations such as commit, merge, rebase, stash, and worktree
management; Crab owns remote discovery, object transfer, and ref publication.
The real-Git lifecycle runner additionally exercises independent-client
`pull --rebase` and `pull --ff-only`, notes round trips, pushes from linked
worktrees, recursive bucket-backed submodules, and bare mirror clones. These
checks supplement rather than replace the version/provider qualification gates.

The helper supports `git push --dry-run`, including atomic mixed create,
force-update, and deletion previews. Git checks its advertised ref snapshot,
ancestry, and lease expectations before submitting the proposed batch. Crab
returns preview statuses before opening staging, preparing protected write
sessions, acquiring write locks, uploading objects, or publishing refs. A
preview is not a commit receipt or a guarantee of later authorization or
concurrency outcomes. User-installed client hooks still follow Git's own
execution rules; this does not promise to undo their side effects.

Push publication requires complete remote ancestry. Local shallow markers,
grafts and replacement refs cannot change pack contents, visibility evidence or
fast-forward checks. A commit with missing parents cannot become a remote root.
A first push with unavailable ancestors fails before refs change. An incremental
push can reuse ancestry already published at its excluded remote tip. Crab leaves
local Git history settings unchanged. The supported shallow/deepen fetch profile
below retains its explicit client boundary.

Recursive submodule operations need explicit trust for a custom transport,
as required by [Git's protocol policy](https://git-scm.com/docs/git-config#Documentation/git-config.txt-protocolallow).
For repositories whose submodule URLs have been reviewed, scope that trust to
the invocation rather than allowing every protocol globally:

```bash
git -c protocol.crab.allow=always clone --recurse-submodules crab://bucket/team/repo
```

"All commands" is not a support claim. Signed push certificates, push options
intended for server-side hooks, and remote archive service need separate protocol contracts.
Signed commits/tags are Git objects and are distinct from signed push requests.
PRs, CI execution, and mandatory server-side hooks cannot execute inside a
passive bucket. Clients with unrestricted bucket write credentials can bypass
client-side policy; mandatory branch protection needs a separate trusted
authorization boundary and is not part of the bucket-only deployment promise.

### Fetch and discovery

Repository discovery requires an authenticated Capsule v2 root. A Crab pack v1
layout or manifest cannot substitute for that authority. Replica readiness also
requires primary and replica Capsule views, including when reusing readiness
cache entries; missing roots never select manifest/index probing. Initialized
empty Capsule repositories remain valid. Classic Git helper
fetch commands remain available for authenticated layered cold clones; Git's
wire protocol version is independent of Crab's stored repository format.
Read routing retains the selected store, layout and verified Capsule root
together. A root captured from primary before replica selection cannot supply
the replica's upload-pack view: it can name replaced checkpoint objects absent
from a replica that has passed current-state readiness. Primary push discovery
keeps its own captured authority. Classic helper capability, list and fetch
batches also share their selected repository; read ref views cannot supply
primary push admission.
The session supports:

- protocol-v2 capability advertisement;
- `ls-refs` with ref prefixes, symrefs, peeled tags, unborn HEAD, and hidden
  refs;
- `fetch` with wants, haves, done, tags, sideband/progress, shallow/deepen,
  `deepen-relative`, `deepen-since`, and `deepen-not`;
- the bounded filter matrix: `blob:none`; `blob:limit=<n>[kmg]`; `tree:<depth>`;
  `object:type={tag,commit,tree,blob}`; `sparse:oid=<full SHA-1>`; and
  repeated filters or percent-encoded `combine:` intersections;
- `thin-pack` and `ofs-delta` request options are accepted; authenticated
  external bases are eligible only for a complete, unfiltered, non-shallow
  transition. Other responses remain self-contained;
- standard local Git pack installation on the v2 path.

The line helper normalizes `--shallow-since` date expressions through Git's
date parser before passing epoch seconds to the shared planner. Date and
ref-exclusion selectors apply to requested tips as well as their parents.
If no commits qualify, fetching fails before pack installation or shallow-file
publication; it does not substitute a depth-one checkout.

An initialized empty Capsule root is a valid ref snapshot. Empty clones and
`ls-remote` complete through v2; clients requesting `unborn` receive the
configured HEAD target unless hidden. Object wants against that empty snapshot
are rejected before pack production.

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
proof's complete per-ref closure directly. Each monotonic ref update retains a
bounded transition from recent prior tips to the current tip, so an unfiltered
incremental fetch can select the proven `want - have` closure without walking
the complete object graph. A rewrite or deletion without an exact transition,
shallow or depth request, filter, or want that is not an exact ref target uses
the bounded traversal planner. Pack generation reads up to the operation's default
10,000-object bound as one locator batch so adjacent pack ranges can be
coalesced; fetched-byte and inflated-byte budgets remain the memory and I/O
bounds. Locator batches spanning at least one exact-read wave and at least half
of the pinned pack inventory use one ordered SlateDB scan, clipped to the
requested SHA-1 range. The scan abandons itself and returns to exact reads if
stale rows would make it examine more than twice the requested object count,
so sparse and stale-heavy repositories remain bounded.

For a layered capsule's ordinary unfiltered fetch, the same authenticated
transition chain can end negotiation as soon as it covers every visible want
from client haves. The helper sends `ready` with the pack, but no individual
ACK for a historical have that may no longer be visible. Hidden, unknown,
ambiguous, or incomplete chains do not grant early completion; shallow,
filtered, and tag-expanded requests retain their existing complete-admission
path. The response still requires full pack planning and budget checks.

Complete Capsule metadata supplies the materialized visibility proof for
filtered, shallow and tag-sensitive requests. The shared planner applies its
bounded visibility-selection and traversal paths. Its separate catalog-ordinal
planner is not a wire-path promise for Capsule repositories.

Failures detected before the `packfile` response section use Git's terminal
`ERR` packet. Failures after that section begins use sideband channel 3. This
keeps request rejections distinguishable from truncated pack generation.

Each wire session captures its physical origin, authenticated root and stable,
epoch-filtered ref heads with their committed multi-ref activations. Discovery
reads that ref state without requiring checkpoint or capsule payloads. Fetch
opens its metadata only when requested. Ordinary layered fetches can keep the
large visibility body cold; a request needing complete visibility promotes from
the same capture. A concurrent push may advance a ref without changing root
generation, so the root digest alone cannot pin this transition.

Every requested OID is admitted from the captured visibility proof before
object traversal or response-pack production. Complete proofs retain one binary
SHA-1 dictionary with sparse-or-bitmap ref closures. Ordinary tip-bound fetches
use authenticated transitions only under the exact advertised-tip and request
shape restrictions above. Missing dependencies and corrupt metadata fail the
fetch; upload-pack does not read a manifest-only layout, repair a locator,
compact a ref journal, or migrate old visibility keys.

Upload-pack admission is repository-scoped and distributed: each helper
process must hold one of the fixed object-store read leases for the duration
of its session. A rotated, jittered retry probes one slot at a time, leases
renew while the session is active, and normal completion or cancellation
releases the slot through the existing holder-checked lock path. A crashed
helper leaves a bounded TTL lease for reclamation. This bounds aggregate
provider pressure across helpers while retaining the per-process remote-Git
object and range-read budgets.

Git owns the local promisor/filter configuration and installs the initial
protocol-v2 response. For a later raw-OID request, Git re-enters Crab through
the line-oriented helper fetch command. Crab pins one authenticated capsule
view, authorizes the OID against visible-ref closure, generates the selected
pack, and uses Git's standard pack layout with an atomically installed
`.promisor` sidecar; it does not invent a second repository configuration or
object format.

Complete, incremental, filtered, shallow and promised-object fetches resolve
the common Git directory before installation. Linked worktrees share its object
database, pack-install lock, configuration and shallow boundary; their private
Git directories retain worktree-specific index and HEAD state. This also applies
to lazy policy-file reads from a VFS publication worktree.
Push staging and incremental state derive their shared root from that selected
metadata too, including GIT_DIR-only invocations from another repository.

Rollback qualification accepts one of two explicit outcomes from the
immediately prior Crab binary: it services an authorized promised raw OID with
byte identity, or it rejects the request before writing response bytes,
`.pack`, or `.promisor` files. A downgrade that takes the refusal path must
hydrate or unfilter the repository while a compatible binary is still
available; it must never silently install a complete fallback pack.

The line-oriented helper remains available for older Git and for authenticated
complete-pack installation. Both wire profiles require Capsule storage authority.
Push uses the helper and Capsule per-ref publication; there is no receive-pack
takeover.

## Explicitly unsupported

The first profile rejects stateful `connect`, `packfile-uris`, `object-info`,
`ref-in-want`, and other capabilities not
listed above. A v2 session cannot fall back to the line protocol after the
terminal handoff; it reports the protocol failure instead.

This wire protocol is distinct from Crab's long-running clean/smudge
`filter-process` protocol v2. It is also distinct from Crab's lazy pointer
checkout: `crab clone` does not request a Git partial clone, while ordinary Git
can request one of the supported filter forms directly.

## Operations and repair

Protocol qualification separates ref discovery from fetch admission and
steady-state filter reads. A fresh clone must read the published Capsule state
without first repairing canonical storage. Fetching back into the writer is
insufficient: Git may skip transfer when a tip is already local. Filter reads
leave canonical object counts and bytes unchanged; generated response-pack
cache and coordination objects are accounted for separately. Read admission
and response caching still use their existing object-store permissions.

The add/commit/push RustFS smoke exercises the current physical fault boundary:
loss of a published capsule and corruption confined to its embedded Git pack
or index range. Each rejection probe uses an empty cache and requires no local
HEAD or checked-out payload. It compares the remote root and ref-head bytes
before and after the fault; discovery can itself reject a damaged capsule,
so successful `ls-remote` is not the mutation oracle.

Direct pushes and protected/service publication carry authenticated visibility
with immutable Git members before per-ref CAS makes a transaction visible.
Protected receive extracts closure from its verified materialization ODB before
releasing that workspace. Upload-pack consumes this evidence; it does not
publish a replacement when evidence is missing.

Explicit maintenance and recovery own canonical verification and publication.
`crab metadb rebuild` verifies embedded visibility and the full content closure
before publishing a checkpoint. `crab doctor --metadb` reports Capsule authority
and read-only local cache health. Missing or corrupt authority is an error, and
a stored pack-v1 manifest cannot replace it.

When replication is configured, protocol-v2 capability admission and the
terminal `stateless-connect` upload-pack session bind to the same selected
store and repository prefix. The helper does not reselect between advertising
and serving the immutable repository view.
