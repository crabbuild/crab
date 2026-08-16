---
name: crab-cli-verification
description: Prove a Crab CLI command end to end against a disposable S3-compatible backend. Use when compilation or unit tests are insufficient and a real object-store, Git, cache, mount, workflow, or hydration side effect must be demonstrated.
---

# Crab CLI verification

Prove the user action, the real side effect, and the visible result. A command
that parses or exits zero without changing the intended state is not verified.

## Disposable S3 fixture

Use a local S3-compatible service with a private run prefix. A typical fixture
uses endpoint `http://127.0.0.1:9000`, region `us-east-1`, disposable access
credentials, and a bucket dedicated to verification. Export the backend
settings only in the verification shell and disable metadata-service lookups.

Keep each run under a unique temporary directory with:

- a seed Git checkout;
- deterministic large files and their hashes;
- a clone or consumer checkout;
- command stdout/stderr logs;
- structured reports and a cleanup record.

Never use a bucket-wide destructive operation. Scope every object key and
cleanup action to the unique run prefix.

## Core proof pattern

1. Define the command contract, required starting state, intended side effect,
   and byte-identity condition.
2. Create the remote, initialize a Crab checkout, track a deterministic file,
   add and commit it, and publish it through the real transfer path.
3. Run the command under test in the seed or a fresh consumer.
4. Inspect the command’s own side effect: refs, packs, xorbs, shards,
   metadata, cache files, workflow entries, mount state, or structured output.
5. Clone, hydrate, mount-read, or otherwise consume the affected data and
   compare the result with the recorded source hash.
6. Run the relevant integrity command and record exact commands and reports.

## Command-specific assertions

- Read-only commands must report the expected pointer, hydration, index, or
  storage facts in text and JSON modes.
- Materialization commands must prove size and Blake3 identity before/after.
- Remote mutations must prove changed keys and refs plus a fresh read path.
- Cache operations must show bounded local files or counters changed without
  changing content identity.
- Negative paths must make the failure condition explicit and assert the
  documented error or exit code and absence of a partial mutation.
- Mounts and daemons must be tested through an actual filesystem read/write
  and then cleanly unmounted.

## Closeout

Report the command contract, run identifier and scoped remote prefix, fixture
location, exact commands, observed side effects, byte-identical evidence,
cleanup result, and any proof that was skipped with its reason. Do not print
credentials, authorization headers, or signed URLs.
