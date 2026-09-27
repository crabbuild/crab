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

## Module map

| Module | Responsibility |
| --- | --- |
| `builder` | `CellNodeBuilder` validation and required-owner wiring |
| `node` | `CellNode`, its task group, lifecycle, qualification, and scale down |
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
