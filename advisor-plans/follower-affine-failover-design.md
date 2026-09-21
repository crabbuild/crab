# Follower-affine Cell failover hardening

Status: IMPLEMENTED — protected scale/provider/release qualification remains
Priority: P0
Planned against: `c86dd43423ae` (`origin/main`, 2026-09-20)
Design authority: this document extends `crates/crab-cell-runtime/docs/failover-and-followers.md`

## Goal

Reduce owner-loss recovery time and bucket/network work while preserving the
existing RPO=0 durability contract. The common case should recover on a healthy
surviving physical follower because that node already has the fsynced tail. A
different eligible node must still take over when no follower can.

The target is a warm durability successor, not a hot SQLite standby.

## Current state

The protocol already has the hard safety pieces:

- a process-wide node lease fences an expired boot session permanently;
- `NodeDirectory::claim_expired` uses object-store CAS to create a renewable
  recovery claim;
- recovery seals reachable follower witnesses, rejects conflicts, pins an
  immutable manifest/bundle, attaches each overlay to Cell control, then seals
  the dead session;
- `Control::takeover` increments the Cell epoch and enters `Recovering`;
- `CellRuntime::takeover_restored` restores into a fresh directory and activates
  only after the exact recovered root is verified;
- the Compose qualification kills two owners and proves follower-only commits
  survive owner-local disk loss.

The slow path is orchestration and repeated I/O:

1. discovery still uses one bounded rendezvous scan, but every node scheduler
   can inspect expired sessions;
2. the claimant is now the live scheduler whose stable physical follower is in
   the failed log whenever that follower is eligible;
3. each follower tail page rescans and revalidates the whole lane;
4. recovery scans all 256 catalog shards before reading the sealed tail, even
   though authenticated frames already identify affected Cells;
5. production recovery reaches a same-host follower through mTLS HTTP;
6. pinned bundles are uploaded to object storage and then downloaded again for
   activation even when the successor just materialized them locally;
7. one aggregate recovery histogram hides which phase dominates RTO.

## Terms

- **Owner:** the one boot session authorized by Cell control to serve a Cell.
- **Follower:** a physical node retaining checksum-verified node-log fragments;
  it is not a Cell owner or read replica.
- **Recovery coordinator:** the elected scanner that discovers an expired owner
  and assigns recovery work. It does not gain Cell authority by assignment.
- **Recovery executor:** the live boot session holding the CAS recovery claim.
- **Preferred successor:** a live, eligible current boot session whose stable
  physical `NodeId` appears in the failed log's member set.

## Target sequence

```text
owner lease expires
  -> live schedulers read the expired session and its stable follower NodeIds
  -> each follower scheduler filters for its own physical NodeId and eligibility
  -> the eligible follower reserves local work capacity and CAS-claims recovery
     for its own current boot session
  -> seal all reachable witnesses and compare their evidence
  -> seek/stream the uncovered tail using the local follower index when present
  -> derive affected Cell scopes from authenticated frame headers
  -> load and validate only those catalog/control records
  -> build recovery overlays and publish immutable bundles + manifest
  -> attach overlays and seal the dead node session
  -> route takeover to the preferred successor when still eligible
  -> CAS a new Cell epoch, restore a fresh sparse Db, verify, serve
```

If no eligible original follower claims during the two-second grace window, the
preferred rendezvous scanner uses the bounded any-node fallback. Once a target
has claimed, current fencing permits reassignment only after the 30-second claim
expires. This design does not invent unsafe claim release or transfer. Scheduler
preference is advisory; the persisted recovery claim, node lease, Cell control,
and actor admission remain authoritative.

## Safety invariants

| Boundary | Required invariant |
| --- | --- |
| Owner death | Expired node lease is terminal; the old session never revives. |
| Assignment | A coordinator may nominate an executor, but only the nominated target may claim for its own live session after local admission. |
| Witness | Every reachable witness is sealed and checked for conflicting bytes; locality never means trusting one copy blindly. |
| Discovery | Tail-derived Cell scopes are authenticated and are revalidated against catalog and current control before use. |
| Pinning | Every recovered overlay is published under its content digest before control names it. |
| Activation | A fresh `crab_ltx::Db` restore is mandatory; follower disk is never opened as the writable database. |
| Ownership | Cell control CAS increments epoch before service; placement cannot write authority. |
| Fallback | Any eligible node may recover after the follower-first grace period or when all original followers are ineligible. |
| Loss | Missing complete durability evidence fails closed; no empty-tail assumption is allowed. |

