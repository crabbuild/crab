# Client-side Git protocol v2 and partial-clone design

## Status and authority

- **Status**: Implemented client-side profile; provider/release qualification remains
- **Decision**: The architecture is entirely client-side
- **Planned against**: `origin/main` commit `8286b925`, 2026-08-14
- **Implementation plan**: `plans/013-add-git-protocol-v2-and-partial-clone.md`

This document is normative for Plan 013. It resolves conflicting legacy
language that either says Crab never performs a Git protocol role or proposes
using gitoxide client transport APIs after `stateless-connect`.

## Decision

Crab does not deploy or require a Git server. The locally installed
`git-remote-crab` child process temporarily performs the upload-pack side of
Git wire protocol v2 for the Git process that spawned it. Repository data and
coordination remain directly in object storage.

The word “server” is therefore avoided unless qualified as “Git protocol
role.” The preferred terms are:

- **Git process**: the user's ordinary `git` executable; protocol client.
- **local helper**: `git-remote-crab`, spawned on the same machine by Git.
- **local upload-pack session**: the temporary protocol-v2 role inside that
  helper after `stateless-connect git-upload-pack`.
- **object store**: S3/GCS/Azure/R2/MinIO remote system of record.
- **Crab service**: a deployed Crab-controlled network process. Plan 013 does
  not require or introduce one.

## Deployment boundary

```text
┌──────────────────────────── User machine ────────────────────────────┐
│                                                                     │
│  git clone/fetch/cat-file                                           │
│       │                                                             │
│       │ spawns local child; line protocol then pkt-line stdio       │
│       ▼                                                             │
│  git-remote-crab                                                    │
│    ├─ remote-helper adapter                                         │
│    ├─ local protocol-v2 upload-pack session                         │
│    ├─ generation-pinned planner and admission                       │
│    ├─ object range reader and delta reconstruction                  │
│    ├─ filtered pack producer                                       │
│    └─ bounded local cache / temporary files                         │
│       │                                                             │
└───────┼─────────────────────────────────────────────────────────────┘
        │ authenticated object-store API calls
        ▼
┌──────────────────── Object storage account ─────────────────────────┐
│ manifest, refs, pack inventory, packs, indexes, locators,           │
│ reachability/visibility evidence, commit graph, shards, xorbs       │
└─────────────────────────────────────────────────────────────────────┘
```

There is no Crab listener, HTTP smart-protocol endpoint, upload-pack daemon,
pack-generation service, callback, message queue, or service database. The
local helper makes outbound requests only to the configured object-store and
native credential endpoints already required by the selected provider.

## Why `stateless-connect` still applies

Remote-helper `stateless-connect` describes how Git hands its existing stdio
pipes to a helper that can speak the upload-pack wire protocol. It does not
require that helper to proxy a remote network server. In Crab, the helper
answers directly from the object-store-backed repository.

The current `gix_transport::client::Transport` scaffold is the wrong role:
`gix-transport` and `gix-protocol` fetch APIs model a client connecting to an
upload-pack server. Git is already that client. Crab instead uses
`gix-packetline` for framing and server-neutral gitoxide object/pack/traversal
mechanics while owning the local upload-pack state machine.

Official contracts to verify during implementation:

- Git remote-helper `stateless-connect`: https://git-scm.com/docs/gitremote-helpers
- Git protocol v2: https://git-scm.com/docs/gitprotocol-v2
- Git partial clone and promisor state: https://git-scm.com/docs/partial-clone

## State ownership

| State | Owner | Lifetime |
|---|---|---|
| Manifest, refs, canonical packs/indexes | Object store | Durable |
| Locator and reachability/visibility evidence | Object store | Durable, generation-covered |
| Push lock/CAS evidence | Object store/provider coordinator already selected by Crab | Operation/durable contract |
| Git refs, ODB packs, `.promisor`, partial-clone config | Local Git repository | Durable on user machine |
| Downloaded indexes, decoded objects, pack work files | Local Crab cache/temp directory | Bounded, recoverable |
| Wants/haves, ACK state, selected objects, sideband state | Local helper process | One stateless exchange/session |

No protocol session state is stored in a Crab service. Each new Git invocation
can reconstruct everything required from local Git state plus one pinned
object-store snapshot.

## Component design

### 1. Remote-helper adapter

`crab/src/git/remote_helper.rs` continues to own the outer line protocol. On
`stateless-connect git-upload-pack`, it:

