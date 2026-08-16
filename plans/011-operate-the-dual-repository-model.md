# Phase 011: Operate Crab Core and private products as one compatible system

> **Executor instructions**: Begin after the public launch is stable. Add
> recurring controls and ownership; do not use scheduled workflows as a reason
> to weaken per-PR gates. Test alert delivery without exposing private results.
> Update Phase 011 in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> In public Core run
> `git diff --stat <launch-sha>..HEAD -- .github docs Cargo.toml Cargo.lock`; in
> private internal run
> `git diff --stat <private-launch-sha>..HEAD -- .github docs/open-source Cargo.toml Cargo.lock crab-web crab-desktop crates`.
> Refresh owners/commands if either operating surface changed.

## Status

- **Priority**: P1
- **Effort**: M (two to three days setup, then recurring ownership)
- **Risk**: MED — process drift can fork protocols or leave public issues and
  vulnerabilities unattended.
- **Depends on**: Phase 010
- **Category**: dx / security / migration
- **Planned at**: private source commit `98b5c1e8`, 2026-08-14; launch SHAs are
  supplied by Phase 010

## Why this matters

The repository split is successful only if public contributions flow into
products, security issues receive timely handling, and private servers stay
compatible without copying Core source. This phase turns the launch checklist
into durable release, dependency, documentation, and triage practices.

## Current operating model

- Public Core owns CLI, remote helper, shared storage/data/workflow mechanics,
  auth/cache clients and DTOs, SDK, Python bindings, Core docs, CI, and releases.
- Private internal owns auth/cache server implementations, deployments, managed
  operations, desktop/web source, private docs/evidence, and release
  qualification against an immutable public Core SHA.
- Shared wire/storage/serialized-format changes originate publicly. Private
  implementations consume them downward.
- Private releases pin a full public Core SHA associated with a reviewed tag;
  public `main` is never a production dependency.
- Public docs are canonical and private web sync is generated/tag-pinned.

## Commands/checks to automate

| Cadence | Check | Expected result |
|---|---|---|
| Every public PR | format, tests, clippy, architecture/boundary, dependency review, secret scan | required green |
| Nightly public | full feature/target matrix and advisory/license scan | green or owner-alerted issue |
| Every private Core bump | exact-revision checker plus auth/cache/desktop/docs compatibility | green before merge |
| Nightly private | compatibility against latest public release and separately latest public default branch | release green; default-branch failures file early warning only |
| Weekly | public issue/PR/security/dependency triage | every item owned/labeled |
| Every release | signed tag, archives, checksums, SBOM, attestations, public E2E, private compatibility | all recorded |
| Monthly | docs link/help drift and public→private path scan | zero drift/leak |

## Scope

**In scope**:

- Public `GOVERNANCE.md`, maintainer/release/backport/security procedures, and
  support/version policy.
- Private `docs/open-source/DUAL-REPO-WORKFLOW.md` and Core bump automation.
- Scheduled public health and private compatibility workflows.
- Issue/PR triage labels, ownership, response targets, release cadence, and
  dependency update process.
- 30/60/90-day review and measurable launch follow-ups.

**Out of scope**:

- New paid features or service architecture.
- Splitting more repositories without an ownership/cadence case.
- Promising support SLAs not approved by maintainers/business.
- Automatically merging dependency or public-contribution changes.
- Giving public CI access to private repositories or secrets.

## Steps

### Step 1: Publish governance and compatibility ownership

Add public `GOVERNANCE.md` naming maintainer roles, decision process, release
authority, security ownership, contributor path, and succession expectations.
Add a compatibility-owner matrix for CLI/storage formats/auth/cache protocols,
SDK, Python, release, and docs. Keep personal/private contact data out of the
repo; use durable team aliases.

**Verify**: every CODEOWNERS-sensitive surface has at least two maintainers or a
documented single-owner risk/backup plan.

### Step 2: Define versioning, deprecation, and release policy

