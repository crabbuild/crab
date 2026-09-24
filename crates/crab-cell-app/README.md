# crab-cell-app

Author-facing composition for statically linked Cell applications. The crate
compiles a runtime registry together with a bounded, deterministic topology
descriptor, so an application hands the host one `CompiledApplication` instead
of assembling modules, namespaces, and cell types itself. Node lifecycle,
storage providers, authority, and HTTP policy stay outside this boundary in
`crab-http-server`.

## Module map

| Module | Responsibility |
| --- | --- |
| `lib.rs` | Application builder, cell-type bindings, compiled application, and the author handle |
| `tests.rs` | In-src tests for the private registry validation path, listed in [`tests-allow-list.txt`](tests-allow-list.txt) |

## Tests

`tests/` holds two binaries: `reference_application` (application binding,
commits, fleet behavior, primitives, and process performance, with shared
fixtures in `tests/reference_application/harness.rs`) and `contracts` (the
descriptor, digest, and identity contracts).

```sh
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> \
  cargo test -p crab-cell-app --locked
```

See `AGENTS.md` for contributor rules and
`crates/crab-cell-runtime/docs/application-framework.md` for the framework
contract this crate implements.
