# crab-server

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-server/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns the repository HTTP application, native Git endpoints, authentication, and embedded React assets. It composes remote-git/read/write/storage APIs; shared Git and storage mechanics remain in their owner crates.

## Read first

Read `crates/crab-server/CELLAR.md` for native Cellar storage, lifecycle,
and migration limitations.

1. `crates/crab-server/src/main.rs` — CLI configuration and lifecycle entry.
2. `crates/crab-server/src/server.rs` — `serve`: process composition and ordered shutdown; `src/server/http.rs` composes routes and `src/repository.rs` owns pinned reads and maintenance.
3. `crates/crab-server/src/app.rs` — `admit / repository`: request repository/principal boundary and error mapping.
4. `crates/crab-server/src/auth.rs` — `Principal / Authentication`: identity and configuration; `auth/state.rs` owns durable sessions, `auth/http.rs` owns login/session endpoints, and `auth/transport.rs` bounds provider requests.
5. `crates/crab-server/src/receive.rs` — `receive / publish_objects`: bounded native receive and publication orchestration.

Trace one path: `crates/crab-server/src/main.rs` → `serve` in
`crates/crab-server/src/server.rs` → router/handler composition.
Receive continues through `crates/crab-server/src/receive.rs` to the
shared publication owner `crates/crab-write/src/journal.rs`.

The native runtime lives in `src/cells/runtime.rs`; `src/cells/client.rs` owns
typed ingress, `src/cells/application.rs` owns transaction/replay dispatch, and
`src/cells/schema.sql` is the canonical initial schema. HTTP authorization and
Cell error mapping stay in `src/app.rs`. No previous-runtime readers or catalog
and session migration paths are supported.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| HTTP receive | `crates/crab-server/src/receive.rs` | `crates/crab-write/src/journal.rs` |
| Content reads | `crates/crab-server/src/contents.rs` | `crates/crab-remote-git/src/snapshot.rs` |
| Auth or routing | `crates/crab-server/src/server/http.rs` | `crates/crab-server/src/server/http/boundary.rs`, `src/auth.rs` |
| Read readiness | `crates/crab-server/src/repository.rs` | `src/server/background.rs`, `src/maintenance.rs`, `crates/crab-write/src/generation.rs` |
| Management and capacity | `crates/crab-server/src/server/management.rs` | `src/server/capacity.rs`, `src/metrics.rs` |
| Git imports | `crates/crab-server/src/git_import.rs` | `src/git_import/source.rs` (validation), `src/git_import/git.rs` (process isolation) |
| Browse APIs | `crates/crab-server/src/api.rs` | `src/api/query.rs` (Git queries), `src/api/representation.rs` (encoding/ranges) |

## Invariants

- Keep transport identity/session checks and repository authorization before storage or Git effects; inspect auth and app boundary together.
  Source: `crates/crab-server/src/app.rs`.
- Git receive validation, publication outcome, and protocol acknowledgement are distinct steps; trace validate/publish and fault tests before changing error mapping.
  Source: `crates/crab-server/src/receive.rs`.
- Repository operations and maintenance share long-lived runtime ownership; preserve explicit shutdown/draining when changing listener lifecycle.
  Source: `crates/crab-server/src/server.rs`.

## Features and platform

No declared Cargo features. Both library and auto-discovered main binary exist. `crates/crab-server/build.rs` requires packages/ui/dist/index.html and rejects symlink assets. Build the frontend before any Cargo check/test/build for this crate; Node requirements are in its README.

## Verification

Repository lifecycle tests are mounted under `repository::maintenance_tests`; HTTP integration tests remain mounted from `server.rs`, including `crates/crab-server/src/receive_tests.rs`, `crates/crab-server/src/receive_fault_tests.rs`, and `crates/crab-server/src/auth_tests.rs`.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
npm ci --prefix packages/ui
npm run build --prefix packages/ui
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-server --locked --lib server::auth_tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-server --locked --lib server::receive_tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-server/README.md` — quick start and reading routes.
- `crates/crab-server/REFERENCE.md` — detailed contracts and qualification history.
- `crates/crab-server/Cargo.toml` — dependency and feature authority.

Read `crates/crab-server/build.rs` and `packages/ui/package.json` for embedding changes. Dedicated image/service proof: `.github/workflows/http-server-container.yml`; native fetch proof: `.github/workflows/git-protocol-v2-partial-clone.yml`. Browser UI changes require their own frontend checks.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
