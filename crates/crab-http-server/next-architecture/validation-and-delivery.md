# Validation, delivery, and worked examples

[Design index](README.md) · Proposed architecture; not implemented.

## Validation and real-repository qualification

### Proof obligations before claiming working support

| Obligation | Required evidence |
| --- | --- |
| Single published lineage | Deterministic competing CAS tests and model of legal transitions |
| Acknowledged-write durability | Abrupt process death after each commit boundary with local disk loss |
| No tentative reads | Reads/errors issued during blocked publication never reveal new values |
| Lost-response replay | Same request on another node produces the same resource/result |
| Exact restore | Snapshot plus deltas matches SQL semantics and expected database checksum |
| Capture correctness | WAL reset, checkpoint race, partial transaction and page-size cases |
| Proxy authorization | Spoofed envelope, revoked credential, wrong repository and scope escalation rejected |
| Git coexistence | Native push, browser edits, PR merges and remote helper share correct publication rules |
| Migration correctness | Old fleet stopped, complete semantic inventory imported, all repositories restored and verified before reopening; incomplete requests and tombstones preserved |
| Rollout correctness | Drain, Pod kill, disk loss, incompatible reader, owner rebalance |
| UI completeness | User action causes real storage change visible after reload and cold restart |

### Storage provider qualification

Use a unique dedicated probe prefix below the configured test root:

1. Create absent coordination object and verify origin read.
2. Attempt conditional create again; require rejection.
3. Read token and conditionally update; require success.
4. Update using stale token; require rejection and unchanged current value.
5. Race independent clients on the same token; require one accepted transition.
6. Simulate response loss after an accepted write; require reconciliation.
7. Verify requested byte ranges, offsets, lengths and content.
8. Verify large immutable-object integrity and interrupted upload behavior.

Record the RustFS image digest/version, storage topology and volume persistence
alongside test results. An S3-compatible label is not evidence that preconditions
work. Apply the same contract suite to every advertised cloud provider.

### Deterministic state-machine testing

Model control operations as acquire, renew, publish, release, migrate, compact
and restore-generation transitions. Generate reordered completions, stale tokens,
lost responses and process pauses. Assert that no acknowledged request disappears
from a legal successor graph and no stale owner advances it.

Use property tests for segment coverage, checksum linkage, manifest bounds,
request deduplication and counter migration. Golden fixtures protect actual LTX
encodings and decoder capability changes. Do not test only a mocked happy-path
`replicate()` function that bypasses WAL lifecycle.

### Dedicated fault environment

Run two or three real Crab processes against real RustFS. Kill the process,
not just an HTTP future. Inject pauses longer than a lease interval, isolate peer
traffic independently of storage traffic, fail uploads, lose CAS responses,
fill the cell scratch allocation and replace Pods with empty local disks.

Required ordering cases include:

- Old owner upload before takeover, late publication after takeover.
- Publication before takeover with both responses delayed.
- Delayed renewal response after local self-fence.
- Takeover crashes before snapshot publication, followed by another takeover.
- Read materializes old state while a successor publishes new state.
- Compaction completes while head advances and a backup pins old inputs.
- Git commits, a later push advances/reverts the ref, then SQL completion retries.
- Asset upload completes while metadata owner is replaced.

### Routing and balancing qualification

Run these cases against multiple real processes with independent scratch and a
qualified RustFS store. Observe request/operation IDs and control epochs through
logs and origin reads; a balanced-looking dashboard alone is insufficient.

| Scenario | Required result |
| --- | --- |
| Request enters every non-owner Pod | Same published result and permissions; direct peer routing to the owner |
| Two nodes acquire one idle UUID concurrently | One CAS winner; loser releases capacity and refreshes owner |
| Stale route points to restarted process at reused IP | Session/epoch mismatch rejected; no stale execution |
| Peer link fails but owner keeps renewing | No takeover based only on transport failure; bounded public error |
| Local restore budget exhausted, peer has room | One bounded acquire request; receiving peer rechecks capacity and authority |
| Capacity sample stale, every candidate full | Bounded overload; no recursive forwarding or activation storm |
| Mutation accepted before proxy timeout | Retry with original durable identity resolves one result; non-replayable body is not resent |
| Add two Pods to a busy three-Pod fleet | New entry capacity immediately; only demanded/released cells change owner |
| Idle handoff with losing candidate or candidate death | Published head preserved; another contender can acquire; no empty reset |
| SIGTERM while outbox completion needs SQL | Completion channel remains available until accepted workers settle |
| One repository saturates while others are quiet | Per-cell backpressure; renewal and unrelated cells retain capacity |
| Proactive rebalance with stale/mixed samples | Movement pauses; existing ownership and request routing remain valid |

