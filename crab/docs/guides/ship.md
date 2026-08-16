# crab ship

One-shot add + commit + push — stage, commit, and upload in a single command.

## Synopsis

```
crab ship [OPTIONS] -m <MESSAGE> <PATTERNS>...
```

## Description

`crab ship` combines `crab add`, `git commit`, and `crab push` into a single
command. Designed for ML/data workflows where you want files staged, committed,
and uploaded without running three separate commands.

Uses the native `crab push` pipeline for concurrent xorb uploads rather than
`git push`, giving better performance for multi-file pushes.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATTERNS>` | Yes | Glob patterns to ship (e.g. `*.safetensors`, `.`) |

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `-m, --message` | (required) | Commit message |
| `-j, --jobs` | `16` | Maximum concurrent file-processing tasks |
| `--remote` | `origin` | Git remote to push to |
| `-b, --branch` | current branch | Branch to push |
| `--rebase-on-non-fast-forward` | `false` | On a single current-branch non-fast-forward, run `git pull --rebase --autostash` and retry; retry transient push-lock contention in the same loop |
| `--rebase-retry-limit` | `64` | Maximum integration retry attempts |
| `--no-push` | `false` | Skip the push step (just add + commit) |
| `--json` | `false` | Structured JSON output |

## What It Does

1. Runs `crab add <patterns>` (parallel chunking + dedup + staging).
2. Publishes auto-generated `.gitattributes` rules in the same locked Git-index
   update as the pointers, then checked-stages `.gitattributes` and `.crab.toml`.
   If Git rejects metadata staging, ship stops before committing.
3. Runs `git commit -m <message>`.
4. Runs `crab push <remote>` (native concurrent push pipeline).

If no tracking patterns exist in `.gitattributes`, `crab ship` auto-detects
large file extensions and tracks them before staging — the same auto-track
behavior as `crab add`.

## JSON Output

`--json` emits one terminal `ship` envelope. Its payload contains the add
summary, whether a new commit was created, the commit object ID, the optional
push summary, and per-phase timings. Child `add` and `push` operations do not
emit separate envelopes.

## Examples

### Ship model files

```bash
crab ship '*.safetensors' -m "fine-tuned model v2"
```

### Ship everything in the repo

```bash
crab ship . -m "weekly checkpoint"
```

### Ship without pushing (local commit only)

```bash
crab ship '*.bin' -m "WIP: training in progress" --no-push
```

### Ship with higher parallelism

```bash
crab ship -j 16 'data/**' -m "updated training data"
```

### Ship from concurrent agents

```bash
crab ship . -m "agent update" --rebase-on-non-fast-forward
```

This opt-in keeps normal Git push semantics by default. When many agents target
the same current branch and their commits do not conflict, the loser of a
non-fast-forward race rebases on the new remote tip and retries. If the target
ref is temporarily locked by another pusher, the command retries the push until
the same attempt budget is exhausted. When no lock wait is configured, this mode
waits up to 30 seconds inside each push attempt before retrying the whole
command; pass `--lock-wait-secs` to tune that budget. The rebase pull uses
conservative ref-aware pack filtering internally, so large repos avoid
downloading unrelated branch packs when Crab can prove they are irrelevant.

## Error Handling

- If `crab add` fails (no tracked patterns, no matching files), the command
  stops before committing.
- If there's nothing to commit (files already up to date), the command reports
  this and exits cleanly.
- If `crab push` fails (network error, auth issue), the local commit is
  preserved — re-run `crab push` to retry.

## Related Commands

- [`crab add`](add.md) — stage files without committing.
- [`crab push`](../design/push.md) — native concurrent push (standalone).
- [`crab init`](init.md) — initialize a repository.
- [`crab track`](track.md) — manually configure tracking patterns.
