# Phase 008: Publish accurate Core documentation and package metadata

> **Executor instructions**: Work in the private public-candidate repository.
> Write for a contributor/user with no access to Crab internal systems. Every
> command and link must be verified from a fresh clone. Do not describe planned
> managed-service behavior as shipped Core behavior. Update Phase 008 in
> `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat <phase-007-candidate-sha>..HEAD -- README.md Cargo.toml crab/Cargo.toml crates/*/Cargo.toml crab-sdk/Cargo.toml crab-py/pyproject.toml crab/docs docs .github/profile`
> If commands, features, or package surfaces changed, regenerate the
> documentation inventory before editing.

## Status

- **Priority**: P1
- **Effort**: L (two to four days)
- **Risk**: MED — inaccurate setup or boundary claims will mislead users and
  contributors immediately.
- **Depends on**: Phases 005 and 007
- **Category**: docs / dx
- **Planned at**: private source commit `98b5c1e8`, 2026-08-14; candidate SHA is
  supplied by Phase 007

## Why this matters

The current monorepo has no root README and its strongest overview lives under
`.github/profile`. Documentation also contains internal platform designs and a
private web copy. Public Core needs one canonical, truthful story: what users
can run themselves, which optional integrations contact managed services, how
to contribute, and what remains proprietary.

## Current state

- Root `README.md` is absent at planned commit.
- `.github/profile/README.md` contains reusable product overview material but is
  not a repository onboarding document.
- CLI docs live under `crab/docs`; `crab-web/content/docs` contains a separate
  copy rather than a reliable pinned source.
- Internal/private docs include managed-platform, cache-service implementation,
  enterprise readiness, `.kiro/specs`, `openspec`, and `crab/roadmap` material.
- Most Cargo packages have `license` and `description` but lack consistent
  `repository`, `homepage`, `documentation`, `readme`, `rust-version`,
  `keywords`, and `categories` metadata.
- `crab-py/pyproject.toml:65` points to `https://github.com/crab/crab`, not the
  approved `https://github.com/crabbuild/crab` URL.
- The public release is source/binaries first; packages remain
  `publish = false` and docs must not promise crates.io/PyPI distribution.

## Public message to preserve

Use this product boundary consistently:

```text
Crab Core is the open-source CLI, Git remote helper, data plane, storage and
workflow libraries, client/shared auth and cache contracts, SDK, and Python
bindings. Repositories can live directly in user-controlled S3/GCS/Azure object
storage without a mandatory Crab server. Crab's hosted auth, cache server, and
managed service are optional proprietary services and are not part of this
repository.
```

Do not call shared client crates “the closed-source auth/cache.” Name the
private components as server/hosted implementations.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| CLI help | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo run -p crab --locked -- --help` | exit 0; output matches docs |
| Rust docs | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo doc --workspace --no-deps --locked` | exit 0; no rustdoc warnings promoted by CI |
| Doc links | project-selected Markdown link checker over tracked `*.md` | exit 0; no broken internal/private link |
| Metadata | `cargo metadata --locked --no-deps --format-version 1` | all public package URLs use `crabbuild/crab` |
| Private leakage | `rg -n 'managed-platform|cache-service-implementation|replica-enterprise-readiness|\.kiro/specs|openspec|crab/roadmap' .` | no private document/path reference except explicit boundary prose |
| Fresh start | follow README quickstart in a fresh clone and temporary object-store test environment | documented visible result |

## Scope

**In scope**:

- Root `README.md` and public architecture/contributor documentation.
- Public CLI/SDK/Python docs and examples.
- Cargo/pyproject package metadata and consistent repository URLs.
- Public feature matrix, supported platforms, build/test/install instructions,
  release/channel status, and limitations.
- Managed-service boundary, telemetry/network behavior, security/privacy links,
  license, support, and contribution paths.
- A pinned mechanism in private `crab-web` to consume public docs; implemented
  in Phase 009, specified here as the canonical-source contract.

**Out of scope**:

- Publishing the private web source.
- Managed-service API/operator documentation.
- Internal roadmap, pricing, billing, tenancy, sales, or enterprise claims.
- New product features or command aliases.
- crates.io/PyPI publication promises.
- Release execution; Phase 010.

## Steps

### Step 1: Create the repository landing page

Build `README.md` from verified content, not a raw copy of the organization
profile. Include: one-sentence value, architecture sketch, status/support,
install, five-minute object-storage quickstart, how `git-remote-crab` is used,
Core-versus-managed boundary, platform limitations, build/test, docs,
contribution/security, and Apache-2.0 license.

