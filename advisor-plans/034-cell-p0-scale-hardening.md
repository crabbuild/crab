# Plan 034: Close the P0 scale gaps in the Cell metadata plane, hot path, and warm reuse

> **Executor**: read root `AGENTS.md`, `crates/AGENTS.md`, and
> `crates/crab-cell-runtime/AGENTS.md` before editing. This plan changes
> durable coordination behavior; do not land a slice without its exit
> evidence. Work from a checkout with its own `CARGO_TARGET_DIR`. Never
> manufacture provider, Kubernetes, scale, or release evidence.
>
> **Drift check**:
> `git diff --stat HEAD -- crates/crab-cell-runtime crates/crab-http-server/src/cells crates/crab-ltx advisor-plans`

## Status

- **Priority**: P0; effort: XL; risk: HIGH.
- **Depends on**: plans 015, 023, 024, 025, 031, 032 (placement, host, and
  qualification infrastructure), and an isolated qualification environment.
- **Category**: architecture / correctness / performance / release.
- **Planned at**: `7f36da6bb83`, 2026-09-24.
- **Implementation status**: slice 1 (catalog page locator) and slice 2 stage
  1 (resident Cells tick themselves) are DONE; the remaining slices are TODO
  and each needs its own reviewable commit. Slice 3a is folded into 3b,
  because its trigger is unreachable from the actor today.

## Why this matters

Crab models every entity as a Cell and gives one Cell one SQLite writer, one
LTX root chain, and one durability proof. That is the right unit of
consistency, but three properties of the current implementation bound a
deployment long before the node hardware does:

1. **Metadata cost grows with the Cell population.** A version-one catalog
   head named page digests only, so `CellCatalog::lookup` downloaded every
   page in a shard (up to 256 MiB) to answer one cold route. Slice 1 fixed
   the locator half. Due-work discovery still reads one control record per
   Cell per pass: `DueCellScan` walks a shard and loads each control to read
   `next_due_ms`, bounded at `MAX_DUE_PER_CYCLE = 128` per
   `SCAN_INTERVAL = 1s` in `crates/crab-http-server/src/cells/scheduler.rs`.
   Timers, queue leases, workflow timers, and effect retries all ride that
   scan, so their discovery latency grows linearly with the population.
2. **A Cell performs one durability boundary at a time.** The actor keeps
   `busy` from `BeginWork` through the commit's proof, and
   `CellExecutor::accepts_publication` refuses the next commit until the
   newest pending commit is durable. A Cell is the consistency domain for
   one entity, so an application cannot add shards to a hot entity the way
   it can for a key space.
3. **Every wake is a restore.** `Residency` has no dormant state, eviction
   publishes the Cell unowned, and `discard()` quarantines the local
   artifacts. Only the LTX directory cache survives an activation, so an
   idle-eviction/re-acquire cycle pays a remote restore (or a full sparse
   re-materialization) even when the same node already holds the exact
   bytes.

`advisor-plans/025` records the symptom that ties these together: the local
isolated-RustFS primitive workload failed its own unchanged threshold with
p99 16.778 s against 5 s. That is not a threshold problem; it is the
dominant term of a cold-activation, proof, publication, or restore path
being paid inside a measured operation.

## Non-goals

- No V8, JavaScript, WebAssembly, or uploaded code path.
- No change to the single-writer, exact-root, or acknowledgement contract.
  A slice may change *when* a wait happens, never *what* a success means.
- No second durability path, fallback reader, or alias for a retired shape.
- No repair of provider, Kubernetes, or scale evidence from local runs.

## Slice 1 — Catalog page locator (DONE)

A version-two catalog head names, for every immutable page, its digest and
the first Cell id it can contain. Routing binary-searches those keys and
reads one page; the reader verifies the page digest, requires the page to
open at its located key, stays inside the shard, and ends below the next
locator key, so a page that disagrees with its locator is a hard error rather
than a reported absence. The locator list is trusted authority: only
provisioning writes a head, and it does so through the head CAS.
Provisioning keeps the full-shard read because it republishes the page set.

Evidence: `crates/crab-cell-runtime/tests/runtime/catalog.rs` proves one
page read with the other page's object deleted, rejects an unordered
locator, and rejects a head whose page disagrees with its locator; the
existing pinned-shard restore path rebuilds locators from verified pages, so
the backup manifest still carries digests only.

## Slice 2 — Index due work without making the index authoritative

Target: due-work discovery costs scale with *due* work, not with the Cell
population, while `control.next_due_ms` and the Cell's own SQLite state stay
authoritative.

Design:

- Publish one small immutable hint object per armed deadline under
  `due/<minute-bucket>/<cell>` carrying `(cell, incarnation, epoch, due_ms,
  root digest, control revision)`.
- The owner writes a hint when the *bucket* of `next_due_ms` changes, not on
  every commit, and retires it with a proof once the deadline is consumed.
  Retirement must follow the wake rules that `crab-cell-runtime` already
  uses for node-log recovery: conditional writes only, create before the
  first acknowledgement that needs it, and a delete that cannot name a
  later installation.
- The scheduler lists the current and overdue buckets, then loads each
  candidate's control and re-derives `next_due_ms` before ticking. A hint
  never authorizes a tick; it only selects candidates.
- Keep a **full-scan backstop** over every shard on a longer period (for
  example five minutes, spread across cycles). A lost or never-published
  hint then costs bounded delay instead of a lost timer, which is what
  makes the accelerator safe to reason about before its publication
  protocol has a model check.

Design refinement (2026-09-24, from source):