Document public SemVer scope, support window, deprecation process, storage/
serialized-format compatibility, wire protocol versioning, prereleases,
backports, and security releases. Private server versions may be independent,
but their compatibility matrix must name supported Core ranges.

**Verify**: release checklist rejects an undocumented breaking public contract
and requires migration notes/tests for approved breaking changes.

### Step 3: Automate public health without private access

Schedule feature/OS matrices, advisories/licenses, docs drift, stale dependency
exceptions, and release dry runs using public resources only. Alerts create a
public issue when safe or notify maintainers privately for security-sensitive
results. Workflow permissions remain least privilege.

**Verify**: manually dispatch each schedule; success records expected summary,
and a harmless failure reaches the correct owner without leaking secrets.

### Step 4: Automate private compatibility and Core revision bumps

Run two private lanes:

- **release lane**: required; tests private products against the pinned/latest
  supported public release;
- **early-warning lane**: non-release-blocking; tests against public default
  branch to catch upcoming breakage.

Provide a bump script/PR template that changes the one Core SHA, regenerates the
lockfile/docs provenance, runs all compatibility gates, and records public
release notes. It must not auto-merge.

**Verify**: simulate a good bump and an intentionally incompatible fixture; the
first passes, the second blocks release lane with an owned diagnostic.

### Step 5: Establish contribution and triage operations

Define labels for bug/security/docs/good-first-issue/needs-reproduction and
public-Core-versus-managed-service scope. Route managed-service reports to the
private support/security path without closing useful Core issues. Set realistic
review/response targets and a weekly rotation.

**Verify**: triage sample issues for Core bug, private service bug, security
report, and unsupported question; each reaches the right public/private channel
without disclosure.

### Step 6: Measure the first 90 days

At 30, 60, and 90 days review:

- clone/install/release success and artifact failures;
- public CI duration/flakiness;
- open issue/PR age and contributor conversion;
- security/dependency exception age;
- private compatibility failures and time-to-pin;
- docs search/link/help drift;
- whether repository split boundaries caused repeated cross-repo friction.

Create issues only for evidence-backed problems. At day 90 decide whether
crates.io/PyPI publication or further private repository splits have earned a
separate proposal.

**Verify**: each review has dated owner, metrics source, decisions, and linked
follow-ups; no private customer/usage data is published.

## Test plan

- Manual dispatch success/failure of every scheduled workflow.
- Good and bad Core revision bump fixtures.
- Release checklist rehearsal without publication.
- Four sample issue-routing cases.
- Monthly public/private path and docs drift scans.
- Restore drill using previous pinned Core SHA and release artifacts.

## Done criteria

- [ ] Public governance, support, versioning, deprecation, security, backport,
      and release ownership is documented.
- [ ] Every critical surface has owner and backup/escalation path.
- [ ] Public scheduled checks require no private access and alert correctly.
- [ ] Private release and early-warning compatibility lanes are tested.
- [ ] One-script/one-PR Core bump changes exactly one immutable revision and
      runs required compatibility/docs checks.
- [ ] Triage rotation and public/private routing examples are verified.
- [ ] 30/60/90-day reviews are scheduled with owners and safe metrics.
- [ ] Recovery to the previous Core pin is rehearsed.
- [ ] Phase 011 status is `DONE`.

## STOP conditions

Stop and report if:

- Public workflows need private repo access or secrets.
- Private compatibility output would expose proprietary code, endpoints,
  customers, or credentials publicly.
- Automation would auto-merge Core bumps or dependency changes.
- No owner/backup exists for security or release authority.
- A support/version promise lacks staffing or business approval.

## Handoff artifact

The ongoing maintainers receive public governance/release docs, private
dual-repo procedure, schedule/workflow run URLs, compatibility bump tool, triage
rotation, recovery proof, and 30/60/90 review calendar. There is no further
migration phase; unresolved evidence-backed improvements become normal issues
or new plans.

## Maintenance notes

Revisit the repository boundary when repeated changes cross it, not merely
because another split seems tidy. The best evidence for a future
`crab-platform` or crates.io publication is sustained independent ownership,
release cadence, and external demand.
