# Phase 001: Freeze the Crab Core public/private boundary

> **Executor instructions**: Follow this phase step by step. Run every
> verification command and confirm its expected result before moving on. If a
> STOP condition occurs, stop and report; do not infer ownership. When complete,
> update Phase 001 in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 98b5c1e8..HEAD -- Cargo.toml crab/Cargo.toml crab-sdk/Cargo.toml crab-py/pyproject.toml crab-desktop/agent/Cargo.toml crates crab/deploy crab/docs .github`
> If package membership, dependency edges, deployment paths, or product scope
> changed, refresh the inventories before asking for approval.

## Status

- **Priority**: P0
- **Effort**: M (one to two days, mostly owner/legal review)
- **Risk**: HIGH — a wrong inclusion can publish proprietary code; a wrong
  exclusion can make Core unbuildable.
- **Depends on**: none
- **Category**: direction / migration
- **Planned at**: commit `98b5c1e8`, 2026-08-14

## Why this matters

The split must follow compile-time ownership, not marketing labels. The CLI
directly depends on auth/cache client contracts, while server packages are
top-level implementations. An approved machine-readable boundary prevents
later phases from guessing which files are safe and makes the public export
repeatable.

## Current state

- `Cargo.toml:1` defines one 23-package workspace containing public candidates,
  server packages, SDK/Python, and the desktop agent.
- `crab/Cargo.toml:152` directly links `crab-auth`, `crab-auth-store`,
  `crab-cache`, and `crab-cache-store`; these must be public for the CLI to
  compile.
- `crab/docs/architecture/multi-crate-transition.md:30` says `crab-auth` owns
  client/shared auth and must not own server binaries or route handlers.
- `crab/docs/architecture/multi-crate-transition.md:60` gives the equivalent
  boundary for `crab-cache`.
- `crates/crab-auth-server/Cargo.toml:1` owns the private receive/view helper
  binaries. The Python HTTP endpoint lives at `crab/deploy/auth`.
- `crates/crab-cache-server/Cargo.toml:1` owns the private cache-service runtime.
- Internal designs exist under `crab/docs/architecture/managed-platform.md`,
  `crab/docs/architecture/cache-service-implementation.md`, `.kiro/specs`,
  `openspec`, and `crab/roadmap`.
- The repository has Apache-2.0 at `LICENSE:1`, but contributor provenance and
  third-party license policy have not been approved for a public snapshot.

The ownership rule to preserve is:

```text
public reusable contracts/mechanics -> no dependency on private composition
private servers/products -> immutable dependency on public reusable code
```

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Enumerate packages | `cargo metadata --no-deps --format-version 1` | exit 0; 23 current packages before the split |
| Find server references | `rg -n 'crab-(auth|cache)-server|crab/deploy/(auth|cache-service)' Cargo.toml crab crates .github` | reviewed matches only; command may exit 0 |
| Find tracked paths | `git ls-files` | exit 0 |
| Confirm no source mutation | `git status --short` | only approved boundary artifacts changed |

Do not run a Cargo command that compiles in this phase.

## Scope

**In scope**:

- Create `docs/open-source/BOUNDARY.md`.
- Create `docs/open-source/public-paths.txt` as an allowlist of path prefixes.
- Create `docs/open-source/private-paths.txt` as an explicit denylist.
- Create `docs/open-source/package-ownership.toml` with one record per Cargo
  package: `public-core`, `private-platform`, or `private-product`.
- Create `docs/open-source/DECISIONS.md` containing the approvals below.
- Review and approve repository names, licensing, contributor provenance,
  trademark/branding, and launch scope.

**Out of scope**:

- Moving or deleting source.
- Renaming a GitHub repository.
- Changing visibility.
- Fixing dependencies or CI.
- Publishing to crates.io, PyPI, Homebrew, or GitHub Releases.

## Required decisions

Record an owner, date, and explicit `approved` or `rejected` result for each:

1. Public repository: `crabbuild/crab`.
2. Private historical repository: `crabbuild/crab-internal`.
3. License: Apache-2.0 for the approved public snapshot.
4. Initial distribution: source and binary releases only; every Rust package
   remains `publish = false` until a separate crates.io plan is approved.
5. Public products: CLI, remote helper, mount helpers, core crates, `crab-sdk`,
   `crab-py`, and `crab-agent-tools`.
6. Private products: auth server/helpers, cache server, deployment/IaC,
   managed-service code and operations, `crab-desktop`, and `crab-web` source.
7. Public documentation exclusions: internal roadmaps, active OpenSpec/Kiro work,
   managed-platform implementation designs, enterprise evidence, operational
   runbooks, billing/tenancy, and deployment internals.
8. History policy: preserve full history only in the private repository; public
   history starts from one curated import commit.
9. Contributor provenance: confirm the right to publish contributions from all
   authors in `git shortlog -sne --all`; record the evidence location without
   embedding private correspondence.
10. Trademark policy: define allowed community use of the Crab name/logo and
    whether a separate `TRADEMARKS.md` is required.

## Canonical package classification

The initial `package-ownership.toml` must classify at least:

```toml
[public-core]
packages = [
  "crab", "crab-agent-tools", "crab-auth", "crab-auth-store",
  "crab-cache", "crab-cache-store", "crab-coordination", "crab-diff",
  "crab-git", "crab-lfs", "crab-metadata", "crab-read", "crab-sdk",
  "crab-staging", "crab-storage", "crab-types", "crab-vfs",
  "crab-workflow", "crab-xet", "crab-py",
]

