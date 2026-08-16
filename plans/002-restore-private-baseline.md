# Phase 002: Restore a trustworthy private-monorepo baseline

> **Executor instructions**: Complete this phase in the existing private
> monorepo before copying any source. Run every verification command with the
> external Cargo target directory shown below. Stop on a STOP condition rather
> than weakening a gate. Update Phase 002 in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 98b5c1e8..HEAD -- Cargo.toml Cargo.lock crab/Cargo.toml crab/Makefile crab/scripts/check-architecture-gates.py crab/scripts/check-crate-interface-builds.py crab/scripts/check-crate-behavior.py crab/docs/architecture/multi-crate-transition.md crates/*/Cargo.toml`
> Re-run the baseline command if any path changed and update the known failures
> below before editing.

## Status

- **Priority**: P0
- **Effort**: L (two to four days)
- **Risk**: MED — checker updates can accidentally bless a real ownership leak.
- **Depends on**: Phase 001
- **Category**: tests / tech-debt / migration
- **Planned at**: commit `98b5c1e8`, 2026-08-14

## Why this matters

The existing architecture gate is red. Exporting from a red baseline would make
it impossible to distinguish split regressions from pre-existing drift and
would invite the executor to delete checks merely to get green. This phase
makes the gate describe current intentional architecture and proves server
packages remain top-level composition boundaries.

## Current state

- `Cargo.toml:1` lists 23 workspace packages.
- `crab/Cargo.toml:1` reports CLI version `1.0.14`.
- `crates/crab-auth-server/Cargo.toml:1` and
  `crates/crab-cache-server/Cargo.toml:1` report version `1.0.12`.
- `crab/scripts/check-architecture-gates.py:1884` maintains explicit package,
  server, dependency, feature, and ownership expectations.
- `crab/Makefile:310` exposes `architecture-check` and its component gates.
- At planned commit `98b5c1e8`,
  `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 make architecture-check`
  fails. Observed failure classes:
  - shipped binary version mismatch (`1.0.12` versus `1.0.14`);
  - stale package/dependency expectations after `crab-agent-tools`, `crab-vfs`,
    and new edges were added;
  - stale `object_store` feature/version expectations;
  - ownership expectations for `crab-workflow`, `crab-xet`,
    `crab-cache-store`, metadata features, and pack-path delegation.
- The same run positively showed no normal reverse dependency from public
  packages to `crab-auth-server` or `crab-cache-server`; only two declared dev
  fixture edges exist. Phase 003 removes those dev edges.
- `crab/docs/architecture/multi-crate-transition.md` currently claims a green
  architecture status that no longer matches the executable gate.

## Commands you will need

First verify `/Volumes/Workspace` is mounted and create only the checkout-owned
target directory if missing. Never fall back to a local `target/` directory.

| Purpose | Command | Expected on success |
|---|---|---|
| Metadata | `cargo metadata --locked --no-deps --format-version 1` | exit 0 |
| Architecture | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 make architecture-check` | exit 0; every architecture component passes |
| Checker tests | `cd crab && PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_check_*architecture*.py'` | exit 0 |
| Server reverse tree | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 cargo tree --workspace -i crab-auth-server` and cache equivalent | only the server itself plus documented dev fixture owners; no normal public consumer |
| Format | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- `crab/scripts/check-architecture-gates.py` and its unit tests.
- Other existing `check-crate-*` scripts only where the same stale inventory is
  duplicated.
- `crab/docs/architecture/multi-crate-transition.md` status/evidence section.
- `crates/crab-auth-server/Cargo.toml` and
  `crates/crab-cache-server/Cargo.toml` version metadata.
- Root/workspace manifest corrections only when the executable dependency graph
  proves the current declaration is wrong.

**Out of scope**:

- Removing the two dev dependency edges; Phase 003 owns that work.
- Moving packages or changing public APIs.
- Dependency-advisory remediation; Phase 007 owns it.
- Changing Makefile install/release composition.
- Adding compatibility shims or new feature flags.
- Changing architecture policy to accommodate an unexplained edge.

## Steps

### Step 1: Capture a reproducible failing baseline

Run `make architecture-check` once and save only package names, failing rule
names, and non-sensitive output in
`docs/open-source/evidence/002-architecture-before.txt`. Do not commit absolute
paths, environment variables, credentials, or cloud identifiers.

**Verify**: the evidence file identifies every failing sub-gate and records
commit `98b5c1e8`; it contains no line matching
`(?i)(secret|token|password|access[_-]?key)\s*[:=]`.

### Step 2: Reconcile package and version inventories

Generate the actual package set with `cargo metadata`. Update checker constants
so all 23 packages are classified. Bring the two shipped private server binary
versions to `1.0.14` only because the current pre-split shipped-binary gate
requires one version. Add an inline note to Phase 004's private release policy
that service versions become independent after the repository split.

**Verify**: the shipped-binary-version component passes and metadata still
contains exactly 23 packages.

### Step 3: Reconcile dependency policy from proof

For every checker expectation that differs from `cargo metadata`, inspect the
owning package manifest and at least one production consumer. Classify each
edge as allowed or violation using
`crab/docs/architecture/multi-crate-transition.md`. Update the checker only for
allowed edges. Fix source/manifests for violations; do not add blanket
exceptions.

Preserve these hard assertions:

- server packages are not normal/build dependencies of public packages;
- `crab-auth` and `crab-cache` remain client/shared owners;
- provider SDKs stay behind their declared feature boundaries;
- SDK/Python/desktop do not gain a dependency on the `crab` CLI package.

**Verify**: dependency-policy, feature-policy, and ownership sub-gates each pass
when invoked independently.

### Step 4: Reconcile interface and behavior probes

Update crate-interface/behavior checks only where a documented public symbol or
feature was intentionally renamed. If a probe finds a missing delegation or
ownership rule, fix the production owner rather than deleting the probe.

**Verify**:

- `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 make crate-interface-check` exits 0.
- `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 make crate-behavior-check` exits 0.

### Step 5: Update architecture evidence truthfully

Replace any stale “passes as of” statement in
`multi-crate-transition.md` with the successful commit, date, commands, and
scope. Do not claim full tests or live-cloud evidence if they were not run.

**Verify**: every command cited in the new evidence block has been run from a
clean tree and exited 0.

## Test plan

- Add checker regression cases for: unclassified package, public normal edge to
  a server, public dev edge to a server, private server consuming a public
  crate, and SDK accidentally consuming CLI.
- Model tests after the existing Python checker self-tests in `crab/scripts/`.
- Run the full architecture target twice: once after source changes and once
  from a clean checkout/worktree at the resulting commit.

## Done criteria

- [ ] `make architecture-check` exits 0 at a recorded commit.
- [ ] All 23 packages are explicitly classified.
- [ ] Auth/cache server binary versions satisfy the current pre-split version
      contract.
- [ ] No unexplained dependency-policy exception was added.
- [ ] Checker tests cover public normal and dev edges to private packages.
- [ ] Architecture documentation cites only commands actually run.
- [ ] `cargo fmt --all -- --check` exits 0.
- [ ] `git diff --numstat` is reviewed; checker/source growth is justified and
      no unrelated source changed.
- [ ] Phase 002 status is `DONE`.

## STOP conditions

Stop and report if:

- Passing the gate appears to require deleting a policy assertion without a
  replacement.
- A private server is a production dependency of a public candidate package.
- A stale expectation cannot be resolved by reading its owner, caller, and
  documented architecture.
- The fix changes a serialized format, storage layout, remote protocol, CLI
  behavior, or any repository invariant.
- `/Volumes/Workspace` is unavailable.

## Handoff artifact

Provide the successful command transcript in
`docs/open-source/evidence/002-architecture-after.txt`, the resulting commit
SHA, and a list of the two remaining dev fixture edges for Phase 003.

## Maintenance notes

Architecture inventories must fail closed when a package is added. Do not call
unpublished packages “private” in checker names: `publish = false` is a
distribution choice, while proprietary ownership is defined by Phase 001.
