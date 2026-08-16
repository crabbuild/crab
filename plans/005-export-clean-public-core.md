# Phase 005: Export a clean, private Crab Core candidate repository

> **Executor instructions**: Build the candidate from an explicit allowlist; do
> not remove private paths and then assume the remainder is safe. Create the new
> GitHub repository with private visibility. Do not import Git history. Run all
> checks against the exported directory before the first push. Update Phase 005
> in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 98b5c1e8..HEAD -- docs/open-source Cargo.toml Cargo.lock crab crates crab-sdk crab-py .github scripts .gitignore .gitmodules`
> Then run `python3 scripts/check-open-source-inventory.py`. Any unclassified
> package/path or changed Phase 001 decision is a STOP condition.

## Status

- **Priority**: P0
- **Effort**: L (three to five days)
- **Risk**: HIGH — the export defines what will eventually be irrevocably
  public.
- **Depends on**: Phases 003 and 004
- **Category**: migration / security
- **Planned at**: commit `98b5c1e8`, 2026-08-14

## Why this matters

The current Git object store includes historical deployment environments and
large generated binaries. Deleting their current paths would not remove them
from history. A deterministic clean snapshot gives Crab Core an auditable
starting point while the private historical repository remains intact.

## Current state

- The root workspace mixes public and private packages in `Cargo.toml:1`.
- The public CLI graph requires all public packages classified in Phase 001,
  including auth/cache clients and adapters.
- Private source includes `crates/crab-auth-server`,
  `crates/crab-cache-server`, `crab/deploy/auth`, and cache deployment code.
- Internal planning/design material exists under historical planning/spec
  material, `openspec`, `crab/roadmap`, and named managed-service architecture
  documents.
- `.gitmodules` points at optional `xet-core` reference material, while
  `Cargo.toml:49` uses published Xet crates. The submodule is not required in
  public Core.
- `crab-py/python/crab/_crab.abi3.so` is a tracked generated binary of roughly
  79 MiB and must not enter the new repository.
- Root `LICENSE` is Apache-2.0. Root public README/governance is completed in
  Phases 007–008.

## Public content contract

Include:

- root `Cargo.toml`, `Cargo.lock`, `Cross.toml`, formatter/lint configuration,
  `LICENSE`, `.gitignore`, and public-specific `AGENTS.md`;
- `crab` CLI/remote-helper source, tests, benches, schemas, examples, public
  docs, and only scripts needed to build/test/release Core;
- `crates/crab-agent-tools`, `crab-auth`, `crab-auth-store`, `crab-cache`,
  `crab-cache-store`, `crab-coordination`, `crab-diff`, `crab-git`, `crab-lfs`,
  `crab-metadata`, `crab-read`, `crab-staging`, `crab-storage`, `crab-types`,
  `crab-vfs`, `crab-workflow`, and `crab-xet`;
- `crab-sdk` and `crab-py` source, excluding built extensions;
- root `scripts/` only when an inventory entry proves it supports public Core;
- approved public GitHub configuration, initially with private-candidate
  workflows disabled until Phase 006.

Exclude:

- both server crates and `crab/deploy/auth`/cache-service deployment code;
- `crab-desktop`, `crab-web`, historical planning/spec material, private
  planning, `openspec`, `crab/roadmap`, managed-platform/cache-server/enterprise
  designs and evidence;
- `.git`, `.gitmodules`, `xet-core`, build outputs, `.env*`, credentials,
  caches, logs, test evidence containing environment identifiers, and compiled
  Python/Rust/native binaries;
- private CI, issue links, runbooks, deployment configs, and secrets metadata.

## Commands you will need

Use separate checkout-owned targets:

- source checks: `/Volumes/Workspace/crabbuild-target/crab-internal`
- candidate checks: `/Volumes/Workspace/crabbuild-target/crab-public`

| Purpose | Command | Expected on success |
|---|---|---|
| Inventory | `python3 scripts/check-open-source-inventory.py` | exit 0 |
| Export | `python3 scripts/export-open-source.py --output /Volumes/Workspace/CrabPublicCandidate` | exit 0; prints source SHA and file count |
| Reproducibility | run export twice to two empty directories, then `diff -qr <a> <b>` | no differences |
| Metadata | from candidate: `cargo metadata --locked --no-deps --format-version 1` | only public-core packages |
| Boundary | from candidate: `python3 scripts/check-open-source-boundary.py` | exit 0; zero forbidden packages/paths |
| Git history | after init: `git rev-list --count HEAD` | `1` after initial commit |

Do not reuse an existing output directory. The exporter must refuse a non-empty
destination rather than deleting it.

## Scope

**In scope in `crab-internal`**:

- `scripts/export-open-source.py` plus standard-library unit tests.
- Reviewed export manifest/template files under `docs/open-source/export/`.
- `.gitignore` rules for generated native/Python artifacts, especially `*.so`,
  `*.dylib`, `*.dll`, `*.pyd`, and build directories.

**In scope in the new candidate**:

- Generated/copy-approved public files.
- Public-only root workspace manifest.
- Public-specific `AGENTS.md` and sibling `CLAUDE.md` symlink.
- Initial Git repository and one import commit.
- Private GitHub repository `crabbuild/crab` with no release/package publication.

**Out of scope**:

- Public visibility.
- Full CI/release rewiring; Phase 006.
- Dependency/legal/security approval; Phase 007.
- Final README/docs; Phase 008.
- Removing duplicated public source from `crab-internal`; Phase 009.
- crates.io or PyPI publication.

## Steps

### Step 1: Implement a fail-closed exporter

Create `scripts/export-open-source.py` using only the Python standard library.
It must:

- require a clean source tree and an empty/nonexistent output directory;
- read the approved allow/deny lists and package ownership;
- copy only allowlisted tracked files (never walk-and-exclude);
- reject symlinks escaping the source tree;
- reject any allowlisted file also matching the denylist;
- reject files above an approved size threshold unless explicitly listed;
- reject compiled/native artifacts and `.env` files regardless of allowlist;
- generate a manifest with source commit, relative path, size, and SHA-256;
- use a reviewed public root `Cargo.toml` template containing only public
  members and dependencies;
- produce deterministic content, ordering, modes, and line endings.

Do not place timestamps, local absolute paths, usernames, or environment data
inside the exported tree.

**Verify**: unit tests cover empty-output enforcement, allow/deny overlap,
symlink escape, forbidden suffix, oversized file, deterministic manifest, and
private path rejection.

### Step 2: Generate and inspect the candidate

Export to `/Volumes/Workspace/CrabPublicCandidate`. Review the manifest rather
than relying only on directory names. Run searches for private package names,
deployment paths, managed-platform titles, internal roadmap markers, credential
file names, and generated binaries. User-facing doctor messages may mention a
managed cache server; source imports/deployment details may not.

**Verify**:

```text
find . -type f -size +10M -print                 -> no unexpected files
find . -type f \( -name '*.so' -o -name '*.pyd' -o -name '.env*' \) -print
                                                   -> no output
