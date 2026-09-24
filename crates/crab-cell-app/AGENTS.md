# AGENTS.md

Scoped rules for `crates/crab-cell-app/`. Root and `crates/` guidance apply.

- Keep this crate dependency-light: runtime contracts only; no HTTP, provider,
  storage construction, credentials, or node lifecycle policy.
- Stable IDs, role, shard count, and descriptor bytes are application contracts.
- Do not expose raw SQLite, authority, replica, local paths, or arbitrary
  operation IDs through the author handle.
- Layout: the integration suites are `tests/reference_application.rs` (with
  `tests/reference_application/`) and `tests/contracts.rs`; `src/tests.rs` is the
  only in-src test location and is listed in `tests-allow-list.txt`.
- Run `python3 crab/scripts/check-cell-ltx-layout.py` after layout changes.
- Run the crate tests, format, Clippy, and dependency-tree checks after API
  changes.
