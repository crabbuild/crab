# Phase 009: Pin private services and products to an immutable public Core RC

> **Executor instructions**: Work in `crabbuild/crab-internal` on a dedicated
> migration branch. First add immutable Git dependencies and prove every private
> consumer. Remove duplicated public source only after all checks pass. Use
> explicit `git rm` path lists, never a broad recursive deletion. Update Phase
> 009 in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat <phase-004-private-sha>..HEAD -- Cargo.toml Cargo.lock crates crab crab-desktop crab-web .github docs/open-source`
> Also compare the Phase 008 candidate SHA/tag with the currently approved RC.
> Any mismatch requires requalification.

## Status

- **Priority**: P0
- **Effort**: L (four to seven days)
- **Risk**: HIGH — private services/products could silently compile against a
  different Core than users receive.
- **Depends on**: Phases 006 and 008
- **Category**: migration / tests / architecture
- **Planned at**: private source commit `98b5c1e8`, 2026-08-14; private and Core
  SHAs are supplied by prior handoffs

## Why this matters

After extraction, public Core must have one source of truth. Leaving private
path copies creates divergent protocols and bug fixes. Private auth/cache
servers, desktop agent, and documentation site should consume an immutable Core
release candidate exactly as an external downstream would, proving the public
API is sufficient before launch.

## Current state

- The private monorepo still contains public Core source as a temporary migration
  state after Phase 005.
- `crates/crab-auth-server/Cargo.toml:17` depends on many shared/core crates via
  `workspace = true` path dependencies.
- `crates/crab-cache-server/Cargo.toml:17` depends on public cache/storage/Xet
  crates.
- `crab-desktop/agent/Cargo.toml:12` consumes `crab-sdk` as its primary Core
  boundary.
- `crab/deploy/auth` packages/invokes private Rust receive/view helpers and must
  remain private.
- `crab-web/content/docs` currently duplicates Core docs; Phase 008 declared the
  public repository canonical.
- Public packages are intentionally `publish = false`; Cargo Git dependencies
  by immutable revision work without crates.io publication.
- Cargo's official
  [Git dependency reference](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html)
  confirms that one repository URL can supply multiple workspace packages and
  that `rev` can select a commit hash. `publish = false` only disables registry
  publication; it does not prevent a Git dependency.

## Target private repository shape

```text
crab-internal/
├── Cargo.toml                 private workspace only
├── Cargo.lock                 pins one public Core Git SHA
├── crates/
│   ├── crab-auth-server/
│   └── crab-cache-server/
├── services/                  auth API/cache deployment/managed service
├── crab-desktop/
├── crab-web/
├── docs/internal/
└── .github/workflows/         private service/product/compatibility CI
```

Public `crab/`, public shared crates, `crab-sdk`, and `crab-py` are absent from
the private current tree after migration, while their history remains in Git.

## Commands you will need

Use `/Volumes/Workspace/crabbuild-target/crab-internal` for private builds and
do not share the public target directory.

| Purpose | Command | Expected on success |
|---|---|---|
| Core ref | `git ls-remote https://github.com/crabbuild/crab.git refs/tags/<rc-tag>^{}` | exact approved Core commit SHA |
| Metadata | `cargo metadata --locked --format-version 1` | private members plus Git-sourced public packages at one SHA |
| Auth server | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-internal cargo test -p crab-auth-server --locked` | exit 0 |
| Cache server | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-internal cargo test -p crab-cache-server --locked` | exit 0 |
| Desktop | `cd crab-desktop && npm run typecheck && npm run test && npm run build:agent` | exit 0; agent uses Git `crab-sdk` |
| Docs sync | private web sync command with `<rc-tag>` and SHA | exit 0; provenance file matches Core |
| Duplicate check | approved script over private tree | zero current public Core source packages |

## Scope

**In scope**:

- Private root Cargo workspace/dependency declarations and lockfile.
- Both server package manifests and private server/auth/cache conformance tests.
- Desktop agent dependency on public `crab-sdk`.
- Relocating private deployment code out of the public `crab/` path before that
  path is removed from the private current tree.
- Tag/SHA-pinned public documentation synchronization into private `crab-web`.
- Private compatibility CI and coordinated public/private development procedure.
- Removal of duplicated public Core current-tree source after proof.

**Out of scope**:

- Publishing private code or making the private repository less restricted.
- Submodules or vendored copies of public Core.
- Depending on public `main` for release builds.
- Changing auth/cache wire behavior.
- Splitting private products/services into more repositories.
- Deleting private Git history.

## Steps

### Step 1: Create and identify the immutable RC

In the still-private public candidate, create an annotated/signed
`<launch-version>-rc.1` tag only after Phases 006–008 are green. Record its full
40-character commit SHA. Verify the tag object and commit signatures according
to governance policy.

**Verify**: `git ls-remote` resolves the tag to the approved candidate SHA; the
tree matches Phase 007/008 evidence.

### Step 2: Rehome private content currently under public paths

Move `crab/deploy/auth`, cache-service deployment content, and private docs into
approved private paths such as `services/auth-api`, `services/cache-service`,
and `docs/internal`. Update private-only workflows/package paths. Preserve
server package names and protocols; this is a repository-layout change only.

