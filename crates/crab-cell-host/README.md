# crab-cell-host

Provider-neutral lifecycle boundary for a compiled Cell application. `CellNode`
owns one embedded runtime and its shared admission ledger, drives startup,
durability, scale down, and shutdown, and reports one lifecycle status. A
product server supplies providers, authentication, and network transports; it
must not construct a second runtime alongside this host.

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