[private-platform]
packages = ["crab-auth-server", "crab-cache-server"]

[private-product]
packages = ["crab-desktop-agent"]
```

`BOUNDARY.md` must additionally classify non-Cargo paths. The public allowlist
must include root Cargo/license/config files, `crab/`, approved `crates/*`,
`crab-sdk/`, `crab-py/`, public scripts/docs/tests, and selected public GitHub
workflows. The private denylist must name server crates, `crab/deploy/auth`,
`crab/deploy/cache-service`, desktop/web, `.kiro/specs`, `openspec`,
`crab/roadmap`, private architecture/design docs, and live evidence.

## Steps

### Step 1: Generate and review the complete inventory

Export Cargo package names from `cargo metadata` and tracked paths from
`git ls-files`. Group every package and top-level path by ownership. For mixed
directories such as `crab/docs` and `.github/workflows`, enumerate individual
files rather than allowing the whole directory.

**Verify**: compare sorted Cargo package names with the three TOML arrays. A
small Python/TOML check must report `23 classified, 0 missing, 0 duplicated`.

### Step 2: Write the boundary rationale

In `BOUNDARY.md`, explain why clients/shared DTOs are public while server
implementations are private. Cite the current architecture contract at
`crab/docs/architecture/multi-crate-transition.md:30` and `:60`. State that
public code must not use private packages even as dev dependencies.

**Verify**:
`rg -n 'normal, build, or dev|auth/cache client|server implementation' docs/open-source/BOUNDARY.md`
returns all three concepts.

### Step 3: Record approvals and unresolved exceptions

Complete all ten required decisions in `DECISIONS.md`. Each entry has:
decision, owner, date, evidence/reference, status, and consequence. Use
`pending` rather than guessing. No later phase starts while any P0 decision is
pending.

**Verify**: a script or manual table check reports `0 pending P0 decisions`.

### Step 4: Add an inventory consistency check

Add a read-only script under `scripts/check-open-source-inventory.py` that:

- parses `cargo metadata --no-deps`;
- parses `package-ownership.toml` with Python 3.11 `tomllib`;
- rejects missing or duplicate package ownership;
- rejects a tracked path matching both allowlist and denylist;
- rejects private server/deployment prefixes from the allowlist.

The script must not copy files or mutate the tree.

**Verify**: `python3 scripts/check-open-source-inventory.py` exits 0 and prints
the classified package and path counts.

## Test plan

- Unit-test the inventory checker with temporary manifests for: complete
  classification, missing package, duplicate package, and allow/deny overlap.
- Put tests at `scripts/test_check_open_source_inventory.py` and use only the
  Python standard library.
- Run:
  `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest scripts/test_check_open_source_inventory.py`
  → all four cases pass.

## Done criteria

- [ ] All ten required decisions have named approval; no P0 status is pending.
- [ ] Every current Cargo package is classified exactly once.
- [ ] Every tracked top-level path is covered by a public allowlist or private
      denylist rule; mixed directories use file-level rules.
- [ ] Public and private path rules have zero overlap.
- [ ] `python3 scripts/check-open-source-inventory.py` exits 0.
- [ ] Inventory checker unit tests exit 0.
- [ ] No source, manifest, workflow, or repository setting changed.
- [ ] Phase 001 status in `plans/README.md` is `DONE`.

## STOP conditions

Stop and report if:

- Any contributor's right to relicense/publish under Apache-2.0 is disputed or
  unconfirmed.
- A package has both public and private responsibilities that cannot be divided
  without changing its API.
- Managed-service source exists outside the current denylist.
- The repository name `crabbuild/crab` or `crabbuild/crab-internal` is not
  available/approved.
- Legal or trademark review requires a different license or attribution model.

## Handoff artifact

The next phase receives the approved `docs/open-source/` directory plus the
successful inventory-check output. Treat these files as the source of truth;
any boundary change requires reopening Phase 001.

## Maintenance notes

Review `package-ownership.toml` whenever a Cargo package is added. Review both
path lists whenever a new deployment, workflow, design document, generated
artifact, or product directory appears. A reviewer should reject wildcard
allowlist expansions unless every newly included file is inspected.
