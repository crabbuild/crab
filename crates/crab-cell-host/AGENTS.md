# AGENTS.md

Scoped rules for `crates/crab-cell-host/`. Root and `crates/` guidance apply.

- This is a provider-neutral lifecycle facade. HTTP, auth, provider
  construction, and user authorization stay in product crates.
- One `CellNode` owns one `CellRuntime`; do not add a second scheduler,
  authority, publisher, or durability path here.
- Builder validation must fail before starting the runtime. Shutdown and drain
  must await the runtime and be safe to call once.
- Layout: the integration suite is `tests/node.rs` (with `tests/node/`). The
  crate holds no in-src tests, so it has no `tests-allow-list.txt`.
- Run `python3 crab/scripts/check-cell-ltx-layout.py` after layout changes.
