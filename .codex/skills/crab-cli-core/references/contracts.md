# Crab CLI contracts

Read this reference before changing a command, diagnosing a data-path issue,
or claiming that a fix is safe.

## Authority order

1. Repository `AGENTS.md` and any nearer scoped guide.
2. The CLI definition and dispatch in `crab/src/main.rs`.
3. The command module and its callers/callees.
4. Tests, snapshots, and the matching guide or design document.
5. Upstream dependency source and documentation for dependency-backed
   behavior.

Do not infer defaults, timing, error shapes, storage paths, or compatibility
from a command name alone.

## Data-path invariants

- Every SlateDB instance is closed on every exit path.
- A ref lock is released after every acquisition, including errors and
  cancellation.
- GC never deletes referenced objects or objects inside the grace period.
- Reconstruction is byte-identical to the original or returns an error.
- Staged xorbs are flushed before a bundle push begins.
- Shard reconstruction terms cover every chunk for the file.
- `chunks_for_file(file_hash)` returns every chunk for that file version.

## Output and errors

- Treat `--json` and `--jsonl` as public machine-readable contracts. Find the
  schema constant and serializer in source before changing fields.
- `--json` is a terminal envelope; `--jsonl` is a stream of event envelopes.
  Do not make a consumer parse human progress text.
- Preserve typed source errors through `CrabError`; do not stringify away the
  source error or invent a second error path.
- Keep human output and structured output behavior aligned for the same command.

## Storage and safety

- Repository staging lives under `.crab/staging/`; the global cache is separate
  and normally rooted under the Crab cache directory.
- Inspect the configured remote and scope before any remote mutation.
- Never use bucket-wide GC as a repository operation. Prefer a repository
  scope and a dry run, then prove the object set affected.
- Treat recovery, tiering, replica repair, force GC, overlay reset, and cache
  deletion as destructive or externally visible operations. Ask for explicit
  confirmation when the user has not already authorized them.

## Verification

- For Rust compilation or tests, set `CARGO_TARGET_DIR` to a dedicated path
  under `/Volumes/Workspace/crabbuild-target/` on every invocation.
- For remote behavior, use the existing `crab-cli-verification` skill and its
  local RustFS fixture when feasible.
- Validate the command's side effect, not only its exit code. For content
  paths, compare the original and reconstructed bytes or their documented
  content hashes.
- Check `git status --short` before and after. Preserve unrelated user changes.
