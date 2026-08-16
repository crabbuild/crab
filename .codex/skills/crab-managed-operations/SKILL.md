---
name: crab-managed-operations
description: Manage Crab authentication, managed-service organizations and repositories, memberships, service accounts, audit events, and dataset release manifests. Use whenever a request mentions `crab login`, `logout`, `auth`, organization or repository administration, members, service accounts, `crab audit`, or `crab release` manifests.
compatibility: Crab CLI with the selected managed Crab service and the required administrative permissions.
---

# Crab managed operations

Keep identity, administrative control-plane state, auditability, and dataset
release records separate from local Git/object-store mechanics. Never expose
tokens or infer permissions from a successful local configuration read.

## Command scope

`login`, `logout`, `auth status/refresh`, `organization`, `repo`, `member`,
`service-account`, `audit`, and `release create/verify/export/list`.

The existing `crab-release-publish` skill owns publishing Crab CLI archives to
the release repository and updating Homebrew; `crab release` here owns
repository dataset release manifests.

## Authentication

1. Inspect the active profile, provider, service origin, config precedence,
   and headless/interactive context before login.
2. Use the documented provider flow. Let the credential store and refresh path
   manage tokens; do not copy tokens into shell history or reports.
3. Verify with `crab auth status` and a least-privilege read against the
   intended service. Distinguish authentication from authorization and object
   storage reachability.
4. For logout or provider changes, identify whether the action affects one
   service or all cached providers before deleting credentials.

## Managed administration

- Confirm the selected service, organization, repository, actor, and requested
  operation before any create/update/delete action.
- Inspect existing membership or repository state first; use idempotent paths
  where the command provides them and do not silently change ownership.
- Treat service-account credentials as secrets. Show identifiers and redacted
  status only.

## Audit and release manifests

- `audit log`, `verify`, and `export` operate on the local JSONL audit chain.
  Verify schema, digest, sequence, redaction, and operation filters.
- `release create` binds a Git revision to pointer inventory, workflow metadata,
  params, metrics, and optional signatures. It should refuse an unintended
  dirty worktree; use an explicit override only when authorized.
- `release verify --deep` is content proof: it must reconstruct pointer-backed
  files and check identity, not merely parse JSON.
- `release export` is a portability boundary. Preserve signature and identity
  metadata and verify the exported bytes.

## Read first

- `crab/docs/guides/auth/enterprise-auth.md`
- `crab/docs/guides/auth/enterprise-auth-static.md`
- `crab/docs/guides/auth/enterprise-auth-aws.md`
- `crab/docs/guides/auth/enterprise-auth-gcp.md`
- `crab/docs/guides/auth/enterprise-auth-azure.md`
- `crab/docs/guides/auth/enterprise-auth-crab-auth.md`
- `crab/docs/guides/audit.md`
- `crab/docs/guides/release-manifests.md`
- `crab/src/{auth,audit,release}/`
- `crab/src/cmd/{login,logout,auth_status,managed_admin,audit,release}.rs`
- `.codex/skills/crab-cli-core/references/contracts.md`
