# crab-server

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-server/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns the repository HTTP application, native Git endpoints, authentication, and embedded React assets. It composes remote-git/read/write/storage APIs; shared Git and storage mechanics remain in their owner crates.

## Read first

Read `crates/crab-server/CELLAR.md` for native Cellar storage, lifecycle,
and migration limitations.

1. `crates/crab-server/src/main.rs` — thin binary entry; `src/cli/mod.rs` owns command parsing and dispatch, with ECS discovery and CLI tests beside it.
2. `crates/crab-server/src/server/mod.rs` — `serve`: process composition and ordered shutdown; `src/server/http/mod.rs` composes routes and `src/repository/mod.rs` owns pinned reads and maintenance.
3. `crates/crab-server/src/app.rs` — `admit / repository`: request repository/principal boundary and error mapping.
4. `crates/crab-server/src/auth/mod.rs` — `Authentication`: identity and configuration; `auth/principal/` owns credentials and UUID-bound current grants; `auth/state.rs` owns durable sessions, `auth/http/mod.rs` owns login/session endpoints, and `auth/transport.rs` bounds provider requests.
5. `crates/crab-server/src/git/receive/mod.rs` — `receive / publish_objects`: bounded native receive and publication orchestration.

Trace one path: `crates/crab-server/src/main.rs` → `src/cli/mod.rs` → `serve` in
`crates/crab-server/src/server/mod.rs` → router/handler composition.
Receive continues through `crates/crab-server/src/git/receive/mod.rs` to the
shared publication owner `crates/crab-write/src/journal.rs`.

The Cell repository facade in `src/cells/repository/mod.rs` exposes typed
operations. `models.rs` owns common records/inputs; `issues.rs` owns issue and
comment operations. Feature commands and their persistence helpers live under
`checks/`, `labels/`, `statuses/`, `settings/`, `releases/`, and `pulls/`. Pull
readiness belongs to `pulls/readiness/`, reviewer invitations and fulfillment
belong to `pulls/review_requests/`, and review threads belong to
`pulls/review_threads/`; browse projection reads and
writes belong to `projection/`. `dispatch.rs` alone decodes operation IDs. `search/` owns the shared issue/PR
query grammar; `search/results/` owns tagged issue/PR results and the mixed
list operation; `search/navigation/` owns query edits and state destinations;
`search/index/` owns transactional projections, full counts,
sorting/query-bound cursors, and explicit bounded preparation; `search/completion/` owns repository-scoped
suggestions and browser UTF-16 replacement ranges. `search/milestones/` owns
milestone projection/catalog reads, grammar-preserving facet destinations and
the native choice read. `search/authors/` owns qualified identity digests,
resolved binding validation, global author-facet destinations and personal-view selection. `search/labels/` owns bounded active-label choices and
include/exclude/no-label transitions; it reuses the canonical label decoder.
`search/navigation/` shares global-conjunction extraction, bounded destinations
and choice records between facets and state controls. Canonical milestone
commands update that projection in their transaction; assignment changes dirty
the discussion projection. The HTTP
choice endpoint stays with other search reads in `api/discussion_search/`.

Collaboration documents live in `cells/repository/collaboration/`. Its `target.rs`
owns shared discussion addresses, `activity/` owns append-only events and subject
indexes, and `reactions/` owns actor membership/counts. Activity append calls stay
inside domain commands so their SQL/KV changes and events commit together.
`links/` owns versioned issue/PR relationships and both subject indexes; its
single activity event is indexed into both timelines.

The native runtime lives in `src/cells/runtime.rs`; `src/cells/client.rs` owns
the authority-checked `CellClient` and target-bound read receipts.
`src/cells/application.rs` owns shared `Transaction`, replay dispatch and the
output gate. `src/cells/address.rs` identifies Repository and Directory targets;
commands and reads declare their native class. Repository initialization lives
in `src/cells/repository/schema.rs`, using the canonical `src/cells/schema.sql`.
`cells/repository/assignments/` owns stable actor references and name snapshots;
its `migration/` alone reads retired subject bytes and performs bounded explicit
conversion. `api/assignees/resolution.rs` binds current candidates and renders
historical assignees with public profiles, rechecking grants after Directory
reads. Private assignment inventory/migration live in `server/management/assignments.rs`
and `cli/assignments.rs`. `src/cells/directory/` owns qualified private account identities and optional
public profiles. `accounts/` owns stable UUID registration; `profiles/` owns
chosen names/handles and immutable submissions. `migration/` alone reads format 1,
with bounded, restartable format-2 conversion and colocated historical-input tests.
Private status/migration/registration are in `server/management/directory.rs` and
the `cells` CLI. Registration does not grant permissions.
`cells/directory/lookup/` owns bounded qualified account-ID reads and public
profile reads by qualified key or actor UUID. `cells/directory/search/` owns bounded indexed handle resolution
and completion. Handle completion returns bounded public matches plus an exact
truncation flag. `api/discussion_search/authors/` binds public qualifiers to
qualified accounts before native repository reads and exposes the authorized
Author menu using those same public Directory choices. `api/profiles/attribution/` deduplicates list authors into one
Directory read; issue and PR handlers recheck durable membership afterward.
`src/api/profiles/` exposes session-authenticated profile reads/edits without
exposing authentication claims. `src/server/management/directory.rs` owns explicit
private initialization; serving never creates its schema implicitly. HTTP authorization and
Cell error mapping stay in `src/app.rs`. No previous-runtime readers or catalog
and session migration paths are supported.

