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

## Playbook 7: Operate Crab as a team data backbone

**When to use:** A team is evaluating Crab and object storage as production
Git and large-data infrastructure rather than as a developer convenience.

### Define the service contract first

Record these values before rollout:

| Contract | Team decision |
|----------|---------------|
| Recovery point objective | Maximum acceptable refs and object data lost |
| Recovery time objective | Maximum time to restore clone, fetch, and hydrate |
| Canonical collaboration plane | Usually GitHub/GitLab; Crab has no PR, review, CI, or branch-policy service |
| Object-store durability | Node/zone count, erasure or replication policy, and versioning/immutability |
| Identity lifetime | Short-lived workload or user federation; no shared team access key |

A single-node RustFS process is useful for compatibility tests, not a
production durability topology. Qualify the exact multi-node RustFS or managed
object-store topology, including one-node loss, one-disk loss, latency, request
timeouts, and a network partition, before committing to an RPO.

Use test credentials such as `crab`/`crab` only on an isolated development
endpoint. Production RustFS credentials must be unique per workload or user,
least-privilege, regularly rotated, and separated between normal writes,
backup, and destructive administration.

### Backup and restore

Crab manifest history protects against logical ref/manifest mistakes while the
same object store remains healthy. It is not an independent backup: history
and current data share the bucket and account failure domain.

1. Enable provider object versioning and, where required, retention/object
   lock before onboarding repositories.
2. Replicate or back up the complete repository prefix, including manifests,
   ref journal, packs and indexes, metadb objects, shards, xorbs, and history.
   Backing up only the current manifest is insufficient.
3. Keep a copy in a separate account or failure domain with separately
   administered delete credentials.
4. At least monthly, restore the backup into an isolated bucket/prefix. Clone,
   run `crab recover history list`, verify a sampled historical generation,
   run `crab fsck`, and hydrate representative large files. Measure the result
   against the declared RPO and RTO.
5. Before production GC, require a recent successful restore drill and run
   `crab gc --scope repo --dry-run`. Never use bucket-wide GC in a shared
   bucket without the repository registry and an explicit change window.

### Monitoring schedule

| Frequency | Check | Alert condition |
|-----------|-------|-----------------|
| Every minute | Object-store node health, request error/latency, disk capacity, replication/heal backlog | Any unavailable node, sustained 5xx/timeout rate, capacity threshold, or stalled repair |
| Every push | Push exit status and structured ref outcomes | Any `internal`, `unpack-failed`, `missing-object`, or repeated conflict beyond retry policy |
| Hourly | `git ls-remote`/read probe from an independent runner | Advertised refs differ from the collaboration-plane policy or cannot be read |
| Daily | `crab doctor`, `crab fsck`, and backup freshness | Unrepaired error, expired backup, lock/admission saturation, or unexpected request amplification |
| Weekly | `crab recover history verify <generation>` on a rotating sample | Digest, Git connectivity, shard, xorb, or index verification failure |
| Monthly | Isolated backup restore drill | RPO/RTO miss or non-byte-identical hydration |

`crab stat perf` is repository-local and cumulative. Collect it after pushes if
you use it for cost trends; it is not a central metrics exporter. Correlate its
upload, resume-probe, and metadb-flush counters with provider billing and
RustFS/OpenTelemetry request metrics. Before committing to object storage as a
team's Git backbone, run the concurrent-push RustFS harness with the intended
writer count. Its push-bracketed HTTP-attempt and live-inventory snapshots give
the request-class and storage inputs for the provider's current price sheet;
repeat against the production provider because latency, retries, LIST paging,
minimum object sizes, and transfer rates are topology-specific.

### Current verification limits

- `crab fsck` validates manifest-selected pack/index presence and Crab's
  shard/xorb chain, but its Git-object check does not currently reconstruct the
  repository and run full Git connectivity. Historical
  `crab recover history verify` does run strict Git fsck for a selected root.
- The generic object-store API does not expose remote multipart enumeration,
  so `crab fsck --repair` cannot currently discover or abort provider-side
  abandoned multipart uploads. Configure a provider lifecycle rule for
  incomplete uploads.
- Locator or visibility acceleration damage is reported rather than rebuilt by
  `fsck`; use `crab metadb rebuild` and verify again.
- Cross-ref fan-out publishes immutable per-ref visibility evidence before the
  ref journal commit. The compaction owner combines every writer's evidence and
  publishes the generation proof before advancing the compacted manifest, so
  it does not need sibling pack bodies in its local Git ODB. Evidence-less
  transactions from older clients, failed evidence uploads, or a lost locator
  writer lease remain repair cases. Treat `Git locator coverage is stale` and
  `Git visibility proof unavailable` from `crab doctor --metadb` as
  repair-required, then run `crab metadb rebuild`. Do not use `fsck` success
  alone as proof that these accelerators are current.
- Git locator writers suppress SlateDB's immediate background garbage-collector
  scan and run one foreground collection pass when exact coverage crosses
  each 32-generation boundary. Continue tracking metadb object growth: the
  dependency still performs boundary reads during normal manifest and
  compaction transactions, and collection retains objects younger than five
  minutes.
- Client-side mirror hooks are bypassable and GitHub/GitLab and Crab ref
  updates are not one transaction. Enforce pointer availability in CI and
  alert on ref divergence.