- Split the population by residency instead of indexing every Cell. A Cell
  this node owns and holds resident can tick itself: `MaintenanceTickCommand`
  takes `MaintenanceTickRequest::expected_commit_sequence` and returns
  `MaintenanceTickOutcome::Stale` when the Cell moved past it
  (`crates/crab-cell-runtime/src/primitives/maintenance.rs`), and the actor
  runs one command at a time, so a fleet Tick that races a local Tick is a
  no-op rather than a double fire. The host already owns both halves needed
  to drive it — `Registry::run_maintenance_once` and
  `CellClient::local(registry, handle)` — so the loop belongs in
  `crab-cell-host` beside the node's other supervised tasks, and it needs one
  runtime accessor: resident Cells whose published `next_due_ms` has passed,
  with the published sequence that fences their Tick.
- Only non-resident Cells need fleet-wide discovery, which is the part the
  hint objects should cover. A hint published when a Cell leaves residency is
  enough for that path; a resident Cell does not need one.
- Record the scan capacity as a fleet number, not only as a metadata cost:
  `MAX_DUE_PER_CYCLE = 128` due Cells per second per scanning node also bounds
  due throughput. Hints plus self-ticking raise the effective rate; they do
  not change the per-Cell class budgets.

Stage 1 landed on 2026-09-24: a resident Cell no longer waits for a shard
scan. The actor mirrors the published head's `next_due_ms` and commit
sequence, `CellRuntime::due_resident(now_ms, limit)` answers from its own map
and returns `DueResident` handles, and the product scheduler ticks those Cells
first through `RepositoryCellRouter::resident_scheduler_cell`, drawing on the
same per-cycle budget before any catalog page or control record is read.
Evidence: `resident_due_list_mirrors_the_published_head` proves the mirror and
the sequence fence from the public runtime API, and
`resident_due_cell_ticks_without_a_shard_scan` proves the fast path ticks a
resident Cell with no shard scan and leaves it owned.

Stage 1 defect found and fixed on 2026-09-24: the fast path could spend the
whole per-cycle budget, and the shard scan then still attempted one item
before checking what was left, subtracting below zero (`attempt to subtract
with overflow`). `scan_once_bounded` now skips the shard scan when the fast
path used the cycle, and `resident_ticks_do_not_overspend_the_cycle_budget`
pins it: the test was run against the unguarded code first and failed with the
overflow panic, then passed with the guard restored. The scheduler test
fixture was factored into `resident_due_fixture` so both resident tests share
one node, advertisement, router, and scheduler setup.

Correction to the refinement above: the loop lives in the product scheduler,
not in `crab-cell-host`. The host owns no tenant/application identity, so it
cannot build a `CellTarget`; the scheduler already owns the identity, the
registry, the router, and the Tick metrics. A host-owned loop stays a
follow-up for the Cellule extraction, where the host will carry an identity.

Stage 2 remains: hint discovery for non-resident Cells, the lengthened
full-scan backstop, and the object-store call-count receipt at 10³/10⁵/10⁶
Cells.

Stage 2's first half landed on 2026-09-24: hint publication and consumption,
with the backstop unchanged.

- `crab-ltx` owns the key space: `due_hint_prefix(bucket)` and
  `due_hint_path(bucket, cell)`, where a bucket is one minute of `due_ms`. The
  hint has no body, so a candidate costs one listing entry rather than a GET.
- `crab_cell_runtime::cell::due::{publish, take, bucket_for}` is the runtime
  contract. A clean release publishes one key when the released control still
  has `next_due_ms`; a failed write is a debug log, because the backstop covers
  a missing hint.
- The product scheduler runs `tick_due_hints` first each cycle with at most
  half the budget. It lists the current bucket and `HINT_LOOKBACK_BUCKETS`
  behind it, consumes what it returns (deleting the key), then confirms each
  candidate against its catalog proof and control before routing and ticking.
  Hints select candidates; they never authorize a Tick, and the shard scan
  still runs on the remaining budget.

Recorded semantics: a hint is discoverable from its own bucket for the
lookback window, so an older hint is backstop territory rather than a missed
deadline. Consumption keeps the hint population bounded by releases instead of
by Cell count.

Evidence: `clean_drain_publishes_and_consumes_one_due_hint` (runtime) proves the
release path writes the key and that taking it is a one-shot, and
`hinted_due_cell_ticks_without_a_shard_scan` (scheduler) proves an idle due Cell
is ticked from a hint with no shard scan. Still owed for the exit evidence: the
same measurement pass at 10³/10⁵/10⁶ Cells with a lengthened backstop period,
compared against the 40-control-read baseline above.

The second half landed the same day: the shard scan is now the *backstop*, not
the primary discovery path. A scheduler cycle ticks resident Cells and consumes
hints, then runs the shard scan only every `BACKSTOP_CYCLES = 30` cycles, so an
unhinted deadline waits at most one period instead of a full shard pass, and
the population scan stops costing a shard of GETs per second. Two tests pin the
new shape: `backstop_scan_runs_on_its_period` (first cycle scans, then the
thirty-cycle period) and `foreground_cycle_ticks_hints_without_the_backstop`
(the cycles between backstops still tick from a hint).

One instrument gap closed with it: the scheduler built its own `CellCatalog`
and `CellAuthority` without a telemetry sink, so the scan's per-cell control
reads were invisible. Both now bind the router's runtime handle
(`router.runtime().telemetry_handle()`), which means
`crab_cell_control_reads_total` measures the backstop's population cost in
production — the number the 10³/10⁵/10⁶-Cell receipt must report.

The instrument immediately produced a number worth acting on:
`hinted_tick_spends_a_bounded_metadata_budget` measures **one hinted candidate
at 10 catalog reads and 6 control reads**, because the scheduler, the router,
and the activation path each confirm the Cell separately. Hints therefore win
only while due candidates stay far rarer than Cells — the backstop pass is one
control read per Cell in the shard. Two follow-ups follow from that number, and
both are smaller than the remaining slices: (1) share one resolution between
the scheduler, the router, and the activation instead of re-reading the catalog
proof and control three times, and (2) only then consider growing
`BACKSTOP_CYCLES` beyond its conservative thirty.

