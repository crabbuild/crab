# crab tier

Generate, apply, and roll back lifecycle rules that move cold xorbs to cheaper
storage classes automatically.

## Synopsis

```
crab tier plan [OPTIONS]
crab tier plan --apply [OPTIONS]
crab tier rollback <BACKUP>
```

## Description

Most Crab buckets follow a heavy-tail access pattern: a small hot set is read
continuously while a long cold tail is rarely or never touched. Without
lifecycle rules, every byte pays the Standard-tier rate regardless of access
frequency.

`crab tier` generates provider-specific lifecycle rules scoped to the
`.crab/xorbs/` prefix, transitions objects through warm and cold storage
classes on a configurable schedule, and keeps a backup of the prior
configuration so you can roll back at any time.

Rules are read-only by default. Nothing is written to the bucket until you pass
`--apply`.

## Quick Start

### Preview the lifecycle plan

```bash
crab tier plan
```

This probes the bucket, reads existing lifecycle rules, and prints the plan
Crab would apply. No credentials beyond read-only are needed.

### Apply the plan

```bash
crab tier plan --apply
```

Crab requires a provider conditional-write guard before lifecycle mutation,
writes a backup of the current configuration to
`.crab/tier/backups/<RFC3339-ts>-pre-apply.json`, submits the new rules, and
requires an equivalent provider read-back before reporting success. The backup
uses the repository's shared Crab directory, even when invoked from a
subdirectory, and is synced before the provider write begins. Providers that
cannot supply a conditional guard fail closed.

### Roll back

```bash
crab tier rollback .crab/tier/backups/2026-04-27T20:00:00Z-pre-apply.json
```

Restores the lifecycle configuration that was in place before the apply.

## Lifecycle Design

### How rules are generated

`crab tier plan` executes the following steps:

1. **Probe the bucket** — detect versioning, object lock, and any existing
   lifecycle rules. Identify prior `crab-*` rule IDs.
2. **Read the store layout** — the only tier-eligible prefix in V1 is
   `.crab/xorbs/`. Shards, file-index, refs, manifests, packs, locks,
   audit, and tombstones are never tiered.
3. **Compute the rule set** from `[tier]` configuration and defaults.
4. **Apply per-class size clamps** — S3 Glacier classes enforce a 40 KiB
   minimum object size. Rules include `ObjectSizeGreaterThan = 40960` so
   tiny objects stay in the prior class.
5. **Return the plan** — a provider-neutral `TierPlan` rendered into the
   provider's native format (S3 XML, GCS JSON, Azure JSON).

### Rule ID namespace

Every rule ID is prefixed with `crab-`. This namespace lets `--merge` mode
distinguish Crab-managed rules from user-managed rules and replace only the
former.

### Tier-eligible prefixes

| Prefix                          | Tier-eligible | Notes                        |
|---------------------------------|---------------|------------------------------|
| `.crab/xorbs/`                | **yes**       | Content-addressed blob store |
| `.crab/shards/`               | no            | Metadata — must stay warm    |
| `.crab/file-index/`           | no            | Metadata — must stay warm    |
| `<repo>/refs/`                  | no            | Mutable refs                 |
| `<repo>/manifests/`             | no            | Mutable                      |
| `<repo>/packs/`                 | no            | Git packs                    |
| `<repo>/locks/`                 | no            | Advisory locks               |
| `.crab/audit/`                | **never**     | Immutable audit log          |
| `.crab/tombstones/`           | no            | Disaster recovery            |

## Configuration

All tiering settings live under the `[tier]` section in `.crab/config.toml`.
Every field has a sensible default; an absent `[tier]` block means defaults
apply. An unreadable or invalid Crab configuration is an error; maintenance
does not silently substitute defaults.

