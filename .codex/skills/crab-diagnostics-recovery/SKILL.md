---
name: crab-diagnostics-recovery
description: Diagnose Crab installations and plan or apply repository recovery. Use whenever a user mentions `crab doctor`, environment or error reports, logs, support bundles, missing or corrupt objects, `crab fsck` evidence, `crab recover`, historical roots, or restoring files and remote content after data loss.
compatibility: Crab CLI with access to the repository, recovery inventories, and any authorized backup or replica sources.
---

# Crab diagnostics and recovery

Start with evidence and a read-only diagnosis. Recovery is a typed, planned
operation: identify what is repairable, what is inventory-only, and what is
unrecoverable before writing files or remote objects.

## Command scope

`doctor`, `env`, `errors`, `logs`, `version`, `update`, `recover plan/show/apply`
and `recover history` subcommands. Use `crab-storage-ops` for ordinary fsck,
GC, metadata rebuild, or compaction; this skill consumes their reports when
planning recovery.

## Diagnostic loop

1. Capture `crab version`, `crab env`, config origin, Git state, relevant logs,
   and the exact command/error code. Redact credentials, tokens, signed URLs,
   and private service endpoints.
2. Run the appropriate `doctor` mode. Distinguish a configuration problem,
   local cache problem, metadata/index problem, remote availability problem,
   and missing source bytes.
3. Prefer structured output and retain the JSON/JSONL report. Do not use a
   human log phrase as a stable contract.
4. For recovery, assemble the release manifest and inventories from the
   documented sources: local files, workflow/import journals, pointer roots,
   shard/xorb/pack lists, file-index data, replicas, and fsck JSONL.
5. Run `recover plan`, inspect every candidate's identity and action, and save
   the plan. Do not apply a plan that has not been reviewed.
6. Apply only the explicitly authorized actions: restore verified files,
   rebuild the file index, restore shards/xorbs/packs, or repair selected refs
   through the canonical push path. Verify hashes before and after each action.
7. Re-run doctor/fsck, inspect the audit event, and prove a fresh clone or
   hydrate for repaired content. Report unrecoverable and inventory-only items
   separately from successful repairs.

## Historical recovery

Read the history-recovery command and repository-recovery guide before
inspecting immutable roots. Preserve the distinction between a historical
manifest being discoverable, its metadata being intact, its bytes being
available, and a remote repair being successfully published.

## Operational boundaries

- `doctor --support-bundle` must be redacted; do not include raw credentials.
- `doctor --cache-service-active-probe` is an opt-in write/read/cleanup probe;
  confirm the target service and scope.
- `update` is an installation change, not a diagnostic workaround. Use the
  release skill for published release operations.
- Never invent bytes from metadata. A successful pointer parse is not data
  recovery.

## Read first

- `crab/docs/guides/doctor.md`
- `crab/docs/guides/env.md`
- `crab/docs/guides/errors.md`
- `crab/docs/guides/logs.md`
- `crab/docs/guides/recovery.md`
- `crab/docs/guides/repository-recovery.md`
- `crab/src/cmd/{doctor,env,errors,logs,version,update,recover,history_recovery}.rs`
- `crab/src/core/{error,error_catalog}.rs`
- `.codex/skills/crab-cli-core/references/contracts.md`