One hardening on top of the hint index: a key under a due bucket that this
module did not write is now deleted when a listing meets it, instead of being
re-listed on every cycle forever. `due_hint_listing_clears_foreign_keys` pins
it (the foreign key exists before the listing, is not a candidate, and is gone
afterwards). The test also caught a path-building subtlety worth recording:
`object_store::Path` normalizes away a trailing slash, so a caller that joins a
bucket prefix by string concatenation writes beside the bucket rather than
inside it. Production builds hint paths through
`CellStorageLayout::due_hint_path`, which is unaffected; only a test built the
path by hand.

The reader now requires the entire canonical hint path, so a nested key with
a valid Cell filename cannot become a candidate or delete a canonical key.
It also counts the remaining batch capacity separately for each bucket;
otherwise one hint in the current bucket could prevent an older bucket from
being visited. `due_hint_listing_rejects_nested_cell_keys` and
`due_hint_listing_uses_remaining_capacity_across_buckets` pin both cases.

Release-path coverage is now pinned end to end, because the thirty-cycle
backstop makes hint coverage load-bearing. Every clean ownership release goes
through `start_deactivate` — a drain, an idle eviction, and a prepared transfer
all route there (`cell/actor/lifecycle/eviction.rs`, `cell/actor/tasks/movement.rs`) —
and `idle_eviction_publishes_the_same_due_hint` proves a *pressure* eviction
leaves the hint in the released head's bucket, so evicted deadlines do not fall
back to the backstop. The fenced path (`start_fenced_deactivate`) deliberately
writes no hint: its observed control may be stale, and the node-log tail is
preserved for takeover instead.

One timing detail the test had to learn, worth keeping: the release runs in a
spawned task, so control turns `Idle` *before* the hint key lands. Discovery
tolerates that by construction (a missing hint is backstop territory), but a
test that asserts the hint immediately after an eviction races the writer and
must poll for the key.

Negative result, 2026-09-24 — the shared-resolution follow-up is **not** a safe
read-elimination, and the attempt was reverted. Changing `route_existing` to
hand its observations to the caller (so `route_target_inner` would not re-read
the catalog proof and control before activating) did remove the duplicate: the
hinted-candidate cost fell from 10 catalog + 6 control reads to 8 + 5, measured
by `hinted_tick_spends_a_bounded_metadata_budget`. But
`server::pulls_tests::pull_request_merge_methods_use_canonical_ref_publication`
then died with a stack overflow, reproducibly and in isolation, and reverting
the change made it pass again. The re-read is therefore load-bearing: the
activation decides from a control read *after* the routing attempt, and handing
it the attempt's own observation changes which branch it takes — in that test,
down a path that recurses. Any future attempt must keep the fresh read, or
first prove which ownership branch depends on it; a 20% metadata saving does
not justify guessing.

The narrower retry — hand back only an *unowned, published* observation, on the
argument that `acquire_idle_restored` still authorizes with its CAS — overflowed
the same test the same way, and was reverted too. So the conclusion is not about
owned observations: with any handed-off observation the activation takes a
branch the fresh read avoids, and the same node re-enters routing until the
stack runs out. Two things follow. First, the post-routing re-read is required
in every case, and the 10/6 candidate cost stays until someone explains the
re-entry with a backtrace. Second, the re-entry itself is worth understanding on
its own: a routing path that recurses when the observation is stale is a latent
hazard, and the fresh read is currently what masks it.

Trace of that failure, 2026-09-24 (temporary depth counter and log in
`activate_or_route`, reverted with the attempt): the router entered
`activate_or_route` **once**, at depth one, with `state=Idle owner=None` — the
handed-off unowned observation — and the stack died below it. So the recursion
is not through the router's own entry points; it is inside the Idle activation
path that the fresh read normally prevents from running at all. Two candidate
explanations remain, and neither is proven: a large async state machine whose
poll exceeds the worker stack once the extra resolution enum is carried across
the call (a `Box::pin` boundary would fix that), or a genuine recursion below
`acquire_idle_restored` that the fresh read masks by choosing a different
branch. Whoever picks this up should capture a real stack (`sample` or `lldb`)
rather than reasoning from this trace alone.

Resolved the same day, with the boxed boundary: it was stack **depth**, not an
unbounded recursion. `RUST_MIN_STACK` bisection put the threshold for the
hand-off version between 2 MiB (overflow) and 3 MiB (pass), so the routing path
already runs near the default worker stack and the extra values tipped it over.
Boxing the activation future at its call site
(`Box::pin(self.activate_or_route(…))` in `route_target_inner`) keeps that
chain on the heap, and the previously overflowing test then passes with the
default stack.

Both changes are now landed and verified: the unowned observation read inside
the activation window is reused instead of re-reading the catalog proof and
control, and the activation future is boxed. The measured hinted-candidate cost
is 8 catalog + 5 control reads, pinned by
`hinted_tick_spends_a_bounded_metadata_budget`, and the saving applies to every
cold activation, not only hint-driven ones. One caveat to keep in mind: if a
claim lands between the routing attempt and the activation, the activation now
fails its ownership CAS and the caller retries, where the fresh read would have
forwarded to the new owner — fail-closed and bounded, but a behaviour difference
in that narrow window.

One leak closed on 2026-09-24: a released Cell whose deadline was already older
than the listing window published a hint nobody would ever list, and nothing
would ever delete — an unbounded residue across a fleet's lifetime. `due::publish`
now takes the release clock and skips a deadline more than
`HINT_LOOKBACK_BUCKETS` buckets behind it, leaving that deadline to the backstop;
`due_hint_publication_skips_a_deadline_outside_the_window` pins both directions
(outside the window publishes nothing, inside publishes the key).

