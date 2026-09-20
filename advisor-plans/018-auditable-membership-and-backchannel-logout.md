# Plan 018: Deliver auditable browser membership administration and OIDC back-channel logout

> **Executor instructions**: Follow this plan in order. Run each verification
> gate before continuing. Preserve unrelated work. If a STOP condition occurs,
> stop and report it instead of adding a fallback or weakening an invariant.
> When complete, update the Plan 018 row in `advisor-plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 0fb67e93990..HEAD -- Cargo.toml Cargo.lock crates/crab-http-server packages/ui advisor-plans`
> If an in-scope file changed, compare the current implementation with the
> evidence below. Stop when route, catalog, session, or authorization ownership
> no longer matches this plan.

## Status

- **Priority**: P1
- **Effort**: XL; deliver as four reviewable commits
- **Risk**: HIGH; changes authorization, a durable catalog format, and session revocation
- **Depends on**: none
- **Category**: security, direction
- **Planned at**: commit `0fb67e93990`, 2026-09-20

## Outcome

After this plan:

- repository administrators can list and atomically replace repository members
  through `GET` and `PUT /api/repos/{owner}/{repo}/members`;
- every accepted replacement retains at least one administrator, uses the
  caller's catalog revision, and records a durable audit event in the same
  catalog commit;
- the React Settings area supports adding, editing, and removing members and
  makes concurrent-edit conflicts explicit without overwriting either edit;
- an OpenID Provider can post a signed Logout Token to
  `/auth/backchannel-logout` and invalidate every indexed browser session and
  derived Git token for the token's exact `(issuer, subject)` identity;
- absent repositories, readable-but-non-admin repositories, and repositories
  hidden from the caller all produce the same membership-endpoint `404`.

This plan deliberately keeps authorization in `Principal` plus the durable
catalog. The UI displays capabilities and server results; it never becomes a
policy authority.

## Current state

### Ownership map

| Surface | Current owner | Evidence and consequence |
| --- | --- | --- |
| Durable membership | `crates/crab-http-server/src/catalog.rs` | `CatalogRecord.members` is part of the global CAS document (`catalog.rs:35-65`). `set_members` performs one conditional replacement and returns conflict instead of rebasing (`catalog.rs:250-287`). Extend this owner; do not add membership to a repository Cell or a second object. |
| Member validation | `crates/crab-http-server/src/config.rs` | `validate_repository` enforces exact unique subjects, case-insensitive unique names, and length/control-character limits (`config.rs:225-244`). Reuse it, then add the “one admin remains” mutation invariant at the catalog boundary. |
| Authorization | `crates/crab-http-server/src/auth.rs` | `Principal::can_admin` derives access from the current `RepositoryConfig` (`auth.rs:142-207`). Membership handlers must authorize against the same catalog snapshot whose revision they mutate, not only the five-second in-memory routing snapshot. |
| HTTP protection | `crates/crab-http-server/src/server.rs` | `boundary_request` authenticates `/api/*`, and requires canonical Origin plus the session CSRF token for unsafe browser requests (`server.rs:1952-2027`). A normal membership `PUT` inherits this. The signed provider callback needs one narrow exemption. |
| Hidden repositories | `crates/crab-http-server/src/app.rs` | `app::repository` folds absent and unreadable repositories into `NotFound` (`app.rs:379-389`). Membership routes need the stricter equivalent `present AND can_admin`, also mapped to `404`. |
| Settings UI | `packages/ui/src/settings.tsx` | Existing General and Branches sections use full-state replacement with `expected_version`, inline errors, and `settings_changed` conflict handling (`settings.tsx:26-75`, `settings.tsx:411-499`). Members should match this interaction pattern. |
| Durable sessions | `crates/crab-http-server/src/auth.rs` | `StoredSession` contains identity, CSRF, expiry, and up to ten Git-token keys (`auth.rs:233-250`). Session objects are addressed only by a bearer hash; there is no identity-to-session reverse index. |
| Revocation | `crates/crab-http-server/src/auth.rs`, `auth/git_tokens.rs` | Logout deletes the parent session (`auth.rs:824-836`). Git-token authentication reloads that parent, so tokens become unusable, while token objects remain for lifecycle collection. Back-channel logout needs indexed multi-session removal and eager token cleanup. |
| Existing proof | `crates/crab-http-server/src/auth_tests.rs`, `auth_tests/git_tokens.rs`, `auth_tests/branches.rs` | The in-process OIDC provider covers signing-key rotation, claims, CSRF, hidden repositories, logout, Git-token scope, and settings conflicts. Extend this fixture rather than introducing mocks that bypass the boundary. |

