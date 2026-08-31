# crab add

Stage files for crab, bypassing git's serial filter protocol.

## Synopsis

```
crab add [OPTIONS] <PATTERNS>...
```

## Description

`crab add` processes files matching the given glob patterns in parallel:
hashing, content-defined chunking (CDC), staging chunks locally, and writing
pointer files. It is significantly faster than `git add` for many large files
because it bypasses git's serial filter-process protocol and processes files
concurrently.

After chunking and staging, `crab add` writes Crab pointer blobs directly to
Git's object database and publishes them in the index — unless
`--skip-git-add` is specified. The working-tree files remain intact.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `patterns` | Yes | One or more glob patterns to match files (e.g. `*.safetensors`, `models/`) |

## Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--jobs` | `-j` | `16` | Maximum number of concurrent file-processing tasks |
| `--dry-run` | | `false` | Show what would be added without staging or writing pointers |
| `--skip-git-add` | | `false` | Skip the final `git add` step (stage chunks only) |

## How It Works

For each file matching the provided patterns:

1. The file content is streamed in bounded buffers.
2. A Blake3 hash of the full content is computed while streaming.
3. Content-defined chunking (CDC) using gearhash splits the stream into
   variable-size chunks.
4. Each chunk is hashed and written to the local staging area
   (`.crab/staging/`).
5. The ordered chunk sequence is sealed as an immutable
   `xet-gear-v1-64k` recipe and leased to this add batch. Add does not build a
   second full prepared-xorb copy by default; push proves remote membership and
   packs only the unique missing chunks.
6. Large same-size files with matching bounded fingerprints are checked as
   possible duplicates. Crab still hashes the full candidate file before
   reusing a representative's staged chunk layout.
7. Pointer blobs are inserted into Git's index. The pointer contains the file
   hash, chunk count, and total size.

The file descriptor is opened without following symlinks and its identity,
size, and modification time are checked again before publication. Pointer and
`.gitattributes` deltas are applied to a freshly reread Git index under its
lock, so unrelated concurrent index entries are preserved. `--dry-run` performs
discovery only and does not create staging, object, attribute, or index state.
When an indexed pointer appears clean by Git's stat cache, Crab first checks a
per-worktree v1 validation token written only after Crab verified those bytes.
The token binds literal path bytes, exact indexed pointer identity and payload,
mode, full size, and every captured filesystem stat field. A hit skips the
redundant file read, including when Git conservatively marks the matching stat
entry as racy. A missing, stale, or corrupt token falls back to a
descriptor-safe full Blake3 hash before skipping CDC and staging, then refreshes
the token. A racy entry without an exact token takes the same one-time hash
instead of needlessly repeating CDC and staging. Cache hits require
high-resolution Unix change-time semantics; unsupported or coarse stat
implementations keep using the full hash. On a supported filesystem, same-size
replacements with restored mtimes still change ctime and cannot reuse a stale
pointer through the cache. Successful hydration seeds the same proof from the
verified post-rename descriptor stat, making the first subsequent add a cache
hit without trusting a later metadata snapshot.

Only files that match patterns listed in `.gitattributes` with `filter=crab`
are processed. If a file matches the command-line glob but is not tracked by
crab in `.gitattributes`, it is skipped.

## Examples

### Add all safetensors files

```bash
crab add '*.safetensors'
```

### Add files from a specific directory

```bash
crab add 'models/*'
```

### Add with higher parallelism

```bash
crab add -j 16 '*.bin' '*.safetensors'
```

### Dry run to preview what would be staged

```bash
crab add --dry-run '*.bin'
```

Output shows each file that would be processed, its size, and chunk count
without actually modifying anything.

### Stage chunks without running git add

```bash
crab add --skip-git-add '*.bin'
git add models/weights.bin  # manually add later
```

## Output

`crab add` prints a summary after processing:

```
Added 5 files (2.3 GB total, 847 chunks)
  models/weights.bin       1.2 GB  423 chunks
  models/embeddings.bin    800 MB  312 chunks
  data/train.safetensors   300 MB  112 chunks
  ...
```

In dry-run mode, the output is prefixed with `(dry run)` and no files are
modified.

## Prerequisites

- The repository must be initialized with `crab init`.
- Files to be added must match patterns in `.gitattributes` with
  `filter=crab`. Use `crab track '*.bin'` to set this up.

## Performance Tips

- Increase `--jobs` on machines with many cores and fast storage, or reduce it
  on memory-constrained machines.
- For very large files (10+ GB), keep `--jobs` aligned with available disk and
  CPU throughput. Chunking is bounded-memory, but each concurrent file still
  consumes read buffers and staging batches.
- `crab add` is most beneficial when adding many large files at once. For a
  single small file, `git add` through the filter driver is fine.

## Related Commands

