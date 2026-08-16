# crab export

Export one resolved Crab snapshot as raw, materialized files in object
storage.

## Synopsis

```
crab export <repo> --to <raw-url> [paths...]
crab export --from <repo> --to <raw-url> [paths...]

Options:
  --revision <rev>        Branch, tag, ref, or commit (default: HEAD)
  --include <glob>        Include repo-relative paths matching a glob
  --exclude <glob>        Exclude repo-relative paths matching a glob
  --cache-dir <dir>       Override the Crab cache root
  -j, --jobs <n>          File-level export concurrency
  --dry-run               Plan without writing target objects
  --force                 Overwrite existing target objects
  --quiet                 Suppress human progress and summary
  --json | --jsonl        Structured output
```

`<repo>` and `--from` accept a local repo path, `file://`, or
`crab://`. `--to` accepts only raw object-storage URLs: `s3://`,
`gs://`, `az://` / `azure://`, or `file://`.

## Overview

`crab export` is the inverse of `crab import`: it reads a single
snapshot from a Crab repository and writes the selected files to a raw
object-storage prefix. The exported objects contain normal file bytes,
not Git history, packs, Crab pointer files, or a Crab repo backup.

Exported object names preserve repo-relative paths:

```
repo path:  crab/large-files/model.bin
--to:       s3://crab/export-demo/
object:     s3://crab/export-demo/crab/large-files/model.bin
```

There is no path stripping or flattening in v1.

## Selection

Use positional paths to select exact files or subtree prefixes ending in
`/`:

```bash
crab export crab://crab/import-demo \
  --to s3://crab/export-demo \
  crab/large-files/
```

Use `--include` and `--exclude` for glob filtering:

```bash
crab export crab://crab/import-demo \
  --to s3://crab/export-demo \
  --include 'crab/large-files/**/*.bin' \
  --exclude '**/*.tmp'
```

If no paths and no includes are provided, the full snapshot is exported.

## Collision policy

By default, export preflights each target object with `HEAD`. If any
target object already exists, the command fails before writing files.
Pass `--force` to overwrite existing target objects.

This is a safety rail, not a distributed lock. Concurrent writers can
still race the export. For repeatable automation, write to a fresh prefix
or use an externally coordinated prefix.

## Large files

Large files stream from the snapshot reader into multipart object-store
writes. Crab pointer files are reconstructed through shard hydration, Git
LFS pointer files stream from the LFS object store with SHA-256
verification, and regular Git blobs stream directly from Git object
contents.

`--jobs` controls file-level concurrency. Each file still writes in fixed
multipart chunks, so export does not buffer an entire large file in
memory.

## Dry-run

Preview the selected files and target keys without writing:

```bash
crab export crab://crab/import-demo \
  --to s3://crab/export-demo \
  crab/large-files/ \
  --dry-run
```

Dry-run marks each file as `would_export` or `would_conflict`.

## Structured output

`--json` emits one `export.summary` envelope. `--jsonl` emits:

- `export.plan` before file writes start.
- `export.file` once for each selected file.
- `export.summary` as the final result.

Per-file statuses are:

- `exported`
- `would_export`
- `would_conflict`

## RustFS round trip

With a local RustFS S3-compatible service configured for the `crab`
bucket and `crab` / `crab` credentials:

```bash
crab import \
  --from s3://crab/crab/large-files \
  --to crab://crab/import-demo \
  --dest-prefix crab/large-files

crab export crab://crab/import-demo \
  --to s3://crab/export-demo \
  crab/large-files/ \
  -j 4
```

Verify object sizes or hashes with your S3-compatible client after the
export completes.
