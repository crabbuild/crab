# Phase 010: Qualify the final release and make Crab Core public

> **Executor instructions**: This is the only phase authorized to change
> `crabbuild/crab` visibility. Treat visibility as irreversible disclosure. Use
> the exact SHAs approved by Phases 007–009; any code/tree change reopens those
> gates. Require two human launch approvers and an organization owner. Update
> Phase 010 in `plans/README.md` only after post-public checks pass.
>
> **Drift check (run first)**:
> `git diff --stat <qualified-core-rc-sha>..HEAD` in public candidate and
> `git diff --stat <qualified-private-sha>..HEAD` in `crab-internal`.
> Both must have no unqualified change. Compare tree hashes, not only commit
> labels.

## Status

- **Priority**: P0
- **Effort**: L (two-day freeze/qualification plus launch window)
- **Risk**: CRITICAL — public disclosure cannot be reliably undone.
- **Depends on**: Phases 007, 008, and 009
- **Category**: migration / release
- **Planned at**: private source commit `98b5c1e8`, 2026-08-14; qualified SHAs
  are supplied by prior handoffs

## Why this matters

The launch must prove that the exact source becoming public is secure,
buildable, documented, and already consumed by closed-source products. A
checklist bound to immutable commits prevents a last-minute documentation,
workflow, or release edit from bypassing review.

## Preconditions

All must be true before the maintenance window begins:

- Phase 001 boundary/legal decisions are approved.
- Private historical repository is `crabbuild/crab-internal`, private, backed
  up, and protected.
- Public candidate `crabbuild/crab` is private and its RC tree is identical to
  security/docs review.
- Public CI and release dry run are green.
- Advisories/licenses/secrets/provenance/governance are approved.
- Private auth/cache servers, desktop, and docs consume the exact RC SHA.
- Public source contains no private implementation/current-tree path.
- The candidate has no unreviewed private forks. GitHub may detach private forks
  when visibility changes; each existing fork owner and retained private copy
  has an approved disposition.
- Every Actions run log and artifact in the private candidate has been reviewed
  as future-public content. GitHub makes Actions history/logs visible when a
  private repository becomes public.
- Release version is selected. At the planned baseline this is expected to be
  `1.0.14`, but use a different version if intervening releases require it.
- Maintainer signing keys, GitHub release permissions, Homebrew tap access, and
  security incident contacts are tested.

## Commands and checks