## Module layout

- Use `<component>/mod.rs` for every component with child modules; keep its
  implementation, submodules, and tests in that directory. Avoid custom `#[path]`
  mounts and sibling `<component>.rs` entry points.
- `api/` owns browser HTTP feature handlers and route assembly; pull merge-tree
  mechanics live in `api/pulls/merge/`, comments in `api/pulls/comments.rs`,
  replies in `api/pulls/review_threads/replies.rs`, and thread persistence in
  `api/pulls/review_threads/`. `git/` owns native Git, object,
  receive, and LFS endpoints and their route assembly. `auth/` assembles login
  and session routes. `server/http/mod.rs` applies the common security boundary
  after merging these routers. Shared request policy remains in `app.rs`.
- `repository/` owns pinned reads, catalog materialization, maintenance, and
  browse projections. Its `settings.rs` supplies policy to both browser and
  native Git paths. `catalog/` owns durable catalog persistence and membership.
- `peer/` owns private transport with TLS in `peer/tls/`; `server/` composes these
  owners and manages ordered startup/shutdown.
- Keep unit tests with their module. Put larger suites in `<component>/tests/`;
  share only reusable setup through `test_support/`, gated by `cfg(test)`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| HTTP receive | `crates/crab-server/src/git/receive/mod.rs` | `crates/crab-write/src/journal.rs` |
| Content reads | `crates/crab-server/src/api/contents.rs` | `crates/crab-remote-git/src/snapshot.rs` |
| Auth or routing | `crates/crab-server/src/server/http/mod.rs` | `crates/crab-server/src/server/http/boundary.rs`, `src/auth/mod.rs` |
| Read readiness | `crates/crab-server/src/repository/mod.rs` | `src/server/background.rs`, `src/repository/maintenance.rs`, `crates/crab-write/src/generation.rs` |
| Management and capacity | `crates/crab-server/src/server/management/mod.rs` | `src/server/capacity.rs`, `src/metrics/mod.rs` |
| Git imports | `crates/crab-server/src/git_import/mod.rs` | `src/git_import/source.rs` (validation), `src/git_import/git.rs` (process isolation) |
| Browse APIs | `crates/crab-server/src/api/mod.rs` | `src/api/query.rs` (Git queries), `src/api/representation.rs` (encoding/ranges) |

## Invariants

- Keep transport identity/session checks and repository authorization before storage or Git effects; inspect auth and app boundary together.
  Source: `crates/crab-server/src/app.rs`.
- Git receive validation, publication outcome, and protocol acknowledgement are distinct steps; trace validate/publish and fault tests before changing error mapping.
  Source: `crates/crab-server/src/git/receive/mod.rs`.
- Repository operations and maintenance share long-lived runtime ownership; preserve explicit shutdown/draining when changing listener lifecycle.
  Source: `crates/crab-server/src/server/mod.rs`.

## Features and platform

No declared Cargo features. Both library and auto-discovered main binary exist. `crates/crab-server/build.rs` requires packages/ui/dist/index.html and rejects symlink assets. Build the frontend before any Cargo check/test/build for this crate; Node requirements are in its README.

## Verification

Tests live under their owner: `api/tests.rs`, `auth/tests/`,
`api/<feature>/tests.rs` (larger suites such as membership use `tests/mod.rs`), `auth/<feature>/tests.rs`, `catalog/tests.rs`,
`cells/repository/<feature>/tests.rs`, `cells/directory/tests.rs`,
`cells/directory/profiles/tests.rs`, `git/receive/tests/`, `git/lfs/tests.rs`,
`cells/directory/lookup/tests.rs`, `cells/directory/search/tests.rs`,
`cells/repository/search/authors/tests.rs`, `api/discussion_search/authors/tests.rs`,
`api/profiles/attribution/tests.rs`,
`api/pulls/tests/`, and `repository/tests/`. `server/peer_e2e_tests.rs`
exercises process composition and native peer handoff.
Shared setup lives in test-only `src/test_support/`; feature suites never import
fixtures from another suite. HTTP fixtures and their deterministic identity
provider belong in `test_support/http/`. Git commands/history belong in `test_support/git.rs`,
server/Cell lifecycle fixtures in `test_support/server.rs`, peer identities
in `test_support/tls.rs`, and archive-race recovery checks in
`test_support/archive.rs`.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
npm ci --prefix packages/ui
npm run build --prefix packages/ui
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-server --locked --lib auth::http::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-server --locked --lib git::receive::tests
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
