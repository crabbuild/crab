# Plan 031: Run bounded fleet rebalance and planned node drain

> **Executor**: Use the isolated `cell-safe-rebalance` worktree. Read root,
> `crates/AGENTS.md`, `crates/crab-http-server/AGENTS.md`, and this plan.
> Keep HTTP/auth/provider policy in the server and authority in the runtime.
>
> **Drift check**: `git diff --stat cebc909940f137e4bd8445e524e77a154bf51a29..HEAD -- crates/crab-http-server/src/{server.rs,peer.rs,metrics.rs} crates/crab-http-server/src/cells/router.rs crates/crab-cell-host/src/lib.rs`.
> Re-read all changed entry points, startup tasks, and shutdown hooks.

## Status

- Priority: P0; effort: XL; risk: HIGH; category: feature/integration.
- Depends on: plans 027–030. Planned at
  `cebc909940f137e4bd8445e524e77a154bf51a29`, 2026-09-21.
- Status: IN PROGRESS; implementation in this branch, acceptance gates pending.

## Why and current state

The server publishes signed capacity through
`NodePublisher::run_shared/advertisement`
(`crates/crab-http-server/src/peer.rs:305-486`) and cold routing sends an
authenticated activation hint via `RepositoryCellRouter::activate_preferred_node`
(`src/cells/router.rs:481-557, 825+`). No task currently scans locally
owned Cells for proactive movement. The heartbeat only enters draining when
`server.cancellation` is cancelled; that token also shuts other tasks down
(`peer.rs:314-323`, `server.rs:1341+`). `Server::accepts_application_peers`
requires `CellNode::is_ready` (`server.rs:883-887`), and readiness rejects
`server.cancellation` (`server.rs:1971+`). Planned drain therefore needs a
separate “no new Cells, still serve owned Cells” signal.

The canonical design assigns the private fleet controller to this server
and the pure planner to the runtime
(`crates/crab-cell-runtime/docs/canonical-ltx-scaling.md:554-570`).
The maintenance scheduler rendezvous is for scans, not Cell ownership.
Router preference is advisory; `CellAuthority` CAS and destination runtime
admission remain decisive.

## Scope and contract

In scope: `crates/crab-http-server/src/server.rs`, `peer.rs`,
`cells/router.rs`, a narrowly scoped controller module if it removes
duplicate policy, `metrics.rs`, server tests, and operator documentation.
Out of scope: creating a second scheduler/authority/publisher, new public
config/env flags, direct owner-to-owner SQLite transfer, provider credential
construction, app descriptor, and arbitrary HTTP APIs.

The controller runs only on a live node lease, examines the signed fleet
snapshot and that node's actor-owned Cells, and executes at most the fixed
planner/budget limits. It never moves a Cell from another owner. One tick
must be bounded in directory scan size, Cell count, attempts, byte demand,
and wall time. It holds permits until release/receiver outcome and reports
separate planned, deferred, released, restored, and failed counts.

## Steps and gates

1. Add a distinct planned-drain signal to `NodePublisher` and the server
   lifecycle. Publish zero destination headroom before initiating scale-down,
   but keep heartbeat lease renewal and work facilities running. Split
   “accepts a new Cell” from “serves an already owned Cell” in peer/route
   guards. **Gate:** a draining node rejects a new activation hint while its
   existing owned Cell can finish a mutation and read it back.
2. Start one host-owned bounded controller task in the existing
   `CellNodeTaskGroup` during server startup. Read signed live
   `NodeDirectory` observations, use the plan-028 pure planner and plan-029
   actor/host release, and refresh membership on the next tick. Resolve each
   selected Cell's target through the existing bounded catalog lookup before
   a peer activation; a Cell ID alone is not a routable target. Do not use
   the maintenance scheduler's rendezvous ownership or add a second
   standalone task owner. **Gate:** when a new node advertises headroom, one
   eligible local Cell is released within the fixed tick budget; a stale or
   unsigned destination causes no release.
3. After confirmed release, reuse the router's authenticated
   `cell.activate`/Describe path to hint one eligible receiver. Bound retry
   and replan count. A failed receiver leaves exact unowned Idle control;
   observe this as Released rather than Activated. Re-read control and visible
   state before reporting restored service. **Gate:** receiver admission
   failure and receiver crash preserve the exact root and no dual owner.
4. Connect operator scale-down to the host's `drain_for_scale_down` result.
   Do not cancel `server.cancellation` or `node_shutdown` until host reports
   zero owned Cells and durability reservations; on blockers, surface an
   incomplete result and keep the process/lease alive. Only then run the
   existing terminal shutdown sequence. **Gate:** deadline with one pending
   Queue/Workflow Cell leaves that Cell routable for settlement and the node
   advertisement renewing.
5. Add low-cardinality metrics/traces for reason and phase, without Cell or
   node IDs as metric labels. Update operator docs to distinguish incomplete
   planned drain from fatal shutdown. **Gate:** focused metric tests and
   `git diff --check` pass.

## Test and verification commands

Add server tests beside `peer::tests::placement_capacity_respects_runtime_reservations`
(`peer.rs:1828+`) and router tests for preferred activation. Cover
scale-up, stale signed observations, no-new-Cell/serve-owned split,
blocked/deadline drain, failed receiver, and cancellation. A unit-only
controller test cannot claim operational scale-up; plan 032 supplies E2E.

Preflight mounted/writable `$HOME/Workspace`, create only the
`crab-cell-safe-rebalance` target, and set it on every Cargo command:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-http-server --locked --lib
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-host --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo clippy -p crab-http-server -p crab-cell-host -p crab-cell-runtime --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all exit zero. If dependencies are missing, `make install` from
`crab/` with the same target, retry once, report the first actionable error.
No local `target/`, baseline edits, or synthetic release receipts.

## Acceptance criteria

- [ ] A live new node triggers bounded proactive releases and normal
  destination activation; an existing owner never changes by planner fiat.
- [ ] Planned drain stops new Cell acquisition before movement while owned
  Cells and heartbeat remain live until settlement or clean completion.
- [ ] Failed receiver leaves exact Idle control, and progress reports never
  equate Released with restored service.
- [ ] One host-owned task, bounded work/metrics, focused tests, format, and
  Clippy pass; no new product config/env surface.

## STOP and maintenance

Stop if the existing peer activation path cannot be reused with its auth
contract, if the deployment shutdown hook terminates a blocked planned drain,
or if bounded directory scans cannot cover the advertised fleet. Reviewers
must check every route that currently equates `is_ready` with serving.
Protected provider/Kubernetes evidence remains plan 032's release gate.
