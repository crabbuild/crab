# Phase 004: Establish `crabbuild/crab-internal` as the private source of record

> **Executor instructions**: This phase includes an external repository rename.
> Perform it only with explicit organization-owner authorization and a named
> maintenance window. Preserve the current repository and history; do not
> rewrite or delete them. Update Phase 004 in `plans/README.md` after remote and
> protection checks pass.
>
> **Drift check (run first)**:
> `git diff --stat 98b5c1e8..HEAD -- .github Cargo.toml Cargo.lock crates/crab-auth-server crates/crab-cache-server crab/deploy crab-desktop crab-web docs/open-source`
> Also run `git remote -v` and compare the current GitHub repository name with
> this plan.

## Status

- **Priority**: P0
- **Effort**: M (one maintenance window plus one day of configuration)
- **Risk**: HIGH — repository rename/protection mistakes can disrupt all clones
  and automation.
- **Depends on**: Phases 001 and 003
- **Category**: migration / dx
- **Planned at**: commit `98b5c1e8`, 2026-08-14

## Why this matters

The current repository contains the full private history and should never be
made public. Renaming it first reserves a durable private source of record and
frees `crabbuild/crab` for the curated public snapshot. Keeping all history
private also preserves recovery and attribution without exposing deleted
deployments or generated artifacts.

## Current state

- The configured origin is `https://github.com/crabbuild/crab.git`.
- The repository mixes public candidates, private servers/deployments, desktop,
  web, internal specs, and evidence.
- `crates/crab-auth-server/Cargo.toml` and
  `crates/crab-cache-server/Cargo.toml` do not currently declare
  `publish = false`.
- `.github/workflows/release.yml:247` contains a private cache-service evidence
  gate, and its CLI build currently depends on that gate at line 531.
- The private repository will temporarily retain public Core source until Phase
  009 proves immutable Git dependencies. Duplication is a migration state, not
  the final architecture.
