# AGENTS.md

Scoped rules for `crates/crab-cell-host/`. Root and `crates/` guidance apply.

- This is a provider-neutral lifecycle facade. HTTP, auth, provider
  construction, and user authorization stay in product crates.
- One `CellNode` owns one `CellRuntime`; do not add a second scheduler,
  authority, publisher, or durability path here.
- Builder validation must fail before starting the runtime. Shutdown and drain
  must await the runtime and be safe to call once.
- A node keeps one drain lane: concurrent scale-down and shutdown callers queue
  on it, and a caller's deadline bounds the releases it starts rather than the
  wait for the lane. Fleet-level pacing stays with the planner's movement
  budget, so do not add a second per-node rate limit here.
