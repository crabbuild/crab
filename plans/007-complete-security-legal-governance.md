# Phase 007: Complete public security, legal, provenance, and governance gates

> **Executor instructions**: Work against the exact private candidate SHA and
> dry-run artifacts from Phase 006. Do not suppress an advisory or allow a
> license without written rationale and named approval. Never copy a credential
> value into output. Update Phase 007 in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat <phase-006-candidate-sha>..HEAD -- Cargo.toml Cargo.lock crab/Cargo.toml crates crab-sdk crab-py .github LICENSE NOTICE SECURITY.md CONTRIBUTING.md CODE_OF_CONDUCT.md deny.toml`
> If dependencies, source files, or artifacts changed, repeat all scans and
> legal review on the new SHA.

## Status

- **Priority**: P0
- **Effort**: L (three to seven days; external legal/security review may extend)
- **Risk**: HIGH — failures can create security exposure or license obligations.
- **Depends on**: Phases 005 and 006
- **Category**: security / dependencies / docs
- **Planned at**: private source commit `98b5c1e8`, 2026-08-14; candidate SHA is
  supplied by Phase 006

## Why this matters

Open source makes every committed file and dependency choice externally
scrutinizable. The current dependency audit is not green, public governance
files are missing, and contributor provenance has not yet been recorded. This
phase creates objective launch gates and an actionable security-reporting path.

## Current state

- Root source is Apache-2.0 (`Cargo.toml:34`, `LICENSE:1`).
- No root `SECURITY.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `CODEOWNERS`,
  `deny.toml`, issue/PR templates, or dependency-update configuration was found
  at planned commit.
- A read-only `cargo deny --manifest-path crab/Cargo.toml --exclude-dev --locked check advisories`
  run at commit `98b5c1e8` failed on known vulnerable/unsound versions including
  `crossbeam-epoch 0.9.18`, `lru 0.12.5`, older `quick-xml` lines, and
  `rustls-webpki 0.101.7`; it also reported unmaintained/yanked transitive
  packages. Re-run against the candidate rather than assuming this list is
  complete.
- A license inventory showed mostly permissive licenses plus MPL-2.0 and a
  target-specific LGPL-2.1-or-later package. This is a review requirement, not
  a claim of incompatibility.
- The private history has at least two Git authors and historical generated
  environments/artifacts. Phase 005 avoids publishing that history, but rights
  to the snapshot still require confirmation.
- No complete secret scan has been accepted yet. Test/example credential-shaped
  strings are not automatically real secrets; scanners need triage without
  copying values.

## Commands you will need

Use pinned versions of scanners in CI and record them in the review report.

| Purpose | Command | Expected on success |
|---|---|---|
| Advisories | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo deny check advisories` | exit 0; no unacknowledged advisory |
| Licenses/bans/sources | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo deny check licenses bans sources` | exit 0 under approved `deny.toml` |
| Lock correctness | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public cargo test --workspace --locked` | exit 0 |
| Secret scan | pinned Gitleaks scan of candidate working tree and one-commit history | exit 0 after reviewed allowlist |
| Second secret scan | pinned TruffleHog filesystem/Git scan with verified-only policy | zero verified live credentials |
| Artifact inventory | generate CycloneDX/SPDX SBOM for each release archive | SBOM validates and matches candidate lockfile |
| Governance | GitHub API query for visibility/protection/security settings | candidate still PRIVATE; configured controls present |

## Scope

**In scope**:

- Dependency upgrades/removals necessary to clear applicable advisories.
- `deny.toml` with reviewed license, duplicate, source, and advisory policy.
- `SECURITY.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `NOTICE`,
  `.github/CODEOWNERS`, issue forms, PR template, and dependency-update config.
- Secret scanning of the public candidate and its artifacts.
- Contributor provenance and trademark decision artifacts from Phase 001.
- SBOM, checksums, artifact signing/attestation, branch protection, dependency
  review, CodeQL/security scanning, and least-privilege workflow permissions.
- Rotation/revocation of any real credential discovered, handled outside Git and
  referenced only by incident ID/location.

**Out of scope**:

- Publishing or changing visibility.
- Scanning customer/private data into public reports.
- Broad dependency modernization unrelated to an advisory/license/source gate.
- Rewriting the private repository's history.
- Creating a CLA/DCO requirement unless the governance owner chooses it.
- Managed service security policy; `SECURITY.md` may direct those reports to a
  private channel but must clearly scope the public project.

## Steps

### Step 1: Create a candidate-specific review record

Create `docs/open-source-review/RELEASE-READINESS.md` in the private candidate
while it remains private. Record candidate SHA/tree, scanner versions, commands,
result summaries, reviewer names, and links to access-controlled incident/legal
evidence. Never store secret values or private correspondence.

**Verify**: record SHA equals `git rev-parse HEAD`; no absolute local paths or
credential-shaped values are present.

### Step 2: Remediate dependency advisories

Run Cargo deny/audit on the whole public workspace and release feature graph.
For each applicable advisory, identify direct owner and shortest safe upgrade or
removal. Read upstream changelog/source before changing API/defaults. Review
`Cargo.lock` as a security surface. Re-run targeted tests plus workspace tests.

