# crab — Continuity: Git Storage With an Object-Store Write-Ahead Log

**A technical design for Crab's next-generation repository storage system,
achieving behavioral parity with Cursor's Continuity architecture as published
in [Git at any scale](https://cursor.com/blog/git-at-any-scale) (Vicent Martí,
Aug 2026).**

-----

## Document Metadata

| Field        | Value                                                                    |
|--------------|--------------------------------------------------------------------------|
| Project      | crab                                                                     |
| Scope        | Continuity-parity Git storage: S3 WAL source of truth, NVMe warm caches, CAS-linearized pushes, gossip-assisted replication, primary-only compaction |
| Status       | Approved design, pre-implementation                                      |
| Companion to | `push.md`, `technical-design.md`, `../architecture/storage-layer.md`, `../architecture/coordination-consistency.md` |
| Model        | https://cursor.com/blog/git-at-any-scale                                 |
| Version      | 1.0                                                                      |

-----

## Table of Contents

- [1. Overview](#1-overview)
- [2. Parity Traceability Matrix](#2-parity-traceability-matrix)
- [3. Background: Why Git Hosting Resists Distribution](#3-background-why-git-hosting-resists-distribution)
- [4. Architecture Overview](#4-architecture-overview)
- [5. Object Storage Layout](#5-object-storage-layout)
- [6. Write-Ahead Log Design](#6-write-ahead-log-design)
- [7. Push Path](#7-push-path)
- [8. Reference Transactions](#8-reference-transactions)
- [9. Read Path](#9-read-path)
- [10. Replication and Gossip](#10-replication-and-gossip)
- [11. Routing and Membership](#11-routing-and-membership)
- [12. Primaries and Consensus-Free Linearization](#12-primaries-and-consensus-free-linearization)
- [13. Compaction](#13-compaction)
- [14. Materialization and Eviction](#14-materialization-and-eviction)
- [15. Failure Modes, Recovery, and Provenance](#15-failure-modes-recovery-and-provenance)
- [16. Observability and Operations](#16-observability-and-operations)
- [17. Performance Targets](#17-performance-targets)
- [18. Security](#18-security)
- [19. Crab Layering: Serverless Now, Fleet Later](#19-crab-layering-serverless-now-fleet-later)
- [20. Implementation Mapping](#20-implementation-mapping)
- [21. Phased Delivery and Test Strategy](#21-phased-delivery-and-test-strategy)
- [22. Explicit Unknowns and Documented Deviations](#22-explicit-unknowns-and-documented-deviations)

-----

## 1. Overview

Continuity is Cursor's Git storage system. Its published architecture rests on
one idea: **a write-ahead log in S3-compatible object storage is the source of
truth for a Git repository; on-disk repositories are a warm cache**. Every
guarantee Cursor advertises follows from that inversion of the Spokes model,
where disk copies were the truth and consistency had to be manufactured around
them.

This document specifies a Crab implementation with **behavioral parity**: every
architecture choice and every externally observable guarantee from the blog is
reproduced precisely. Where Cursor did not publish internals (exact schemas,
wire cadences, batch windows), this document makes the decision explicitly,
records the rationale, and marks it in [Section 22](#22-explicit-unknowns-and-documented-deviations).
We do not claim wire compatibility with Cursor's deployment; we claim parity of
behavior, guarantees, and scaling shape.

### 1.1 The invariant

Everything in this design serves one sentence, stated twice in the source
material:

> **Always correct when degraded, always fast when healthy.**

Concretely:

- **Correctness floor (degraded mode).** A lost gossip datagram, a failed-over
  node, a cold cache, or a partition must never produce a stale read, a lost
  push, or an inconsistent view. All correctness derives from S3, which every
  participant can reach directly.
- **Performance ceiling (healthy mode).** When nothing is wrong, gossip makes
  replicas proactive, rendezvous routing keeps repos hot on the right nodes,
  batching amortizes S3 latency, and pushes run at disk speed. Degradations
  cost performance, never correctness.

### 1.2 Goals

1. Linearizable pushes: every push is totally ordered, acknowledged only after
   full durability in the WAL, and visible atomically.
2. Fully consistent reads from any replica at any time, verified against the
   source of truth with sub-10ms freshness probes in the common case.
3. Horizontal scalability in both directions: hundreds of replicas for a busy
   monorepo, one replica (or zero — pure rematerialization-on-demand) for idle
   agent-created repositories.
4. No external database, no routing table service, no consensus protocol, no
   elected primaries. Object storage plus ordinary Git repositories only.
5. Full provenance: every push and every repack is recorded; any replica can be
   rewound or fast-forwarded to any prior state; upstream Git bugs can be
   pinpointed and reverted.
6. Off-the-shelf Git everywhere: all Git operations run against normal
   repositories on local disks using upstream tooling. No forked Git, no
   custom object formats reachable by clients.
7. Behavioral parity with the published Continuity model, audited row-by-row in
   [Section 2](#2-parity-traceability-matrix).

### 1.3 Non-goals

1. Wire/format compatibility with Cursor's production deployment. Their
   internal protobuf schemas, gossip cadence, and batch tuning are not public;
   we define our own equivalents ([Section 22](#22-explicit-unknowns-and-documented-deviations)).
2. Replacing Git's client-facing protocol. Clients speak ordinary Git; packs go
   over the wire exactly as Git demands. Only server-side storage changes.
3. Distributed filesystems, distributed hash tables, or any design that puts
   Git's DAG walk on the network hop-by-hop. The prior-art postmortems in
   [Section 3](#3-background-why-git-hosting-resists-distribution) close these doors.
4. A relational database for refs or anything else (the Azure DevOps trade-off;
   see [Section 3.4](#34-prior-art-azure-devops-packfiles-in-blobs-refs-in-a-rdbms)).
5. Cross-provider transactionality. The WAL lives in one bucket per
   deployment; multi-bucket replication is out of scope.
6. Migrating existing Crab deployments in this document. Migration sequencing
   is flagged in [Section 19.4](#194-migration-framing) and planned separately.

### 1.4 Relationship to existing Crab surfaces

Crab already ships several primitives this design builds on rather than
replaces:

| Existing primitive | Location | Role in Continuity |
|---|---|---|
| `cas_update` ETag conditional-write loop | `crates/crab-storage/src/cas.rs` | The linearization primitive for WAL index updates |
| `RefJournalTransaction` / `RefJournalHead` prepare–publish | `crates/crab-metadata/src/ref_journal.rs` | Reference transactions inside WAL entries; local visibility mechanics |
| Provider-neutral `Store` + `StoreLayout` | `crates/crab-storage` | All object-store access, path routing, error classification |
| Staging, chunking, dedup data plane | `crates/crab-staging`, `crates/crab-xet` | Unchanged; orthogonal to repository truth |
| Lock-then-push coordination | `crates/crab-coordination` | Optional accelerator; not required for correctness under the WAL |

Where Continuity's model supersedes current behavior (the single manifest CAS
as the only publication point), the change is explicit in
[Section 19](#19-crab-layering-serverless-now-fleet-later).

-----

## 2. Parity Traceability Matrix

Every architecture decision and guarantee stated in the source blog, mapped to
the section of this document that implements it. This matrix is the audit
instrument for "100% behavioral parity": a review touches a row, checks the
section, done.

| # | Source claim / guarantee | Where implemented |
|---|---|---|
| 1 | Packfiles are the irreducible network unit; within the server "Linus is not going to come over and check" | §3.1, §5 |
| 2 | Prior art: JGit DHT fails (DAG round-trips), GitHub NFS/GFS/DRBD fail (FS semantics), Spokes succeeds then hits 3PC ceiling/floor | §3.2–3.5 |
| 3 | Spokes' three optimal choices retained: don't distribute Git itself; real Git repos on NVMe; replicate with consistency | §4.3, §7, §10 |
| 4 | A push has two components: packfile + reference transaction; commits invisible until a ref points at them | §6.2, §7, §8 |
| 5 | Pack fan-out is unsynchronized; only the small ref transaction synchronizes — against one local repo, not a quorum | §7.2 |
| 6 | **Never acknowledge a push until fully persisted to the WAL** | §6.4, §7.2 step 7 |
| 7 | Each push is stored as its own WAL entry; pushed packfile written to disk and uploaded to S3 simultaneously | §6.2, §7.2 steps 3–5 |
| 8 | Visibility gate: successful local ref-transaction prepare **and** pointer recorded in the WAL index → forces linearizability of all pushes | §6.4, §7.2 step 6, §8 |
| 9 | Not one S3 write per push; tuned batching avoids the PUT-latency throughput cap; ingest as fast as disk allows | §6.5, §17 |
| 10 | Repositories live "anywhere"; treated like a warm cache; system stateless; no routing tables, no relational DB | §11.1 |
| 11 | Rendezvous hashing maps repo ID → ranked node list; all routing state is repo ID + healthy-node set | §11.2 |
| 12 | No elections, no consensus for primaries; any server can act as primary; S3 atomic CAS serializes; preferred primary = rendezvous rank 1; CAS retries delay pushes but stay correct | §12 |
| 13 | Optimistic replication via UDP gossip; unreliable transport irrelevant; packets carry only catch-up hints | §10.2 |
| 14 | Replica freshness: conditional GET with cached ETag; 304 (metadata-only, <10ms) → serve immediately; 200 → catch up from WAL, then serve | §9.2 |
| 15 | Everything built on top (agents, web UI, RPC surfaces) sees a globally consistent view | §9.1, §17.2 |
| 16 | Elasticity both directions: monorepos across hundreds of replicas; millions of agent repos at one replica; idle repos garbage-collected and rematerialized on demand | §10.4, §14 |
| 17 | Primary-only compaction applying to on-disk repo **and** WAL; replicas follow compaction events through the WAL; replicas download compacted packs instead of repacking (bandwidth for CPU); compaction frontier; no Spokes-style maintenance failovers | §13 |
| 18 | Linear read scaling to ≥100 replicas without push regression; ~120 pushes/s on S3 Standard; >300 pushes/s on S3 Express One Zone; compaction becomes the ceiling; future disk-layout work must not relax durability/consistency | §17 |
| 19 | Full provenance: track every fundamental operation; rewind and fast-forward every replica; pinpoint and revert Git bugs | §15.4 |
| 20 | "Always correct when degraded, always fast when healthy" | §1.1, §15 |
| 21 | Agent-era load: giant enterprise monorepos *and* vast numbers of tiny throwaway repos motivate bidirectional elasticity | §3.5, §16.1, §19 |

Auditor's note: rows 6, 8, 12, and 14 are the load-bearing guarantees. If a
change breaks any of them, it is not a tuning change — it is a redesign.

-----

## 3. Background: Why Git Hosting Resists Distribution

This section condenses the published reasoning because every Continuity
mechanism exists to dodge one of these walls. Readers who know the history can
skip to [Section 4](#4-architecture-overview).

### 3.1 Packfiles: the irreducible unit

Git stores objects (blobs, trees, commits, tags) content-addressed by SHA-1,
compressed into **packfiles**. Two properties dominate everything:

1. **Packfiles are both storage and networking.** The Git protocol transfers
   packs on push and fetch regardless of how a server stores data internally.
   You cannot change the wire format without changing every client.
2. **Packfile physical layout is adversarial to random access.** Objects are
   placed by a size-minimizing heuristic, mostly stored as deltas on top of
   other objects, with no correlation to DAG topology. Reading one object =
   logical hops through the graph *plus* physical hops through delta chains in
   the pack. Every nontrivial Git operation is a random walk across gigabytes.

Within your own server you may store anything however you like — but the wire
speaks packs, and whatever you store must eventually become packs cheaply.
Continuity therefore keeps packs as its data plane end-to-end: pushed packs are
stored as-is, replicated as-is, and repacked only at controlled points.

### 3.2 Prior art: distributing Git itself (object-level KV)

Git's content-addressed store maps naturally onto a distributed key-value
store: key = SHA-1, value = object. It does not work. The repository is a
directed acyclic graph; every operation walks it pointer-by-pointer, and the
value of the next pointer is unknown until the previous object is fetched. Put
that behind a network round trip per hop and trivial operations collapse.

Shawn Pearce's JGit DHT backend at Google proved the point empirically: the
system worked for normal operations, but `git clone` performance — forced
through pack generation over the DHT — was bad enough to discard the design.

**Decision:** no object-level distribution. The unit of storage and transfer is
the packfile, never the individual object.

### 3.3 Prior art: GitHub and distributed filesystems

GitHub's early scaling attempts kept the Rails monolith unchanged and tried to
make the *filesystem* distributed: NFS (fastest failure), GFS2 (short-lived),
DRBD (longer-lived, still terrible). Root cause: Git assumes local-filesystem
semantics — locking, tearing, syncing, fsync ordering — tuned for a laptop,
with no attention to network behavior. Combined with §3.1's random-access
pattern, networked filesystems crawl unless the whole pack set is locally
cached, and at six-figure repository counts caching everything is impossible.

GitHub ultimately moved repositories onto dedicated fileservers behind an RPC
layer, then solved replication with Spokes.

**Decision:** never put Git's working set on a networked/block filesystem.
Local NVMe or bust — and when local NVMe is absent (Layer A, serverless),
materialize it on demand ([Section 14](#14-materialization-and-eviction)).

### 3.4 Prior art: Azure DevOps — packfiles in blobs, refs in a RDBMS

Azure DevOps stores packfiles in blob storage and references in Microsoft SQL
Server. It works and scales. The trade-off is operational: a relational
database becomes a hard dependency for every reference transaction — another
stateful system to run, back up, and fail over. Cursor rejected this because
Git-data consistency outweighs the convenience, and Continuity inherits that
rejection.

**Decision:** no external database anywhere in the truth path. The only
linearization point is a conditional write (CAS) on one object-storage key.

### 3.5 Prior art: Spokes, and why its two flaws matter here

Spokes (~2013, GitHub) became the industry pattern: application-level
replication of whole Git repositories onto NVMe disks, kept fully consistent.
Its three choices were proven optimal and Continuity retains all of them:

1. Don't distribute Git itself; operate at the packfile level.
2. Store plain Git repositories on local NVMe.
3. Replicate data, but keep all copies consistently in sync — because Git
   clients, CI fleets, and humans genuinely cannot tolerate eventual
   consistency (push-then-fetch misses, CI runners cloning stale trees).

Consistency was bought with **three-phase commit** on each push's reference
transaction: fan out packs unsynchronized (invisible until refs update), then
run 3PC on the small, cheap ref update. Elegant — and, thirteen years later,
doubly flawed:

- **Ceiling too low.** 3PC latency is bound by the slowest participant (the
  tail at scale). More replicas → worse push throughput. Enterprise monorepos
  now need dozens-to-hundreds of replicas for CI traffic. Three-phase commit
  cannot get there.
- **Floor too high.** Agents create vast numbers of tiny, barely-touched,
  often-throwaway repositories. 3PC forces ≥3 live replicas for each, forever,
  because dropping below quorum would risk data loss.
- **Operational tax (pets, not cattle).** On-disk copies are the source of
  truth, so every copy is precious: a routing database maps every repo to
  every replica, checksums are continuously maintained, repairs are urgent,
  and two corrupt copies out of three freeze pushes entirely (no quorum).

Continuity's answer is structural, not incremental: move the source of truth
out of the disks into the object store, and consistency stops being something
you manufacture with consensus — it falls out of the WAL.

-----

## 4. Architecture Overview

### 4.1 Components

```
                    ┌──────────────────────────────────────────────┐
                    │            OBJECT STORE (S3-compatible)      │
                    │            SOURCE OF TRUTH                   │
                    │                                              │
                    │  {repo}/wal/{seq}.entry     ← one per push   │
                    │  {repo}/wal/{seq}.pack      ← immutable pack │
                    │  {repo}/index.json          ← CAS'd, ETag'd  │
                    │  {repo}/snapshot/{seq}      ← compaction cut │
                    └───────▲──────────────▲───────────────▲───────┘
                            │              │               │
             persist-before │   catch-up / │  freshness    │ compaction
             ack (CAS)      │   replay     │ probe (304)   │ events
                            │              │               │
              ┌─────────────┴──┐   ┌───────┴────────┐  ┌───┴────────────┐
              │  STORAGE NODE  │   │  STORAGE NODE  │  │  STORAGE NODE  │
              │  (primary-ish) │   │   (replica)    │  │   (replica)    │
              │                │   │                │  │                │
              │  NVMe bare     │   │  NVMe bare     │  │  NVMe bare     │
              │  Git repo      │◄──┼──(warm cache)  │◄─┼──(warm cache)  │
              │  (warm cache)  │   │                │  │                │
              └────────▲───────┘   └───────▲--------┘  └───────▲────────┘
                       │                   │                   │
                  UDP gossip hints (lossy, optimization-only)
                       │                   │                   │
              ┌────────┴───────────────────┴───────────────────┴────────┐
              │   CLIENTS: git (fetch/clone/push), agents, web UI,      │
              │   REST/RPC surfaces — all see the consistent view       │
              └─────────────────────────────────────────────────────────┘
```

Three roles, one process type. There are no special binaries: a *primary* is
just whichever node currently accepts pushes for a repo (any node may; see
[Section 12](#12-primaries-and-consensus-free-linearization)), a *replica*
serves reads. Both are ordinary hosts with an NVMe-backed bare Git repository
acting as a **warm cache** of WAL state.

### 4.2 Data flow summary

- **Push:** client speaks Git to a node → `git receive-pack` into the local
  bare repo → pack uploaded to S3 and entry written concurrently with local
  commit → ref transaction prepared locally → entry pointer appended to the
  index via one CAS → ack. Never acked before full persistence ([Section 7](#7-push-path)).
- **Read:** node probes freshness with a conditional GET on the index (cached
  ETag) → 304 means serve from disk; 200 means apply new WAL entries, then
  serve ([Section 9](#9-read-path)).
- **Replication:** gossip datagrams hint "repo R is at seq N" so replicas can
  pull proactively instead of lazily; loss changes timing, never outcomes ([Section 10](#10-replication-and-gossip)).
- **Compaction:** primary-only geometric repack produces new packs + a
  snapshot + a compaction event in the WAL; replicas consume it like any other
  entry ([Section 13](#13-compaction)).

### 4.3 What is deliberately inherited from Spokes

Plain Git repositories on NVMe, operated with upstream tooling. This buys:

- All Git performance work upstream, free, forever.
- No forked object formats, no custom readers on the client path.
- Product effort goes to features, not to maintaining weird repositories.

### 4.4 What replaces Spokes

| Concern | Spokes | Continuity |
|---|---|---|
| Source of truth | On-disk replicas | WAL in object storage |
| Consensus | 3PC per push | Single-object CAS per batch |
| Primary election | Required | None; any node, CAS resolves races |
| Routing | External DB table | Rendezvous hashing, stateless |
| Corruption handling | Repair race against quorum loss | Evict local copy, rematerialize |
| Min replicas per repo | 3 (quorum) | 0 (pure rematerialization is valid) |
| Max replicas per repo | Tail-bound (~3–few) | Arbitrary (S3 absorbs fan-out) |

-----

## 5. Object Storage Layout

All keys are relative to a deployment bucket and a `{repo}` prefix (repository
ID assigned at creation). Layout is owned by `crab-storage`'s `StoreLayout`
routing, extended with WAL paths; callers never format these strings.

```text
{repo}/index.json                     WAL index (the linearization point)
{repo}/wal/{seq}.entry                WAL entry: canonical JSON record for push/compaction event
{repo}/wal/{seq}.pack                 Immutable packfile blob referenced by an entry
{repo}/snapshot/{seq}.json            Compaction frontier snapshot (materialization shortcut)
{repo}/meta/repo.json                 Repository identity, creation time, class hint
```

Rules:

1. **Immutability.** `wal/*` and `snapshot/*` objects are write-once. Entries
   are created with strict create-if-absent semantics; a retry that loses a
   race picks the next sequence. Nothing ever rewrites a persisted entry.
2. **Sequence space.** `{seq}` is a zero-padded u64, dense per repository,
   allocated only by successful index CAS (see §6.4). Lexicographic order of
   keys equals logical order.
3. **Digest discipline.** Every entry records the blake3 of its payload and a
   chain digest binding it to its predecessor; packs record blake3 of their
   bytes. Verification is mandatory at every materialization and catch-up
   (byte-identical reconstruction is a workspace invariant).
4. **No directories, no listings on the hot path.** Reads are driven by the
   index; nothing enumerates prefixes during push/fetch. Listing happens only
   in offline GC tooling.
5. **Encoding.** Canonical JSON (sorted keys, fixed number formatting) hashed
   with blake3 — the established Crab contract style. This deviates from
   Cursor's `.pb` protobuf naming by deliberate choice; rationale in
   [Section 22](#22-explicit-unknowns-and-documented-deviations).

### 5.1 Why the index is small

The index holds only pointers (sequence ranges → entry keys) plus the
frontier marker — not payloads. Its size grows with sealed WAL segments, and
[Section 13](#13-compaction) folds consumed segments away, so index CAS
payload stays bounded regardless of repository age.

-----

## 6. Write-Ahead Log Design

### 6.1 The core primitive

A repository's history is an append-only log of **entries** in object storage.
The log is authoritative: if an operation is not in the WAL, it did not
happen; if it is in the WAL, every reader will eventually — and after one
freshness probe, immediately — see it.

Two object kinds per push:

1. `{seq}.pack` — the pushed packfile bytes, immutable.
2. `{seq}.entry` — the record: ref transaction + pack manifest + chain digest.

Plus one mutable object, mutated only by CAS:

3. `index.json` — the ordered list of pointers into the log.

### 6.2 Entry format

```jsonc
// {repo}/wal/00000000000000000042.entry
{
  "version": 1,
  "sequence": 42,
  "kind": "push",                          // "push" | "compaction"
  "transaction_id": "blake3:9f41…",        // blake3 over canonical transaction JSON
  "transaction": {
    // RefJournalTransaction shape from crates/crab-metadata/src/ref_journal.rs:
    // version, parents (per-ref expected-old), edits[], head?, packs[], shards[]
    "version": 1,
    "parents": { "refs/heads/main": "blake3-or-sha-of-old-value" },
    "edits": [ /* RefJournalEdit entries */ ],
    "packs": [ /* PackManifestEntry for each new immutable pack */ ]
  },
  "packs": [
    {
      "pack_id": "blake3:1c88…",
      "key": "{repo}/wal/00000000000000000042.pack",
      "size_bytes": 184273,
      "content_digest": "blake3:1c88…"
    }
  ],
  "previous_entry_digest": "blake3:e0d2…",  // chains to entry seq 41
  "writer_node": "node-7f3a",
  "written_at": "2026-08-21T09:15:04.112Z"
}
```

Design points:

- **One entry per logical operation** (push or compaction event), matching the
  source model where each push is its own object. Idle repositories therefore
  have exactly one entry per push with no batching overhead.
- **`previous_entry_digest` chaining** makes the log tamper-evident end-to-end:
  materialization verifies the whole chain, so a truncated, reordered, or
  spliced log cannot pass validation.
- **`parents` carries expected-old values**, reusing `RefJournalTransaction`
  semantics. This is what makes CAS-based linearization equivalent to Spokes'
  prepare-lock-with-expected-value check ([Section 8](#8-reference-transactions)).
- **Compaction events** (`"kind": "compaction"`) are ordinary entries whose
  payload describes a frontier move; they replay like everything else
  ([Section 13](#13-compaction)).

### 6.3 Index format

```jsonc
// {repo}/index.json
{
  "version": 1,
  "head_sequence": 42,
  "head_entry_digest": "blake3:9f41…",
  "segments": [
    // sealed entry ranges, oldest first; consumed ranges fold away at compaction
    { "first_sequence": 0,  "last_sequence": 31 },
    { "first_sequence": 32, "last_sequence": 42 }
  ],
  "compaction_frontier": {
    "sequence": 31,
    "snapshot_key": "{repo}/snapshot/00000000000000000031.json"
  }
}
```

The index is the **only mutable truth object** and the **linearization
point**: an entry exists logically iff a pointer range covering it is present
in a successfully committed index version. Its ETag is the cluster-wide
freshness token used by reads (§9.2) and gossip hints (§10.2).

### 6.4 Linearizability argument

Claim: pushes to a repository are linearizable; all readers observe the same
total order.

1. Each index update is a single conditional write keyed on the previous
   version's ETag (`cas_update`, `crates/crab-storage/src/cas.rs`). S3 (and
   every API-compatible store Crab supports) provides strong consistency for
   read-after-write and compare-and-swap on a single key.
2. Therefore index versions form a total order, and each accepted batch of
   entries occupies a contiguous sequence range in exactly one index version.
3. A push becomes visible at the moment its range lands in the committed
   index — atomically for every observer, because visibility is defined *as*
   index membership, not as local disk state.
4. Expected-old verification happens against that same order: a push prepared
   against stale refs fails its parents' check at commit time and retries
   rebased on the new state ([Section 12](#12-primaries-and-consensus-free-linearization)).
5. Durability precedes visibility: entry objects (and pack blobs) are fully
   uploaded before any index CAS can reference them, so a visible entry is
   always fully persisted. Hence **ack ⇒ durable**, and durable-but-invisible
   is possible while the reverse is not.

This is the published guarantee set verbatim: never ack before full persist;
all pushes linearized; every view consistent.

### 6.5 Batching (group commit)

Per-push cost on the critical path would otherwise be one sequential PUT round
trip — on busy repositories, PUT latency caps throughput exactly as the source
warns. Continuity batches:

```
push A ─┐
push B ─┼─► concurrent work: upload A.pack/B.pack/A.entry/B.entry ─┐
push C ─┘                                                          │
                     ┌─────────────────────────────────────────────┘
                     ▼
        ONE index CAS appending ranges for A, B, C together
                     │
                     ▼
        ack A, ack B, ack C  (each only after ITS OWN entry was durable)
```

Rules:

- The batching window is bounded (default 5 ms; deployment-tunable). A lone
  push pays one window plus one CAS — identical to unbatched behavior.
- Entries and packs are uploaded concurrently during the window; the window
  delays *commit*, never durability bookkeeping.
- A push's ack waits for: its own pack+entry durability **and** inclusion in a
  committed index version. Nothing else.
- CAS contention between concurrent batches resolves by retry with jittered
  backoff (existing `cas_update` behavior: base 50 ms, cap 500 ms); losers
  re-read the new index and re-append their still-uncommitted ranges. Entries
  already uploaded are never rewritten — sequence allocation happens inside the
  winning CAS, so no uploaded entry is ever stranded under two sequences.
- Ingest rate target: bounded by local disk speed and Git compaction, not by
  S3 latency ([Section 17](#17-performance-targets)).

### 6.6 What batching must never do

- Never let one push's failure fail unrelated pushes in the same batch
  individually — a failed push simply does not appear in the next CAS attempt;
  others proceed.
- Never publish a partial batch: a committed index version covers whole
  ranges; there is no such thing as half an append.
- Never reorder within a batch: sequences are allocated contiguously in the
  single winning CAS.

-----

## 7. Push Path

### 7.1 Entry conditions

The pushing party is either (a) a storage node receiving client Git traffic or
(b) a Layer A client process pushing directly against the bucket
([Section 19](#19-crab-layering-serverless-now-fleet-later)). Both run the same
state machine; only the transport differs.

### 7.2 State machine

```
 git push
    │
    ▼
[1] RECEIVE      git receive-pack into NVMe bare repo (quarantine active);
    │            thin-pack fixup; new objects land in quarantine dirs
    ▼
[2] EXTRACT      compute closure delta → PackManifestEntry list;
    │            build RefJournalTransaction (edits + expected-old parents)
    ▼
[3] UPLOAD       upload {seq-candidate}.pack to S3  ──┐
    │            (strict create; retry-safe)          │ concurrent with
    ▼                                                 ▼
[4] PREPARE     prepare ref tx locally: verify expected-old values against
    │           the LOCAL VISIBLE STATE, hold refs in prepared (invisible)
    │           journal-head state
    ▼
[5] FRESHEN     freshness probe (conditional GET on index, §9.2).
    │           If 200: catch up first (apply newer entries), re-verify
    │           parents, rebase transaction if needed → back to [4].
    ▼
[6] COMMIT      group-commit window → ONE index CAS appends this push's
    │           range (with co-batched peers). Conflict → refetch index,
    │           rebase, retry (bounded attempts; then surface conflict).
    ▼
[7] PUBLISH     apply committed tx locally (journal heads move; refs become
    │           visible on THIS node instantly)
    ▼
[8] ACK         respond to git client. Only now. Never earlier.
    │
    ▼
[9] GOSSIP      hint emission (async, best-effort): "repo R head=N etag=E"
```

Notes on individual steps:

- **Step 1 quarantine:** upstream `git receive-pack` already quarantines
  incoming objects until refs accept them; we rely on that rather than invent
  staging semantics, keeping step 7 rollback trivial (drop quarantine dir).
- **Step 4 vs Step 6 ordering:** preparing locally before the global commit
  means the primary's own repo is always ready to serve the instant the index
  moves; it also means expected-old failures surface before we spend the CAS.
- **Step 5 is mandatory even on the preferred primary.** Local state is a
  cache; correctness demands checking the truth before committing. On a quiet
  repo the probe is a 304 (<10 ms, metadata-only).
- **Step 6 conflict loop** implements exactly the published race animation:
  RACE → CONFLICT → REFETCH → REBASE → RETRY → DONE. Bounded by
  `DEFAULT_MAX_ATTEMPTS` (10) with existing jitter policy; exhaustion returns
  a typed conflict error to the client (non-atomic ref update rejected; the
  user re-runs push, which is standard Git UX).
- **Steps 3–4 concurrency:** the pack upload overlaps local preparation; on
  healthy networks the window is dominated by receive-pack itself.

### 7.3 Crash windows

| Crash after | On-disk/S3 residue | Recovery action | Client sees |
|---|---|---|---|
| [1] mid-receive | Partial quarantine dir | Drop quarantine on next touch; nothing leaked | Push failed, retry |
| [3] partial pack upload | No strict-create success → absent/incomplete blob ignored | Re-upload on retry; incomplete blobs GC'd (§14.3) | Push failed, retry |
| [4] prepared, not committed | Prepared journal heads locally | Heads expire; prepared tx invisible by design (marker never written) | Push failed, retry |
| [5] freshened/rebased | Same as [4] | Same | Push failed, retry |
| [6] CAS succeeded, node died pre-[7] | **Push IS committed globally** | Any replica applying the entry publishes it; original node's local heads catch up via normal follow | Ack lost → client retries; retry lands as fast-forward no-op or rejected non-fast-forward — both correct |
| [7] applied, died pre-[8] | Committed globally | Nothing to do | Ack lost → same as above |

The load-bearing row is the sixth: once the CAS wins, the push is real
everywhere forever, regardless of what the crashing node did afterward. This
is the practical meaning of "the WAL is the truth".

### 7.4 Cancellation and cleanup

Every acquired lock, quarantine directory, open segment buffer, and staged
upload handle is released on success, error, cancellation, and timeout — the
workspace-wide invariant. Concretely: receive-pack child processes are killed
and reaped on drop; journal-head prepares carry expiry; strict-create uploads
are idempotent by content address. No path leaves background tasks holding a
repo lock behind a dropped session.

-----

## 8. Reference Transactions

### 8.1 Reusing the journal contract

Crab's ref journal (`crates/crab-metadata/src/ref_journal.rs`) already
implements exactly the semantics Continuity needs, so the design adopts it
verbatim rather than inventing parallel machinery:

| Journal concept | Continuity role |
|---|---|
| `RefJournalTransaction` (immutable body, blake3 `id()`, sorted canonical edits) | The payload inside every WAL entry |
| `parents` map (expected-old per edited ref) | The CAS precondition evaluated at commit |
| `RefJournalHead { committed_transaction, prepared_transaction }` | Per-ref local visibility state on each node's materialized repo |
| Prepared-state-invisible-until-marker | Step [4] of the push machine; crash-safe by construction |
| Atomic visibility marker | Local publish in step [7] |

### 8.2 Two-level visibility

There are two visibility domains and they must never be confused:

1. **Global (truth):** index membership. Once a committed index version
   contains an entry's range, the push is visible to every correct observer,
   forever. This level has no locks — only the CAS.
2. **Local (cache):** the materialized repo's journal heads. A node publishes
   locally after global commit (primary, step [7]) or during catch-up
   (replica). Local heads can lag truth arbitrarily; they can never lead it,
   because prepare happens against freshened state and publish follows a won
   CAS.

Consequence: a stale node is *slow*, not *wrong*. Serving rules enforce this
(§9.2) so lag is never observable.

### 8.3 Multi-ref atomicity

A transaction touching N refs commits atomically globally (one range in one
index version) and applies atomically locally (journal marker flip). Partial
application is impossible at either level. HEAD retargets ride the same
transaction (`head` field) as today.

### 8.4 Relationship to lock-then-push

The workspace invariant "lock-then-push serialization per ref" exists to
prevent lost updates across racing writers. Under Continuity:

- Correctness no longer depends on external locks: expected-old parents +
  single-CAS commit provide serialization.
- Existing coordination backends (`crates/crab-coordination`) may still be
  deployed as a *contention reducer* for high-traffic refs (fail fast instead
  of CAS-retry loops), but they are an optimization with their existing
  release-on-every-exit-path obligations — never a correctness dependency.

This mirrors the source model: Spokes needed its quorum machinery for
correctness; Continuity needs only S3.

-----

## 9. Read Path

### 9.1 The consistency contract

Every read surface — `git fetch`, `git clone`, agent RPC, web UI, REST —
observes the same repository state at the same moment. There is one allowed
answer to "what is head_sequence": whatever the latest committed index says,
as of a freshness probe taken within this request. No surface may serve from
local state without either a 304 probe or having applied the newest entries
already.

### 9.2 Freshness probe

Each replica caches `(head_sequence, index_etag)` from its last contact with
the index. On any read request:

```
replica                        object store
   │  GET index.json               │
   │  If-None-Match: <cached etag> │
   │ ─────────────────────────────►│
   │                               │
   │  304 Not Modified             │   ← metadata-only, ~<10 ms typical
   │ ◄─────────────────────────────│      no payload transfer
   │  → serve read from NVMe       │
```

```
   │  GET index.json               │
   │  If-None-Match: <stale etag>  │
   │ ─────────────────────────────►│
   │  200 OK + new index + new ETag│
   │ ◄─────────────────────────────│
   │ → catch up (§10.3), then serve│
```

Rules:

- **304 path is the fast path** and dominates in healthy clusters because
  gossip keeps replicas proactive (§10.2); the probe merely proves it.
- **200 path cost is proportional to actual lag**, which gossip minimizes;
  correctness does not depend on gossip having fired.
- The probe is unconditional per read request. There is no trust interval, no
  "probably fresh" window, no lease arithmetic — that is what makes eventual
  consistency impossible here rather than merely rare.
- Probe failure (S3 unavailable) fails closed: the node serves a typed
  unavailability error rather than unverified local state. Degraded mode is
  honest about degradation.

### 9.3 Clone vs fetch

Both funnel through the same catch-up-then-serve pipeline. Clones additionally
benefit from snapshots (§13.3): a clone materializing from a frontier snapshot
downloads one consolidated pack set plus tail entries rather than replaying
every historical pack.

### 9.4 Why building on top becomes trivial

Agents, CI schedulers, web UIs, and REST services read through this same
pipeline, so they inherit the global order without bespoke cache-coherence
code: after a successful push response, a subsequent read on *any* node
returns a view containing that push. This is precisely the property the source
credits for making upstream infrastructure easy.

-----

## 10. Replication and Gossip

### 10.1 The model: optimistic, self-verifying

Replication is *optimistic*: nodes tell each other "repo R moved" and everyone
verifies everything against the store anyway. There is no replication
protocol whose correctness matters — only timing optimization whose failure
is free.

### 10.2 Gossip datagrams (Layer B)

```
UDP, cluster-scoped, fire-and-forget:

GossipDatagram {
  cluster_epoch: u64,          // bumps on membership-affecting config change
  node_id: NodeId,
  membership_version: u64,     // sender's view counter
  hints: [ { repo_id, head_sequence, index_etag } ]   // bounded batch
}
```

Properties and rules:

- **Unreliable by design and by transport.** UDP datagrams may be lost,
  duplicated, reordered, or delivered to nodes that no longer care. All four
  are harmless: a hint only says "you might want to look at R"; every consumer
  re-proves freshness via §9.2 before serving.
- **Loss changes latency, never correctness.** A replica that misses every
  hint still serves correctly on demand (probe → 200 → catch up), just with a
  colder cache.
- **No acks, no retries, no sequencing** at the gossip layer. If it were
  reliable, it would be a consensus protocol — which is exactly what this
  design exists to avoid.
- Emission is async post-ack ([Section 7](#7-push-path) step 9) so gossip
  never sits on the push critical path.

### 10.3 Catch-up protocol

A replica learning of `head_sequence = N` for repo R (via hint, via its own
§9.2 probe returning 200, or lazily on first touch):

1. Fetch committed index; identify entries in `(local_head, N]`.
2. Fetch each entry object + referenced pack blobs not present locally;
   verify blake3 digests and the entry chain.
3. Apply transactions in sequence order through the local journal
   (prepare → publish per transaction; multi-ref atomic).
4. Update cached `(head_sequence, etag)` atomically with local publication.

Catch-up is idempotent and resumable: partially applied ranges simply replay;
digest verification makes replays safe.

### 10.4 Elasticity both directions

Because replicas pull from S3 directly and S3 scales without configuration:

- **Up:** point N extra read-hungry nodes at a hot monorepo; each starts
  catching up independently; read throughput grows linearly until NIC/disk
  limits (verified to ≥100 replicas upstream; acceptance target §17).
- **Down:** a repository with no traffic needs **zero** replicas: its disk
  copies are evicted (§14.2) and rematerialized from WAL on next touch.
  Availability is decoupled from replica count because truth never lives on
  disks. This is what kills Spokes' "three idle replicas forever" floor for
  agent-created repositories.

-----

## 11. Routing and Membership

### 11.1 Stateless routing

Where does repository R live? **Anywhere.** Any node asked about any repository
can serve it: probe freshness, catch up if needed, serve. Routing therefore
answers a different question — not "where must R go" but "where will R
*probably already be*, so we skip a cold materialization":

> Rendezvous hashing over `(repo_id, healthy_node_set)` produces a ranked node
> list; pushes prefer rank 1; reads try ranks in order before falling back to
> any healthy node.

All persistent routing state is: the repo ID and the current healthy-node set.
There is no routing database, exactly as published ("no relational database to
operate").

### 11.2 Rendezvous hashing

```
score(node, repo) = blake3(node_id || repo_id)   // fixed seed order
ranked_list(repo) = sort_desc(healthy_nodes, by score)
preferred_primary(repo) = ranked_list[0]          // soft preference only
```

Properties we rely on:

- **Stable under churn:** adding/removing one node relocates only the repos
  whose top-ranked node changed — minimal reshuffling on deploy/failover.
- **Deterministic everywhere:** every node computes the same ranking from the
  same two inputs; no coordination needed to agree on placement.
- **Degradation-safe:** if views of the healthy set disagree transiently, the
  worst case is an extra materialization on a non-preferred node — wasted
  work, zero incorrectness.

Replica-count policy per repository (how far down the ranking to pre-place):
a small class hint stored in `{repo}/meta/repo.json` (`class: monorepo |
default | ephemeral`) chosen by operator tooling or automatic size heuristics;
monorepos target wide placement, ephemeral repos target 1. The hint tunes
*economics only*; correctness is identical at any count, including zero.

### 11.3 Membership (Layer B)

Nodes gossip liveness counters with their hints; a peer is unhealthy after a
bounded silence window (deployment-tunable default 10 s across K consecutive
misses). Membership changes bump `membership_version`, which flows through
datagrams so views converge quickly. Split-brain on membership is explicitly
acceptable: disagreeing views cause misrouted requests that still resolve
correctly (any-node-serves), costing only latency.

-----

## 12. Primaries and Consensus-Free Linearization

### 12.1 There is no election

The question "which server is the primary for repo R?" has the published
answer: **it doesn't matter.** No leases, no ballots, no terms. Any node that
can reach the bucket can run the push state machine; the single-object CAS is
the entire consensus machinery.

### 12.2 Why CAS suffices where 3PC was used

Spokes needed multi-node agreement because *replicas were the truth*: a
commit required N disks to agree. Continuity commits against one key whose
store guarantees compare-and-swap atomicity:

- Concurrent pushes race on one index ETag; exactly one wins per version;
  losers refetch/rebase/retry (§7.2 step 6). Total order falls out of S3's
  own serialization of conditional writes — the same primitive `cas_update`
  already exercises across the workspace.
- The "quorum" is conceptually of size 1: the store. Its availability and
  durability are the deployment's existing object-storage SLA.

### 12.3 The preferred primary is an optimization

Rank-1-in-rendezvous is *preferred* for pushes because it minimizes cold
materializations, not because it holds authority. During deploys, failovers,
or network blips, any healthy node takes pushes; correctness is unchanged;
only locality suffers (CAS retries may tick up under split preferences).
Healthy steady state = fast; degraded = correct — the standing invariant.

### 12.4 Contention behavior

| Scenario | Behavior |
|---|---|
| Two nodes push different changes to same ref concurrently | Both prepare locally; one CAS wins; loser's parents check fails → rebase onto new state → retry or surface conflict to client |
| Same change pushed twice (client retry after lost ack) | Second attempt computes identical transaction against post-first state; lands as no-op fast-forward or is rejected as non-fast-forward by expected-old check — both are correct outcomes |
| Batch collision during group commit | Whole losing batch reapplies as one unit at next sequence window; no partial appends possible |
| CAS attempts exhausted | Typed conflict error to client; standard Git retry UX; no silent drop |

-----

## 13. Compaction

### 13.1 Two compaction problems, one mechanism

1. **WAL-side:** unbounded logs make full restores ever-costlier; consumed
   segments should fold away.
2. **Git-side:** every push adds a packfile; per-pack index lookups multiply
   until operations crawl; Git's incremental geometric compaction helps but
   eventually a real repack is due.

Continuity solves both with **one event type**, because the WAL already
carries everything: a compaction event is just another entry.

### 13.2 Primary-only execution

```
primary (rank-1 node)                          replicas
     │
     ├─ choose cutoff seq C (all committed ≤ C)
     ├─ git repack geometric over packs ≤ C     │  (pushes keep flowing past C)
     ├─ upload consolidated pack(s) + snapshot/C.json
     ├─ emit compaction entry {C, new_packs, superseded, segment_fold}
     └─ ONE index CAS: append entry + move compaction_frontier
                                                   │
                                                   ▼
                                    catch-up sees compaction entry:
                                    download new packs once,
                                    apply frontier,
                                    delete superseded local packs
                                    (bandwidth-for-CPU trade — never repack)
```

Rules:

- **Only the preferred primary compacts**, killing the Spokes failure mode
  where concurrent maintenance on multiple replicas caused failovers.
- **Cutoff semantics:** the event consolidates strictly `≤ C`. Pushes landing
  after C continue appending normally; nothing about live traffic pauses.
- **Replicas never repack.** They consume compacted outputs through the normal
  catch-up path. CPU is spent once, bandwidth is spent N times — the explicit
  published trade.
- **Idempotency:** replaying a compaction event twice is a no-op (frontier
  monotonicity + digest checks).

### 13.3 Snapshots (the frontier shortcut)

`snapshot/{C}.json` records fully materialized state at C: refs, HEAD, the
consolidated pack manifest, and the folded segment range. Uses:

- Fast materialization: nearest-snapshot + tail replay instead of genesis
  replay (§14.1).
- Fast clones: consolidated packs serve most of the closure.
- Provenance anchor: snapshots name exactly which entries they absorb
  (§15.4).

### 13.4 Triggers

Compaction fires when any of: entries since last frontier > threshold
(default 256), pack-set bytes since last frontier > threshold, scheduled
interval on busy repos, or explicit operator command. All four converge on
the same event path; there is no special "GC mode".

-----

## 14. Materialization and Eviction

### 14.1 Materialization (replay)

A node needing repo R without a local copy:

```
1. GET index.json (fresh, §9.2)          → head H, frontier F
2. GET snapshot at/below F               → base state (or genesis if none)
3. fetch entries (F, H]; verify chain digests + pack blake3s
4. apply transactions in order through local journal
5. assert computed state_digest == digest implied by entry chain
   → mismatch is a hard error; never serve an unverified reconstruction
6. mark warm; serve
```

Byte-identical reconstruction or error — the workspace invariant, applied to
repository truth. Verification happens *before* any byte is served.

### 14.2 Eviction

Idle repositories are evicted from a node's disk after a TTL of no traffic
(default 24 h, tunable per class: ephemeral repos aggressively short).
Eviction is safe because disks hold only cache:

- The WAL retains everything needed to rebuild.
- In-flight reads finish before deletion (refcounted open sessions).
- Next touch rematerializes transparently (§14.1).

This is the mechanism that lets millions of agent-created repositories exist
at effectively zero steady-state disk cost while remaining fully durable and
instantly available.

### 14.3 Orphan collection

Artifacts that can outlive their usefulness: interrupted pack uploads
(strict-create never committed), prepared-but-uncommitted journal heads,
quarantine dirs from crashed receives. Each carries creation metadata;
offline tooling deletes them past a grace period (default 7 days), mirroring
the workspace rule that GC must never delete anything inside its grace
window. Orphans are never on any read path, so their removal is purely
reclamation.

-----

## 15. Failure Modes, Recovery, and Provenance

### 15.1 Failure matrix

| Failure | Immediate effect | Correctness impact | Recovery |
|---|---|---|---|
| Gossip datagram lost/dup/reordered | Replica colder than it could be | None (probe verifies; §9.2) | Automatic on next probe/hint |
| Node crash mid-push | See §7.3 window table | None; CAS boundary defines reality | Client retry; orphan GC |
| Node disk corruption (repo bytes) | Local copy unusable | None — evict + rematerialize from WAL | Self-healing on next touch |
| All replicas of a repo lost simultaneously | Cold repo | None — truth in S3 | Rematerialize anywhere |
| S3 brief unavailability | Pushes/reads fail closed with typed errors | No stale serves possible | Retry when store returns |
| Membership disagreement (split view) | Misrouting → extra materializations | None (any-node-serves) | Converges via membership_version |
| CAS attempt exhaustion under extreme contention | Push rejected with conflict | None; nothing half-applied | Client retry UX |
| Index object corrupt (store-level) | Detected by version+digest checks | Fail closed | Restore from deployment backups; entries themselves intact (immutability) |
| Upstream Git bug corrupts a pushed pack | Entry chain/pack digest verification fails at catch-up | Contained: bad entry identified precisely | Rewind past bad entry (§15.4); fix upstream; replay |

Compare Spokes' worst case — two of three disk copies corrupt ⇒ quorum gone ⇒
pushes frozen until manual repair. Here the same physical disaster costs one
rematerialization.

### 15.2 Why "correct when degraded" holds structurally

Every correctness claim reduces to one sentence: *truth is one immutable log
behind one CAS'd pointer, and every reader re-proves freshness against it.*
There is no protocol state whose loss could fork history: no leases to expire
wrongly, no quorum to lose, no routing table to go stale dangerously.
Degradations convert into latency (cold probes, catch-ups, retries) — never
into divergence.

### 15.3 Cancel-safety obligations

All async tasks (gossip listener, batch windows, compaction, catch-up) must
be cancellation-safe per workspace rules: dropped futures leave no held locks,
no half-published journal state, no un-reaped children. Group-commit windows
in particular must treat task cancellation as "this push exits the batch",
never as "batch aborts".

### 15.4 Provenance (full operation history)

Because every push and every repack is an immutable, sequenced, hash-chained
entry:

- **Audit:** for any sequence N, exactly what changed, who wrote it, when.
- **Rewind/fast-forward:** any replica can be pointed at any prior sequence
  and rebuilt deterministically (snapshots make this cheap near frontiers).
- **Bug pinpointing:** when upstream Git misbehaves, the offending entry is
  identified by sequence; revert = append a compensating transaction, keeping
  history append-only even for repairs.
- **Compaction does not destroy provenance:** folded segments are referenced
  by snapshots (which entries they absorbed); retention policy may archive —
  but never mutate — superseded entry objects within the audit horizon.

-----

## 16. Observability and Operations

### 16.1 Structured events (tracing)

Following workspace conventions (`tracing` at boundaries, structured fields):

```text
push_committed   { repo_id, seq_start, seq_end, attempts, batch_size, duration_ms }
cas_retry        { repo_id, attempt, delay_ms }          // from existing cas_update
freshness_probe  { repo_id, result: "304"|"200", latency_ms }
catch_up         { repo_id, from_seq, to_seq, packs_fetched, bytes }
materialize      { repo_id, base_snapshot, replayed_entries, state_digest_ok: true }
compaction       { repo_id, cutoff, packs_in, packs_out, bytes_saved }
evict            { repo_id, idle_days }
gossip_hint_rx   { repo_id, hint_head, local_head }      // debug level
```

Error paths log once at the boundary with source-preserved errors — never
stringified-and-discarded, never per-layer spam.

### 16.2 Health signals

- **Freshness lag** per replica: `head_sequence(store) − head_sequence(local)`
  (should be ~0 under gossip; spikes diagnose gossip or catch-up trouble).
- **CAS conflict rate** per repo (sustained elevation ⇒ hot-ref contention).
- **Probe p99**: the <10 ms metadata-only claim is a monitored SLO.
- **Materialization rate**: sustained non-zero on steady traffic means routing
  is mis-tuned (class hints or membership churn).

### 16.3 Deployment topologies

| Mode | Store | Notes |
|---|---|---|
| Standard | S3 Standard | Baseline; throughput target §17 |
| Low-latency | S3 Express One Zone | Same code path; PUT latency drops → push ceiling rises to Git compaction bound |
| Any-compatible | GCS/Azure/others via `crab-storage` providers | CAS semantics required; verified per provider |
| Serverless (Layer A) | Bucket only | No nodes at all; clients materialize locally (§19) |

Operational runbook deltas vs today: no new database to back up (there is
none), no repair-from-peer-replica procedures (rematerialization replaces
them), one new offline job (orphan GC §14.3).

-----

## 17. Performance Targets

The published numbers become acceptance criteria, not aspirations.

### 17.1 Push throughput

| Deployment | Published figure | Acceptance criterion |
|---|---|---|
| S3 Standard | up to ~120 pushes/s while compacting + replicating | ≥100 pushes/s sustained on synthetic load, all linearizable, ack-after-persist verified |
| S3 Express One Zone | >300 pushes/s, bottlenecked by Git compaction | ≥250 pushes/s with compaction pipeline active; bottleneck demonstrably local-disk/Git, not S3 |

Bottleneck honesty: when compaction binds, that is the published end-state;
future disk-layout work may raise it but must not relax durability or
consistency guarantees (explicit source commitment adopted here).

### 17.2 Read scaling

- Synthetic stress to **100 replicas**: read throughput grows linearly;
  zero push-throughput regression attributable to replica count.
- Freshness probe p99 < 10 ms in-region (metadata-only conditional GET).
- Post-push-read consistency: a read issued after an ack on any node observes
  the pushed refs (tested as a cross-node property, not per-node).

### 17.3 Latency budgets (healthy steady state)

| Operation | Budget |
|---|---|
| Fetch/clone serve after 304 probe | Local NVMe speed (Git-bound) |
| Catch-up of K missed entries | K × (entry fetch + apply), parallelized across pack fetches |
| Cold materialization (snapshot present) | Snapshot download + tail replay |
| Push overhead beyond receive-pack | One freshness probe + amortized CAS share + async upload overlap |

### 17.4 Load-shape coverage

Test matrix includes both published regimes: single giant monorepo (wide
replica placement, CI-scale fetch storms) and vast fleets of tiny ephemeral
repos (one-or-zero replicas each, churn-heavy create/idle/evict cycles).

-----

## 18. Security

1. **Credentials:** object-store credentials live only in storage-provider
   config paths that already exist (`crab-auth`, `crab-auth-store`). WAL
   entries, snapshots, logs, traces, and cache keys never contain credentials,
   tokens, or bucket-signing material — existing workspace rule, extended to
   all new objects and datagrams (gossip payloads carry no secrets by
   construction: IDs, sequences, ETags only).
2. **Integrity:** blake3 at every boundary — entry chain digests, pack
   content digests, snapshot state digests, transaction body hashes.
   Verification precedes service everywhere (§14.1 step 5).
3. **Tamper evidence:** hash chaining makes silent log rewriting detectable;
   combined with store-side immutability policies (object lock / retention
   where available), history rewrite requires breaking both.
4. **Least privilege:** nodes need read/write on the deployment prefix only;
   Layer A clients already operate under scoped credentials today.
5. **Transport:** client↔node Git transport keeps whatever TLS/auth Crab uses
   today (out of scope here); node↔store uses provider TLS; gossip is
   integrity-irrelevant (hints cannot forge truth — verification is against
   authenticated store reads), but datagrams are still cluster-scoped and
   contain no sensitive payload.
6. **Lockfile/dependency discipline:** new dependencies (any UDP/socket
   crates) follow the explicit-approval rule; nothing here requires new
   crypto primitives — blake3 and provider SDKs suffice.

-----

## 19. Crab Layering: Serverless Now, Fleet Later

Continuity as published assumes a node fleet. Crab is serverless today:
clients talk straight to object storage, and nothing on the data path listens
on sockets (only cache-service/VFS do). The design therefore ships in two
layers that share all core mechanics.

### 19.1 Layer A — serverless subset (deployable now)

The WAL model does not actually require a fleet; it requires the log and the
CAS. Layer A runs the identical state machines inside client processes:

- **Truth:** same bucket layout, same index CAS, same entry chain
  ([Sections 5–6](#6-write-ahead-log-design)).
- **Push:** the remote-helper push path executes §7.2 with the local
  worktree's repository playing the "NVMe warm cache" role; freshness probe
  and group commit included.
- **Read:** fetch/clone probe-and-catch-up against the bucket before serving;
  local repo materialized from WAL exactly as a node would (§14.1).
- **Consistency guarantee preserved:** two clients on two laptops get the
  same linearized history, because truth never left S3.

What Layer A lacks: proactive gossip, shared warm caches, fleet-wide read
scaling. What it keeps: every correctness property.

### 19.2 Layer B — node fleet (the full published system)

Adds: storage-node daemon composition crate (§20), UDP gossip, rendezvous
routing, preferred-primary placement, primary-only compaction service,
eviction/reaping. All Layer A mechanics are reused verbatim inside nodes;
Layer B is additive infrastructure around them.

### 19.3 Boundary table

| Capability | Layer A (serverless) | Layer B (fleet) |
|---|---|---|
| Linearizable pushes | ✅ | ✅ |
| Ack-after-full-persist | ✅ | ✅ |
| Consistent reads via ETag probe | ✅ | ✅ |
| Materialization from WAL | ✅ (client-local) | ✅ (node NVMe) |
| Compaction events in WAL | ✅ (client-triggered) | ✅ (primary-only service) |
| UDP gossip hints | ➖ | ✅ |
| Rendezvous routing / placement classes | ➖ | ✅ |
| Idle eviction + rematerialize-on-touch | ➖ (local GC only) | ✅ |
| Hundreds-of-replicas read scaling | ➖ | ✅ |

### 19.4 Migration framing

Current Crab publication uses the single manifest CAS as the only publication
point (`crab/docs/design/push.md`). Under Continuity the WAL index becomes the
repository-truth publication point for repos stored in Continuity mode; the
existing manifest/journal objects remain the per-repo metadata they are today
and are referenced by entries rather than replaced wholesale. Repositories opt
in per-repo (`meta/repo.json` marks the mode); mixed-mode buckets are valid;
conversion is itself expressed as WAL entries so provenance stays complete.
Detailed migration sequencing belongs to its own numbered plan under `plans/`
once implementation begins — deliberately out of scope here.

### 19.5 Invariant reconciliation

Existing workspace invariants under this design:

1. *SlateDB closed on every exit path* — untouched; metadata plane unchanged.
2. *Lock-then-push serialization* — satisfied structurally by CAS +
   expected-old (§8.4); coordination locks remain optional accelerators with
   unchanged release obligations.
3. *GC grace periods* — orphan collection honors them (§14.3).
4. *Byte-identical reconstruction or error* — enforced at every
   materialization/catch-up (§14.1).
5. *Staged xorbs flush before bundle push* — data-plane rule; orthogonal and
   unaffected (packs ride the same pipeline as today until publication).
6. *Shard terms cover ALL chunks* — xet-plane invariant; untouched.
7. *Staging chunks_for_file completeness* — staging invariant; untouched.

-----

## 20. Implementation Mapping

### 20.1 Crate placement

Per `crates/AGENTS.md` ownership rules — reusable mechanics in shared crates,
product wiring in binaries, server crates as top-level composition boundaries:

```text
crab-types ──► crab-storage ──► crab-wal ──► consumed by crab/ CLI + Layer B node
                     ▲              ▲
                     │              │ uses RefJournalTransaction types
              crab-metadata ────────┘   (crab-wal depends on crab-storage + crab-metadata;
                                          neither gains a WAL dependency)

Layer B only:
crab-continuity-server   composition boundary like crab-auth-server:
                         gossip listener, rendezvous router, compaction service,
                         eviction reaper, receive endpoint → binaries
```

Dependency rules honored:

- `crab-wal` owns WAL mechanics (entries, index CAS orchestration, replay,
  verification) but no provider policy (`crab-storage` owns transport/paths).
- `RefJournalTransaction` types stay owned by `crab-metadata`; `crab-wal`
  embeds them in entries rather than duplicating schemas.
- No lower crate imports `crab-wal`. Feature-gated provider tests follow the
  existing `--features` discipline; default features stay minimal.
- The node crate never pushes policy downward; `crab/src/cmd/*` decides when
  operations run.

### 20.2 Module sketch — `crates/crab-wal`

```text
src/
  lib.rs            // re-exports only what consumers need
  entry.rs          // WalEntry codec + chain digests
  index.rs          // WalIndex codec + cas_append / cas_read_frontier
  replay.rs         // snapshot+tail materialization, verification
  push.rs           // push state machine (§7.2) as a library flow
  read.rs           // freshness probe + catch-up (§9)
  compaction.rs     // cutoff selection + event emission (§13)
  error.rs          // thiserror WalError wrapping StorageError/MetadataError sources
```

### 20.3 Type sketches

Shapes only — canonical JSON via serde, version constants, no panics outside
tests, mirroring `ref_journal.rs` conventions:

```rust
pub const WAL_ENTRY_VERSION: u32 = 1;
pub const WAL_INDEX_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WalEntry {
    pub version: u32,
    pub sequence: u64,
    pub kind: WalEntryKind,                       // Push | Compaction
    pub transaction_id: String,                   // blake3 of transaction body
    pub transaction: RefJournalTransaction,       // owned by crab-metadata
    /// Pack blobs referenced by this entry; strict-created before commit.
    pub packs: Vec<WalPackRef>,
    /// Digest of the previous entry; genesis is a fixed constant.
    pub previous_entry_digest: String,
    pub writer_node: String,
    pub written_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WalIndex {
    pub version: u32,
    pub head_sequence: u64,
    pub head_entry_digest: String,
    /// Sealed contiguous ranges, oldest first; folded at compaction.
    pub segments: Vec<SegmentRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_frontier: Option<CompactionFrontier>,
}
```

Key functions (signatures convey contracts; bodies per referenced sections):

```rust
/// Appends committed ranges via one conditional write. Linearization point.
pub async fn cas_append(
    store: &Store,
    layout: &StoreLayout<Store>,
    repo: &RepoId,
    batch: EntryBatch<'_>,
) -> Result<CommittedRange, WalError>;

/// Conditional GET against cached ETag. 304-equivalent => Ok(Unchanged).
pub async fn probe_freshness(
    store: &Store,
    layout: &StoreLayout<Store>,
    repo: &RepoId,
    cached: CachedHead<'_>,
) -> Result<Freshness, WalError>;

/// Snapshot + tail replay with full verification. Byte-identical or error.
pub async fn materialize(
    store: &Store,
    layout: &StoreLayout<Store>,
    repo: &RepoId,
    target: MaterialTarget<'_>,   // Head | Sequence(u64) for rewind
) -> Result<MaterializedRepo, WalError>;
```

`probe_freshness` requires one small `crab-storage` extension: a conditional
GET variant passing `If-None-Match` through to `object_store::GetOptions`
(304 mapped to a distinct typed result, not an error).

### 20.4 Layer B composition sketch — `crab-continuity-server`

```text
src/
  gossip.rs      // UDP datagram encode/decode, listener task (cancel-safe)
  membership.rs  // healthy-set tracking, membership_version accounting
  routing.rs     // rendezvous scoring, ranked lists, class-aware placement
  serve.rs       // Git transport endpoints delegating into crab-wal flows
  compact.rs     // preferred-primary compaction scheduler (§13)
  evict.rs       // TTL reaper honoring in-flight refcounts (§14.2)
```

Binaries follow `crab-auth-server` precedent: thin `main` wrappers over the
composition crate; all logic testable as library code.

### 20.5 What deliberately does not get built

No routing database, no consensus library, no lease manager, no replication
queue, no custom pack format, no forked Git. Every one of those is a
component the architecture exists to avoid; adding any back would be a
regression to Spokes' cost structure.

-----

## 21. Phased Delivery and Test Strategy

### 21.1 Phases

| Phase | Deliverable | Proof gate |
|---|---|---|
| P1 | `crab-wal` core: entry/index codecs, CAS append, replay + verification | Unit + property tests on InMemory store; chain-tamper detection proven |
| P2 | Push/fetch integration (remote-helper path, Layer A) | Cross-process linearizability suite; crash-window table exercised via fault injection |
| P3 | Compaction events + snapshots + frontier | Replay equivalence pre/post compaction (byte-identical); orphan GC grace respected |
| P4 | `crab-continuity-server`: gossip, membership, routing, eviction | Fleet-in-a-box integration tests incl. lossy/duplicated/reordered hint injection |
| P5 | Scale + chaos qualification vs §17 criteria | Benchmark harness report meeting acceptance table; chaos matrix green |

Each phase lands independently usable (P1–P3 = complete Layer A product).

### 21.2 Test conventions

Property tests named for the invariant they protect (workspace style):

```text
replay_is_deterministic_and_byte_identical_across_replica_orders
concurrent_pushes_serialize_into_contiguous_total_order
lost_duplicated_reordered_gossip_hints_cannot_stale_serve
compaction_event_preserves_visibility_of_all_prior_commits
evicted_repo_rematerializes_byte_identically_on_next_touch
cas_exhaustion_surfaces_conflict_without_partial_publication
entry_chain_detects_truncation_splice_and_reorder
```

Crash-window tests inject failure at each §7.3 row boundary and assert both
residue correctness and client-visible semantics. Concurrency suites use
multi-thread tokio runtime per workspace convention. Snapshot fixtures are
never edited to silence failures.

### 21.3 Verification commands

Narrow-first, per scoped guidance:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-wal --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo clippy -p crab-wal -p crab-continuity-server --all-targets -- -D warnings
```

Broad workspace, live-provider, and fleet-scale proof runs in CI or the
dedicated environment once narrow gates pass.

-----

## 22. Explicit Unknowns and Documented Deviations

Cursor published the architecture, not the internals. Where this design had
to decide, it decided — visibly.

### 22.1 Deliberate deviations from visible Cursor choices

| Choice | Cursor (as published/inferred) | Crab decision | Rationale |
|---|---|---|---|
| Serialization format | Protobuf (`gitwal.pb` naming) | Canonical JSON + blake3 IDs | Workspace-wide contract style; matches `ref_journal.rs`, keeps codecs inspectable |
| Ref machinery | Unpublished | Reuse `RefJournalTransaction` prepare/publish verbatim | Semantics already exist, tested, and match the published model exactly |

### 22.2 Internals we specified without upstream reference

| Internal | Our spec | Marked because |
|---|---|---|
| Batch window length | 5 ms default, tunable (§6.5) | Cursor states batching exists, not its shape |
| Segment folding policy | Thresholds in §13.4 | Same |
| Gossip cadence/payload limits | Bounded hint batches, fire-and-forget (§10.2) | Cadence unpublished; payload shape certainly theirs differs |
| Membership liveness window | 10 s / K misses default (§11.3) | Operational tuning, unpublished |
| Replica-count classes | monorepo / default / ephemeral hints (§11.2) | Cursor describes the extremes, not the mechanism |
| Eviction TTL defaults | 24 h general, shorter ephemeral (§14.2) | Published behavior ("garbage collect idle"), not numbers |
| Orphan GC grace | 7 days (§14.3) | Workspace GC-grace discipline applied |

None of these affect the parity rows in [Section 2](#2-parity-traceability-matrix);
they are economics and ergonomics knobs beneath identical guarantees. If
Cursor later publishes internals that differ, guarantees still hold — only
tuning tables change.

### 22.3 Open questions for implementation start

1. Repo ID allocation scheme for Layer A client-created repos (UUID vs
   content-derived) — interacts with rendezvous stability.
2. Whether `meta/repo.json` class hints are operator-managed or auto-derived
   from observed traffic initially (recommend auto-derive first, override
   second).
3. Express One Zone topology: single-zone risk accepted at which durability
   tier? (Deployment policy, not code.)
4. Migration plan sequencing for existing repos (per §19.4) — separate
   numbered plan under `plans/`.
