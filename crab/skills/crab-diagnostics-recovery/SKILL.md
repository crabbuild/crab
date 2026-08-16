---
name: crab-diagnostics-recovery
description: Diagnose Crab installations and plan or apply repository recovery. Use for doctor, environment or error reports, logs, support bundles, missing or corrupt objects, fsck evidence, historical roots, and restore operations.
---

# Crab diagnostics and recovery

Start read-only. Recovery is a typed, reviewed plan: distinguish repairable
state, inventory-only evidence, unavailable bytes, and irrecoverable loss.

## Diagnostic loop

1. Capture version, environment, config origin, Git state, relevant logs, and
   the exact command and error code. Redact credentials, tokens, signed URLs,
   and private endpoints.
2. Run the appropriate `doctor` mode and classify the failure as configuration,
   local cache, metadata/index, remote availability, authentication, or missing
   source bytes.
3. Prefer structured output and save the JSON/JSONL report. Human log phrases
   are clues, not automation contracts.
4. Gather recovery inventory from release manifests, pointer roots, workflow or
   import journals, shard/xorb/pack listings, file indexes, replicas, and fsck
   reports. Mark each source and confidence level.
5. Run `recover plan`, inspect each candidate identity and action, and retain
   the plan before applying anything.
6. Apply only explicitly authorized actions: restore verified files, rebuild an
   index, restore objects, or repair selected refs through the canonical path.
7. Re-run doctor and integrity checks, inspect the audit event, and prove a
   fresh clone or hydration for repaired content.

## Command boundaries

- `env`, `errors`, `logs`, and `version` collect evidence or explain stable
  diagnostics; they should not mutate repository data.
- Support bundles must be redacted. Active cache-service probes are opt-in
  write/read/cleanup tests and require a clearly bounded target.
- `update` changes the installed binary; it is not a recovery workaround.
- History recovery can discover immutable roots without making their bytes
  readable. Keep discoverability, metadata integrity, byte availability, and
  remote repair as separate statuses.

## Recovery invariants

- Never invent bytes from metadata or a pointer.
- Verify size and content identity before replacing a file or publishing an
  object.
- Do not advance refs until the repaired object closure is complete.
- Keep original evidence and a rollback path until verification finishes.
- Report successful, skipped, inventory-only, and unrecoverable items
  separately.

## Verification

Exercise a missing cache object, corrupt shard, incomplete pointer staging,
expired credential, unavailable replica, interrupted apply, and a successful
restore. Assert stable error codes, no partial ref advance, redacted output,
and byte-identical recovery from a clean consumer.