## Efficiency model

Measure recovery as explicit phases rather than one timer:

```text
RTO = lease detection
    + coordinator assignment
    + claim CAS
    + follower seal/witness comparison
    + tail read and verification
    + affected-Control validation
    + overlay build and immutable pin
    + Cell takeover/restore/activation
```

The implementation should make work scale with the uncovered tail and affected
Cells, not total lane size or the whole application catalog:

- tail page seek: `O(log records + page bytes)` from a derived local index;
- recovery inventory: `O(frames + pages in affected catalog shards)`, with
  each affected shard loaded once and bounded scope deduplication;
- memory: bounded page/overlay streaming plus explicitly capped frame-digest
  and scope metadata, never complete frame payloads;
- same-host transport: no loopback TLS/body encode, but identical authorization
  and checksum verification;
- object storage: immutable pin remains mandatory; verified local bytes may be
  reused only after matching the control-pinned digest.

Plan 018 establishes baseline values before any SLO threshold is chosen. Do not
invent an RTO target without those receipts.

## Assignment and placement policy

The failed log stores stable physical member `NodeId`s. Resolve each to its one
current live boot session, then apply the signed placement eligibility rules:

1. reject stale, draining, critically pressured, incompatible, or admission-
   incapable sessions;
2. prefer the live member matching the scheduler's stable physical NodeId during
   the two-second grace window;
3. after the grace window, let the preferred scanner use any eligible node, with
   stable NodeId ordering as the deterministic tie-break;
4. reserve bounded recovery capacity before claim; a pre-claim rejection
   advances immediately, while a target crash after claim waits for the existing
   claim expiry;
5. never treat a scheduler hint or placement observation as ownership authority.

Do not add a public configuration knob. Stable `NodeId` selects the physical
follower; the new boot `SessionId` is always the claimant and owner. Sealing
clears the tombstone claimant, so takeover affinity re-runs this deterministic
ranker from the sealed log's stable member set; it must not assume the tombstone
retains the executor identity.

## Local acceleration contract

Local follower state is an acceleration source, never authority:

- a composite transport dispatches to `FollowerStore` only when the requested
  member equals the process's stable `NodeId`; all other members use existing
  mTLS transport;
- local and remote paths return the same receipt/page types and run the same
  frame/LTX verification;
- the lane index is derived, crash-rebuildable data. Chunk files and seal
  watermarks remain authoritative local evidence;
- materialized bundles may be retained in a digest-addressed local recovery
  cache. `load_overlay` may consume that cache only after manifest scope and
  bundle digest match the control-pinned reference;
- cache miss, restart, or eviction uses the canonical object-store reader.

## Rollout

No feature flag, environment variable, legacy reader, or second recovery path.
Land one canonical behavior per slice:

1. Plan 018: phase metrics and receipt baseline; correct stale deployment docs.
2. Plan 019: crash-rebuildable lane index and seek-only pages.
3. Plan 020: tail-derived affected-Cell inventory and bounded streaming.
4. Plan 021: follower-affine assignment and successor routing with bounded
   any-node fallback.
5. Plan 022: same-host transport and digest-verified local artifact reuse.
6. Plan 023: qualify an immutable candidate image, then promote that exact
   digest; source-only Compose evidence is not release-image proof.

The index, tail-derived inventory, and local transport preserve existing wire
and authority formats. If execution discovers a shipped persistent-format or
mixed-version contract that requires migration, stop and record that contract
before adding compatibility code.

## Qualification gates

- deterministic/in-process tests cover every phase, retry, crash point, and
  broken safety variant;
- Compose receipt records phase durations, selected physical node, whether it
  was an original follower, fallback reason, bytes/frames scanned, and object/
  peer calls without high-cardinality metric labels;
- both owner-loss cycles must select a surviving original follower when it is
  eligible, then prove any-node fallback with all followers unavailable;
- large-tail qualification must show page-read work grows with returned bytes,
  not lane prefix length;
- large-catalog qualification must show only affected catalog shards are read,
  once each, and control reads grow with affected Cells;
- release claims remain under plans 015 and 023 and require protected multi-Pod,
  provider, latency, signed receipt, and exact exercised-image digest evidence.

## Non-goals

- writable or readable hot SQLite replicas;
- serving before immutable recovery pin and control CAS;
- changing acknowledgement quorum or RPO;
- making local SSD authoritative across loss of all replicas;
- prewarming whole databases on followers;
- a second scheduler, placement authority, or recovery manifest format;
- public failover tuning flags before measured product requirements exist.
