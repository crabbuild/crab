# crab-auth-server

`crab-auth-server` packages the service-side helper binaries used for
protected pushes and path-scoped views. It is not a long-running HTTP server;
it is a deterministic, JSON-speaking execution boundary that a managed auth
service or another orchestrator can invoke.

## Why it exists

Protected pushes need more than ordinary object uploads. The service must
validate the requested ref changes, verify every staged object and dependency,
materialize metadata, coordinate the final manifest/ref commit, and clean up
temporary state. Path-scoped views need the inverse operation: expose only
authorized Git paths while preserving a valid filtered repository.

Keeping those workflows in one server-side crate prevents clients from
becoming trusted to perform authorization or finalization themselves.

## Architecture

```text
crab-auth-receive                         crab-auth-view
        │                                         │
 prepare protected session                 resolve source manifest
        │                                         │
 verify staged objects + Git state          filter authorized paths
        │                                         │
 coordinate manifest/ref CAS                materialize/repack view
        │                                         │
 commit + cleanup                           JSON result
```

`crab-auth-receive` uses `ReceiveContext` and the receive workflow to validate
ref updates, staged Xet/Git objects, manifests, indexes, receipts, and
active-active options. Its `Prepare`, `Verify`, and `Commit` commands make the
publish boundary explicit; commit requires the verified plan digest.

`crab-auth-view` materializes a filtered view using repeated `--read-path`
and `--deny-path` rules, a scope hash, and a source repository URL. It builds
the view's Git workspace and Crab metadata and reports the resulting prefixes,
source generation, and cache status.

Both binaries emit structured JSON with safe error handling. `Doctor` reports
the Git version and helper readiness. Cleanup is attempted after receive
verification and commit, but cleanup warnings do not turn a successful
finalization into a false failure.

## Usage

Check helper prerequisites:

```text
crab-auth-receive doctor
crab-auth-view doctor
```

A protected-push orchestration sequence is:

```text
crab-auth-receive prepare \
  --repo-url s3://example-bucket/team/repository \
  --push-id push-123 \
  --provider s3 \
  --ref-updates-json '[...]'

crab-auth-receive verify \
  --repo-url s3://example-bucket/team/repository \
  --push-id push-123 \
  --provider s3

crab-auth-receive commit \
  --repo-url s3://example-bucket/team/repository \
  --push-id push-123 \
  --provider s3 \
  --plan-digest <digest-from-verify>
```

Use real authorization-issued URLs, providers, ref-update JSON, and plan
digests in production. Never pass raw credentials through command arguments or
log helper output containing token material.

## Boundaries

- [`crab-auth`](../crab-auth/README.md) owns auth and protected-push wire
  contracts; this crate executes the server-side workflow.
- [`crab-staging`](../crab-staging/README.md) owns local staged bytes and
  recovery; receive verifies them before publication.
- [`crab-coordination`](../crab-coordination/README.md) owns ref/write
  serialization; receive does not replace its CAS authority.
- [`crab-read`](../crab-read/README.md) owns canonical reconstruction used by
  the view pipeline.