That change also surfaced a clock-consistency requirement worth stating: hint
publication compares the release clock (wall time) with the control's
`next_due_ms`, so a Cell whose logical clock diverges from wall time by more
than the window is treated as already stale. Production uses wall-clock logical
time, so the comparison is sound; two runtime tests had been driving commands
with logical times of a few seconds while the release path used wall time, and
they now use `now_ms()` like production does. Anyone writing a test that asserts
a hint must keep those two clocks consistent.

Stage 2's baseline is now pinned by evidence (2026-09-24): a shard pass that
finds **nothing** due still costs one control record per Cell in the shard plus
two catalog reads to pin it. `due_scan_reads_one_control_record_per_cell`
(`crates/crab-cell-runtime/tests/runtime/catalog.rs`) provisions 40 Cells in
one shard, drains `DueCellScan`, observes zero due work, and asserts exactly 40
control reads and 2 catalog reads. At 65,536 Cells per shard that is a full
shard of GETs per pass for no work, which is the cost the hint buckets must
replace: one bounded LIST per due bucket plus one control read per *candidate*.
The backstop stays authoritative until hint publication has a model check.

The population-independence claim is pinned locally as well:
`foreground_discovery_scales_with_hints_not_with_population` releases sixteen
Cells, hints only two of them, runs one foreground cycle, and measures
**16 catalog + 10 control reads** — exactly two candidates at the pinned 8/5,
with the other fourteen Cells costing nothing. The backstop cycle that follows
over the same sixteen Cells measures **355 catalog + 72 control reads**, because
the scan pays per Cell and every due Cell it finds also routes. The test asserts
the foreground number exactly and a floor for the backstop, so a change that
quietly reintroduced a population scan into the foreground path fails there.
This is the local form of the slice's exit evidence; the 10³/10⁵/10⁶-Cell
receipt still needs a scale environment.

Exit evidence: object-store call counts and p99 for the first tick of a
distant timer at 10³/10⁵/10⁶ Cells in one application; a hint removed out
from under a due Cell still fires within the backstop bound; a hint naming a
consumed deadline never fires a second occurrence.

## Slice 3 — Let one Cell carry several commits in flight

Target: a hot entity's sustained write rate is bounded by the durability
proof pipeline, not by one proof per round trip, without ever releasing a
response that its proof does not cover.

Why the one-line relaxation is wrong: `CellExecutor::confirm_durable` rejects
an out-of-order confirmation ("durability proof skipped an earlier commit")
and the actor treats a rejected confirmation as a fence-worthy invariant.
That check is only true while at most one commit is unproven, which the actor
enforces by keeping `busy` set until the proof lands. Allowing several
unproven commits therefore requires a monotone proof, not a wider gate.

Why 3a cannot land on its own: the worker's `PendingPublication` signal is
unreachable from the actor as the code stands. The actor holds `busy` from
`BeginWork` through the commit's proof, the kernel blocks command dispatch at
the publication caps, and the case that does reach the executor
(`cell/actor/requests.rs` derives `must_fence` from `WorkerState::Pending`)
therefore only fires if publication accounting has already diverged. A
"wait instead of fence" branch with no reachable trigger cannot be proven by a
behavioral test, so it lands with 3b, where pipelining makes the trigger real
and the same branch carries the backpressure contract.

Design:

- Replace the per-commit durable flag with a **durable-through watermark**:
  a proof that covers commit `N` marks every pending commit `<= N` durable.
  Proofs are naturally monotone (the follower log acknowledges an offset,
  and object roots publish in order), so out-of-order completion becomes
  legal rather than fatal.
- Admit up to `MAX_UNPROVEN_COMMITS` unproven commits, bounded by the
  existing pending count, byte high-water, and per-Cell deadline, and bound
  the *wait* a later commit may impose on the writer.
- Gate reads per response instead of refusing to run them: a query records
  the commit sequence it observed and its reply waits for the watermark that
  covers it. This is the output gate the reader path needs once reads can
  run beside unproven writes.
- Treat the worker's `PendingPublication` as **backpressure, not a fence**:
  requeue the command and let the proof or publication completion resume the
  actor. Fencing stays correct for real worker invariants, SQLite engine
  failures, and deadlines.

Alternative with a smaller blast radius, if measurement shows the hot path
is dominated by application-level bursts rather than by proof latency: add a
typed **batch command** capability — several typed commands with their own
request identities, digests, and stored outcomes, committed in one SQLite
transaction and published as one root and one proof. Prefer this first when
the workload is a burst of independent writes to one entity, because it does
not change the proof model.

Exit evidence: a benchmark that reports ops/s and p99 for one Cell at 1/2/4
vCPU with a fleet proof and with an object proof; interleaved proof
completions never release a reply the watermark does not cover; a read never
observes a commit above its released watermark; duplicate and rejected
outcomes keep their exact per-command semantics; the existing publication,
fence, and shutdown suites still pass unchanged.

## Slice 4 — Dormant residency and warm local reuse

Target: a same-node wake reuses verified local bytes instead of restoring.

Blocker found 2026-09-24: the reuse half of this slice is **fenced inside
`crab-ltx` by design**, so it cannot land as a runtime-only change. A spike
that wrote a clean-close receipt, matched it against the idle control, and
opened the file failed with `AlreadyExists`, and the source says why:

- `Db::open_with_host` documents that "an existing capture directory is
  refused, even after a clean close" (`crates/crab-ltx/src/db.rs`).
- `Db::open_inner` atomically creates `CaptureEngine::meta_path_for(path)` and
  a comment states the rule: "Never unlink it on close: an old open file must
  not acquire a new epoch."
- `Db::resume_with_host` is not the missing piece: it restores from a verified
  plan (origin reads) and only re-seeds the capture continuation.