Record convergence time, voluntary movement count, restore/snapshot bytes,
publication latency, and maximum request queueing. Repeat with uneven database
sizes and a permanently active repository. A planner cannot require all nodes to
reach their ideal count when no eligible idle cells exist. The proactive cases
gate the later rebalancer, not the initial on-demand release.

### Real repositories and UI acceptance

Use 5–10 real repositories from the mounted qualification checkout collection.
Treat them as read-only source inputs. Import into dedicated Crab test prefixes;
perform branch/content/PR mutations only in those imported test copies. A
suggested ten-slot coverage matrix is:

| Slot | Input characteristic | UI/storage purpose |
| --- | --- | --- |
| 1 | Small text repository | Fast complete collaboration workflow |
| 2 | Rust workspace | Tree, history, blame and source navigation |
| 3 | TypeScript frontend | Nested files, text edits and diffs |
| 4 | Repository with many branches/tags | Ref browsing and release selection |
| 5 | Longer commit history | Pagination and bounded history reads |
| 6 | Binary-heavy repository | Binary diffs and downloads |
| 7 | LFS-enabled repository where available | Real LFS side effects |
| 8 | Repository with non-ASCII paths | Byte/path handling and rendering |
| 9 | Larger tree or monorepo | Resource limits and loading/error states |
| 10 | Additional representative team repository | Concurrent users and permission boundaries |

Names are selected from available real inputs, not hardcoded into production.
Use separate synthetic fixtures for empty repositories and adversarial corruption;
do not describe those as real-repository qualification.

For each relevant repository, populate issues/comments/labels/assignments,
PRs/reviews/checks/statuses and releases/assets using supported APIs. Record
expected resource identities in test evidence. Confirm results through both the
browser and an independent client, then repeat after stopping all Crab servers
and discarding their local cell directories in the dedicated environment.

Exercise clone, fetch and push with a native Git client separately from metadata
tests. Select a request entering a non-owner Pod and prove the internal proxy
path. Trigger a real owner failure during creation and merge and verify truthful
UI recovery. Preserve drafts and request IDs on network errors.

Level 3 acceptance means user action → real RustFS side effect → visible result.
Level 4 adds visible error paths. Level 5 requires the broader performance,
accessibility, retention, upgrade and operational evidence. A compiled schema or
an in-memory HTTP test alone does not satisfy these levels.

### Verification scope for the current implementation

The local `crab-ltx` crate is implemented. Its unit/integration suite covers real
SQLite, checkpoint boundary cuts, auto-vacuum shrink/regrowth, exact cold restore,
process kill and original-directory loss, snapshot/compaction byte identity,
independent CRC and frame/block vectors, malformed inputs and admission limits.
A runnable local round-trip example restores a visible SQL issue row.