Unmaintained/yanked dependencies require a documented disposition: remove/
upgrade, accept temporarily with owner/removal date, or prove unreachable from
supported build targets. No open-ended ignore.

**Verify**: advisories command exits 0; temporary exceptions contain advisory
ID, rationale, owner, expiry date, and tracking issue.

### Step 3: Approve license and source policy

Generate the full dependency/license graph for all supported targets/features.
Have counsel or the designated license owner decide MPL/LGPL and any other
non-permissive/unknown case. Encode only approved licenses and Git sources in
`deny.toml`. Generate third-party attribution if obligations require it.

**Verify**: `cargo deny check licenses bans sources` exits 0; each non-obvious
allow entry links to an approval; unknown licenses are zero.

### Step 4: Scan source, history, and artifacts for secrets

Run two independent pinned scanners over: candidate working tree, candidate
one-commit history, dry-run release archives after extraction, and generated
SBOM metadata. Triage test/example matches by location and rule. Put narrow
false-positive allow rules in a reviewed config; never allow an entire
directory or generic token pattern.

If a real credential is found, stop publication, revoke/rotate it, search the
private origin for reuse, and rerun all scans. Removing text alone is not
sufficient.

**Verify**: zero verified live credentials; every ignored match has a specific
file/rule/rationale and reviewer.

### Step 5: Confirm contributor and trademark provenance

Use Phase 001 approvals to create `NOTICE` and, if required, `TRADEMARKS.md`.
Credit contributors without publishing private email addresses unless they
consented. Record signed/traceable publication approval for all material code
owners in an access-controlled location.

**Verify**: legal/provenance checklist has no pending author, third-party code,
asset, logo, or dataset item.

### Step 6: Add public governance and reporting paths

Create concise files at standard locations:

- `SECURITY.md`: supported versions, private report channel, response targets,
  scope distinction between public Core and managed services;
- `CONTRIBUTING.md`: setup, tests, architecture boundaries, issue/PR process,
  certificate/DCO/CLA rule if approved;
- `CODE_OF_CONDUCT.md`: adopted standard and enforcement contact;
- `CODEOWNERS`: least-broad ownership for security, release, storage formats,
  auth/cache clients, and workflow files;
- issue forms and PR template with reproduction/tests/security and public/private
  scope checks.

**Verify**: all links and contacts work from an unauthenticated/public context
without exposing internal addresses or issue trackers.

### Step 7: Harden GitHub and release provenance

Enable required reviews/checks, signed-tag policy where supported, dependency
review, secret scanning/push protection, CodeQL (or appropriate Rust scanners),
Dependabot/Renovate, least-privilege workflow permissions, release checksums,
SBOMs, and artifact attestations. Pin third-party Actions to immutable commits
or an approved policy. Follow GitHub's official
[artifact attestation guidance](https://docs.github.com/en/actions/concepts/security/artifact-attestations):
generating an attestation is not sufficient, so test consumer-side verification
and bind the result to repository, workflow, commit, and artifact digest.

**Verify**: GitHub API/settings export and a test PR prove controls; dry-run
release produces valid checksums, SBOM, and attestation for every artifact.

## Test plan

- Run advisory/license/source checks across default and release features.
- Run targeted tests after each dependency group, then workspace tests.
- Run two secret scanners on source, Git objects, extracted archives, and SBOMs.
- Test security contact and private-report route with a harmless message.
- Test branch protection using a non-admin PR.
- Verify SBOM package versions against `Cargo.lock` and archive hashes against
  published checksum files.

## Done criteria

- [ ] No unacknowledged applicable vulnerability, yanked package, unknown
      license, or unapproved source remains.
- [ ] Any temporary advisory exception has owner, issue, rationale, and expiry.
- [ ] Two secret scanners report zero verified live credential in candidate or
      release artifacts.
- [ ] Contributor provenance and trademark/license approvals have no pending
      item.
- [ ] Public governance files exist and their links/contacts work.
- [ ] Branch, workflow, dependency, code scanning, and release provenance
      protections are tested.
- [ ] Workspace tests pass with the reviewed lockfile.
- [ ] Candidate remains private.
- [ ] Phase 007 status is `DONE`.

## STOP conditions

Stop and report if:

- A live credential or private customer/tenant data is found.
- An advisory cannot be remediated or time-bounded before launch.
- A dependency license/source is rejected or ownership is unknown.
- Any contributor disputes publication rights.
- Security reporting cannot be made private and reachable.
- Source/dependencies change after scans without rerunning the complete gate.

## Handoff artifact

Provide the candidate SHA, redacted readiness report, advisory/license reports,
secret-scan summaries, provenance approvals, governance file list, GitHub
settings export, and dry-run SBOM/checksum/attestation verification. Phase 010
must verify the launch SHA is identical or repeat this phase.

## Maintenance notes

Schedule advisory, license, secret, and dependency-review checks on every PR and
regularly on the default branch. Security exceptions are expiring debt, not a
permanent allowlist. Re-run provenance review for any code imported from the
private repository after launch.