So one local database path is one capture session, permanently. A receipt that
proves the file's content equals the published root is necessary but not
sufficient to reopen it. The spike still produced design facts worth keeping:

- The receipt needs `(cell, incarnation, root digest, commit sequence, schema,
  code digest)` only. The **epoch must not be compared**: acquiring an idle
  Cell advances the epoch while leaving the root untouched, and any writer that
  did commit advances the root digest instead. The first spike attempt failed
  exactly on the epoch comparison.
- The receipt must be consumed (unlinked) before anything opens the file, so a
  crash between the open and the next clean close cannot leave a receipt that
  describes a half-written database.
- A restore over a stale local file must discard the destination, its
  `.crab-ltx-checksums` file, and the `-wal`/`-shm`/`-journal` sidecars first;
  `CellWritableDatabase::prepare_writable` requires all of them absent.

Two viable shapes, in preference order:

1. **A `crab-ltx` resume capability.** Mint a new capture session over an
   existing file whose previous session closed cleanly: write a clean-close
   marker inside the capture meta directory, require that marker (and a
   matching file identity) before a new epoch may replace the directory, and
   seed the capture continuation from the file's own WAL position. This is
   protocol work with the same proof obligations as the seal/recovery paths,
   and it is the version that removes the restore from every same-node wake.
2. **A fresh path per activation.** Give each activation a new destination
   (for example `<cell>.<epoch>.sqlite`), record the previous path in the
   receipt, and move or copy the proven file onto the new path before the
   normal open. The epoch fence is avoided by construction because the new
   path has no capture directory; the cost is one local file copy per wake, and
   the product router owns the naming policy plus an LRU under the disk budget.

The second shape's core mechanism is now **validated** (2026-09-24):
`a_cleanly_closed_database_survives_a_rename_to_a_fresh_path` drains a
bootstrapped Cell, renames its database file to a path nothing has opened, and
opens it there with `Db::open_with_host` — the rows are intact. So the fence is
strictly per path: a warm wake can move the previous activation's file onto the
fresh activation path and open it locally, with no origin read, as long as a
receipt proves the file still holds the published root. That test is kept as a
dependency contract, because it is the assumption the whole warm-path design
rests on.

What slice 4 still owes, now with the mechanism settled: the resume receipt
(write on clean close, consume before open, matching cell, incarnation, root
digest, commit sequence, schema, and code), a per-activation destination name
the runtime derives from the caller's stem, the rename path for a matching
receipt, a rename of the `-wal` sidecar with it, and a bounded sweep of older
files under that stem that keeps the newest one and charges what it keeps to
the shared disk budget.

Naming basis verified (2026-09-24): `Control::takeover` increments the epoch
(`control.rs`), and both activation transitions require the epoch to be
unchanged, so **one epoch is one ownership session** and `<stem>.e<epoch>` is a
path no other session can use. The product also builds each destination under
the node's per-boot session directory, so a boot starts with fresh paths; the
fence can only bite within a boot, which is exactly the eviction-and-reacquire
case warm reuse exists for. No counter or path registry is needed.

Implementation steps, in order, each verifiable on its own:

1. `crab-ltx`: re-add `CellReplica::open_existing(&self, path) -> Result<Db>` as
   a thin `Db::open_with_host` wrapper, documented as requiring the caller's
   proof that the file holds this replica's root.
2. `crab-cell-runtime/src/cell/resume.rs`: the receipt — `{cell, incarnation,
   root digest, commit sequence, schema, code, database path}` — plus
   `write`, `take` (read then unlink), `matches(control)` (epoch deliberately
   excluded), and `discard(path)` (the database, `-wal`/`-shm`/`-journal`, and
   the `.crab-ltx-checksums` file). The receipt lives at `<stem>.resume.json`.
3. Runtime activation (`acquire.rs::activate_restored_reserved`): derive
   `path = <stem>.e<epoch>` from the observed control; take the receipt before
   opening anything; on a match, rename `receipt.database` (and its `-wal`) to
   `path` and open it locally; otherwise discard and restore the exact root
   into `path` as today.
4. `RestoredActivation.database` becomes an enum of the paged restore and the
   local database, with the worker branching on it (the shape already prototyped
   and reverted once).
5. `ActiveCell` remembers the path it opened, and `start_deactivate` writes the
   receipt after a clean release; the fenced path writes none.
6. Sweep: after activation, delete other `<stem>.e*` files. They are caches; the
   newest one is the receipt's, and the disk ledger already counts whatever
   remains in the session directory.

Implementation attempt, 2026-09-24 — reverted, with a sharper blocker. The
receipt, the fresh-path move, the fallback, and the sweep were all written and
the fallback path passed its test (`warm_wake_falls_back_when_the_receipt_names_a_missing_file`),
but the warm path itself failed the executor's open verification:

```text
Control("restored SQLite position does not match root")
```

`CellExecutor::from_restored` requires `db.position() == root.position`, and a
plain `Db::open_with_host` on a moved file starts an **unseeded** capture
session, so its position is zero while the root has moved past zero. crab-ltx
seeds that continuation only through `CaptureEngine::seed_continuation`, which
is `pub(crate)` and reachable only from `open_sparse` (the restore) and
`resume_with_host` (which materializes the plan from the origin). So warm reuse
needs two things that do not exist yet:

1. A public crab-ltx API that opens an existing database at a fresh path **and**
   seeds the capture continuation from `(position, checksums, page_size,
   page_count)`.
2. A local source for that continuation in the runtime. Position, page size, and
   page count are small enough for the receipt; the page-checksum index is the
   open question — the restore path writes it as `<db>.crab-ltx-checksums`, and
   a bootstrapped session has none. The choice is between renaming that file
   with the database and refusing reuse when it is absent, bounding and charging
   a checksum index inside the receipt, or having the capture engine persist its
   checksum state on a clean close. That decision belongs with the crab-ltx API.