**Verify**: private deployment tests/builds pass at new paths and no private file
remains under the public CLI directory slated for removal.

### Step 3: Convert private Rust consumers to one Git revision

In private root `[workspace.dependencies]`, declare every public Crab package
with `git = "https://github.com/crabbuild/crab"` and the same full
`rev = "<40-char-sha>"`; preserve required feature flags/default-feature
settings. Private manifests continue using `{ workspace = true }`. Retain only
external workspace dependencies needed by private members.

Add `scripts/check-core-revision.py` that rejects branches/tags as the sole pin,
mixed revisions, path dependencies to removed public source, and a lockfile
resolving another commit. Keep the human-friendly RC tag in a comment or
machine-readable private version file, but enforce the SHA.

**Verify**: metadata reports every `git+https://github.com/crabbuild/crab` package
at exactly one approved SHA; checker negative tests catch mixed/mutable refs.

### Step 4: Qualify private servers against public contracts

Run auth helper/API integration tests and cache server unit/integration/RustFS
smoke tests. Verify public client DTOs/routes are the only shared contract.
Record compatibility ranges for Core version and auth/cache wire protocol.

**Verify**: real private server processes accept requests produced by the public
RC clients and return responses those clients parse; success and representative
error paths pass.

### Step 5: Qualify desktop and Python/product consumers

Build/test desktop agent against Git-sourced `crab-sdk`. If private product code
uses Python bindings, build them from the public RC source/release artifact, not
the old path. Run at least one Level 3 desktop/user action that reaches a real
Core side effect and visible result.

**Verify**: metadata/build logs identify only the approved Core SHA; desktop
typecheck/tests/agent build and selected E2E smoke pass.

### Step 6: Implement tag-pinned public-doc synchronization

Add a private web script/workflow that checks out/downloads docs from the exact
Core tag/SHA, applies only documented site transformations, writes a provenance
file, and fails on local hand-edited drift. Hosted-service docs remain a separate
private source tree.

**Verify**: two syncs are deterministic; provenance records tag/SHA; editing a
generated Core doc makes the drift gate fail.

### Step 7: Remove duplicated public Core current-tree source

Only after Steps 3–6 pass, list every path to remove and compare it with the
Phase 001 public allowlist. Use `git rm` on explicit paths in one migration
commit. Do not delete private relocated content, backup bundles, or history.
Regenerate the private lockfile and rerun all gates from a fresh clone.

**Verify**:

- private current tree contains no public Core package manifest/source copy;
- Cargo metadata still resolves all required Core packages from the approved
  Git SHA;
- server, desktop, docs-sync, and compatibility tests remain green.

### Step 8: Document coordinated-change workflow

Add `docs/open-source/DUAL-REPO-WORKFLOW.md`: public contract change lands and
tags RC first; private CI qualifies exact SHA; public release occurs; private
pin updates. For urgent private testing, use a temporary branch SHA in a review
branch, never a committed local path patch or `main` pin.

**Verify**: a dry-run dependency bump PR demonstrates checker, tests, and
rollback to the previous SHA.

## Test plan

- Checker unit tests for one SHA, mixed SHAs, branch-only ref, path leak, and
  lockfile mismatch.
- Real auth endpoint/helper and cache-server/client compatibility tests.
- Desktop typecheck, unit tests, agent build, and one E2E user-visible action.
- Deterministic docs sync and generated-drift failure test.
- Fresh private clone full metadata/tests after public-source removal.

## Done criteria

- [ ] Every private Rust consumer resolves public Core from one full immutable
      SHA associated with the approved RC tag.
- [ ] No private release build uses Core `main`, a local path, a submodule, or a
      vendored source copy.
- [ ] Auth/cache real-process compatibility tests pass for success/error paths.
- [ ] Desktop and other private product consumers pass their required gates.
- [ ] Private web consumes deterministic tag/SHA-pinned public docs.
- [ ] Public Core source duplicates are absent from the private current tree;
      original history remains intact.
- [ ] Coordinated update and rollback procedure is tested/documented.
- [ ] Phase 009 status is `DONE`.

## STOP conditions

Stop and report if:

- Any private consumer requires a symbol/file that was omitted from public Core.
- The approved RC SHA changes after Phase 007/008 review.
- Cargo resolves mixed Core commits or cannot use non-published Git packages.
- Relocating deployment code changes runtime behavior or loses infrastructure
  state.
- Any private test requires reintroducing a public-to-private dependency.
- A public source path is about to be removed before fresh-clone proof passes.

## Handoff artifact

Provide the Core RC tag/SHA, private migration SHA, dependency-resolution report,
server/product/desktop/docs-sync run URLs, compatibility matrix, explicit
removed-path list, and tested rollback procedure. Phase 010 launches only these
qualified commits.

## Maintenance notes

Public Core owns shared protocols and clients. Private servers may implement
them but may not fork DTOs or route constants. Upgrade one exact Core revision
per private PR, with lockfile review and compatibility evidence.
