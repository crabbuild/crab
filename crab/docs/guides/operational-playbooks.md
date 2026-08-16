# Operational Playbooks

Runbooks for common storage economy operations on production Crab
buckets. Each playbook includes prerequisites, step-by-step commands,
verification, and rollback procedures.

## Playbook 1: Enable lifecycle tiering on a production bucket

**When to use:** First-time tiering setup on a bucket that has been
running without lifecycle rules.

**Prerequisites:**
- Write credentials for the bucket (see [IAM requirements](crab-tier.md#iam-requirements))
- Backup of any existing lifecycle rules (IaC or manual export)

**Steps:**

```bash
# 1. Baseline cost snapshot
crab doctor --cost --json > before-tier.json

# 2. Preview the lifecycle plan
crab tier plan

# 3. Review the plan in detail (JSON for IaC integration)
crab tier plan --output=json > tier-plan.json

# 4. Dry-run to verify CAS guard and conflict detection
crab tier plan --apply --dry-run

# 5. Apply (backup is written automatically)
crab tier plan --apply

# 6. If the bucket has existing non-Crab rules, use --merge
crab tier plan --apply --merge

# 7. Verify the applied rules
crab tier plan --json

# 8. Wait for provider evaluation (S3 ~24h, GCS ~24h, Azure up to 7d)

# 9. Post-tiering cost comparison
crab doctor --cost --json > after-tier.json
```

**Rollback:**

```bash
# List available backups
ls .crab/tier/backups/

# Restore the pre-apply configuration
crab tier rollback .crab/tier/backups/<timestamp>-pre-apply.json
```

## Playbook 2: Optimize xorbs for a large ML repository

**When to use:** Xorb sizes are mismatched for the workload (e.g.,
many small xorbs from incremental pushes on an ML repo).

**Prerequisites:**
- Sufficient disk space for the xorb optimization journal
- No concurrent `crab gc` running

**Steps:**

```bash
# 1. Check current xorb distribution
crab stat classes

# 2. Dry-run to estimate cost and duration
crab optimize xorbs --profile=ml --dry-run

# 3. Execute the optimization
crab optimize xorbs --profile=ml --apply

# 4. If interrupted (SIGTERM/crash), resume
crab optimize xorbs --profile=ml --resume

# 5. Verify post-optimization hydration
crab hydrate --all
crab fsck

# 6. Clean up orphan source xorbs
crab gc
```

**Abort and cleanup:**

```bash
# Flag the run as aborted
crab optimize xorbs --abort

# If the journal is corrupt or you want a clean start
crab optimize xorbs --drop-journal --yes-really

# Reclaim orphan xorbs from the aborted run
crab gc
```

## Playbook 3: Hydrate archived xorbs

**When to use:** A `crab hydrate` fails because xorbs have been
transitioned to Glacier or Azure Archive by lifecycle rules.

**Steps:**

```bash
# 1. Hydrate with auto-restore (default)
crab hydrate --all

# 2. If you need faster restore (S3 Glacier Flexible only)
crab hydrate --all --restore-tier=expedited

# 3. If you want to see restore progress
crab hydrate --all --jsonl

# 4. If auto-restore is disabled and you want to fail fast
crab hydrate --all --no-restore
```

**Monitoring restore progress:**

The `--jsonl` flag streams `tier.event` events showing restore
submissions and completions. Each event includes the xorb hash,
storage class, restore tier, and estimated ready time.

## Playbook 4: Investigate and reduce storage costs

**When to use:** Monthly storage bill is higher than expected.

**Steps:**

```bash
# 1. Full cost report
crab doctor --cost

# 2. JSON output for programmatic analysis
crab doctor --cost --json > cost-report.json

# 3. Per-class breakdown
crab stat classes

# 4. For very large buckets, use sampling
crab doctor --cost --sample 0.25

# 5. Review recommendations and apply selectively
# (recommendations are never auto-executed)
```

**Common recommendations:**

| Recommendation | Action | Risk |
|---------------|--------|------|
| Apply IA tiering | `crab tier plan --apply` | Low |
| Apply Glacier tiering | `crab tier plan --apply` (adjust `to_deep_days`) | Medium |
| Optimize xorbs | `crab optimize xorbs --apply` | Medium |
| GC orphans | `crab gc` | Low |

## Playbook 5: Force-delete objects in retention window

**When to use:** You need to delete objects that are within their
minimum retention period (e.g., cleaning up after a security incident).

**Warning:** This incurs early-deletion penalties from the cloud
provider. The penalty is the remaining storage cost for the minimum
retention period.

**Steps:**

```bash
# 1. See the estimated penalty
crab gc --dry-run --force-early-delete --yes-really

# 2. If the penalty is acceptable, proceed
crab gc --force-early-delete --yes-really

# 3. Verify the audit record (when crab-audit is enabled)
# The force-delete is recorded with penalty_estimated_usd, class, age_days
```

## Playbook 6: Concurrent operations safety

**Allowed concurrency:**

| Operation A | Operation B | Allowed? |
|------------|------------|----------|
| `tier plan --apply` | `tier plan --apply` | One wins via CAS; other gets `TierLifecycleConflict` |
| `optimize xorbs --apply` | `push` | Yes — reconciliation handles it |
| `optimize xorbs --apply` | `gc` | No — `ConcurrentMaintenance` error |
| `optimize xorbs --apply` | `optimize xorbs --apply` | No — `CRAB-E0332` error |
| `gc` | `tier plan --apply` | Yes — independent operations |

**If you see `ConcurrentMaintenance`:**

```bash
# Check if xorb optimization is running
ls .crab/restripe/journal.db

# If the process crashed, the lock may be stale
# Resume or abort the xorb optimization first, then run GC
crab optimize xorbs --resume   # or --abort
crab gc
```