Every shell block must run in a fresh clone. Use placeholders for bucket/account
names and never include real endpoints/credentials.

**Verify**: a reviewer with no private access follows install and quickstart to
a real local/RustFS side effect and visible Git result (Level 3 proof).

### Step 2: Replace internal architecture docs with public architecture

Create concise public documents, for example:

- `docs/architecture/overview.md`: CLI/remote-helper flow, chunking/dedup,
  metadata, object storage, coordination, VFS, SDK;
- `docs/architecture/crate-map.md`: each public package and dependency layer;
- `docs/architecture/service-boundary.md`: public auth/cache clients versus
  proprietary hosted servers, wire compatibility, no mandatory managed server;
- `docs/compatibility.md`: storage/serialized-format and protocol stability.

Do not publish internal transition plans, deployment topology, evidence runbooks,
roadmaps, or private source paths. Mention proprietary services only at the
contract level users need.

**Verify**: every public workspace member appears once in the crate map; the
boundary checker and leakage search pass.

### Step 3: Audit CLI, SDK, and Python documentation

Regenerate or verify command reference against `crab --help`; verify examples
compile/run where feasible. Document supported SDK/Python API status and clearly
mark unstable surfaces. Remove links to private code/issues. Fix
`crab-py/pyproject.toml` repository URL and all equivalent metadata.

**Verify**: help/reference diff has no undocumented shipped subcommand or
documented nonexistent flag; SDK doctests/examples and Python test examples
pass.

### Step 4: Normalize package metadata without expanding publication contract

Add workspace-inherited `repository`, `homepage`, `documentation`, and approved
minimum Rust version where truthful. Add package readmes only where useful.
Keep `publish = false`; metadata must not claim a crates.io page. Use the same
canonical GitHub and `https://crab.build/docs/` URLs.

**Verify**: a metadata script reports zero missing/incorrect required fields and
all packages remain non-publishable.

### Step 5: Document network, privacy, and optional services

List when Core contacts object-storage providers, OIDC/cloud identity endpoints,
optional Crab Auth/cache endpoints, and telemetry if present. State defaults and
configuration from code; do not guess. Link managed service privacy/terms from
the website only if those pages are public and current.

**Verify**: source owners review each network/default claim; tests or code
locations are cited in the docs PR description.

### Step 6: Declare public docs canonical and define web synchronization

Add `docs/MAINTAINERS.md` stating public Core docs are canonical. Define a
tag-pinned sync command for private `crab-web`, with generated provenance
recording Core tag/SHA and a CI drift check. Do not maintain two hand-edited
copies.

**Verify**: the contract specifies input ref, output path, transformation rules,
link handling, and failure behavior. Phase 009 implements it.

## Test plan

- Fresh-clone install/quickstart against local RustFS or another approved local
  backend; verify push/clone visible result.
- Run all shell snippets through a docs test harness where practical.
- Compile Rust docs/examples and run Python documentation examples.
- Link-check all Markdown with public/no-auth access.
- Diff generated CLI help/reference.
- Search for private paths, internal issue trackers, employee-only links, local
  absolute paths, and credential-like examples.

## Done criteria

- [ ] Root README explains value, setup, public/private boundary, status,
      security, contribution, and license.
- [ ] Fresh-clone quickstart reaches a real visible result without private
      access.
- [ ] Public crate map includes every workspace member and no private package.
- [ ] CLI reference matches shipped help; SDK/Python examples pass.
- [ ] Every package has approved canonical metadata and remains
      `publish = false`.
- [ ] Network/privacy/default claims are source-owner verified.
- [ ] Markdown links pass unauthenticated checks and no private document leaks.
- [ ] Public-docs canonical/sync contract is approved.
- [ ] Candidate remains private.
- [ ] Phase 008 status is `DONE`.

## STOP conditions

Stop and report if:

- A quickstart requires private credentials, private binaries, or managed
  service access despite being described as self-hosted Core.
- Documentation conflicts with actual CLI defaults or supported platforms.
- A public doc reveals internal deployment, customer, pricing, roadmap, or
  evidence detail.
- Package metadata suggests a publication/support commitment not approved in
  Phase 001.
- Public URLs/privacy terms do not exist or are stale.

## Handoff artifact

Provide the candidate SHA, fresh-clone quickstart transcript, link/help/metadata
reports, public doc inventory, and the approved web-sync contract. Phase 009
uses that exact ref for private documentation consumption.

## Maintenance notes

Docs change with behavior/API changes. Public Core is the canonical CLI/SDK
documentation source; the private website consumes a tag/SHA and may add hosted
service pages beside it, never overwrite Core facts.
