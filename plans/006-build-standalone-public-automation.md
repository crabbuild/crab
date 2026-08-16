# Phase 006: Make public Core build, test, install, and release independently

> **Executor instructions**: Work only in the private public-candidate repository
> created by Phase 005. Remove private service assumptions from automation; do
> not weaken public CLI checks. Use `/Volumes/Workspace/crabbuild-target/crab-public`
> for every compiling Cargo command. Update Phase 006 in `plans/README.md` when
> complete.
>
> **Drift check (run first)**:
> `git diff --stat <phase-005-candidate-sha>..HEAD -- Cargo.toml Cargo.lock crab/Makefile crab/scripts .github Cross.toml`
> Replace the placeholder with the Phase 005 handoff SHA. If install/release
> scripts changed, re-map every produced binary before editing.

## Status

- **Priority**: P0
- **Effort**: L (three to five days plus matrix CI)
- **Risk**: HIGH — release changes can produce incomplete or mislabeled user
  artifacts.
- **Depends on**: Phase 005
- **Category**: dx / tests / migration
- **Planned at**: private source commit `98b5c1e8`, 2026-08-14; candidate SHA is
  supplied by Phase 005

## Why this matters

The source export is not viable until a contributor can clone, test, install,
and release it without private packages or evidence. Current local build/install
targets compile and install the cache server even though public release archives
already exclude it. Current release CI also gates CLI builds on private
cache-service evidence. This phase makes public automation match the public
product boundary.

## Current state

- `crab/Makefile:317` builds `crab-cache-server` on both Darwin and other hosts.
- `crab/Makefile:847` passes a cache-server binary to the installer.
- `crab/scripts/install-binaries.py:52` requires and installs the cache server at
  lines 67–68 and 83–84.
- `crab/scripts/check-release-archive-contents.py:13` already forbids
  `crab-cache-server`, `crab-auth-receive`, and `crab-auth-view` from CLI
  archives. Preserve this invariant.
- `.github/workflows/release.yml:247` implements private cache-service evidence;
  the public CLI build job depends on it at line 531. Public releases cannot
  depend on private repository evidence or secrets.
- Public Core still needs OS/feature coverage for FUSE, NFS, storage providers,
  coordinators, replication control planes, SDK, and Python bindings.

## Required public commands

The final root/public README must be able to state these commands truthfully:

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo fmt --all -- --check` | exit 0 |
| CLI tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo test -p crab --locked` | exit 0 |
| Workspace tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo test --workspace --locked` | exit 0 on supported host |
| Clippy | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo clippy --workspace --all-targets --locked -- -D warnings` | exit 0 |
| Architecture | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public make architecture-check` | exit 0 without private files |
| Install | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public make install PREFIX=<temp-prefix>` | installs only public binaries/helpers |
| Archive contract | `cd crab && python3 scripts/check-release-archive-contents.py` | exit 0 |

## Scope

**In scope**:

- `crab/Makefile` public build/test/install/release targets.
- `crab/scripts/install-binaries.py` and install-layout tests.
- Release/archive/Homebrew scripts and tests.
- `.github/workflows/` public CI, release, code scanning, and dependency review.
- Public feature/OS matrix and cache configuration.
- Public release checksums, SBOM generation hook, and artifact attestation hook;
  policy enforcement completes in Phase 007.

**Out of scope**:

- Any server/service build or deployment.
- Private live enterprise evidence.
- Publishing a real release/tag; Phase 010.
- Dependency advisory remediation; Phase 007.
- Product documentation; Phase 008.
- Adding new CLI features or changing default features.

## Steps

### Step 1: Make local build and install Core-only

Remove cache-server variables, Cargo builds, installer parameters, installed
files, uninstall paths, and messages from the public Makefile/installer. Keep
`crab`, `git-remote-crab`, and applicable FUSE/NFS helpers. Preserve the rule
that repository instructions use `make install`, not `cargo install`.

Update install-layout tests to assert exact contents. Do not merely make the
cache binary optional: the public installer must have no knowledge of it.

**Verify**: install into a newly created temporary prefix and compare sorted
entries with the platform contract. `crab-cache-server`, `crab-auth-receive`,
and `crab-auth-view` are absent; `crab version` and remote-helper invocation
work.

### Step 2: Classify and split checker scripts