1. resolves credentials and the concrete object-store layout locally;
2. pins the repository snapshot and validates required index coverage;
3. writes and flushes the helper acknowledgement;
4. permanently hands stdin/stdout to the local upload-pack session;
5. never returns to line-command parsing.

It advertises `stateless-connect`, not stateful `connect`. Push remains on the
existing helper `push` and manifest-CAS pipeline.

### 2. Local upload-pack wire adapter

The wire adapter owns pkt-line framing and protocol-v2 section ordering. It is
an in-process state machine, not a reusable network server framework. It:

- advertises the exact supported v2 capability matrix;
- dispatches `ls-refs` and `fetch`;
- enforces packet, argument, section, and response limits;
- maps semantic plans into acknowledgments, shallow information, and packfile
  sideband output;
- writes no diagnostics to stdout.

### 3. Generation-pinned read session

One session binds manifest generation, pack-index hash, refs, peeled refs,
HEAD, pack inventory, locator coverage, commit graph, and visibility evidence.
Any mismatch fails before ref advertisement or pack output. A concurrent push
may publish a later generation but cannot alter the session's selected view.

### 4. OID admission and visibility

Protocol-v2 wants and lazy promisor fetches are OID-first. Admission must prove
that each requested commit, tree, blob, or tag is reachable from the visible
refs in the pinned snapshot. Locator presence proves only where bytes live.

Hidden-ref enforcement is advisory when a user holds unrestricted raw bucket
credentials: such a user can bypass the helper and read storage directly.
Strong tenant or ref isolation requires scoped provider credentials or a
separately designed filtered object-store view, not a Crab protocol server.
The helper still applies hidden-ref policy to prevent accidental disclosure
and to preserve Git's interface contract.

### 5. Object-store Git reader

The reader resolves admitted OIDs through generation-covered locator rows,
coalesces range requests, reconstructs REF/OFS delta chains, and verifies CRC,
object type/size, and final OID. It has explicit per-object and per-session
budgets and checks cancellation between I/O and CPU stages.

It never downloads a whole canonical pack merely to claim filtered transfer
unless a reviewed performance decision says that is cheaper for the specific
unfiltered case. Such a choice must remain observable and cannot silently
acknowledge a partial-clone filter.

### 6. Pack planner and producer

The planner computes admitted wants minus proven common haves, applies shallow
and filter semantics, includes required tags and delta bases, and returns the
smallest shape consumed by the producer.

The producer runs locally, streams through bounded memory/temp space, verifies
the final pack checksum, and sends bytes to the Git process. Thin packs are
allowed only when external bases are proven present in client haves.

### 7. Local cache

Caching is optional and local. Cache keys include repository identity, object
OID, immutable pack identity, and coverage where applicable. Cache corruption
causes verification failure and refetch; it never changes repository truth.
No cache service may become required for correctness.

## Supported filter matrix

The development-line profile accepts only these bounded forms:

| Form | Semantics |
|---|---|
| `blob:none` | Omit ordinary blobs while retaining the reachable commit/tree/tag closure. |
| `blob:limit=<n>[kmg]` | Omit blobs whose size is at least `n`; suffixes use binary powers of 1024. |
| `tree:<depth>` | Retain tree/blob entries at tree-relative depth strictly below `depth`. |
| `object:type={tag,commit,tree,blob}` | Retain only the requested Git object type while traversing required commits and trees. |
| `sparse:oid=<full SHA-1>` | Read a visible specification blob and retain blobs selected by its sparse-checkout patterns. |
| repeated `filter` / `combine:` | Intersect nested filters after bounded percent decoding. |

Filter input is limited to 4 KiB, 16 members per combine, and eight nesting
levels. `sparse:path`, `blob:depth`, and other unlisted forms fail before
planning or object I/O. A gitlink in a tree is retained as tree metadata; its
submodule commit is not dereferenced from the superproject repository.

## Data flows

### Ref discovery

1. Git spawns the local helper.
2. Helper selects the object store and pins a snapshot.
3. Git requests `stateless-connect git-upload-pack`.
4. Local session advertises v2 and answers `ls-refs` from the pinned manifest.
5. Session terminates cleanly after Git's stateless exchange.

### Complete fetch

1. Git sends wants/haves and optional shallow arguments.
2. Local planner authorizes OIDs and computes the closure.
3. Local reader fetches verified ranges from object storage.
4. Local producer streams the pack to Git.
5. Git installs and verifies its local pack and updates refs.