Run the [crate's scoped commands](../../crab-ltx/README.md#verification) for tests,
Clippy and format checks. Existing dependency versions remain unchanged and the
workspace resolves a single SQLite linkage. The existing workspace CI will run
the new member; broad CI and cross-platform results must be recorded separately.

This does **not** complete phase 2's external golden/interoperability, fuzz,
filesystem fault and measured-resource qualification. It does not prove phases
1 or 3 onward: RustFS control CAS, HTTP output barriers, ownership takeover,
browser workflows and Kubernetes operations are still unimplemented. Validate
design links/anchors alongside runtime changes; do not report those server gates
as passed because the library tests pass.

## Delivery sequence

Each phase ends with an observable vertical slice and an evidence artifact.
Do not introduce placeholder backends or partially wired production routes.

| Phase | Work | Exit evidence |
| --- | --- | --- |
| 1. Protocol foundation | Control schema/transitions, immutable manifest graph, provider capability diagnosis | Model/property tests and independent RustFS CAS race |
| 2. Replication mechanics — local slice implemented | [Pinned Celld source integration](crab-ltx.md), managed capture/checkpoint, rolling checksum, exact restore and complete-chain compaction | Local SQLite/cold-restore/process-kill and CRC tests pass; external interoperability, broad platform/fault/memory proof remains |
| 3. Single-node issue slice | SQL issue/comment model, dedup, response barrier, tracked cancellation | Browser create/edit/retry, kill process, restore from RustFS |
| 4. Multi-node ownership | Session identity, leases, peer TLS, route policy, cold capacity admission, strong owner reads | Wrong-node routing, competing acquisition, overload, stale owner and lost-response tests |
| 5. Domain parity | PR/reviews, labels/assignees, statuses/checks, release metadata/assets | Existing domain/API suites plus real UI workflows |
| 6. Cross-domain recovery | Durable outbox, canonical Git evidence and pending-work rules | Merge/tag crash and ABA/later-push qualification |
| 7. Hard cutover | Stop old fleet, offline full inventory import, verify every repository, start SQL/LTX-only fleet | Real copied dataset comparison, interrupted import/resume and full-fleet acceptance before reopening |
| 8. Operational completeness | Kubernetes lifecycle, idle handoff, backups, schema/format rollout, resource limits | Three-Pod rolling update, scale-out/in, disk loss and backup restore |
| 9. Measured optimization | Bounded proactive rebalance, batching, compaction and then safe collection as justified | Convergence/pressure tests and before/after benchmarks with unchanged fault invariants |

Stages use isolated test instances until the new architecture is complete enough
for the hard cutover. The new runtime contains one SQLite/LTX application path
from the outset. Do not implement a legacy-serving adapter, backend toggle or
migration-aware intermediate release. The offline importer preserves the source
data contract without making the old store reachable from request handlers.

Celld source reuse is approved. The first two phases must complete dependency
alignment, attribution, capture/checksum qualification and control serialization.
The cross-domain phase must resolve canonical Git evidence before
automated outbox failover is enabled. Garbage collection is delivered only after
pinning and backup retention are proven.

Expected changes by owner:

- `server.rs`: stable UUID propagation, lifecycle and route composition.
- `app.rs`: accepted command ownership, error mapping and response barrier.
- Application domain modules: SQL queries and transactions replacing JSON calls.
- `crab-ltx`: capture, encoding, restore and compaction mechanics.
- `crab-storage`: only necessary reusable provider/path/conditional contracts.
- Shared Git crates: only evidence-backed publication recovery APIs, if needed.
- `packages/repository`: explicitly required retry/error/cursor contract changes.
- Helm/deployment docs: peer identity, security and phased drain.
- `REFERENCE.md`: implemented state and qualification evidence updated as phases land.

Replace and delete retired domain JSON runtime paths as their SQL equivalents
land in the new architecture; remove tests that assert only removed internals.
The hard-cutover release contains no retired collaboration backend. Retain
offline import fixtures that protect real stored-data contracts. Review net code
growth by responsibility rather than accepting an adapter stack around the old
store.

## Worked examples

### Successful issue creation through the wrong Pod

Repository `team/service` resolves to UUID R1. Pod B owns epoch 19 and published
application revision 142. The public Service sends the request to Pod A.

```text
Client: POST issue, request_id Q, title "Document setup"
Pod A: authenticate, check membership, resolve R1 → Pod B
Pod B: verify delegation and owner activation
SQLite: allocate issue 51, insert issue, save Q result, revision 143
Capture: produce complete LTX coverage for revision 143
Storage: upload segment and manifest H43
Control: CAS epoch 19/head H42 → epoch 19/head H43
Client: receives issue 51 only after the CAS is proven
```

If Pod B dies immediately after success, Pod C takes over, inherits H43,
restores issue 51 and Q's result, and publishes its new-epoch snapshot. The UI
reload still displays issue 51. If the success response was lost, retrying Q with
the original actor/body resolves to the same issue.

### Local commit while object storage is unavailable

Pod B commits issue 52 locally but its upload fails. Revision 144 is tentative.
The cell stops new application commands and tries to resolve publication. It
does not show issue 52 in list responses or disclose its number in an error.

The client can receive a generic timeout explaining an unknown outcome. If
connectivity returns while ownership is valid, B can publish and complete Q2.
If B loses ownership first, its unpublished local tail is abandoned. A retry of
Q2 on the successor is evaluated against the restored deduplication state and
may execute there. The client was never told an unrecoverable write succeeded.

### Merge published before SQL completion

Pod B publishes SQL intent M7, including expected base X and intended merge Y.
Canonical Git publication commits X → Y, then B crashes before recording SQL
completion. Another developer subsequently pushes Y → Z.

Pod C restores the pending intent and checks canonical operation evidence. A
receipt proving M7 committed lets it mark the PR merged at Y while the current
branch remains Z. A simple test for `current_ref == Y` would misclassify this
case. If no adequate evidence exists, C leaves an explicit reconciliation state
and does not move Z or invent a completed/failed result.

### Three-Pod rolling upgrade

Pods A/B/C use decoder capability F1. A new release first adds F2 decoding while
continuing to write F1. Roll all Pods, verify eligible peer capabilities, then
publish a capability policy allowing F2 writers. Takeovers now accept either
encoding. An older F1-only binary is not an eligible rollback target after F2
publication.

Each terminating Pod drains its owned cells while renewing authority, resolves
accepted publication, releases control records, and leaves immutable recovery
graphs. Requests entering another Pod activate or proxy to the new owner. A
forced kill can increase recovery time but does not change the published-head
durability rule.

## Source references

Local implementation references are linked at their owning sections. External
contracts were consulted on 2026-09-13; moving documentation URLs should be
rechecked when freezing an implementation.

| Source | Used for |
| --- | --- |
| [Pinned Celld guarantees](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/docs/guarantees.md) | Separation of ownership, durability proof and takeover recovery |
| [Pinned Celld balancing](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/crates/logic/rebalance.rs) | Weighted owned-cell count, donor/receiver eligibility and bounded handoff |
| [Pinned Celld capacity routing](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/crates/logic/lib.rs#L3860-L3965) | Cold-placement capacity sampling and reservations |
| [Pinned Celld limitations](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/docs/limitations.md) | Hibernated-cell movement and internal transport security boundary |
| [Pinned Celld LTX README](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/crates/ltx/README.md) | Library scope, source provenance, format capability boundary |
| [Pinned Celld WAL capture](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/crates/ltx/src/db.rs) | Managed capture, read locks and checkpoint ownership |
| [Litestream Go library](https://litestream.io/guides/go-library/) | Embedding API and SQLite driver constraints |
| [ltx-rs](https://github.com/superfly/ltx-rs) | File-format library scope |
| [SQLite WAL](https://www.sqlite.org/wal.html) | Writer/read/checkpoint model and local filesystem requirement |
| [SQLite WAL hook](https://www.sqlite.org/c3ref/wal_hook.html) | Post-commit callback and hook replacement semantics |
| [SQLite backup](https://www.sqlite.org/backup.html) | Consistent database snapshot mechanism |
| [Kubernetes Service](https://kubernetes.io/docs/concepts/services-networking/service/) | Public balancing and direct endpoint discovery |
| [Kubernetes StatefulSet](https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/) | Stable network/storage identity |
| [Kubernetes probes](https://kubernetes.io/docs/concepts/workloads/pods/probes/) | Readiness/liveness semantics |
| [Kubernetes Pod lifecycle](https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/) | Termination and process replacement |
| [Kubernetes NetworkPolicy](https://kubernetes.io/docs/concepts/services-networking/network-policies/) | Network isolation and enforcement prerequisites |

The combined owner/head CAS protocol, strict published-manifest restore rule,
Crab schema, routing decisions, and delivery plan are this proposal's design.
They are not claims that Celld or current Crab already implements these exact
mechanisms.