What the attempt leaves standing: the rename mechanism (pinned by
`a_cleanly_closed_database_survives_a_rename_to_a_fresh_path`), the observation
that the product already names every activation file with a fresh `Uuid` so the
path fence never bites, the receipt design, and the verified fallback behaviour.
Nothing was left in the tree: the wiring was reverted and the runtime, http-server,
and LTX suites are green again.

The crab-ltx API that closes this, designed from source on 2026-09-24:

- `Db::persist_continuation(&self, path: &Path) -> Result<()>` writes a
  self-describing continuation: version, position (txid and checksum), page
  size, page count, then the dense per-page checksum list. `PageChecksums`
  already holds that list as a memory base or a file base and has `persist` for
  the file form, so the writer is a header plus the same body the restore path
  already produces at `<db>.crab-ltx-checksums`.
- `Db::open_resumed_with_host(path, limits, host, continuation: &Path) -> Result<Db>`
  opens the database at `path`, reads the continuation, and seeds through
  `CaptureEngine::seed_continuation(position, checksums, page_size, count)` —
  which already validates that the aggregate checksum matches the position. The
  new checks are that the file length equals `page_count * page_size` and that
  the continuation version is supported. Any mismatch is a hard error, and the
  caller discards the file and restores.
- The runtime then keeps only the receipt (identity, database path, continuation
  path); the checksum index's home is settled by writing it beside the database
  and moving it with the file on a wake.

Tests for the pair: commit, persist, move the file, open it seeded, commit
again, and assert the new capture continues the TXID chain from the recorded
position; plus a rejection case for a page-count or aggregate-checksum mismatch.

Both landed on 2026-09-24, with one refinement and two fences the design did
not have:

- `Db::persist_continuation` writes the dense page checksums beside the
  database and the continuation record next to them; `Db::open_resumed_with_host`
  reads both, requires the file to be exactly the recorded image, and seeds the
  capture session. The dense copy verifies its own fold, so a base file that no
  longer matches the maintained aggregate is refused instead of copied.
- The sidecar stays a fixed-width file rather than moving into a
  header-prefixed continuation: `PageChecksums::from_file` already reads that
  exact shape, and a memory base would make every resumed Cell's resident
  footprint grow with its page count. The three files travel together through
  `CellReplica::open_resumed`/`discard_resumed`.
- Two fences are enforced by the writer, not by the reader: a database whose WAL
  is not checkpointed is refused (the file may sit behind the continuation it
  would seed from), and a sparse activation must be fully materialized (an
  unfaulted page is a hole, not data). Both fall back to the exact restore.

The runtime half is `crates/crab-cell-runtime/src/cell/resume.rs`: a fixed-width
record carrying cell, incarnation, schema, code, and the control `RootRef`, one
per released database at `<database>.resume`. A clean release writes it from
`CellExecutor::close_resumable` before the writer closes; an activation consumes
the record that matches the observed control, moves the database onto the fresh
activation path, and discards every other record with the file it names. The
epoch is deliberately not compared, and a record that fails any check is
discarded rather than served.

Evidence (all green on 2026-09-24):

- `a_recorded_continuation_continues_the_chain_after_a_move` (crab-ltx): commits,
  persists, moves the file, opens it seeded, commits again, and asserts the new
  capture continues the TXID chain.
- `a_resume_refuses_a_continuation_that_does_not_match_the_file`,
  `a_resume_refuses_a_database_that_is_not_checkpointed`, and
  `a_dense_checksum_copy_refuses_a_base_that_no_longer_folds_to_it` pin the
  three refusals.
- `a_warm_wake_continues_the_local_database_without_the_origin` (runtime):
  bootstrap, commit, drain, then a same-directory wake records **zero origin
  requests** and exactly `[Ownership, Resume, Activate]`, and the commit before
  the release is still readable with the next one continuing the chain.
- `a_resume_record_that_names_another_root_is_discarded` (runtime): a tampered
  record makes the wake record `[Ownership, RootOpen, Restore, Activate]`, read
  the origin again, and delete the file the record named. Run against an
  always-matching record first, the assertion on `origin_requests() > 0` failed,
  so the test measures the fence rather than the happy path.

`ActivationPhase::Resume` joins the exported phase set, so
`crab_cell_activation_phase_seconds{phase="resume"}` is how an operator sees a
re-acquired Cell take the fast path; a re-acquired Cell with no `resume` sample
fell back to a restore.

Two known limits, both deliberate and both still open:

- **The dormant window is uncharged.** The retained file's bytes are reserved
  while the Cell is resident and released with the session, so an idle Cell's
  local image sits outside the disk ledger until the next activation reserves it
  again. Charging it needs a reservation handle the runtime can hold across the
  release, which is the same change dormant residency needs.
- **Ownership still turns over.** A release still returns the Cell to idle, so a
  wake pays the re-claim CAS and a new epoch. Dormant residency — holding
  ownership while shedding the resident handle — removes that claim, and is a
  coordination-contract change rather than a local-image change.
- **A process restart is still cold by construction.** The product keys its
  scratch directory by the boot session
  (`crates/crab-http-server/src/peer.rs`, `session_dir` = `sessions/<session>`),
  and this slice deliberately keeps the runtime's record scoped to the
  destination's own directory: the record names a file name, never a path
  outside it. Warm wake therefore covers idle eviction, pressure shedding, and
  a transfer back to the same node, but a restart re-acquires from the origin.
  Making it survive a restart is a product-level adoption step — on boot, move
  the newest previous session directory's `*.resume` slots (database, checksum,
  continuation, record) into the new session's matching Cell directories — and
  it does not need any further runtime or `crab-ltx` change. It does need its
  own proof (crash residue from the previous boot must be discarded rather than
  adopted), so it is a separate slice.

Design:

- Record a **resume receipt** when a Cell closes cleanly: `(cell,
  incarnation, epoch, root digest, commit sequence, schema, code digest,
  database path)`.
- Reuse the local database only when the authoritative control names the
  same incarnation, epoch, root digest, schema, and code, and the local
  `sys_meta` sequence matches the root. Any difference, any fence, any
  takeover, or any interrupted close discards the receipt and restores the
  exact root as today.
- Keep a **dormant** residency that holds ownership without a resident
  handle, so an idle Cell can be shed from memory without a release CAS and
  re-admitted with a rename. The epoch must not change while the Cell stays
  owned, and the receipt must be keyed by it.
- Bound the local snapshot cache with an LRU under the shared local-disk
  budget, and charge every retained byte to the same ledger that inflation
  and restore scratch already use.

Exit evidence: activation-cost distribution warm versus cold after idle
eviction and after a process restart; a stale receipt, a receipt from another
incarnation, a receipt whose root advanced, and a receipt written by a fenced
process are each rejected rather than served; churn under the disk budget
never exceeds the ledger.

## Slice 5 — Name the dominant term before tuning it

Target: the p99 in `advisor-plans/025` is attributed to a path, not guessed.

Design: extend the local primitive smoke to emit a phase-attributed receipt
that separates ownership resolution, cold activation, root open, restore or
sparse fault, proof wait, publication wait, handler, and reply. Report it
with the same bounded enums the runtime already exports for LTX phases so no
Cell id, path, or key becomes a label. Re-run the workload, attribute the
tail, and only then decide whether it is a cold-activation, restore, proof, or
contention problem.

Exit evidence: one retained local artifact naming the dominant phase for the
failing run, reproduced twice. This is local diagnosis, not protected
evidence; the protected provider, Kubernetes, and scale rows stay with plans
024 and 032.

Instrument gap found 2026-09-24: catalog reads are invisible to production
telemetry. `CellCatalog` calls `CellLayout::store()` directly for its head and
page objects, while LTX origin reads are counted through
`CellTelemetry::ltx_origin_request` and `CellTelemetryHandle`. Slices 1 and 2
both claim object-store call counts as exit evidence, so the counter for a
bounded catalog phase (`head` versus `page`; no Cell id, path, or digest label)
is the first piece of work in this slice. A trait method with a default body
is source-compatible with the in-tree test sinks; the Prometheus adapter in
`crates/crab-http-server/src/metrics.rs` is the production implementor to
extend.

The counter landed on 2026-09-24: `CatalogReadKind::{Head, Page}` is recorded
through `CellTelemetry::catalog_read` with an outcome and duration,
`CellCatalog::with_telemetry` binds a catalog to the node's sink,
`CellRuntime::telemetry_handle()` feeds the routing catalog, and the
Prometheus adapter renders `crab_cell_catalog_reads_total` and
`crab_cell_catalog_read_seconds` for `head` and `page`. The runtime suite pins
the routing claim directly: one lookup on a provisioned shard reports exactly
one head and one page read. The remaining slice-5 work is the phase-attributed
receipt over the qualification workload.

Control reads joined the same seam on 2026-09-24: `CellTelemetry::control_read`
is recorded by `CellAuthority::load`, the routing router binds the node's sink
through `CellAuthority::with_telemetry`, and the adapter renders
`crab_cell_control_reads_total` and `crab_cell_control_read_seconds`. That
makes the due scan's per-Cell cost visible in production instead of only its
Tick outcomes.

First attribution result (2026-09-24, in-memory provider, one-entry shard,
reproduced by `cold_activation_reports_metadata_and_origin_reads`): one cold
route costs **2 catalog reads, 2 control reads, and 3 LTX origin requests**.
Four of the seven object-store requests are metadata. A resident route costs
none of them. Before the page locator, the catalog half of that number was one
head plus every page in the shard, so the same route cost up to 257 reads at
full shard occupancy. This is a count attribution, not a latency measurement:
the provider-time claim still needs the same counts crossed with measured
per-call latency, which is what the phase-attributed receipt over the
qualification workload owes. What it does establish is the shape of the
problem: the p99 tail cannot be explained by handler work, because a cold
route's object-store requests dominate it and a resident route issues none.

Phase timing landed on 2026-09-24: `ActivationPhase::{Ownership, RootOpen,
Restore, Activate}` is recorded through `CellTelemetry::activation_phase` from
the acquisition path (`crates/crab-cell-runtime/src/cell/actor/acquire.rs`),
and the Prometheus adapter renders `crab_cell_activation_phase_seconds{phase}`
for those four bounded labels. The cold-route test now pins the phase order
next to the read counts, so one local run decomposes a cold route into claim,
root verification, local restore, and activation, while the catalog and
control counters cover the metadata reads that lead into it. A warm route
records no phase at all. What still needs a provider run is crossing these
phase timings with the qualification workload's tail — the receipt that names
the dominant term for the measured p99.

## Slice 6 — Re-run the protected gates

After slices 2–5 land, re-run the profiles whose thresholds the workload
missed. `scale-v1`, the provider profiles, and the fault profiles remain
protected receipts; a local green suite is necessary but not sufficient, and
no slice may promote a local run to a receipt.

## Deliver in dependency order

| Slice | Work | Depends on | Exit evidence |
| --- | --- | --- | --- |
| 1 | Catalog page locator | None | DONE (one-page lookup, lying-head rejection) |
| 2 | Resident self-tick (stage 1, DONE) then a non-resident hint index plus full-scan backstop (stage 2) | 1 | Timer latency for a resident Cell (stage 1); discovery cost and timer latency at scale (stage 2) |
| 3a | Backpressure instead of fence on `PendingPublication` — folded into 3b | None | See 3b |
| 3b | Durable-through watermark and pipelined commits | 3a | Interleaved proofs, read gating, hot-Cell benchmark |
| 3c | Typed batch command (if measurement prefers it) | None | One transaction, one proof, per-command outcomes |
| 4 | Resume record and local continuation — DONE; dormant residency and the dormant-window disk charge remain | 1 | DONE (zero-origin wake, stale-record and torn-image rejection); warm/cold cost distribution on real hardware still owed |
| 5 | Phase-attributed local receipt | None | Dominant term named and reproduced |
| 6 | Protected re-qualification | 2–5 | Signed profile rows |