### Filtered clone and fetch

1. Git sends one of the supported filters in a v2 fetch: `blob:none`,
   `blob:limit=<n>[kmg]`, `tree:<depth>`,
   `object:type={tag,commit,tree,blob}`, full-SHA-1 `sparse:oid`, or a bounded
   repeated/combine intersection.
2. Local planner parses and canonicalizes the filter before object I/O, then
   applies its omission policy to the generation-pinned object closure.
3. Local producer streams a filtered pack; `blob:limit` omits blobs at or above
   the limit, and `sparse:oid` selects paths from the visible specification
   blob. Tree gitlinks remain tree entries and are not dereferenced as
   superproject objects.
4. Git records promisor config and `.promisor` pack metadata locally.
5. Checkout may trigger later object requests.

### Lazy promised-object fetch

1. A local Git operation discovers missing promised OIDs.
2. Git spawns a new local helper and sends the batched OIDs.
3. New helper invocation pins the current valid repository snapshot.
4. Admission verifies each OID is still authorized.
5. Reader and producer return requested objects plus required bases.
6. Git installs another local promisor pack and resumes the original operation.

There is no callback from object storage and no long-running Crab daemon.

### Push

Push does not move to upload-pack or protocol-v2 receive-pack. The local helper
continues to generate/upload immutable data, acquire the existing per-ref
coordination, and CAS the manifest directly against object storage. Fetch-side
metadata such as locator and reachability coverage is published as part of the
same canonical generation contract.

## Correctness invariants

1. **Client-only path**: direct object-store clone/fetch/push never requires a
   Crab network service.
2. **One snapshot**: refs, graph, visibility, locator, and pack inventory come
   from one generation.
3. **Authorization before bytes**: no requested OID is read before admission.
4. **Verified reconstruction**: every object is CRC/OID verified or errors.
5. **Bounded execution**: packet, object, delta, memory, temp-disk, egress, and
   concurrency budgets are explicit.
6. **Stdout purity**: only helper or pkt-line protocol bytes reach stdout.
7. **Terminal takeover**: after `stateless-connect`, the line parser never
   regains stdin.
8. **No mid-session fallback**: a failed v2 exchange fails clearly; it does not
   restart as a complete legacy fetch.
9. **Promisor completeness**: every advertised filter supports omission,
   metadata, lazy fetch, maintenance, offline error, and downgrade behavior.
10. **Push continuity**: existing atomic helper push remains canonical.

## Resource and failure model

All heavy lifting consumes user-machine resources. Product behavior must make
that cost visible and bounded:

- range-read concurrency is capped and cancellable;
- pack generation uses bounded memory and recoverable temporary files;
- object-store throttling is retried under existing provider policy;
- insufficient local disk fails before ref update and reports required space
  when determinable;
- cancellation/disconnect stops new reads, closes metadata sessions, and
  removes only current-session temporary state;
- corrupt or stale remote metadata fails closed;
- object-store unavailability returns a retryable fetch error;
- existing local objects remain usable offline, while a missing promised
  object reports the unavailable remote without corrupting the ODB.

## Compatibility and rollback

- Older Git continues using the existing helper fetch capability.
- `stateless-connect` is advertised only when the full local v2 path is ready.
- There is no stateful `connect` or receive-pack takeover in the first profile.
- Once a released Crab version creates promisor clones, later versions must
  continue lazy promised-object service. Disabling new filtered clones is not
  sufficient rollback because existing clones would be stranded.
- A downgrade to a binary that cannot service existing promisor repositories
  must be detected and refused or accompanied by an explicit unfilter/hydrate
  migration.

## Performance contract

Client-side generation is acceptable only if qualification records:

- object-store bytes and full/range request counts;
- range coalescing and cache hit rates;
- objects planned, omitted, reconstructed, and sent;
- local peak RSS, CPU, temporary disk, and wall time;
- resulting local ODB size;
- comparisons with the legacy complete-pack path.

Fixtures must separate normal Git blobs, deep history, many small files, and
Crab pointer-heavy repositories. Pointer-heavy fixtures alone cannot prove Git
partial-clone savings because the Git blobs are already small pointers.

## Documentation reconciliation

Plan 013's Phase 0 must update these canonical surfaces in the same bounded
documentation commit before implementation proceeds:

| Document | Required correction |
|---|---|
| `crab/docs/design/technical-design.md:78` | Reconciled to “No deployed Git protocol server”; the local helper role and “no HTTP smart endpoint” remain explicit. |
| `crab/docs/design/technical-design.md:2278` | Replace the unresolved partial-clone note with the phased client-side design and release gate. |
| `crab/docs/architecture/gitoxide.md:24` | Reconciled to the local upload-pack state machine plus server-neutral gitoxide mechanics. |
| `crab/docs/architecture/git-integration.md:181` | Add separate legacy complete-pack and future client-side v2/range-pack flows. |
| Historical Gitoxide adoption design | Remove `gix_transport::client` as stateless-connect glue; diagram Git and local helper on the same machine. |
| Historical Gitoxide adoption requirements | Rewrite Req 2 around a Crab-owned local upload-pack session; `gix-protocol`/`gix-negotiate` client APIs are not owners. |
| Historical Gitoxide adoption tasks | Replace task 5 with local pkt-line, session, planner, pack, and E2E tasks. |
| Historical smart-HTTP parity design | Remove `connect`/receive-pack and the claim that v2 changes the outer helper command set; scope to `stateless-connect git-upload-pack`. |
| Historical smart-HTTP parity requirements | Prohibit advertising an incomplete capability or graceful full-fetch fallback after takeover. Separate SHA-256 work. |
| Historical smart-HTTP parity tasks | Keep v2 fetch in Plan 013 and move SHA-256 to its own follow-up. |
| Historical transport-gap design | Delete the claim that filtering while indexing a downloaded complete pack is partial clone. Point to the range reader and promisor flow. |
| Historical transport-gap requirements | Require actual omitted bytes, lazy retrieval, promisor metadata, and measured savings. Remove client-side `gix-protocol` ownership. |
| Historical transport-gap tasks | Reopen falsely completed filter tasks until real Git and RustFS lifecycle gates pass. |
| `crab/docs/guides/mount.md:51` | State that the development-line proof-gated profile is RustFS-qualified, link to the exact filter matrix, and retain the provider/release qualification caveat. |

The reconciled docs must distinguish Git wire protocol v2 from Git's
long-running clean/smudge filter-process protocol v2.

## Verification design

Release qualification is not complete until real Git proves the local-only path:

1. Run with no Crab service processes or service URLs configured.
2. Use RustFS/S3 as the only repository remote and record all network
   destinations; repository data traffic must go only to approved provider
   endpoints.
3. Prove v2 `ls-refs`, clone, fetch, shallow/deepen/unshallow, tags, every
   supported filter, lazy batched OIDs, checkout, fsck, GC/repack, and push.
4. Prove blobs are absent initially and byte-identical after lazy retrieval.
5. Prove hidden/dangling/arbitrary OIDs are denied before object range reads.
6. Kill/cancel at each session boundary and verify no metadata session, temp
   file, or child process leaks.
7. Record source SHA, binary digest, Git/provider versions, metrics, and a
   redaction check in retained release evidence.
8. Run the supported Git compatibility set (2.30.9, 2.40.4, 2.45.4, and the
   current runner Git) against the packaged Linux artifact, plus current Git
   on the packaged macOS and Windows artifacts.
9. Run the packaged artifact against the immediately prior tagged Crab binary;
   it must either service an authorized promised raw OID byte-identically or
   refuse before writing a pack or promisor sidecar, with hydrate/unfilter as
   the documented migration path.

## Rejected alternatives

- **Hosted upload-pack service**: violates the client-only product invariant.
- **Smart HTTP endpoint**: unnecessary for the `crab://` helper and adds a
  deployed data-plane dependency.
- **Client `gix-protocol` over helper stdio**: role inversion; Git is already
  the client after stateless takeover.
- **Acknowledge filter, then download complete packs**: not partial clone and
  provides no object-store savings.
- **Locator existence as authorization**: can expose unreachable or hidden
  objects through the Git interface.
- **Required cache service**: changes correctness topology and prevents direct
  object-store operation.
- **Stateful `connect`/receive-pack in the first release**: replaces the proven
  push path without delivering value required for v2 fetch.

## Done condition

The design is delivered only when a released local Crab binary, with no Crab
server running, can use ordinary Git against an object-store repository for
v2 ref discovery, complete fetch, the supported filter matrix, and lazy
promised-object retrieval while passing the correctness, security, resource,
performance, and rollback gates above.