```toml
[tier]
enabled                  = false       # opt-in; no behavior change until true
to_ia_days               = 30          # days before transition to warm-cold class
to_deep_days             = 180         # days before transition to deep-cold class
noncurrent_days          = 30          # noncurrent version expiration (versioned buckets)
restore_tier             = "standard"  # expedited | standard | bulk (S3); high | standard (Azure)
restore_duration_days    = 7           # how long restored copies stay readable
restore_max_concurrency  = 16          # max parallel RestoreObject calls
restore_timeout_secs     = 21600       # 6 hours; ArchiveRestoreTimeout [E0211] after this
optimize_xorbs_output_class = "standard" # class for newly written xorbs during `crab optimize xorbs`
```

Related hydrate setting:

```toml
[hydrate]
auto_restore             = true        # automatically restore archived xorbs on hydrate
```

Environment variable overrides follow the `CRAB_*` convention:

| Config key              | Env var                          |
|-------------------------|----------------------------------|
| `tier.to_ia_days`       | `CRAB_TIER_TO_IA_DAYS`         |
| `tier.restore_tier`     | `CRAB_TIER_RESTORE_TIER`       |
| `tier.restore_timeout_secs` | `CRAB_TIER_RESTORE_TIMEOUT_SECS` |
| `hydrate.auto_restore`  | `CRAB_HYDRATE_AUTO_RESTORE`    |


## IAM Requirements

Each operation requires a minimum set of provider permissions. Read-only
commands (`tier plan` without `--apply`) work with read-only credentials.
Insufficient permissions produce `TierApplyUnauthorized [CRAB-E0201]` listing
the missing permission.

### S3

| Operation                | Required permissions                                                  |
|--------------------------|-----------------------------------------------------------------------|
| `tier plan` (read-only)  | `s3:GetLifecycleConfiguration`                                        |
| `tier plan --apply`      | `s3:GetLifecycleConfiguration`, `s3:PutLifecycleConfiguration`        |
| Versioning probe         | `s3:GetBucketVersioning`                                              |
| `hydrate` with restore   | `s3:GetObject`, `s3:HeadObject`, `s3:RestoreObject`                   |
| Cost inventory (live)    | `s3:ListBucket`, `s3:GetObjectAttributes`                             |
| Cost inventory (report)  | `s3:ListBucket`, `s3:GetObject` (on the Inventory prefix)             |