- [`crab track`](crab-track.md) — register file patterns for crab tracking.
- [`crab reset`](crab-reset.md) — unstage files and clean staging data.
- [`crab status`](crab-status.md) — see hydration state of tracked files.
- [`crab staging stats`](crab-staging.md) — inspect the staging area.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` streams progress events (one JSON object per line) followed by a
  terminal `result` event.

### crab add --json

```json
{
  "schema": "add",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.500Z",
  "data": {
    "files_staged": 5,
    "files_skipped": 0,
    "validation_cache_hits": 0,
    "validation_cache_hit_bytes": 0,
    "files_failed": 0,
    "chunks_staged": 847,
    "bytes_processed": 2469606195,
    "lock_wait_duration_ms": 2,
    "chunking_worker_duration_ms": 1800,
    "remote_lookup_duration_ms": 120,
    "compression_worker_duration_ms": 2100,
    "payload_write_duration_ms": 640,
    "staging_duration_ms": 3400,
    "planning_duration_ms": 420,
    "flushing_duration_ms": 25,
    "indexing_duration_ms": 80,
    "duration_ms": 3925
  }
}
```

### crab add --jsonl

Each line is a complete JSON object:

```
{"schema":"add.event","version":"1.0","timestamp":"2026-04-24T18:32:17.100Z","type":"progress","data":{"operation":"staging","current":3,"total":5,"bytes":1572864000,"total_bytes":2469606195,"rate_bytes_per_sec":45000000.0}}
{"schema":"add.event","version":"1.0","timestamp":"2026-04-24T18:32:17.450Z","type":"file_done","data":{"path":"models/weights.bin","bytes":1288490188,"duration_ms":340,"status":"ok"}}
{"schema":"add.event","version":"1.0","timestamp":"2026-04-24T18:32:17.500Z","type":"result","data":{"files_staged":5,"files_skipped":0,"validation_cache_hits":0,"validation_cache_hit_bytes":0,"files_failed":0,"chunks_staged":847,"bytes_processed":2469606195,"lock_wait_duration_ms":2,"chunking_worker_duration_ms":1800,"remote_lookup_duration_ms":120,"compression_worker_duration_ms":2100,"payload_write_duration_ms":640,"staging_duration_ms":3400,"planning_duration_ms":420,"flushing_duration_ms":25,"indexing_duration_ms":80,"duration_ms":3925}}
```

The worker-duration fields are cumulative across parallel file tasks and can
therefore exceed the command's wall-clock `duration_ms`.
`lock_wait_duration_ms` covers staging-owner acquisition and publication
recovery before file work starts.
`validation_cache_hits` and `validation_cache_hit_bytes` report clean indexed
files that were accepted from v1 verification tokens without rereading their
contents.

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.

## Pattern Syntax

`crab add` uses git's pathspec syntax for its pattern arguments. Patterns
are parsed by the same engine git uses (`gix-pathspec`), which means:

- `*.bin` — matches files ending in `.bin` anywhere in the tree.
- `models/**` — matches every path under `models/`.
- `**/*.bin` — matches `.bin` files at any depth (but not in the repo root;
  the leading `**` requires at least one directory separator).
- `:(exclude)*.tmp` — "pathspec magic" prefix that excludes paths from the
  result. Equivalent to git's `git add -- ':(exclude)*.tmp'`.
- `:(glob)models/**.bin` — force glob semantics on the remainder of the
  pathspec, useful when you want `**` to span separators explicitly.
- `:(icase)README.md` — case-insensitive match; the rest of the pathspec
  behaves as usual.

Pathspec magic is enabled when crab is built with the `gix-pathmatch`
feature (the default once the flag flips default-on). You can combine magic
prefixes as git does — `:(exclude,icase)` excludes case-insensitively.

### Why pathspec instead of globset

Before the consolidation, four separate glob engines inside crab produced
subtly different answers for the same pattern. A file at `dir/model.bin`
matched `*.bin` under the clean filter but not under `crab add`'s
walker. The pathspec engine used by git is the single source of truth now,
so `crab add`, `crab hydrate`, `crab dehydrate`, `crab status`, the
clean filter, and the filter-process classifier all agree on whether any
given path is tracked.

One consequence: the narrow rule that `**/*.bin` must have at least one
directory prefix is now honored consistently. Use a bare `*.bin` if you
want to match files in the repo root as well.

## Pattern syntax

`crab add` compiles every glob through `gix-pathspec`, which matches
git's pathspec grammar when the `gix-pathmatch` feature is enabled
(default-on in recent builds). That means you can use the same
selector syntax you'd use with `git ls-files` or `git grep`:

- `*.bin` — matches any `.bin` file in any directory.
- `models/**` — matches everything under `models/`.
- `models/*.safetensors` — matches top-level entries of `models/` only.
- `:(exclude)*.tmp` — excludes `.tmp` files from the result set even if
  another selector would pull them in.
- `:(glob)docs/**/*.md` — forces the `**` to be interpreted as "across
  path separators".
- `:(icase)*.BIN` — case-insensitive match.

Multiple selectors compose, so
`crab add 'models/**' ':(exclude)models/archive/**'` stages every
file under `models/` except the archive.

### .gitignore handling

The walker consults `.gitignore` and `.git/info/exclude` through
`gix-ignore` so files that git would skip are also skipped by
`crab add`. Nested `.gitignore` files are honored with the usual
closer-wins precedence. `.gitattributes` remains the gate for tracking
via `filter=crab` — `.gitignore` does not opt files *out* of a
crab filter; it only keeps the walker from descending into build
artifacts or vendored trees.

### Pathspec-magic support

`gix-pathspec` understands pathspec magic natively, so crab supports
the subset gitoxide exposes: `:(exclude)`, `:(glob)`, `:(icase)`,
`:(top)`. Attribute-based magic (`:(attr:foo=bar)`) is accepted at
parse time but currently has no effect on the match — crab doesn't
consult attribute values during pathspec matching. This lines up with
the "matches git's behavior" user story while keeping the walker
cheap.

Historical note: before the `gix-pathmatch` consolidation, each crab
command (`add`, `hydrate`, `dehydrate`, `status`, plus `clean` /
`filter_process`) had its own hand-rolled glob matcher with slightly
different semantics. A file at `dir/model.bin` matched `*.bin` under
the clean filter but not under `crab add`. With the consolidation
they all share one `gix_pathspec::Search` and one
`gix_attributes::Search`, so tracking decisions are consistent across
the CLI surface.
