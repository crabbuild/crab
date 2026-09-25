# crab-cell-app

Author-facing composition for statically linked Cell applications. The crate
compiles a runtime registry together with a bounded, deterministic topology
descriptor, so an application hands the host one `CompiledApplication` instead
of assembling modules, namespaces, and cell types itself. Node lifecycle,
storage providers, authority, and HTTP policy stay outside this boundary in
`crab-http-server`.

`ApplicationHandle::new` checks the author type's application name and the client's compiled
registry digest against the hosted application before any typed call can start.
`CellNode::application_handle` returns the same binding error to its caller.

`cell_client!` generates scoped Rust accessors and typed command, prepare, query,
and pending-resolution methods from explicit stable namespace and operation IDs.
Its constructor checks each binding against the compiled registry. Accessors
accept only their declared `CellKey` type and derive canonical shard targets
from its stable encoding and the application's `CellType` descriptor.
The reference application exercises generated calls through `CellNode` hosts;
the macro documentation includes valid and compile-fail examples.

## Module map

| Module | Responsibility |
| --- | --- |
| `lib.rs` | Application builder, cell-type bindings, compiled application, and the author handle |
| `client_macro.rs` | Generated scoped client and operation methods |
| `tests.rs` | In-src tests for the private registry validation path, listed in [`tests-allow-list.txt`](tests-allow-list.txt) |

## Tests

`tests/` holds two binaries: `reference_application` (application binding,
commits, fleet behavior, primitives, and process performance, with shared
fixtures in `tests/reference_application/harness.rs`) and `contracts` (the
descriptor, digest, and identity contracts).
The reference suite also runs three `CellNode` hosts to test ambiguous results,
duplicate delivery, owner loss, and overlap between two release IDs with
unchanged module contracts through the public APIs.

```sh
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> \
  cargo test -p crab-cell-app --locked
```

See `AGENTS.md` for contributor rules and
`crates/crab-cell-runtime/docs/application-framework.md` for the framework
contract this crate implements.
