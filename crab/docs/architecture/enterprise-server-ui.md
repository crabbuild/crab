# Server and UI enterprise contract

Assessment date: 2026-11-07. Scope: the self-hosted `crab-server` product and its
embedded `packages/ui` application. This is a coverage and delivery contract,
not a declaration of complete GitHub Enterprise parity or production readiness.

## One product, two build units

The Rust server embeds the Vite output and owns HTTP, authorization, native
commands, durable data, Git publication, and operator management. The React UI
owns presentation, drafts, pending submissions, and user interaction. The
existing container already ships both as one executable/image; moving source
directories would not consolidate their security or data contracts.

Keep one owner per invariant. The UI consumes authoritative permissions,
versions and results; a disabled control is not an authorization check. Git,
LFS, browser mutations and background work retain their existing late grant
checks and stable repository UUID constraints. Cellar owns runtime execution,
leases, replay, output durability and recovery, not enterprise product policy.

## Common data models

The canonical types remain beside their Rust owner. Test-only `ts-rs` metadata
generates `packages/ui/src/lib/contracts.ts`; its consumers import these records
directly. `api/contracts.rs` is the explicit public-wire export list and
freshness check. It does not export arbitrary persistence records.

Additional Rust declarations make previously untyped HTTP envelopes and the
self-session identity projection explicit. Reusing domain records and generating
the client declarations replaces parallel hand-maintained models; the extra
generation/check code is development-only and adds no production dependency.

| Model family | Rust owner under `crates/crab-server/src` | Contract and boundary |
| --- | --- | --- |
| Session and self identity | `auth/http/mod.rs` | Explicit authenticated/mode/user/CSRF response; self identity projects only issuer, subject and name. Private groups, tokens and session storage fields stay out. |
| Public actors | `cells/directory/` | Stable actor UUID, chosen profile, version/timestamps and bounded public summary. A profile or handle grants no access. |
| Repository access | `config/mod.rs`, `catalog/mod.rs`, `api/members/` | Distinct read/triage/write/admin roles, individual and exact signed-group grants, complete replacement at an expected catalog revision. |
| Branch protection | `config/mod.rs` | Exact branch, required approvals and checks. Git and merge owners enforce the policy. |
| Organizations | `cells/organization/`, `api/organizations/` | Versioned organization, member/owner roles, enriched memberships and revision-bound pages. Explicit private bootstrap creates the first owner. |
| Invitations | `cells/organization/invitations.rs` | Pending/accepted/declined/cancelled/expired states, expiry, consent and captured membership version. The generated type includes `member_version`, which the old UI declaration omitted. |
| Teams | `cells/organization/teams/`, `api/organizations/teams/` | Closed/secret visibility, member/maintainer roles, policy, bounded listings/rosters and commands. Current organization membership incarnation governs eligibility. |
| Team submissions | The same team owners | Request identity, tagged command and applied/cancelled acknowledgement; uncertain writes retain their original account, target and payload. |

UUIDs serialize as strings. JSON integers use TypeScript `number`; native model
owners retain their safe-integer checks. Role families remain separate:
organization `owner`, team `maintainer`, and repository `admin` have different
authority. A role's string representation does not create inheritance.

`serde(deny_unknown_fields)` remains enforced by Rust. `ts-rs` warns that it
does not represent that input-validation rule; generated structural types are
not runtime validators. Public response structs also prevent an internal field
from silently entering a JSON envelope when a private record grows.

The initial export covers the common identity/access/organization/team surface.
Browse, issue/PR, milestone and planning records still have feature-local wire
declarations; converting those requires their real HTTP serializers and tests,
not speculative future schemas or TypeScript re-export shims.

## Enterprise coverage

