# crab-cell-host

Provider-neutral lifecycle boundary for a compiled Cell application. `CellNode`
owns one embedded runtime and its shared admission ledger, drives startup,
durability, scale down, and shutdown, and reports one lifecycle status. A
product server supplies providers, authentication, and network transports; it
must not construct a second runtime alongside this host.

Shutdown first cancels admission and joins work producers. Lease maintenance
registered with `CellNodeTaskGroup::spawn_lease_maintenance` keeps renewing the
node session while the runtime drains accepted work and closes its covered
node log. Only then does the host cancel the node-shutdown token and join lease
maintenance, allowing session withdrawal. Both phases share the original task
limit and absolute shutdown deadline. Ordinary tasks stop on the work
cancellation token; lease maintenance stops on the node-shutdown token.

Hosts admitting snapshot readers must also provision native-memory admission
on the `SqlWorkerPool` passed to `CellNodeBuilder::with_runtime`. The default
budget covers the configured writer count at 64 KiB per writer. Use
`SqlWorkerPool::with_native_memory_limit` to supply a larger explicit envelope
without increasing writer or file-descriptor capacity. Each read snapshot
currently reserves 12 MiB; refreshing can retain both old and replacement
snapshots. The reference Compose host reserves 32 MiB for native admission
and 64 MiB for retained cuts within its 1 GiB container limit. Admission
reservations are separate from measured RSS and the container memory ceiling.

Call `CellNode::install_read_replicas` during startup after installing the task
group. It retains the shared `ReadReplicaManager`, supervises refresh and
placement eviction, and cancels activation before closing views during drain.
Pass the returned manager to the peer dispatcher as its replica resolver and
replica control implementation.
The operator supplies the application storage layout, signed directory,
private local root, and LTX limits. An authenticated owner hint calls
`activate`; it must still pass current owner and reader-selection checks.
Queries never activate or refresh a missing reader.

The supervisor refreshes admitted views from published roots and removes views
that are no longer selected. Call `CellNode::install_read_replica_recruitment`
with the application identity and an activation-authorized `ReplicaPeerClient`
to recruit readers automatically for locally owned Cells. Recruitment scans
all compiled namespace roles and advances its cursor before I/O. The supervisor
retains at most 64 dirty Cells, one discovered candidate list, and 16 concurrent
activation hints across Cells. Discovery, queued hints, and activation share a
30-second deadline per prepared Cell. Explicit operator passes visit at most
64 Cells within 30 seconds. The five-second poll interval is not a freshness
or replacement SLO. Expired readers are
replaced through signed live membership and the same receiver admission path.
Pending activation hints recheck the selected boot at its observed lease
expiry. A renewal preserves the in-flight request; an expired or withdrawn
session releases the wait so a later pass can recruit its replacement.

Successful object publication, activation, and schema migration also notify
recruitment through a bounded runtime channel. The host coalesces queued hints
per Cell and refreshes through the same signed peer path without waiting for
the next tick. Pending hints coalesce by Cell and node session; a stalled reader
does not prevent completed healthy readers from receiving subsequent hints.
Commands never await readers. Fleet-only acknowledgement sends
no publication hint until its root reaches object storage. Periodic scans
still repair dropped hints and reconcile membership and target changes; this
does not create a bounded-staleness guarantee.

The task group owns recruitment alongside refresh; cancellation interrupts
provider and peer waits before drain. Explicit operator hints can use the
returned recruiter's `reconcile` method. HTTP authentication, administrative
policy, and repository scope stay in the server. The issue service and
independent reference hosts use these same implementations.

## Module map

| Module | Responsibility |
| --- | --- |
| `builder` | `CellNodeBuilder` validation and required-owner wiring |
| `node` | `CellNode`, its task group, lifecycle, qualification, and scale down |
| `read_replicas` | Selected immutable views, refresh, eviction, and terminal close |
| `read_replicas/recruitment` | Scoped owner recruitment, bounded fanout, and replacement |
| `durability` | Node-log durability supervision and rotation |
| `facility` | Facility registration and drained owners |
| `status` | `NodeState` and `NodeStatus` reporting |
| `tasks` | Bounded supervision for the node's facilities |

## Tests

`tests/node.rs` is the suite, with modules for builder validation, components,
lifecycle (including concurrent scale down), qualification, and task
supervision. The crate holds no in-src tests, so it has no
`tests-allow-list.txt`.

```sh
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> \
  cargo test -p crab-cell-host --locked
```

See `AGENTS.md` for contributor rules and
`crates/crab-cell-runtime/docs/runtime.md` for the runtime contract this facade
drives.