## Reject these shortcuts

- Letting a hint authorize a tick instead of selecting a candidate.
- Deleting the full-scan backstop before the hint publication has a model
  check and a retirement proof.
- Widening `accepts_publication` without making proof confirmation monotone.
- Releasing a read reply that its watermark does not cover.
- Turning a `PendingPublication` backpressure signal into a fence, or turning
  a real worker invariant failure into a silent requeue.
- Reusing local bytes on a fence, a takeover, or any receipt mismatch.
- Claiming a warm-wake win without a measured cold baseline on the same
  hardware, provider, and workload.
- Promoting a local run, a fixture, or a self-signed artifact to protected
  evidence.

## Stop conditions

Stop and report rather than proceed when: the qualification environment
cannot be isolated; a slice needs a change to the acknowledgement contract
that the existing tests cannot pin; a protected profile is required to prove
a local slice; or the external Cargo target volume is unavailable.

## Current state and handoff (2026-09-24)

What is true now, with the test that proves each claim:

| Slice | State | Evidence |
| --- | --- | --- |
| 1 | Routing reads one catalog page; a page that disagrees with its locator is a hard error | `catalog_lookup_reads_only_the_page_that_can_hold_the_entry`, `catalog_rejects_an_unordered_page_locator`, `catalog_lookup_rejects_a_head_whose_locator_disagrees_with_its_page`, `catalog_lookup_reports_one_head_and_one_page_read` |
| 2 stage 1 | A resident Cell ticks from memory, fenced by its published sequence; the cycle budget cannot be overspent | `resident_due_list_mirrors_the_published_head`, `resident_due_cell_ticks_without_a_shard_scan`, `resident_ticks_do_not_overspend_the_cycle_budget` |
| 2 stage 2 | A clean release writes one hint; every cycle consumes hints and ticks resident Cells; the shard scan is a thirty-cycle backstop; only canonical keys are accepted and remaining capacity reaches older buckets | `clean_drain_publishes_and_consumes_one_due_hint`, `due_hint_listing_clears_foreign_keys`, `due_hint_listing_rejects_nested_cell_keys`, `due_hint_listing_uses_remaining_capacity_across_buckets`, `hinted_due_cell_ticks_without_a_shard_scan`, `foreground_cycle_ticks_hints_without_the_backstop`, `backstop_scan_runs_on_its_period` |
| 4 | A clean release leaves a resume record beside a checkpointed, fully materialized database; a matching same-node wake moves it onto the fresh activation path and reads no origin object, and every mismatch discards it | `a_recorded_continuation_continues_the_chain_after_a_move`, `a_resume_refuses_a_continuation_that_does_not_match_the_file`, `a_resume_refuses_a_database_that_is_not_checkpointed`, `a_dense_checksum_copy_refuses_a_base_that_no_longer_folds_to_it`, `a_warm_wake_continues_the_local_database_without_the_origin`, `a_resume_record_that_names_another_root_is_discarded` |
| 5 instrument | Catalog, control, and activation-phase costs are visible in production; a cold route measures two catalog reads, two control reads, three origin requests, and four phases | `cold_activation_reports_metadata_and_origin_reads`, `due_scan_reads_one_control_record_per_cell`, `hinted_tick_spends_a_bounded_metadata_budget` |

Pinned costs to measure against: one empty 40-Cell shard pass is 40 control
reads and 2 catalog reads; one hinted candidate is 8 catalog reads and 5
control reads (down from 10/6 once the router reused the unowned observation it
read inside the activation window); one cold route is 2/2/3 plus four phases;
one same-node wake is 0 origin requests plus three phases (`ownership`,
`resume`, `activate`).

Verification commands for the whole set (one external target directory per
checkout; `crab-http-server` needs a built `packages/ui/dist`):

```sh
cd crab
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> cargo test -p crab-cell-runtime --features test-support --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> cargo test -p crab-cell-app -p crab-cell-host --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> cargo test -p crab-http-server --lib --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server --all-targets --features test-support -- -D warnings
node crates/crab-cell-runtime/docs/validate.mjs
python3 crab/scripts/check-cell-ltx-layout.py
```

Last full run, 2026-09-24 (after slice 4): `crab-ltx --features replica`
82/45/33/9 green with five ignored; `crab-cell-runtime --features test-support`
353/11/25/14/43/11/13/117 green with three ignored; `crab-cell-app` and
`crab-cell-host` green; `crab-http-server --lib` 257 green with four ignored;
Clippy clean; layout, policy-entry-point, and docs validators pass.

Two slice-5 tests had to name a cold node explicitly after slice 4 landed:
`cold_activation_reports_metadata_and_origin_reads` and
`shutdown_releases_a_hydration_reservation_after_an_origin_wait` both reused the
released database's own directory, so a same-node wake turned them into warm
routes. Both now activate from `cold_node_directory(&fixture)`, which is what a
node without a resume record actually has.

Still open, in dependency order: the 10³/10⁵/10⁶-Cell receipt for slice 2
(which needs the shared-resolution follow-up first, or acceptance of the 10/6
candidate cost); slice 3 (durable-through watermark with pipelined commits, or
a typed batch command); slice 4's dormant residency and the dormant-window disk
charge (the local-image reuse itself landed 2026-09-24); the provider-side
slice-5 receipt; slice 6 (protected provider, Kubernetes, and scale gates).