GitHub's enterprise model includes users, organizations, teams, repositories,
apps and administration above an organization. Use that as the comparison
baseline, not as evidence that Crab implements an enterprise account.
See [GitHub's enterprise account model](https://docs.github.com/en/enterprise-server@3.21/admin/concepts/enterprise-fundamentals/enterprise-accounts).

“Implemented” below means an existing path and scoped tests or deployment
assets. It does not waive the production qualification gates in `CELLAR.md`.

| Capability | Current server/UI or infrastructure evidence | Remaining work before an enterprise support claim |
| --- | --- | --- |
| Unified delivery | `build.rs`, `assets.rs`, UI build, `deploy/Dockerfile` | Keep source/image/UI/SDK evidence bound together; native server release publication is still gated. |
| Enterprise/customer boundary | One configured storage root, catalog, Directory and independent Organization Cells | No enterprise account, enterprise owner/admin role, customer lifecycle, billing/license model or enterprise policy owner. A storage prefix is not that model. |
| Sign-in and sessions | `auth/http/`, `auth/oidc/`, `auth/backchannel_logout/`, browser session UI | OIDC and GitHub OAuth exist. Direct SAML, SCIM provisioning and a managed-user lifecycle are not implemented by these paths. |
| Public identity | Directory account/profile separation; profile UI and qualified account lookup | Organization discovery, owner-name registration/routing and a unified enterprise directory are incomplete. |
| Groups and provisioning | Signed, bounded OIDC groups can grant repository roles; group sessions have a bounded lifetime | This is not SCIM or team synchronization. Provisioning/deprovisioning needs one durable lifecycle and reconciliation owner. |
| Organization and team authority | Native membership, invitations, standalone teams, consent, incarnation checks; organization/team browser suites | Team-to-repository grants and organization ownership of repository authority need an explicit catalog cutover. No nested or enterprise-level team authority is implied. |
| Repository governance | Stable UUID admission, current catalog grants, archive/protection settings, required review/check merge admission | No complete enterprise ruleset hierarchy, custom-role model, CODEOWNERS enforcement or merge queue is established. |
| Collaboration and planning | Issues, PRs, reviews, milestones, repository/organization projects, search and durable submission recovery | Existing parity plans still contain feature gaps; common contracts for those domains are a later conversion. |
| Audit and investigation | Catalog membership audit outbox/chain, native organization/team activity, durable review/activity events | No unified enterprise audit search/export/stream, comprehensive security-event taxonomy or operator retention policy. Activity is not a complete security audit log. |
| Integrations | GitHub OAuth sign-in, Git import, commit status/check records | No general GitHub Apps installation model, signed webhook delivery/retry owner or hosted Actions runner/runner-group service. CLI workflows and this repository's CI are separate products. |
| Repository security findings | Authenticated object reads, pointer/LFS integrity checks and protected publication | These do not provide SARIF ingestion, code/dependency scanning, security advisories, secret scanning or secret push protection. No corresponding enterprise findings API/UI was found. |
| Notifications and artifact hosting | Native discussion activity and release records | Durable inbox/email delivery and organization package registries are separate missing capabilities; activity records and Git/LFS storage do not implement them. |
| Git and large files | Native HTTP Git and LFS share publication, authorization and integrity owners | A Git SSH hosting service and complete external GitHub API compatibility are not supplied by those HTTP endpoints. |
| Cloud identity and network | S3/GCS/Azure selection; EKS/GKE/AKS workload-identity assets; private mTLS; NetworkPolicy, private probes and secret mounts | Execute protected provider identity/fault qualification. Application claims cannot certify an operator's cloud key, residency or network policy. |
| Availability and operations | Three-replica Helm defaults, handoff/drain, readiness, HPA/PDB and Prometheus assets | Qualify crash/partition, stale-owner fencing, scale-down, zone failure and resource envelopes on each supported deployment. A replica count is not an availability SLO. |
| Backup, restore and upgrades | Complete-root recovery guidance and backup/restore/process qualification tools | Prove source/image/provider-specific restore, key preservation, rollback and measured recovery objectives. Do not promise zero-loss failover from a local smoke pass. |
| Quotas and admission | Transfer admission, bounded requests, staging capacity and native pressure controls | Process admission and disk reservations are not per-customer quotas, billing limits or enterprise API rate policies. |
| Supply chain and release | Locked dependencies, pinned image/chart references, CI image inspection/scanning and release manifests | Publish only after the native runtime/provider release gates pass; no compliance certification follows from these assets. |

GitHub documents IdP-backed membership reconciliation separately from sign-in
and repository grants. Crab's signed group grants therefore must not be labeled
as SCIM/team sync. See [IdP group and team management](https://docs.github.com/en/enterprise-cloud@latest/admin/managing-iam/provisioning-user-accounts-with-scim/managing-team-memberships-with-identity-provider-groups).

GitHub also treats enterprise audit events and operational recovery as distinct
capabilities. Use its [audit overview](https://docs.github.com/en/enterprise-cloud@latest/admin/concepts/security-and-compliance/audit-log-for-an-enterprise)
and [system overview](https://docs.github.com/en/enterprise-server@3.21/admin/overview/system-overview)
to define acceptance cases, not to copy its deployment architecture into Crab.

## Delivery order and acceptance

1. **Contract consolidation:** use actual server records, typed HTTP envelopes,
   generated UI imports and a read-only freshness gate. Preserve role separation,
   private fields, authorization ordering and serialized durable records.
2. **Enterprise and repository authority:** choose a real enterprise/customer
   owner, define organization/repository ownership, then migrate the catalog
   grant authority explicitly. Test removal during reads, Git/LFS publication,
   name reuse, team membership changes and lost replies across replicas.
3. **Identity lifecycle:** specify the required IdPs and provisioning protocol;
   make deprovisioning, session/token revocation and group/team reconciliation
   restartable, idempotent and visible to administrators. No implicit grants on
   sign-in or profile reads.
4. **Audit and integrations:** add a durable event taxonomy, scoped audit API/UI,
   redaction and retention/export behavior; then signed, bounded webhook and app
   delivery with durable retries. Tie events to stable actors/resources.
5. **Operations qualification:** choose supported provider/topology and recovery
   objectives, then run workload-identity, crash/partition, full-root restore,
   rollout/rollback and resource/scale gates against the actual image. Add
   operator UI only for operations that have a real authorized server owner.
6. **Remaining domain contracts and parity:** convert each existing feature's
   serializers and client together; then deliver missing collaboration,
   governance, automation and discovery behavior through those contracts.

Every slice needs a real user action, durable side effect and visible result,
plus unauthorized, stale-version, retry and restart cases. UI mocks and type
generation alone do not qualify an enterprise feature. Broad cloud, fault,
cross-platform and release proof belong in CI or a dedicated test environment.

## Verify this consolidation

Build `packages/ui` before compiling the server. Use a writable target directory
on the mounted workspace volume, unique to the checkout. Run the documented
`contracts:generate` command only when changing the authoritative wire types;
`contracts:check` and ordinary server library tests must pass without writing
tracked files. Architecture CI checks freshness before its live storage proof.

Relevant behavioral suites are `auth::http::tests`, `api::profiles::tests`,
`api::members::tests`, `api::organizations::tests` and
`api::organizations::teams::tests`; these use real native Cells and HTTP/OIDC
fixtures. Browser counterparts are `profile.e2e.ts`, `acl.e2e.ts`,
`organizations.e2e.ts` and `teams.e2e.ts`. The isolated RustFS process smoke
remains the production-binary check for persisted application state and Git.

Local proof for this consolidation: 36 generated declarations, 29 focused
server HTTP tests plus the contract freshness test, 18 browser scenarios and
7 transport tests passed. UI type checking/build/formatting, architecture gates,
server build and scoped strict Clippy passed. The checker accepted CRLF checkout
text, rejected a deliberately changed role declaration, and passed again after
restoring the generated file. The isolated production-binary RustFS smoke passed
profile/organization persistence, mutation replay, Git push and restart recovery,
retaining 120 durable native objects. These are scoped local results; the
enterprise gaps and provider/release gates above remain open.

For deployment ownership, existing operator commands and unresolved release
qualification, read [the runtime contract](../../../crates/crab-server/CELLAR.md),
[deployment profiles](../../../crates/crab-server/deploy/README.md) and
[operations guide](../../../crates/crab-server/deploy/operations.md).