### Existing contracts to preserve

- Catalog mutations increment the monotonic `CatalogDocument.version`, validate
  the complete next document, and use the object-store ETag. The revision in the
  membership API is this catalog version. This intentionally means an unrelated
  catalog mutation can force a reload; do not add a second per-repository version
  in this change.
- A byte-for-byte identical membership replacement is idempotent: it returns the
  current revision and creates no audit event.
- Membership effects reach all replicas through the existing five-second catalog
  refresh. Requests already holding an `Arc<Repository>` drain against their
  admitted snapshot.
- The server has exactly one configured OIDC issuer. Membership continues to use
  the provider's stable `sub`; mutable email and display names never authorize.
- Browser sessions remain opaque server-side records. No provider token or
  session bearer enters React or browser storage.
- Git-token effective access remains the intersection of token scope, active
  parent session, and current membership.

### Dependency contract

Back-channel logout must follow [OpenID Connect Back-Channel Logout 1.0](https://openid.net/specs/openid-connect-backchannel-1_0.html): form-encoded `logout_token`, signed JWT, validated `alg`, `iss`, `aud`, `iat`, and `exp`, required `jti`, required back-channel `events` member, no `nonce`, and an identity selector. Successful or already-completed logout returns `200`; invalid tokens return `400`.

`openidconnect 4.0.1` does not expose a Back-Channel Logout verifier. Add the
already workspace-pinned `jsonwebtoken = 11.0.0` as a direct
`crab-http-server` dependency. Use `decode_header`, an exact matching JWK, a
`DecodingKey` built from that JWK, and `Validation` with explicit issuer,
audience, expiry, and allowed algorithm. Never decode claims before signature
verification to make a revocation decision. `Cargo.lock` should not change.

The first delivery supports subject-scoped Logout Tokens. Register the client
with `backchannel_logout_session_required=false`. Reject a token that has only
`sid`; do not pretend to support provider-session logout until Crab retains and
indexes the verified ID-token `sid` claim.

## Target contracts

### Membership HTTP API

`GET /api/repos/{owner}/{repo}/members`

```json
{
  "revision": 42,
  "members": [
    {"subject":"alice-id","name":"Alice","access":"admin"}
  ]
}
```

`PUT /api/repos/{owner}/{repo}/members`

```json
{
  "expected_revision": 42,
  "members": [
    {"subject":"alice-id","name":"Alice","access":"admin"},
    {"subject":"bob-id","name":"Bob","access":"write"}
  ]
}
```

Successful `PUT` returns the same shape with revision `43`. Use these fixed
error contracts:

| Condition | Status | Code |
| --- | ---: | --- |
| Missing repository or caller is not a current admin | 404 | `repository_not_found` |
| Stale `expected_revision` or ETag race | 409 | `membership_changed` |
| Empty/duplicate/oversize/control-character member fields | 422 | `invalid_membership` |
| Replacement has no administrator | 422 | `administrator_required` |
| Catalog/audit storage unavailable | 503 | `membership_unavailable` |

Cap the JSON body at 256 KiB. Unknown fields remain rejected. Do not return the
current membership in a conflict response; the client must perform a fresh
authorized `GET`.

### Audit storage

Move the catalog to schema version 3 on the first write while retaining a
read-compatible v2 decoder. Add one bounded outbox slot and one global head:

```text
CatalogDocument
  membership_audit_head: optional object path
  pending_membership_audit: optional MembershipAuditEvent

MembershipAuditEvent
  id
  repository_id
  actor: { issuer, subject }
  previous_members_digest
  new_members_digest
  repository_version
  occurred_at
  previous: optional object path
```

The event and changed membership enter `catalog.json` in the same ETag-guarded
write. This is the commit point. Flush the pending event afterward to the
deterministic immutable path
`.crab/http-server/v1/audit/membership/{repository-version}-{event-id}.json`,
then CAS the catalog from `pending` to `membership_audit_head` without changing
its logical `version`. A crash before flush leaves the complete latest event in
the catalog; startup, catalog refresh, and the next mutation retry the flush.
There is never more than one inline event.

Before a new membership mutation, flush any prior pending event. The mutation
may retry a conflict caused only by that metadata flush, but must return
`Conflict` if the logical catalog version differs from `expected_revision`.
Audit flush changes the ETag but not the logical revision because it changes no
routing or authorization state.

Compute membership digests over canonical JSON after sorting a clone by exact
subject, name, then access. Domain-separate BLAKE3 with
`crab membership digest v1`. Store lowercase hex. Preserve the caller's member
order in the catalog; order is presentation, not authorization semantics.

Creation/adoption with non-empty membership records an initial event from the
empty digest. CLI actions use actor `urn:crab:local` / `operator`. Browser
updates use the authenticated OIDC issuer and subject. Never put CSRF values,
cookies, Git tokens, names, or provider tokens in audit events.

### Durable session index

Use hashed keys so issuer and subject do not appear in object names:

```text
.crab/http-server/v1/auth/identity-sessions/
  {blake3("crab identity sessions v1\0" || issuer || "\0" || subject)}/
  {session-key-hex}.json
```

Each marker contains schema version, exact issuer, exact subject, session key,
and expiry. Create the marker before creating the session record; if session
creation fails, delete the marker. A crash can therefore leave only a harmless
marker pointing to a missing session, never an active unindexed new session.
Session removal deletes the parent session first, then its listed Git-token
objects, index marker, and process-local cached objects. Stale markers are
removed while traversing the identity prefix.

For sessions written by the previous release, add an explicit bounded migration
record with `started_at` and `complete_after = started_at + 8 hours`. During
that window, identity revocation streams the legacy `sessions/` prefix as well
as the new index and self-heals markers before deletion. After the window, no
old session can still be active, and revocation uses only the index. This is a
named migration boundary, not an indefinite fallback. Deployment must not run
old binaries beyond that eight-hour window.

### Back-channel endpoint

Add `POST /auth/backchannel-logout`, a 64 KiB form body limit, and a dedicated
small admission semaphore. It is the only unsafe public route exempted from
browser Origin/CSRF checks; canonical Host validation still applies, and the
signed Logout Token is its authentication.

Validation order:

1. Require exactly one non-empty form `logout_token` and compact signed JWS.
2. Rediscover provider metadata/JWKS through the existing bounded, no-redirect
   HTTP client so signing-key rotation works without restart.
3. Require an exact `kid` match, an asymmetric provider-supported ID-token
   signing algorithm, and `typ` absent or equal to `logout+jwt` /
   `application/logout+jwt`. Reject `none`, embedded `jwk`, or ambiguous keys.
4. Verify signature, exact configured issuer, audience containing the Crab
   client ID, expiration, and issued-at no more than 60 seconds in the future.
5. Require non-empty `jti` and `sub`; require the back-channel logout event
   member whose value is an object; reject any `nonce`; reject sid-only tokens.
6. Create or resume a replay record keyed by a domain-separated hash of
   `(issuer, jti)`. Store token digest, expiry, and pending/completed state. The
   same token may resume an interrupted revocation; the same `jti` with another
   digest is invalid.
7. Revoke every exact `(issuer, subject)` session through the durable index and
   mark the replay record completed. No matching live session is still success.

Never log or persist the raw Logout Token. Logs may contain the hashed replay
key, issuer, subject, count revoked, and fixed validation category.

## Commands

Use a checkout-specific external Cargo target. Stop if `$HOME/Workspace` is
not mounted or the target is not writable.

| Purpose | Command | Expected result |
| --- | --- | --- |
| Frontend install | `npm ci --prefix packages/ui` | exit 0; lockfile unchanged |
| Frontend typecheck | `npm run typecheck --prefix packages/ui` | exit 0, no errors |
| Frontend unit tests | `npm run test --prefix packages/ui` | all pass |
| Membership browser test | `npm run test:browser --prefix packages/ui -- repository.e2e.ts` | settings tests pass in desktop/mobile and axe reports no violations |
| Frontend format | `npm run format:check --prefix packages/ui` | exit 0 |
| Embedded assets | `npm run build --prefix packages/ui` | `packages/ui/dist/index.html` exists |
| Catalog tests | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-6d40-auth-membership" cargo test -p crab-http-server --locked --lib catalog::tests` | all pass |
| Membership HTTP tests | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-6d40-auth-membership" cargo test -p crab-http-server --locked --lib server::auth_tests::members` | all pass |
| Back-channel tests | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-6d40-auth-membership" cargo test -p crab-http-server --locked --lib server::auth_tests::backchannel_logout` | all pass |
| Auth regression | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-6d40-auth-membership" cargo test -p crab-http-server --locked --lib server::auth_tests` | all pass |
| Rust format | `cargo fmt --all -- --check` | exit 0 |
| Rust lint | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-6d40-auth-membership" cargo clippy -p crab-http-server --locked --all-targets -- -D warnings` | exit 0 |

## Scope

**In scope**:

- `Cargo.toml`, `Cargo.lock`, `crates/crab-http-server/Cargo.toml`
- `crates/crab-http-server/src/catalog.rs`
- `crates/crab-http-server/src/members.rs` (new)
- `crates/crab-http-server/src/auth.rs`
- `crates/crab-http-server/src/auth/git_tokens.rs`
- `crates/crab-http-server/src/auth/backchannel_logout.rs` (new)
- `crates/crab-http-server/src/server.rs`
- `crates/crab-http-server/src/main.rs`
- `crates/crab-http-server/src/lib.rs`
- `crates/crab-http-server/src/auth_tests.rs`
- `crates/crab-http-server/src/auth_tests/members.rs` (new)
- `crates/crab-http-server/src/auth_tests/backchannel_logout.rs` (new)
- `packages/ui/src/api.ts`, `packages/ui/src/settings.tsx`, `packages/ui/src/style.css`
- `packages/ui/tests/browser/repository.e2e.ts`
- `crates/crab-http-server/README.md`, `DESIGN.md`, `REFERENCE.md`
- `crates/crab-http-server/deploy/README.md`, `deploy/operations.md`, and
  `deploy/helm/crab-http-server/README.md`
- `advisor-plans/018-auditable-membership-and-backchannel-logout.md` and its index row

**Out of scope**:

- Browser repository creation/adoption, roles beyond read/write/admin, groups,
  organization membership, invitations, email-based lookup, or provider admin APIs.
- A browser audit-history view or audit export API.
- Provider refresh tokens, UserInfo polling, token introspection on every request,
  front-channel logout, or RP-initiated provider logout.
- `sid`-only back-channel logout. It requires retaining a new verified ID-token
  claim and a second reverse index; report provider incompatibility instead of
  matching sid to subject.
- Changing the five-second catalog propagation model or interrupting already
  admitted requests.
- Editing generated assets, `packages/ui/dist`, `node_modules`, snapshots,
  baselines, dependency patches, or vendored code.

## Git workflow

- Branch: `codex/auth-membership-admin`
- Commit 1: `feat(http): add auditable membership administration`
- Commit 2: `feat(ui): add repository member settings`
- Commit 3: `feat(auth): add OIDC back-channel logout`
- Commit 4: `docs(http): document membership and logout operations`
- Run `cargo fmt` before each Rust commit. Review any lockfile change; the
  expected result is no `Cargo.lock` delta because `jsonwebtoken 11.0.0` is
  already pinned in the workspace.
- Do not push or open a PR unless the operator asks.

## Steps

### Step 1: Add the catalog v3 audit and revision contract

In `catalog.rs`:

1. Add the v2-compatible decoder and v3 writer. Reject v1 and unknown future
   schemas. Tests must prove a current v2 document loads unchanged and the first
   mutation writes v3 without changing repository identity or non-membership data.
2. Add `MembershipActor`, `MembershipSnapshot`, and `MembershipAuditEvent` with
   the exact bounded fields above. Validate issuer/subject, digest format,
   timestamp, chain pointer, repository ID, and pending-event version.
3. Add `membership(owner, name)` returning the complete current record members
   plus logical catalog revision from one loaded document.
4. Replace `set_members` internally with
   `replace_members(owner, name, expected_revision, members, actor,
   require_administrator)`. Keep a thin CLI call path that loads once and passes
   that revision, preserving the existing one-decision/one-CAS behavior.
5. Reuse `validate_repository` for field rules and reject a replacement with no
   admin when authentication is configured. Do this before any write.
6. Implement the single-slot audit outbox and immediate/best-effort flush. A
   committed membership update must remain readable and auditable when the
   flush is interrupted. Call the flusher before public serving, from catalog
   refresh, and before later catalog mutation.
7. Audit non-empty membership supplied to create/adopt and all changed CLI
   replacements. Do not audit idempotent repeats.

Do not increment the catalog's logical version when only clearing the audit
outbox. Any retry after an ETag-only race must first prove the expected logical
version and the old membership digest are unchanged.

**Verify**: catalog tests cover v2 read/v3 write, valid replacement, stale
revision, ETag race, no-op, missing repository, duplicate fields, final-admin
removal, deterministic digests, initial membership, audit-chain order, crash
between catalog commit and flush, and idempotent flush. The catalog test command
passes.

### Step 2: Add admin-only membership routes

Create `members.rs` and merge its router before the generic repository action
route.

1. Add `GET` and `PUT` at the exact requested path with the JSON and error
   contracts above.
2. Load the catalog snapshot first, materialize a temporary `RepositoryConfig`
   using the active repository's default branch, and require
   `principal.can_admin` against that same snapshot. Convert missing, unreadable,
   read/write-only, stale-removed-admin, and absent-catalog cases to the same
   `404` response.
3. Convert `Principal::identity()` to `MembershipActor`; reject Git principals
   and anonymous callers even if a future route mistake lets them reach the
   handler.
4. Pass the caller's exact `expected_revision` to the catalog owner. Never
   reload and silently reapply a stale member array.
5. Keep Origin/CSRF in `boundary_request`; add no handler-local alternative.
6. Give sibling CLI `set-members` the same final-admin validation and audit path.

Update the HTTP Harness to use one in-memory durable `StorageRoot`,
`Authentication::new_durable`, and a real `CatalogStore`; seed the repository
through the catalog so handler tests exercise the production owner boundary.

**Verify**: `auth_tests/members.rs` proves admin GET/PUT, exact response shape,
case-insensitive route lookup, missing/wrong Origin, missing/wrong CSRF, outsider
404, read-member 404, absent repository 404, stale admin 404 using the newer
catalog snapshot, malformed input, last-admin refusal, no-op revision stability,
two-writer conflict, audit actor/digests/version, and next-request authorization
after downgrade/removal. Run the membership HTTP and full auth commands.

### Step 3: Add Settings → Members

In `api.ts`, add `RepositoryMember` and `MembershipState` types. In
`settings.tsx`, add a `members` section without changing the outer
`repo.can_admin` gate.

1. Fetch membership only when the Members section mounts. Show loading, retry,
   empty, and fixed server-error states.
2. Render subject, display name, and access for every member. Subject text must
   wrap without widening the page.
3. Add accessible add/edit forms for all three fields and an explicit remove
   confirmation. Build a complete replacement array and send one `PUT` with the
   last fetched revision and session CSRF header.
4. On success, replace local state with the server response. On
   `membership_changed`, retain the unsaved draft, show an alert that another
   administrator changed membership, and provide an explicit “Reload members”
   action. Never auto-resubmit against the new revision.
5. Show server validation verbatim from the fixed error message. Client-side
   trimming and required-field hints improve input only; they do not implement
   access or the final-admin rule.
6. If the authenticated OIDC subject is no longer an admin in the accepted
   response, navigate to the repository root after showing success. Local mode
   remains administrator by server contract.
7. Reuse existing settings tokens and responsive breakpoints. Add only the
   classes needed for member rows/forms/conflict alerts.

Extend the existing Playwright API fixture and settings tests rather than
adding a second application harness.

**Verify**: browser coverage adds, edits, and removes members; asserts the full
replacement body and CSRF header; proves a simulated stale revision preserves
the draft until reload; proves the final-admin server error is visible; checks
390px width, keyboard labels/focus, dark theme, and axe. Run frontend typecheck,
unit tests, browser test, format check, and build.

### Step 4: Add the durable identity-to-session index

Refactor session lifecycle before adding the public provider endpoint.

1. Add the identity-key derivation and marker record under `AuthState`. Use
   domain-separated hashes and constant-size object keys.
2. Change durable `store_session` to marker-first/session-second with rollback.
   In-memory test mode may retain its map scan, but production behavior and HTTP
   tests use durable state.
3. Make `load_session` verify marker identity and self-heal an absent marker
   during the compatibility window. A malformed marker or session fails closed.
4. Replace single-key removal with one canonical revoker that deletes the
   session first, invalidates cached `Arc<Session>`, deletes every recorded Git
   token, invalidates matching cached `GitToken`s, then removes markers.
5. Add streaming `revoke_identity(issuer, subject)` over the exact hashed
   prefix. Verify marker body identity before deleting anything. Missing
   sessions and repeated revocation are success.
6. Implement the eight-hour legacy-session migration record and temporary
   session-prefix scan exactly as specified. The scan must stream; do not collect
   an unbounded vector.

Keep the current invariant that a request admitted before revocation may drain;
every later browser or Git request reloads the missing durable parent and fails.

**Verify**: unit tests cover marker-first failure cleanup, stale markers,
multiple sessions for one subject, issuer separation for equal subjects,
concurrent Git-token issue versus revocation, eager token deletion, repeated
revocation, legacy-session migration, and index-only behavior after the window.
Run the full auth regression.

### Step 5: Validate Logout Tokens and expose the provider callback

Add `jsonwebtoken.workspace = true` to the server crate. Put protocol-specific
code in `auth/backchannel_logout.rs`.

1. Require `application/x-www-form-urlencoded`, parse the bounded body with the
   existing `url::form_urlencoded` support, ignore unknown parameters as the
   specification requires, and reject zero or multiple `logout_token` values.
   Add the fixed claim/header structures and validation order from “Target
   contracts.” Bound `sub` and `jti` to 512 characters. Keep provider/JWK errors
   as sources internally and return only a fixed OAuth-style `invalid_request`
   response.
2. Store replay records under a hashed `(issuer, jti)` key. Create `pending`
   before revocation; same-token retry resumes pending work and returns success
   for completed work. Reject same-jti/different-token reuse.
3. Call `revoke_identity` only after complete JWT verification.
4. Route `POST /auth/backchannel-logout`, set the body limit, and exempt only
   this exact path from browser mutation protection. Do not exempt `/auth/*` or
   accept unsigned bearer/header alternatives.
5. Register no new configuration flag. Operators enable delivery by registering
   the documented endpoint at their provider.

Extend the local provider fixture to sign `logout+jwt` tokens with its current
key and rotate keys after server startup.

**Verify**: HTTP tests prove one valid token revokes two browser sessions and
their Git tokens across durable Authentication instances; replay succeeds;
already-logged-out succeeds; rotated signing key succeeds; and bad signature,
unknown/ambiguous key, disallowed algorithm, wrong issuer, wrong audience,
expired token, future `iat`, missing/empty `jti`, missing `sub`, sid-only token,
missing/wrong event, `nonce`, wrong `typ`, oversized body, and tampered payload
all return `400` without revoking the session. Prove the endpoint works without
cookie/Origin/CSRF while ordinary membership `PUT` still requires all three.

### Step 6: Document migration and run broad proof

Update README, design, reference, deployment, operations, and Helm guidance:

- membership endpoint/request/response/error contracts and Settings workflow;
- catalog v2 read/v3 first-write migration, audit path/outbox semantics, and
  rollback warning after a v3 write;
- CLI and browser changes both produce audit events;
- back-channel URI registration:
  `https://git.example.com/auth/backchannel-logout`;
- `backchannel_logout_session_required=false` and the explicit sid-only
  limitation;
- eight-hour identity-index compatibility window, requirement to complete the
  rollout inside that window, and when to register provider delivery;
- 200/400 behavior, key rotation, replay behavior, metrics/log fields, and how
  to verify that browser and Git credentials stop working;
- remove browser membership administration, audit history, and provider
  back-channel logout from the known-gap list; retain immediate arbitrary
  provider revocation if no Logout Token is delivered.

Run every command in the Commands table. Then inspect:

```sh
git diff --check
git diff --numstat 0fb67e93990 -- \
  Cargo.toml Cargo.lock crates/crab-http-server packages/ui advisor-plans
git status --short
```

Expected: no whitespace errors, no generated or dependency-tree files, and no
unrelated modifications. Explain any non-test LOC growth in the PR; the new
files must replace duplicated handler/JWT logic rather than wrap it.

## Test plan

The tests named in each step are mandatory. The minimum retained matrix is:

- **Catalog**: schema migration, revision CAS, final admin, semantic digest,
  atomic pending audit, immutable flush, crash/retry, idempotence.
- **HTTP authorization**: admin success; read/write/outsider/absent all hidden;
  auth checked against the mutation snapshot; CSRF/Origin enforced.
- **UI**: load/add/edit/remove, validation, two-tab conflict, draft retention,
  responsive dark/light accessibility.
- **Session lifecycle**: marker ordering, exact issuer+subject matching,
  multi-session removal, Git-token invalidation, migration cutoff.
- **Logout Token**: complete positive flow, retransmission, key rotation, every
  mandatory claim/header rejection, no side effect on invalid input.
- **Regression**: all existing auth, Git-token, settings, frontend unit, and
  repository browser tests remain green.

## Done criteria

- [x] Both membership routes implement the exact schemas and status/code table.
- [x] Every route authorization decision uses the same catalog snapshot/revision
      as the mutation; no stale in-memory admin grant can read or overwrite it.
- [x] A successful authenticated replacement always contains an admin and has
      one durable, chain-linked event with exact actor, digests, version, time.
- [x] Audit interruption cannot produce an unaudited committed membership or a
      visible audit event for a rejected mutation.
- [x] Settings → Members works at desktop and 390px, passes axe, and never
      retries a conflict automatically.
- [x] Valid provider logout invalidates all exact-identity browser sessions and
      derived Git tokens on the next request across replicas.
- [x] Invalid or replay-conflicting Logout Tokens have no revocation side effect.
- [x] Raw browser, Git, and provider tokens appear in no key, log, audit event,
      response, or test failure output.
- [x] The compatibility window and catalog v3 migration have documented rollout
      and rollback boundaries.
- [x] All Commands-table checks pass with the external Cargo target.
- [x] `Cargo.lock` has the reviewed direct `jsonwebtoken` 11.0.0 resolution delta
      explicitly justified.
- [x] No file outside Scope is modified; `advisor-plans/README.md` marks 018 DONE.

## Execution evidence

Implemented on `codex/auth-membership-admin` in two scoped commits:

- `daa07cdd3b0` — audited catalog membership, admin-only routes, durable
  identity-session revocation, and OIDC back-channel logout.
- `30999e286e6` — Settings → Members, browser coverage, and deployment/docs
  updates.

Rust proof used the checkout-specific external target
`$HOME/Workspace/crabbuild-target/crab-6d40-auth-membership`: catalog 13/13,
membership HTTP 4/4, back-channel logout 4/4, full auth regression 30 passed
with one manual fixture ignored, durable lifecycle 8/8, `cargo check`, and
`cargo clippy --all-targets -- -D warnings`; `cargo fmt --all -- --check` and
`git diff --check` also passed. UI proof passed `npm ci`, typecheck, Vitest
48/48, formatting, production build, and repository Playwright 30/30 including
the Members conflict, final-admin, responsive, dark-mode, and axe coverage.

The lockfile delta is limited to the direct workspace-pinned `jsonwebtoken`
11.0.0 dependency and its required `untrusted` edge. No provider/cloud
qualification was run; deployment must register the exact back-channel URI and
complete the eight-hour identity-index rollout described above.

## STOP conditions

Stop and report instead of improvising if:

- current code no longer uses one global CAS catalog for membership;
- the configured provider emits only sid-scoped Logout Tokens or requires
  encrypted Logout Tokens;
- `jsonwebtoken 11.0.0` cannot verify one signing algorithm already accepted by
  Crab's current OIDC login fixture/provider metadata;
- committing an audit event with membership requires a second independently
  authoritative policy store rather than the bounded catalog outbox;
- catalog v3 cannot retain a strict v2 read path or the release owner requires
  rollback to an older binary after the first v3 write;
- completing session-index migration requires old and new binaries to coexist
  longer than the eight-hour maximum session lifetime;
- any test needs a real credential, a production identity tenant, or edits to a
  baseline/snapshot to pass;
- an implementation step requires dependency patches, vendoring, a refresh
  token, email authorization, or a browser-held provider token.

## Maintenance notes

- Reviewers should scrutinize the audit commit point, ETag-only flush retries,
  authorization against the exact mutation snapshot, marker/session write
  ordering, replay pending/completed transitions, and the CSRF exemption's
  exact-path match.
- The global catalog version is intentionally conservative. If unrelated admin
  traffic creates measurable conflicts, design a per-record revision in a
  separate catalog-schema plan; do not silently change this API's semantics.
- Remove the legacy session-prefix migration branch only after rollout evidence
  proves no pre-index binary or live eight-hour session remains.
- Add sid support only with verified ID-token sid extraction, an
  `(issuer, sid)` reverse index, mixed sub+sid matching rules, and provider
  conformance tests.
- A future audit-history UI/API must traverse only the committed chain plus the
  catalog's current pending head, enforce admin authorization, paginate, and
  preserve the 404 non-disclosure contract.