Example IAM policy for `tier plan --apply`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetLifecycleConfiguration",
        "s3:PutLifecycleConfiguration",
        "s3:GetBucketVersioning"
      ],
      "Resource": "arn:aws:s3:::my-crab-bucket"
    }
  ]
}
```

### GCS

| Operation                | Required permissions                                  |
|--------------------------|-------------------------------------------------------|
| `tier plan` (read-only)  | `storage.buckets.get`                                 |
| `tier plan --apply`      | `storage.buckets.get`, `storage.buckets.update`       |
| Object listing           | `storage.objects.list`, `storage.objects.get`          |
| Class transition         | `storage.objects.update`                              |

### Azure Blob Storage

| Operation                | Required permissions                                                                                  |
|--------------------------|-------------------------------------------------------------------------------------------------------|
| `tier plan` (read-only)  | `Microsoft.Storage/storageAccounts/managementPolicies/read`                                           |
| `tier plan --apply`      | `Microsoft.Storage/storageAccounts/managementPolicies/read`, `Microsoft.Storage/storageAccounts/managementPolicies/write` |
| Object operations        | `Microsoft.Storage/storageAccounts/blobServices/containers/blobs/*`                                   |

## Restore Matrix

When `hydrate` encounters an archived xorb, it needs to know whether a restore
is required and which restore tiers are available. Warm classes are read
directly. Archive classes require a `RestoreObject` call (or equivalent) before
the data is readable.

### Warm classes (no restore needed)

These classes are read directly at standard latency. Some incur a per-GB
retrieval fee modeled in `crab doctor --cost`.

| Provider | Class                        | Retrieval fee |
|----------|------------------------------|---------------|
| S3       | Standard                     | —             |
| S3       | Intelligent-Tiering          | —             |
| S3       | Standard-IA                  | $0.01/GB      |
| S3       | One-Zone-IA                  | $0.01/GB      |
| S3       | Glacier Instant Retrieval    | $0.03/GB      |
| GCS      | Standard                     | —             |
| GCS      | Nearline                     | per-GB fee    |
| GCS      | Coldline                     | per-GB fee    |
| GCS      | Archive                      | per-GB fee    |
| Azure    | Hot                          | —             |
| Azure    | Cool                         | per-GB fee    |

Glacier Instant Retrieval and GCS Archive are **not** archive classes for
restore purposes — reads are direct, though they carry a retrieval fee.

### Archive classes (restore required)

| Class                         | Expedited       | Standard        | Bulk            |
|-------------------------------|-----------------|-----------------|-----------------|
| S3 Glacier Flexible Retrieval | ✓ (1–5 min)     | ✓ (3–5 h)       | ✓ (5–12 h)      |
| S3 Glacier Deep Archive       | —               | ✓ (12 h)        | ✓ (48 h)        |
| Azure Archive                 | —               | ✓ High (<1 h), Standard (≤15 h) | — |

Invalid combinations (e.g., `Expedited` on Deep Archive, `Bulk` on Azure)
produce `RestoreTierUnsupported [CRAB-E0212]` listing the supported tiers for
that class.

Crab's default restore tier is `Standard` where it exists.

### Minimum retention periods

Deleting an object before its minimum retention period incurs an early-deletion
penalty equal to the remaining storage cost. `crab gc` blocks early deletes
by default and reports the estimated penalty.

| Class                         | Min retention (days) |
|-------------------------------|----------------------|
| S3 Standard-IA                | 30                   |
| S3 One-Zone-IA                | 30                   |
| S3 Glacier Instant Retrieval  | 90                   |
| S3 Glacier Flexible Retrieval | 90                   |
| S3 Glacier Deep Archive       | 180                  |
| GCS Nearline                  | 30                   |
| GCS Coldline                  | 90                   |
| GCS Archive                   | 365                  |
| Azure Cool                    | 30                   |
| Azure Cold                    | 90                   |
| Azure Archive                 | 180                  |

When `crab gc` encounters an object within its minimum retention window, it
emits `GcEarlyDeleteBlocked [CRAB-E0220]` with the estimated penalty. Use
`--force-early-delete --yes-really` to proceed (see
[`crab gc`](crab-gc.md)).

### Transition timestamp resolution

The age used for early-delete checks varies by provider:

- **S3**: `last_modified` (S3 does not expose a transition timestamp; the
  mutation timestamp matches the retention window correctly).
- **GCS**: `timeStorageClassUpdated`.
- **Azure**: `AccessTierChangeTime`.

## Rollback

Every `tier plan --apply` writes a backup before modifying the bucket:

```
.crab/tier/backups/<RFC3339-ts>-pre-apply.json
```

The backup file contains:

```json
{
  "provider": "s3",
  "rendered_existing": "<base64-encoded lifecycle XML/JSON>",
  "cas_guard": "\"etag-value\"",
  "saved_at": "2026-04-27T20:00:00Z"
}
```

### Restoring a previous configuration

```bash
crab tier rollback .crab/tier/backups/2026-04-27T20:00:00Z-pre-apply.json
```

This reads the backup, submits the prior lifecycle configuration via the
provider's CAS-guarded API, and confirms the restore. If the bucket's lifecycle
has been modified since the backup was taken, the CAS guard detects the conflict
and the rollback fails safely — re-read the current state and decide how to
proceed.

### Backup retention

Backup files are never auto-deleted. Retention is the operator's
responsibility. Old backups are small (a few KB each) and safe to prune
manually.

## Merge Mode

By default, `tier plan --apply` replaces the entire lifecycle configuration. If
the bucket has user-managed rules (e.g., rules from Terraform or CloudFormation
that handle non-Crab prefixes), a full replace would drop them.

### `--merge` behavior

```bash
crab tier plan --apply --merge
```

With `--merge`:

1. Crab reads the existing lifecycle configuration.
2. Rules whose ID starts with `crab-` are replaced with the new plan.
3. All other rules are preserved unchanged.
4. The merged configuration is submitted via CAS.

Without `--merge`, if existing rules conflict with the new plan, Crab fails
with `TierLifecycleConflict [CRAB-E0200]` and prints both conflicting rules.

### Conflict detection

Two rules conflict when:

- They share the same ID but have different bodies, OR
- They have different IDs but overlap in prefix scope AND actions at the same
  transition day.

`--merge` resolves conflicts for `crab-`-prefixed rules automatically. For
user-managed rules that conflict, the error is raised regardless of `--merge`.

## Flags Reference

| Flag | Subcommand | Default | Description |
|------|------------|---------|-------------|
| `--apply` | `plan` | `false` | Submit the plan to the provider (requires write credentials) |
| `--merge` | `plan --apply` | `false` | Preserve non-`crab-` rules; replace only Crab-managed rules |
| `--dry-run` | `plan --apply` | `false` | Show what would be written without submitting |
| `--output` | `plan` | provider native | Output format: `xml`, `json`, or `yaml`; JSON uses the standard `tier.plan` envelope |
| `--json` | all | `false` | Emit structured JSON via `Envelope` (`"tier.plan"` v `"1.0"`) |
| `--jsonl` | all | `false` | Stream JSONL events via `JsonlStream` (`"tier.event"` v `"1.0"`) |

## Restore-Aware Hydrate

When `hydrate.auto_restore = true` (the default) and a xorb is in an archive
class, `crab hydrate` automatically issues a restore request and polls until
the object is readable.

```bash
# Hydrate with default restore tier (Standard)
crab hydrate --all

# Override the restore tier for this invocation
crab hydrate --all --restore-tier=expedited

# Disable auto-restore (fail immediately on archived xorbs)
crab hydrate --all --no-restore
```

### Restore CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--restore` | `true` (when `auto_restore = true`) | Enable restore for archived xorbs |
| `--no-restore` | | Disable restore; fail with `ArchiveRestoreRequired [CRAB-E0210]` |
| `--restore-tier` | `standard` | Restore tier: `expedited`, `standard`, `bulk` (S3); `high`, `standard` (Azure) |
| `--restore-duration-days` | `7` | How long restored copies remain readable |

### Polling behavior

When a restore is in progress, Crab polls with exponential back-off: 30 s
initial interval, 1.5× multiplier, 10 min cap, full jitter. If the restore does
not complete within `tier.restore_timeout_secs` (default 6 hours), Crab emits
`ArchiveRestoreTimeout [CRAB-E0211]`. The provider-side restore continues;
retry later.

Batch restores are capped at `tier.restore_max_concurrency` (default 16)
concurrent requests.

### JSONL events

Restore progress is streamed as `"tier.event"` v `"1.0"` events:

```
{"schema":"tier.event","version":"1.0","type":"restore_submit","data":{"xorb_hash":"abc123...","class":"GlacierFlexible","restore_tier":"standard","requested_at":"2026-04-27T20:00:00Z","expected_ready_at":"2026-04-27T23:00:00Z","poll_interval_ms":30000}}
{"schema":"tier.event","version":"1.0","type":"restore_complete","data":{"xorb_hash":"abc123...","class":"GlacierFlexible","state":"Ready","completed_at":"2026-04-27T22:45:00Z","wait_ms":9900000}}
```

## Common Workflows

### Enable tiering on a production bucket

```bash
# 1. Baseline cost snapshot
crab doctor --cost --json > before.json

# 2. Review the plan
crab tier plan --output=json > plan.json

# 3. Dry-run to see the CAS guard
crab tier plan --apply --dry-run

# 4. Apply (backup is written automatically)
crab tier plan --apply

# 5. Wait for provider evaluation (S3 ~24 h, GCS ~24 h, Azure up to 7 d)

# 6. Compare costs
crab doctor --cost --json > after.json
```

### Merge with existing IaC-managed rules

```bash
crab tier plan --apply --merge
```

### Forced early delete with cost visibility

```bash
# See the penalty before committing
crab gc --dry-run --force-early-delete --yes-really

# Proceed
crab gc --force-early-delete --yes-really
```

## Error Codes

| Code | Variant | When |
|------|---------|------|
| `CRAB-E0200` | `TierLifecycleConflict` | Existing rules conflict with the plan; use `--merge` or remove the conflicting rule |
| `CRAB-E0201` | `TierApplyUnauthorized` | Credentials lack the required management permission |
| `CRAB-E0202` | `TierProviderUnsupported` | Provider is not S3, GCS, or Azure Blob |
| `CRAB-E0210` | `ArchiveRestoreRequired` | Xorb is archived and `auto_restore` is off or `--no-restore` was passed |
| `CRAB-E0211` | `ArchiveRestoreTimeout` | Restore did not complete within `restore_timeout_secs` |
| `CRAB-E0212` | `RestoreTierUnsupported` | Invalid restore tier for the object's storage class |
| `CRAB-E0220` | `GcEarlyDeleteBlocked` | Object is within its minimum retention window |
| `CRAB-E0221` | `ObjectLockedRetention` | Object is under S3 Object Lock / GCS retention / Azure legal hold |

## Versioning and Object Lock

When bucket versioning is enabled, the generated lifecycle includes
`NoncurrentVersionExpiration` with `NoncurrentDays = tier.noncurrent_days`
(default 30).

Objects under S3 Object Lock, GCS retention policy, or Azure legal hold cannot
be deleted regardless of flags. Attempts produce
`ObjectLockedRetention [CRAB-E0221]`.

When `crab-audit` WORM mode is active (`crab-audit.worm = true`), lifecycle
rules never target the `.crab/audit/` prefix.

## JSON Output

### crab tier plan --json

```json
{
  "schema": "tier.plan",
  "version": "1.0",
  "timestamp": "2026-04-27T20:00:00Z",
  "data": {
    "provider": "s3",
    "rules": [
      {
        "id": "crab-xorbs-to-ia",
        "prefix": ".crab/xorbs/",
        "transitions": [
          { "days": 30, "to_class": "STANDARD_IA" }
        ]
      },
      {
        "id": "crab-xorbs-to-glacier",
        "prefix": ".crab/xorbs/",
        "transitions": [
          { "days": 180, "to_class": "GLACIER" }
        ]
      }
    ],
    "versioning_enabled": true,
    "object_lock_enabled": false,
    "conflicts": []
  }
}
```

### crab tier plan --apply --jsonl

```
{"schema":"tier.event","version":"1.0","type":"backup_written","data":{"path":".crab/tier/backups/2026-04-27T20:00:00Z-pre-apply.json"}}
{"schema":"tier.event","version":"1.0","type":"apply_success","data":{"provider":"s3","rules_applied":2,"cas_guard":"\"abc123\"","applied_at":"2026-04-27T20:00:01Z"}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.

## Related Commands

- [`crab hydrate`](crab-hydrate.md) — materialize pointer files; triggers restore for archived xorbs.
- [`crab gc`](crab-gc.md) — garbage collect; class-aware early-delete protection.
- [`crab doctor --cost`](crab-cost.md) — cost analysis and recommendations.
- [`crab optimize xorbs`](optimize-xorbs.md) — rewrite xorbs to a target size profile.
- [`crab config`](crab-config.md) — read/write `[tier]` settings.
