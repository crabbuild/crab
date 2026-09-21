# AGENTS.md

Scoped rules for `crates/crab-cell-app/`. Root and `crates/` guidance apply.

- Keep this crate dependency-light: runtime contracts only; no HTTP, provider,
  storage construction, credentials, or node lifecycle policy.
- Stable IDs, role, shard count, and descriptor bytes are application contracts.
- Do not expose raw SQLite, authority, replica, local paths, or arbitrary
  operation IDs through the author handle.
- Run the crate tests, format, Clippy, and dependency-tree checks after API
  changes.
