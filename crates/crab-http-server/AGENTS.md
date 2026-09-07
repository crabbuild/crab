# crab-http-server

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-http-server/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns the repository HTTP application, native Git endpoints, authentication, and embedded React assets. It composes remote-git/read/write/storage APIs; shared Git and storage mechanics remain in their owner crates.

## Read first

1. `crates/crab-http-server/src/main.rs` — CLI configuration and lifecycle entry.
2. `crates/crab-http-server/src/server.rs` — `serve / router / Repository::open_current`: repository catalog, routing, and runtime.
3. `crates/crab-http-server/src/app.rs` — `admit / repository`: request repository/principal boundary and error mapping.
4. `crates/crab-http-server/src/auth.rs` — `Principal / Authentication`: session identity and Git-token boundary.
5. `crates/crab-http-server/src/receive.rs` — `receive / publish_objects`: bounded native receive and publication orchestration.

Trace one path: `crates/crab-http-server/src/main.rs` → `serve` in
`crates/crab-http-server/src/server.rs` → router/handler composition.
Receive continues through `crates/crab-http-server/src/receive.rs` to the
shared publication owner `crates/crab-write/src/journal.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| HTTP receive | `crates/crab-http-server/src/receive.rs` | `crates/crab-write/src/journal.rs` |
| Content reads | `crates/crab-http-server/src/contents.rs` | `crates/crab-remote-git/src/snapshot.rs` |
| Auth or routing | `crates/crab-http-server/src/server.rs` | `crates/crab-http-server/src/auth.rs` |
| Read readiness | `crates/crab-http-server/src/maintenance.rs` | `crates/crab-write/src/generation.rs` |

## Invariants

- Keep transport identity/session checks and repository authorization before storage or Git effects; inspect auth and app boundary together.
  Source: `crates/crab-http-server/src/app.rs`.
- Git receive validation, publication outcome, and protocol acknowledgement are distinct steps; trace validate/publish and fault tests before changing error mapping.
  Source: `crates/crab-http-server/src/receive.rs`.
- Repository operations and maintenance share long-lived runtime ownership; preserve explicit shutdown/draining when changing listener lifecycle.
  Source: `crates/crab-http-server/src/server.rs`.

## Features and platform

No declared Cargo features. Both library and auto-discovered main binary exist. `crates/crab-http-server/build.rs` requires packages/repository/dist/index.html and rejects symlink assets. Build the frontend before any Cargo check/test/build for this crate; Node requirements are in its README.

## Verification

Tests are mounted from server.rs, including `crates/crab-http-server/src/receive_tests.rs`, `crates/crab-http-server/src/receive_fault_tests.rs`, and `crates/crab-http-server/src/auth_tests.rs`.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
npm ci --prefix packages/repository
npm run build --prefix packages/repository
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-http-server --locked --lib server::auth_tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-http-server --locked --lib server::receive_tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-http-server/README.md` — usage and detailed contracts.
- `crates/crab-http-server/Cargo.toml` — dependency and feature authority.

Read `crates/crab-http-server/build.rs` and `packages/repository/package.json` for embedding changes. Dedicated image/service proof: `.github/workflows/http-server-container.yml`; native fetch proof: `.github/workflows/git-protocol-v2-partial-clone.yml`. Browser UI changes require their own frontend checks.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