Inventory every script invoked by `make test`, `architecture-check`, feature
gates, install, and release. Keep Core contract checks public. Remove or replace
checks whose owner is private service deployment/evidence. A public check may
assert that private binaries are absent but may not import private code.

Particular scripts to inspect include:

- `check-final-integration.py`
- `check-crate-interface-builds.py`
- `check-crate-behavior.py`
- `check-shipped-binary-versions.py`
- `check-install-layout.py`
- `check-auth-helper-packaging.py`
- `check-architecture-gates.py`
- cache-service and enterprise-evidence scripts.

**Verify**: `rg -n 'crab-(auth|cache)-server|cache-service.*evidence' crab/Makefile crab/scripts .github`
returns only explicit forbidden-content assertions or public compatibility
diagnostics, never a build/import/path requirement.

### Step 3: Build a public pull-request CI workflow

Create a minimal required workflow with separate jobs for:

- formatting and static boundary/inventory checks;
- Linux workspace test and clippy with `libfuse3-dev`;
- macOS CLI/mount-helper build and focused tests;
- Windows no-FUSE feature build/test;
- feature-matrix compile for provider/coordinator/replication features;
- SDK and Python binding build/test;
- release archive content self-test.

Use least-privilege workflow permissions and dependency caching keyed by
`Cargo.lock`, Rust toolchain, target, and features. No job uses production cloud
credentials on pull requests.

**Verify**: workflow lint passes; a pull request runs all required jobs from a
fork-compatible permission model; boundary negative-test fixture fails as
expected on a test branch.

### Step 4: Make release CI public-only

Remove private cache-service and retained enterprise-evidence jobs from the
public release dependency graph. Retain or replace their Core-relevant compile
and deterministic local/RustFS tests. Build archives from tags with pinned
toolchain/lockfile and exact feature sets. Generate SHA-256 checksums and invoke
SBOM/attestation steps, finalized in Phase 007.

**Verify**: workflow graph has no job that checks out a private repo, downloads
private evidence, or requires organization-only deployment secrets. A dry run
produces every platform archive without publishing it.

### Step 5: Verify release archive and Homebrew contracts

Run the existing archive-content checker, inspect every archive, and exercise a
local Homebrew formula against the dry-run artifacts where supported. Preserve
platform-specific helper expectations.

**Verify**: each archive has the exact approved binary list; checksums match;
installed binaries report the candidate version/commit.

### Step 6: Document the one-command contributor gate

Add a public `make check` or root script that invokes the supported local subset
without private services. Keep full OS/E2E validation in CI. State prerequisites
and external target-dir policy in public `AGENTS.md` and contributor docs.

**Verify**: a fresh contributor clone follows only public instructions and gets
exit 0 without access to `crab-internal`.

## Test plan

- Unit-test installer exact layouts on Unix and Windows path semantics.
- Run CLI/workspace tests and clippy on Linux CI; focused builds/tests on macOS
  and Windows.
- Run feature matrix with default, no-default, gix-all, provider, coordinator,
  replication, NFS, and FUSE-supported combinations.
- Dry-run every release target and inspect archive contents/checksums.
- Test pull-request workflows from a fork/no-secrets context.

## Done criteria

- [ ] Fresh public clone passes its documented local gate without private
      access.
- [ ] Public build/install contains no cache/auth server knowledge.
- [ ] Required CI is green on Linux, macOS, and Windows-supported feature sets.
- [ ] Public release workflow has zero dependency on private evidence, repos, or
      secrets.
- [ ] Dry-run archives contain exact public binaries and valid checksums.
- [ ] Archive forbidden-binary checks remain enforced.
- [ ] No actual release/package was published.
- [ ] Phase 006 status is `DONE`.

## STOP conditions

Stop and report if:

- A public test/release requires private source or credentials.
- Removing a private gate eliminates coverage for a Core behavior with no local
  or public replacement.
- A platform archive cannot be reproduced or reports the wrong version.
- A Make target silently writes a local `target/` instead of the external
  checkout-owned target directory.
- Fixing automation appears to require changing product behavior.

## Handoff artifact

Provide the candidate SHA, required CI job list, all green run URLs, dry-run
archive manifest/checksums, and a fresh-clone transcript. Phase 007 treats those
exact artifacts as its security/legal review target.

## Maintenance notes

Private qualification may consume public RC artifacts, but public releases must
never wait on inaccessible evidence. Any future public binary must be added to
the exact-layout and forbidden-private-binary checks in the same change.