| Purpose | Command/check | Expected on success |
|---|---|---|
| Core identity | `git rev-parse HEAD && git rev-parse HEAD^{tree}` | exactly qualified SHA/tree |
| Full public gate | documented public `make check` plus required CI | all required checks green |
| Release dry run | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-public make release-dry` or current public equivalent | artifacts only; no publish |
| Archive check | `cd crab && python3 scripts/check-release-archive-contents.py` | exit 0 |
| Secret/license/advisory | Phase 007 pinned commands | all exit 0 on launch SHA/artifacts |
| Private compatibility | Phase 009 compatibility workflow at Core SHA | all required private checks green |
| Visibility | GitHub organization-owner action | `PUBLIC` only after approval checklist |
| Public clone | unauthenticated fresh clone over HTTPS | succeeds; full public history contains only curated commits |

If the `crab-release-publish` skill is available to the executor, use it for the
final GitHub release/Homebrew publication after visibility changes; do not use
it before all gates below are signed.

## Scope

**In scope**:

- Final freeze, exact-SHA revalidation, signed final tag, visibility change,
  GitHub release, checksums/SBOM/attestations, and Homebrew update.
- Public repository About/topics/settings and organization profile links.
- Public issue/discussion/security-report paths.
- Launch announcement linking only approved public docs.
- Immediate post-launch smoke/monitoring and incident decision.

**Out of scope**:

- Source feature/fix changes during the launch window.
- crates.io or PyPI publication unless separately approved.
- Publishing private server/service/product repositories.
- Pricing/managed-service launch.
- History rewriting after public visibility.

## Steps

### Step 1: Freeze both repositories and assemble the launch record

Block merges/releases in public candidate and private internal repositories.
Create an access-controlled launch record containing exact Core/private SHAs,
tree hashes, RC tag, CI run URLs, scanner/report hashes, artifact checksums,
approvers, and rollback contacts. Do not include credentials or private scan
findings.

**Verify**: two approvers independently compare every recorded hash with live
Git/GitHub state and sign the `GO/NO-GO` record.

### Step 2: Re-run gates on the exact final tree

From a fresh clone, run public local/CI/release dry-run, boundary, archive,
advisory/license, secret, documentation link/help, and artifact provenance
checks. Rerun private compatibility against the same Core SHA.

**Verify**: every required check is green and every release artifact hash equals
the reviewed dry-run result where deterministic; explain platform-signing
differences explicitly.

### Step 3: Create and verify the final signed tag

Create an annotated signed `v<launch-version>` tag at the qualified RC commit
(no code changes). Verify version metadata, tag signature, release notes, and
changelog. Release notes state Core scope and explicitly exclude hosted
auth/cache/managed service source.

**Verify**: local and remote tag resolve to qualified SHA; signature policy
passes; no other release tag points at an unreviewed tree.

### Step 4: Perform the irreversible visibility gate

Immediately before changing visibility, query and record `PRIVATE`, repeat the
secret-scan summary/hash comparison, and obtain both approvers' final `GO`. An
organization owner changes `crabbuild/crab` to public. Do not change source or
settings simultaneously. Follow GitHub's official
[visibility-change caveats](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/managing-repository-settings/setting-repository-visibility):
private workflow history/logs become public, private forks may be detached, and
push rulesets are disabled by the transition. Review or remove sensitive run
artifacts before the change under an approved retention decision; immediately
reapply the approved public push rulesets afterward.

**Verify**: unauthenticated browser/API/HTTPS clone works; private repository
remains private; public history and paths match the approved manifest; prior
Actions pages expose only reviewed-safe logs/artifacts; public branch/tag push
rulesets and required checks are active again.

### Step 5: Publish release artifacts and distribution update

Publish the GitHub release from the signed tag with platform archives,
checksums, SBOMs, and attestations. Update the Homebrew tap/formula using the
repository's release procedure. Do not publish unapproved registries.

**Verify**: download every public artifact anonymously, validate checksum,
SBOM/attestation, exact archive content, executable version, and Homebrew install.

### Step 6: Verify public contribution and reporting surfaces

Open/preview an issue form and PR from a non-member/fork context; verify CI runs
without secrets and the security-report link remains private. Verify README,
docs, license, code of conduct, contributing, and repository metadata render.

**Verify**: unauthenticated links work; fork PR gets expected checks and cannot
access protected secrets.

### Step 7: Run immediate user-level smoke and monitor

Install through a public distribution path. Against a non-production test
bucket/backend, execute init/add/push/clone or the approved Crab CLI E2E path and
verify byte-identical visible output. Exercise optional managed auth/cache only
through private compatibility monitoring, not as a public-Core requirement.

Monitor release downloads, issue/security channels, CI, package/formula status,
and private service compatibility for at least the agreed launch window.

**Verify**: Level 3 Core E2E passes from public artifacts; no P0/P1 incident or
private leakage report is open.

## Test plan

- Fresh-clone full public gates on launch SHA.
- Dry-run and final archive content/checksum/SBOM/attestation verification on all
  targets.
- Independent secret scan immediately before visibility change.
- Private server/product compatibility on exact Core SHA.
- Anonymous clone/download/install and fork-PR permissions.
- User-level push/clone byte-identity smoke from public artifact.

## Done criteria

- [ ] Two approvers sign exact SHA/tree/scanner/artifact `GO` record.
- [ ] Final signed tag resolves to qualified Core SHA.
- [ ] `crabbuild/crab` is public; `crabbuild/crab-internal` remains private.
- [ ] Candidate Actions history/artifacts and fork consequences were reviewed;
      no private content became visible.
- [ ] Public branch/tag push rulesets were reapplied and tested after the
      visibility transition.
- [ ] Anonymous source clone, artifact download, checksum/provenance verification,
      and Homebrew install succeed.
- [ ] Public release contains no private binary/path and reports correct version.
- [ ] Fork contribution workflow and private security reporting work.
- [ ] Public-artifact Level 3 CLI smoke passes with byte-identical reconstruction.
- [ ] No unresolved launch-blocking security/license/provenance/compatibility
      issue exists.
- [ ] Phase 010 status is `DONE`.

## STOP / NO-GO conditions

Do not change visibility if:

- any SHA/tree/artifact differs from reviewed evidence;
- any required check is skipped, pending, flaky, or red;
- a secret/license/provenance question is open;
- private consumers do not pass against the exact Core SHA;
- public candidate contains a private path or history object;
- any candidate Actions log/artifact or private fork is unreviewed;
- fewer than two approvers or no organization owner is available.

After visibility is public, if a critical leak/vulnerability is found: assume
the content has been copied; revoke/rotate affected credentials, stop release
distribution, publish appropriate security guidance, preserve incident evidence,
and follow counsel/security direction. Making the repo private again is not a
remediation by itself.

## Handoff artifact

Provide the public URL/tag/release, launch SHA/tree, artifact/checksum/SBOM/
attestation links, Homebrew formula commit, public E2E transcript, launch record
identifier, and any non-blocking follow-ups for Phase 011.

## Maintenance notes

Never force-push public release tags or rewrite public history to conceal an
incident. Use transparent fixes/advisories. The first public release establishes
the supported API/storage/protocol baseline documented in Phase 008.