- GitHub redirects ordinary clone/fetch/push URLs after a rename, but the
  redirect stops when the old name is reused. Phase 005 intentionally reuses
  `crabbuild/crab`, so every maintained private clone and automation remote must
  move to `crabbuild/crab-internal` before then. GitHub-hosted Action references
  do not redirect after a repository rename at all. See the official
  [repository rename contract](https://docs.github.com/en/repositories/creating-and-managing-repositories/renaming-a-repository).

## Commands and administrative checks

| Purpose | Command/check | Expected on success |
|---|---|---|
| Backup | `git bundle create /Volumes/Workspace/crabbuild-backups/crab-internal-pre-split-<date>.bundle --all` | exit 0; bundle verified with `git bundle verify` |
| Rename | GitHub organization-owner rename `crabbuild/crab` → `crabbuild/crab-internal` | repository remains private |
| Remote | `git remote set-url origin git@github.com:crabbuild/crab-internal.git` | `git remote -v` shows only the private canonical origin |
| Reachability | `git ls-remote git@github.com:crabbuild/crab-internal.git HEAD` | expected HEAD SHA |
| Visibility | `gh repo view crabbuild/crab-internal --json visibility,nameWithOwner,defaultBranchRef` | `PRIVATE`, correct owner/name/default branch |
| Baseline | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-internal make architecture-check` | exit 0 |

Create only the explicit backup directory shown. Do not overwrite an existing
bundle; use a new timestamped filename.

## Scope

**In scope**:

- Rename the existing private GitHub repository.
- Update local remotes, CI references, badges, internal clone URLs, and release
  secrets that must follow the rename.
- Add root `PRIVATE.md` explaining classification, access, and the public Core
  relationship.
- Add `publish = false` to private Rust packages/binaries.
- Configure branch protection, private-team permissions, secret scanning, and
  environment protection for deployments.
- Add `docs/open-source/PRIVATE-REPOSITORY-OPERATIONS.md` with backup, access,
  incident, and coordinated-change procedures.

**Out of scope**:

- Creating or making the public repository visible.
- Deleting public Core source copies.
- Changing package dependencies to Git revisions; Phase 009 owns that.
- Splitting desktop/web/services into additional repositories.
- Rewriting private history.
- Rotating credentials without a finding; Phase 007 owns scan-based rotation.

## Steps

### Step 1: Freeze and back up

Announce a merge/release freeze. Confirm all branches and tags are pushed.
Create and verify a full bundle on `/Volumes/Workspace`. Record its path,
checksum, and verification output in an access-controlled operations log, not
the public tree.

**Verify**: `git bundle verify <bundle>` exits 0 and the backup checksum is
stored in the private operations record.

### Step 2: Rename the GitHub repository

An organization owner renames the private repository to
`crabbuild/crab-internal`. Confirm visibility did not change. Update the origin
URL in maintained clones and automation; do not rely permanently on GitHub's
redirect. Inventory developer clones, deployment agents, GitHub Apps, reusable
workflows/Actions, package metadata, webhooks, and scheduled jobs. Any
`uses: crabbuild/crab/...` reference must be changed immediately because Action
references do not follow the rename redirect.

**Verify**: the GitHub API reports `PRIVATE`; old and new ordinary Git URLs
resolve to the same HEAD during the temporary transition; canonical docs and
all maintained automated writers use only the new URL. Record that the old URL
will become the new public repository in Phase 005.

### Step 3: Re-establish protections and permissions

Require review, passing private CI, no force pushes, and no branch deletion on
the default/release branches. Restrict production environments and repository
secrets to the minimum service teams. Verify deploy keys and GitHub Apps still
point to the renamed repository.

**Verify**: export branch/environment settings to the private operations record
and have a second owner review them.

### Step 4: Mark private packages and repository intent

Set `publish = false` in both private server manifests and any other private
Cargo package not intended for a registry. Add `PRIVATE.md` stating that no file
may be copied to public Core except through the Phase 005 exporter and review.

**Verify**: a Cargo metadata query reports an empty publish allowlist for every
`private-platform` and `private-product` package.

### Step 5: Repair internal links and automation

Search tracked files and GitHub configuration for `crabbuild/crab`. Update only
references that mean the private monorepo; leave intended future public Core
URLs for Phase 005. Classify each changed workflow as private service/product
CI or future public Core CI. Search external automation inventories as well as
tracked files; an untracked deployment remote can still push to the wrong
repository once the old name is reused.

**Verify**: `rg -n 'github\.com/crabbuild/crab([/.]|$)'` has no ambiguous
private clone/reference; every remaining match is annotated in the migration
record as future public Core. A second search for
`uses:\s*crabbuild/crab/` has no private Action consumer.

### Step 6: Unfreeze after baseline proof

Run Phase 002/003 gates from a fresh private clone using the new origin. Resume
merges only after the default branch and release automation are green.

**Verify**: fresh clone HEAD matches the pre-rename HEAD and architecture tests
exit 0.

## Test plan

- Verify clone over SSH and HTTPS with a least-privilege developer account.
- Verify a protected-branch test PR cannot merge without review/checks.
- Verify one non-production workflow can read its required secret after rename;
  do not print the secret.
- Verify production environment approval still blocks unauthorized execution.
- Run Cargo metadata and architecture gates from the renamed fresh clone.

## Done criteria

- [ ] Full-history bundle backup verifies and is access-controlled.
- [ ] `crabbuild/crab-internal` exists and reports private visibility.
- [ ] The current repository's original commit SHA and all tags/branches are
      preserved.
- [ ] Branch/environment protections and team permissions have second-owner
      review.
- [ ] Private Cargo packages declare `publish = false`.
- [ ] Fresh clone and architecture gates pass from the renamed origin.
- [ ] Every maintained private clone, bot, deploy job, webhook, App, and Action
      reference uses `crabbuild/crab-internal`; the old name is safe to reuse.
- [ ] No public repository has been created or made visible by this phase.
- [ ] Phase 004 status is `DONE`.

## STOP conditions

Stop and report if:

- No organization owner or maintenance window is available.
- The backup cannot be created and verified on `/Volumes/Workspace`.
- Rename changes visibility, loses a branch/tag, or breaks an integration that
  cannot be safely restored.
- Automated writers or `uses:` consumers of the old repository name cannot be
  completely inventoried and migrated.
- Any credential appears in logs while verifying automation; revoke/rotate it
  and begin incident handling.
- Phase 002 or 003 proof is not green at the renamed HEAD.

## Handoff artifact

Provide the new canonical private URL, HEAD SHA, verified backup identifier,
and an access-controlled export of repository protections. Phase 005 may then
create the new `crabbuild/crab` candidate without naming conflict.

## Maintenance notes

Keep the full private history indefinitely unless legal retention policy says
otherwise. Do not use GitHub's old-name redirect as a permanent dependency.
Service versions may diverge from public Core after Phase 009, but protocol
compatibility must remain explicit.
