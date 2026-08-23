# crab audit

Inspect and verify Crab audit events.

Status: local audit event records, redaction, digest and sequence verification,
listing, and export are implemented. Release publication, repository recovery
apply, push, replica promotion, auth token flows, tiering, xorb optimization, and
class-aware GC append events to the local audit log.

```bash
crab audit log
crab audit verify
crab audit export --output audit.json
```

The local audit log defaults to `.crab/audit/events.jsonl`. Each event is a
single JSON line with a schema version, event id, Unix timestamp, operation,
outcome, optional actor/repository fields, redacted details, and a Blake3
digest over the event body.

## Commands

### `crab audit log`

Prints local audit events.

```bash
crab audit log
crab audit log --operation release.publish
crab audit log --json
```

Use `--path <PATH>` to inspect an exported or alternate JSONL log.

### `crab audit verify`

Verifies schema version, event digests, duplicate event ids, and timestamp
ordering.

```bash
crab audit verify
crab audit verify --path .crab/audit/events.jsonl --json
```

Verification exits non-zero when any event is malformed, has an unsupported
schema version, fails digest validation, repeats an event id, or regresses in
timestamp order.

### `crab audit export`

Exports events as a portable JSON array.

```bash
crab audit export --output audit.json
crab audit export --operation recover.apply --output recovery-audit.json
```

## Current Scope

This command group provides the local event model, digest and sequence
verification, redaction, listing, and export foundation. The covered mutating
operations include `release.publish`, `recover.apply`, `push`,
`replica.promote`, `auth.login`, `auth.grant`, `auth.refresh`, `auth.logout`,
`tier.apply`, `tier.rollback`, `optimize.xorbs.start`, `optimize.xorbs.finalize`, and
`gc.force_early_delete`. Optional remote event publication is available through
the audit subsystem.
