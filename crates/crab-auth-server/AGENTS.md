# crab-auth-server

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-auth-server/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns JSON-speaking protected receive and path-scoped view helper binaries. It is a service-side execution boundary, not a persistent HTTP router; managed-service authorization inputs and local workspace lifecycle meet here.

## Read first

1. `crates/crab-auth-server/src/lib.rs` — helper modules and error boundary.
2. `crates/crab-auth-server/src/receive.rs` — `ProtectedPushPlan / commit_service_metadata`: prepare/verify/commit orchestration.
3. `crates/crab-auth-server/src/receive/git_workspace.rs` — `materialize_source_push / verify_source_push`: pack installation and Git validation.
4. `crates/crab-auth-server/src/view.rs` — `materialize_view_with_store`: scoped source materialization.
5. `crates/crab-auth-server/src/doctor.rs` — `git_version`: native Git capability checks.

Trace one path: `crates/crab-auth-server/src/bin/crab_auth_receive.rs` → receive
functions in `crates/crab-auth-server/src/receive.rs` → pack/workspace
operations in `crates/crab-auth-server/src/receive/git_workspace.rs`.
`crates/crab-auth-server/src/bin/crab_auth_view.rs` is the separate view entry.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Receive command | `crates/crab-auth-server/src/bin/crab_auth_receive.rs` | `crates/crab-auth-server/src/receive.rs` |
| Scoped view | `crates/crab-auth-server/src/bin/crab_auth_view.rs` | `crates/crab-auth-server/src/view.rs` |
| Git validation | `crates/crab-auth-server/src/receive/git_workspace.rs` | `crates/crab-git/src/pack.rs` |

## Invariants

- Keep prepare, verify, and commit boundaries explicit; commit must use verified state and expected ref changes rather than trusting uploaded bytes.
  Source: `crates/crab-auth-server/src/receive.rs`.
- Read-path authorization and denied paths must remain part of view construction; do not reuse an unscoped view merely because its source generation matches.
  Source: `crates/crab-auth-server/src/view.rs`.
- Operation-owned Git workspace cleanup and worker draining are part of error/cancellation handling, not just the success path. Inspect workspace creation and callers together.
  Source: `crates/crab-auth-server/src/receive/git_workspace.rs`.

## Features and platform

No declared Cargo features. Binaries: `crab-auth-receive` and `crab-auth-view`. Native Git and writable temporary space are required for workspace tests; doctor validates service prerequisites. Cloud-backed prepare/commit/view proof needs a dedicated environment.

## Verification

Receive/git_workspace and view contain inline tests for malformed packs/paths and unavailable workspaces; library test helpers isolate process-global temporary-directory overrides.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-auth-server --locked --lib receive
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-auth-server --locked --lib view
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-auth-server/README.md` — usage and detailed contracts.
- `crates/crab-auth-server/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
