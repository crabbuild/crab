# AGENTS.md

Telegraph style. Root rules apply throughout the repository. Read the nearest scoped `AGENTS.md` before subtree work.

## Start

- Repo: [Crab](https://crab.build) monorepo — serverless Git remote helper. Repositories live in cloud object storage (S3/GCS/Azure), with no Crab data server.
- Replies: repo-root-relative refs only: `crab/src/cmd/push.rs:42`. No absolute paths, no `~/`.
- Fix/triage answers need source, tests, current behavior, and dependency contract proof.
- Reviews/answers: high confidence required. Default to exhaustive relevant codebase search/read, including callers, callees, siblings, tests, docs, and upstream/dependency contracts before verdict. Diff-only review is insufficient.
- Review default: read the whole changed function/module plus callers, callees, sibling implementations, adjacent tests, scoped docs before saying `good`, `bad`, `best fix`, or `proof sufficient`. If challenged, keep reading first; do not defend the earlier verdict until the missing path is checked.
- Before a PR verdict, build a small evidence map: changed surface, entry point, owner boundary, at least one caller and callee, sibling surfaces that share the invariant, existing tests, and current `main` behavior. If any cell is missing, say the gap instead of concluding.
- One-sided fixes need sibling-surface proof, an explanation for why siblings are unaffected, or explicit follow-up work.
- Every PR review must explicitly ask whether the PR is the best fix, not merely a plausible fix.
- Dependency-backed behavior: read upstream docs/source/types first. No API/default/error/timing guesses.
- Missing deps: `make install` (Rust), `npm install` (TS/JS), retry once, then report first actionable error.
- Live-verify when feasible. Never print secrets or AWS creds.
- New `AGENTS.md`: add sibling `CLAUDE.md` symlink; edit `AGENTS.md` only.

## Map

```
CrabBuild/
├── crab/              Rust CLI, remote helper, and product/server composition
├── crates/            Shared Rust contracts, data plane, storage, and orchestration
├── packages/web/      Next.js marketing site and Fumadocs documentation
├── diagram/           Architecture diagrams and rendered assets
├── .github/workflows/ CI, release, service, and evidence workflows
├── .agent/            Repository-local agent workflows
├── .claude/           Repository-local Claude commands and skills
└── .codex/            Repository-local Codex skills
```

Cargo workspace: 20 members — 19 shared crates under `crates/`, plus `crab`.
There is no desktop application, Python package, or SDK package in this workspace; desktop material under `packages/web/` is documentation and marketing content.

## Architecture

### crates (Shared Rust Libraries)

- Scoped guide and subsystem map: `crates/AGENTS.md`.
- Layers: shared contracts → Git/Xet/storage mechanics → metadata/staging/coordination → read/cache/auth/VFS/workflow → server and product composition.
- Low-level crates own reusable contracts and mechanics. Product wiring stays in binaries and product crates; server crates are top-level composition boundaries.
- Public types, feature flags, serialized formats, storage layouts, and errors are cross-crate contracts. Search all consumers before changing them.

### crab (Rust CLI)

- Entry: `crab/src/main.rs` — dual-mode: `git-remote-crab` remote helper + `crab` CLI.
- Subcommands: `crab/src/cmd/` (add, clone, push, hydrate, dehydrate, gc, fsck, mount, status, doctor, and other CLI commands).
- Product modules under `crab/src/` cover command policy, chunking/deduplication, storage, metadata, coordination, Git integration, cache, LFS, auth/read/hydration, tiering/replication, import, and restriping. Reusable contracts and mechanics belong in `crates/`.
- Stack: Rust 2024, tokio, `object_store`, `thiserror`, `tracing`, Blake3.

### packages/web (Next.js)

- Marketing site + Fumadocs documentation. Hosted at crab.build.
- Stack: Next.js 16 (Turbopack), React 19, Tailwind v4, Fumadocs, and shadcn/ui.
- Docs: source content lives under `packages/web/content/docs/`; generated Fumadocs output under `packages/web/.source/` is not hand-edited.
- Follow `AGENTS.md` and adjacent docs/components for web guidance.

### Design Principles

- Fix shape: default to clean bounded refactor, not smallest patch. Move ownership to the right boundary; delete stale abstractions, duplicate policy, dead branches, wrappers, fallback stacks.
- Fix observed local failures with generic product rules; do not hardcode names, ids, log phrases, or user examples in prod code unless they are an explicit contract.
- Compatibility is opt-in. "Shipped" means reachable from a release Git tag; main/GitHub/PR/unreleased code is not shipped.
- Refactor default: one canonical path. Delete the old path unless user explicitly wants compat or the shipped public contract is obvious and cited.
- Config/env surface bar is high. Before adding a config option or env var, first prove existing product behavior, defaults, or doctor migration cannot solve it. Prefer removing or consolidating config/env options when touching these surfaces.
- Fallback is a product decision, not an implementation convenience. Before adding one, name the shipped contract, failure mode, removal plan, and why doctor cannot solve it. Otherwise delete it.
- Keep old behavior only for an explicit public API/config/data contract, tagged upgrade path, security/migration boundary, or dependency contract.
- If unsure, ask before preserving compat. Do not keep aliases, shims, fallback stacks, stale names, or obsolete tests just in case.

## Disk and external-checkout policy

The main disk is limited to 100 GB. Rust build artifacts and repositories used
for real-repository qualification belong on the mounted workspace volume, not
in this checkout or on the main disk.

- Before any Cargo command that can compile (`build`, `check`, `test`,
  `clippy`, `bench`, `doc`, `install`, `package`, or a Make target that invokes
  one), set `CARGO_TARGET_DIR` beneath `/Volumes/Workspace/crabbuild-target`.
- Every repository checkout and worktree must have its own target directory.
  Never share one Cargo target directory between different repositories or
  concurrent worktrees: feature sets, build scripts, and locks can collide.
- Use a stable, descriptive directory such as
  `/Volumes/Workspace/crabbuild-target/crab-main` for this checkout and
  `/Volumes/Workspace/crabbuild-target/crab-<worktree-name>` for another Crab
  worktree. For another repository, include that repository and
  checkout/worktree name.
- Set the variable on every new shell/tool invocation; do not assume an export
  from an earlier command persists. For example:

  ```bash
  CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
    cargo test -p crab --locked
  ```

- Verify the volume is mounted and the chosen directory is writable before a
  long build. Create only the specific per-checkout directory needed. If
  `/Volumes/Workspace` is unavailable, stop and report it rather than falling
  back to a local `target/` directory.
- Do not delete, clean, or reuse another repository's target directory. Run
  `cargo clean` only with the intended `CARGO_TARGET_DIR` explicitly set and
  only when the task actually requires reclaiming or invalidating those
  artifacts.
- Find repositories used to qualify Crab against real repositories under
  `/Volumes/Workspace/Github` first. When a task genuinely requires a missing
  public repository, clone it under
  `/Volumes/Workspace/Github/<owner>/<repository>`; do not clone qualification
  repositories into the Crab tree, `/tmp`, or the main-disk GitHub folder.
- Treat external qualification repositories as read-only inputs. Do not
  modify, update, reset, or clean an existing checkout unless the task
  explicitly requires it. Keep generated Crab artifacts outside their tracked
  source or remove only artifacts created by the current task.

Some Makefile and script targets consume binaries through a literal local
`target/` path after Cargo finishes. Prefer direct Cargo commands with
`CARGO_TARGET_DIR` for normal verification. Before using packaging, install,
release, benchmark, or smoke-test targets, inspect the target and ensure its
artifact lookup also points at the selected external directory; never allow it
to trigger a second local build silently.

## Commands

### Rust (crab)

```bash
cd crab
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make install  # release binaries + install + git-remote-crab symlink
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make test     # full cargo test + error_codes test
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make clippy   # lint
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make fmt      # format
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make check    # fast compile check
```

**Never** `cargo install` or manually copy binaries. Always `make install`.

### packages/web

```bash
cd packages/web
npm install
npm run dev                # next dev --turbopack
npm run build              # next build
npm run typecheck          # tsc --noEmit
npm run lint               # eslint
npm run test               # vitest --run
npm run check:links        # docs/link validation
```

### Cross-compilation

`Cross.toml` at workspace root. Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`. Linux targets need `libfuse3-dev`.

## Code — Rust

- Rust 2024 edition, strict. `cargo fmt` before every commit.
- **No `unwrap()` / `expect()` / `panic!` outside tests** — a panic in filter-process or FUSE corrupts the worktree.
- No `todo!()` / `unimplemented!()` on merged paths.
- Every `TODO` links to an issue: `// TODO(#123): reason`.
- Errors: `Result<T, CrabError>` via crate alias. Extend with `thiserror`. No `anyhow`.
- Preserve source errors with `#[source]` / `#[from]`. Never stringify and discard.
- Prefer `?` for propagation; `match` only when handling variants differently.
- `tracing::error!` at the boundary, not every layer. Structured fields: `debug!(xorb_hash = %hash, "uploaded")`.
- Async: `tokio` only. No `std::sync::Mutex` across `.await`. `spawn_blocking` for CPU-bound work. Cancel-safety matters.
- Params: `&str` / `&[T]` / `&Path` unless storing. Return owned types from constructors.
- `Arc<T>` for shared state; `Arc<Mutex<T>>` only after proving `RwLock`/atomics won't do.
- Naming: `snake_case` modules, `CamelCase` types, `SCREAMING` constants. No `get_` prefix on accessors.
- Lints: prefer `#[expect(clippy::lint)]` over `#[allow(...)]`. See `clippy.toml`.
- No `unsafe` except FFI (FUSE, libgit2). Every `unsafe` block needs `// SAFETY:`.
- Use traits for behavior boundaries. Derive `Default` when all fields have sensible defaults.
- Prefer `From`/`Into`/`TryFrom`/`TryInto` over manual conversions. Prefer `Option<T>` over sentinel values.
- Prefer guard clauses (early returns) over nested `if` blocks. Prefer iterators/combinators over manual loops.
- **No banner/separator comments.** No decorative dividers like `// ── Section ───`. Normal `//` comments explain *why*, not *what*.
- No spec/task IDs in code comments. No requirement IDs, task numbers, or property numbers.
- Doc comments (`///`) for public APIs only: one-sentence summary, then preconditions/postconditions/error conditions. Skip examples for obvious functions.
- Keep public API surfaces small. Use `#[must_use]` where return values matter.

## Code — TypeScript (Web)

- Default to Server Components. `"use client"` only for scroll listeners, hover, animation, forms, browser APIs.
- Use `cn()` from `lib/utils.ts` for conditional class merging.
- shadcn/ui components in `components/ui/`. Marketing components in `components/marketing/`.
- SVG diagrams as React components in `app/diagrams/`. Use CSS custom properties for colors.
- Performance: LCP < 2.5s, CLS < 0.1. `loading="lazy"` on below-fold images. Explicit dimensions.
- Every marketing page needs at least one visual diagram.
- Follow `AGENTS.md` and adjacent components for visual identity, component patterns, and docs conventions.

## Code — General

- Code size matters. Prefer small clear code; maintainability includes not growing LOC without payoff.
- Prefer early returns over nested condition pyramids. Split code into gather → normalize → decide → act.
- Calls should be boring: complex decisions happen above; call args/object fields are names, literals, or simple property reads.
- Keep APIs narrow: export only current caller needs; keep types/helpers local by default.
- Return the smallest useful shape. Avoid broad result objects, flags, metadata unless callers use them.
- New helpers/files must pay rent immediately: fewer call paths, fewer concepts, or less repeated logic. No helpers for one-off compat, naming translation, or speculative resilience.
- Before adding helpers/files, check whether existing code can absorb the behavior with less new surface.
- Avoid adapter layers that only rename fields. Move real responsibility or leave code local.
- Refactors should delete about as much local complexity as they add. If LOC grows, the new ownership/API needs to clearly pay for it.
- For non-trivial refactors, check `git diff --numstat` before closeout. If non-test LOC grew, trim or explicitly justify.
- Prefer deleting branches, modes, adapters, and tests over preserving them. A refactor that adds a second path has probably failed unless the old path is a cited shipped contract.
- Tests alone do not make internals contracts. If compat stays, name the contract and migration/removal plan in code, test, or PR.
- Lean code is a goal. No internal shims, aliases, legacy names, broad fallbacks, or defensive branches just to reduce diff or handle unrealistic edge cases.
- Inline comments: preserve reviewer context at the code site. Required for non-obvious cross-path invariants, lifecycle ordering, ownership boundaries, cleanup/release coupling, fallback behavior, and intentional caller differences.
- Comment shape: 1-3 short lines; state why the branch/helper exists, what contract it protects, and the bad outcome if removed. No syntax narration, PR/user-specific lore, or obvious mechanics.
- Split files around ~700 LOC when clarity/testability improves.

## Tests

- **Rust**: `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make test`. Property tests with `proptest`. Snapshot tests with `cargo insta`. `#[tokio::test(flavor = "multi_thread")]` for concurrency.
- **Web**: `cd packages/web && npm run test` (Vitest); also run `npm run typecheck` and `npm run lint`, plus `npm run check:links` for docs/link changes.
- Name the property, not the requirement: `batch_remove_preserves_entries_for_non_deleted_xorbs`.
- One logical assertion per test. Clean timers/env/globals/mocks.
- Tests prove behavior/regressions, not every internal branch.
- Tests are welcome, but review them before landing for duplication and value. Delete useless tests, such as assertions for behavior or paths just removed.
- Tests protect canonical behavior and migration boundaries, not obsolete internals. Delete tests for removed fallback paths instead of updating them.
- Table-drive repetitive tests when it reduces code and keeps failure names clear.
- Do not edit baseline/inventory/ignore/snapshot/expected-failure files to silence checks without explicit approval.

## Invariants (Don't Break These)

1. All SlateDB instances closed on every exit path — leaks corrupt metadata.
2. Lock-then-push serialization per ref; every acquired lock is released.
3. GC never deletes referenced xorbs or anything inside the grace period.
4. Reconstruction is byte-identical to original or returns an error.
5. Staged xorbs must flush before any bundle push.
6. Shard reconstruction terms must cover ALL chunks for a file.
7. Staging `chunks_for_file(file_hash)` returns all chunks for that file version.

## Methodology

- **Think before coding.** State assumptions. If multiple interpretations exist, present them.
- **Simplicity first.** No features beyond what was asked. No speculative abstractions. If 200 lines could be 50, rewrite.
- **Surgical changes.** Don't "improve" adjacent code. Match existing style. Every changed line traces to the request.
- **Goal-driven execution.** Transform tasks into verifiable goals. Define success criteria. Loop until verified.
- **Search before assuming.** Search the codebase before assuming functionality is missing.
- **Documentation with code.** Documentation and comments must be kept up-to-date with code changes. Docs change with behavior/API.

## Feature Validation

A feature is only "working" if Level 3+ is achieved:

| Level | Meaning |
|-------|---------|
| 0 | Code exists, compiles |
| 1 | Unit tests pass (mocked deps) |
| 2 | Integration wiring (real deps injected) |
| **3** | **E2E smoke: user action → real side effect → visible result** |
| 4 | Error paths work (failures shown in UI) |
| 5 | Production ready (perf, a11y, persistence) |

Watch for: test-only wiring, unreachable side effects, optimistic UI lies, missing failure states, and integration paths that are never exercised.

## Validation

- Before handoff/push: prove touched surface. Before landing to `main`: issue proof plus appropriate full/broad proof unless scope is clearly narrow.
- Small/narrow tests, lints, format checks, and type probes are fine locally.
- Full suites, broad changed gates, E2E/live/cross-OS proof: use CI or dedicated test environment.
- If proof is blocked, say exactly what is missing and why.
- Do not land related failing format/lint/type/build/tests. If unrelated on latest `origin/main`, say so with scoped proof.
- Build before push when build output, packaging, or published surfaces can change.

## Security

- Never commit real credentials, phone numbers, or live config.
- AWS creds for E2E in `.env` (gitignored). Source before S3 operations.
- Never `crab gc --scope=bucket` — bucket-wide GC touches other users' data.
- Dependency patches/overrides/vendor changes need explicit approval.
- Lockfiles are security surface: review `Cargo.lock`, `package-lock.json` changes.
- Never edit `node_modules`.

## Docs

- Product name: **Crab**. CLI/package/path/config: `crab`.
- Blog: `packages/web/content/blog/`; follow adjacent posts and their frontmatter.
- SVG diagrams: `diagram/` and `packages/web/app/diagrams/`; follow nearby diagram components and existing rendering conventions.
- Docs change with behavior/API changes.
- Docs final answers: include relevant `https://crab.build/docs/...` URL(s) when applicable.

## Git

- Conventional-ish commits, concise, grouped.
- No merge commits on `main`. Rebase on latest `origin/main` before push.
- User says `commit`: your changes only. `commit all`: all changes in grouped chunks. `push`: may `git pull --rebase` first.
- User says `ship it`: commit intended changes, pull --rebase, push.
- Do not delete/rename unexpected files; ask if blocking, else ignore.
- After landing/ship final: include 2-5 sentence recap of what landed: behavior change, key files/surface, proof run.

## Do Not

- Modify tests to make them pass — fix the source code instead.
- Create stub/partial implementations without flagging them as such.
- Add features beyond what was asked. No speculative abstractions.
- "Improve" adjacent code, comments, or formatting that isn't part of the request.
- Remove pre-existing dead code unless asked — mention it instead.
- Add `any` or `@ts-nocheck` in TypeScript. Lint suppressions only intentional + explained.
- Hardcode names, ids, or log phrases in prod code unless they are an explicit contract.
- Add runtime shims, aliases, or fallback readers for legacy/retired shapes. Doctor/migration code only.

## Scoped Guidance

- `crates/`: read `crates/AGENTS.md` before changing shared crates.
- `crab/`, `packages/web/`, and `diagram/`: no additional tracked scoped `AGENTS.md`; inspect the nearest README, manifest, docs, tests, and neighboring implementations.
- E2E verification: use the `crab-cli-verification` skill.
- Repository-local agent workflows and task-specific skills live in `.agent/`, `.claude/`, and `.codex/`.