rg -n 'crab-auth-server|crab-cache-server' --glob Cargo.toml
                                                   -> no output
```

### Step 3: Make the public workspace internally complete

Generate a root workspace containing only the approved public packages. Keep
all Crab packages `publish = false` for initial launch. Remove workspace
dependencies for private packages. Retain only external dependencies actually
used by public members. Omit `.gitmodules` and the `xet-core` exclusion.

**Verify**: parse `cargo metadata`; its package-name set exactly matches the
Phase 001 `public-core` set, and every Crab-owned package has publishing
disabled.

### Step 4: Add a public boundary gate

Copy/adapt the approved boundary checker into the candidate. It must fail if a
forbidden server package/path appears, if a workspace member is unclassified,
or if a package declares a dependency on a forbidden package name. It must not
depend on private files to pass.

**Verify**: positive run exits 0; unit fixtures inserting a private package,
path, and dev dependency each fail with a specific message.

### Step 5: Initialize clean history and push privately

Review `git diff --no-index` between the export manifest and candidate. Initialize
a new Git repository, create one signed/import commit by an authorized
maintainer, add private remote `crabbuild/crab`, and push while visibility is
confirmed `PRIVATE`. Credit prior contributors in a temporary provenance note;
Phase 007 finalizes public attribution.

**Verify**:

- `git rev-list --count --all` is `1` unless an explicitly reviewed CI follow-up
  commit is necessary;
- `git log --all -- crab/deploy/auth crates/crab-cache-server` has no output;
- GitHub API reports private visibility;
- the pushed tree hash matches the locally reviewed candidate.

## Test plan

- Exporter unit tests for all fail-closed cases above.
- Run the exporter twice from the same clean source commit and byte-compare.
- Run Cargo metadata and `cargo check -p crab --locked` from a fresh candidate
  clone using `/Volumes/Workspace/crabbuild-target/crab-public`.
- Run public boundary tests with intentionally bad temporary fixtures.
- A second reviewer independently samples every allowlisted mixed directory and
  signs the export manifest review.

## Done criteria

- [ ] Export is deterministic from one recorded private source SHA.
- [ ] Candidate contains exactly the approved public package set.
- [ ] Candidate contains zero private paths, package dependencies, native build
      artifacts, `.env` files, or submodules.
- [ ] All Crab-owned Rust packages have registry publication disabled.
- [ ] Public boundary checker and negative tests pass.
- [ ] Fresh-clone metadata and `cargo check -p crab --locked` exit 0.
- [ ] Candidate repository is still private and has clean import history.
- [ ] Two reviewers sign the export manifest review.
- [ ] Phase 005 status is `DONE`.

## STOP conditions

Stop and report if:

- Any file cannot be confidently classified.
- A public package still imports a private package or file.
- The exporter needs a broad “copy all except” rule.
- A real credential, customer identifier, private endpoint, or proprietary
  deployment detail is found. Revoke/rotate credentials and reopen Phase 001.
- A contributor/license decision from Phase 001 is not approved.
- The GitHub candidate is accidentally public.

## Handoff artifact

Provide the private source SHA, public candidate SHA/tree hash, export-manifest
SHA-256, file/package counts, reviewer approvals, and successful boundary/Cargo
metadata output. Phases 006–008 operate only on that candidate.

## Maintenance notes

The exporter is a one-time migration safety tool, not the long-term source sync.
After launch, public Core is edited in its own repository and private consumers
pin releases. Retain the export manifest in private audit records.
