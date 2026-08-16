# crab import

Create a Crab-backed git repository from the contents of an existing
object-storage bucket.

## Synopsis

```
crab import [<source-url-or-local-path>] [--from <source-url>] [--to <target-url>]
              [--bucket <bucket> --name <repo>]
              [--into <dir>]
              [--dest-prefix <path>]
              [--include <glob>...] [--exclude <glob>...]
              [--branch <name>] [--message <text>]
              [--track <glob>...]
              [--versions {auto|on|off}]
              [--window <duration>]
              [--at <rfc3339>]
              [--since <rfc3339>] [--until <rfc3339>]
              [--author-template <template>]
              [--dry-run] [--estimate] [--resume]
              [--jobs <N>] [--fail-fast] [--force] [--yes]
              [--source-profile <name>] [--target-profile <name>]
              [--json | --jsonl]
```

## Overview

`crab import` turns a raw object-storage prefix full of files into a
fresh Crab-backed git repository. The source objects stay in place
(they are read, not moved), xorbs and shards are written under the
target prefix, and the local `<into>` directory becomes a cloneable
git repo whose history reflects the bucket.

For quick imports, the first positional argument can be either a raw
storage URL or a local filesystem path. `--bucket <bucket> --name
<repo>` builds the target `crab://<bucket>/<repo>` URL for you.

Two modes, auto-detected:

- **Flat** — bucket has no object versioning. One commit reflects the
  current state.
- **Versioned** — bucket has versioning enabled. Each time window
  (default 1 hour) becomes one commit; delete markers surface as git
  deletions.

When `--from` and `--to` resolve to the same physical bucket, source
object bytes are **not re-uploaded**. Xorbs still land in the target
`.crab/` layout so `crab hydrate` works post-import.

## URL rules

| Side      | Allowed schemes                                  | Meaning                                |
|-----------|--------------------------------------------------|----------------------------------------|
| `--from`  | `s3://`, `gs://`, `az://`/`azure://`, `file://`  | Raw object prefix. Not a Crab repo.  |
| `--to`    | same raw scheme as `--from` **or** `crab://`   | Crab repo to create.                 |

`crab://` on `--from` errors with `CRAB-E0118`. A raw `--to`
scheme that disagrees with `--from` errors with `CRAB-E0119`;
cross-cloud imports must write the target as `crab://`.

Plain local paths are accepted as source shorthand and normalized to
`file://` internally:

```bash
crab import ./large-files --bucket crab --name import-demo
```

Use `--dest-prefix` when the source prefix should appear under a
directory inside the imported Git tree:

```bash
crab import \
  s3://crab/crab/large-files \
  --bucket crab \
  --name import-demo \
  --dest-prefix crab/large-files
```

With that mapping, source key `large-20gb.bin` becomes Git path
`crab/large-files/large-20gb.bin`.

## Recipes

### Same-bucket onboarding

Source and target sit in the same bucket under different prefixes.
Source bytes stay put; the target prefix grows a `.crab/` layout.

```bash
crab import \
  --from s3://my-bucket/datasets/v2/ \
  --to   s3://my-bucket/repos/v2
```

Post-import the local `./v2` directory has pointer blobs, a `main`
branch, and `origin` pointing at the target URL. The source objects
at `s3://my-bucket/datasets/v2/` are untouched.

### Cross-bucket onboarding (raw `--to`)

Same cloud, different buckets. Matching raw schemes make the
transport explicit.

```bash
crab import \
  --from s3://data-lake-prod/models/ \
  --to   s3://crab-repos/models-v1
```

### Cross-cloud onboarding (`crab://` target)

Reading from one cloud and writing to another. The target must be
written as `crab://` so the cross-cloud intent is explicit; the
target provider comes from config.

```bash
crab import \
  --from s3://source-bucket/corpus/ \
  --to   crab://azure-backed-repo/corpus
```

### Versioned-bucket history import

`--versions auto` (the default) detects versioning. Versioned buckets
get one commit per time window on the target branch.

Cloud history import uses the provider's native version listing API:
S3 `ListObjectVersions`, GCS `objects.list` with `versions=true`,
and Azure Blob `List Blobs` with `include=versions,deleted`. The
import then reads each selected object version by provider version
ID, so Git commits are built from the historical bytes rather than
the current object contents.

```bash
# Auto-detect; one commit per hour (default window)
crab import \
  --from s3://versioned-bucket/prod/ \
  --to   s3://versioned-bucket/repos/prod

# Require versioning; fail if the bucket isn't versioned
crab import \
  --from s3://versioned-bucket/prod/ \
  --to   s3://versioned-bucket/repos/prod \
  --versions on

# Coarser commits — one per day for a mostly-static bucket
crab import \
  --from s3://versioned-bucket/prod/ \
  --to   s3://versioned-bucket/repos/prod \
  --window 24h
```

If `--versions on` runs against a flat bucket the import errors with
`CRAB-E0120`. If the resulting commit count would exceed
`--max-commits` (default 100 000) the import errors with
`CRAB-E0121`; widen `--window` and retry.

### `--at <timestamp>` single-snapshot import

Pin the tree to a specific instant. Exactly one commit lands whose
tree contains each key's newest live version at or before the
timestamp. Works against flat or versioned buckets.

```bash
crab import \
  --from s3://versioned-bucket/prod/ \
  --to   s3://versioned-bucket/repos/prod-at-release \
  --at   2025-06-01T00:00:00Z
```

### `--since` / `--until` range filter

Restrict the versioned history imported into the repo to a date
range. Versions outside the range are skipped; the first commit
represents the state at `--since`.

```bash
crab import \
  --from s3://versioned-bucket/prod/ \
  --to   s3://versioned-bucket/repos/prod-last-quarter \
  --since 2025-04-01T00:00:00Z \
  --until 2025-07-01T00:00:00Z
```

`--since > --until` errors with `CRAB-E0122`.

### Resume after interruption

Import is resumable. A crash at hour four of a six-hour ingest
leaves `<into>/.crab/import-journal.db` on disk with every
per-object state. Re-run with `--resume`:

```bash
# Resume using only the journal — no --from / --to flags needed
crab import --into ./v2 --resume

# Or pass the same flags; the journal verifies plan equivalence
crab import \
  --from s3://my-bucket/datasets/v2/ \
  --to   s3://my-bucket/repos/v2 \
  --into ./v2 \
  --resume
```

Drifting between the original args and the resume args (different
`--include`, `--branch`, `--window`, etc.) errors with
`CRAB-E0114`. Running `--resume` against a directory without a
journal errors with `CRAB-E0115`. The journal is dropped only on
a full pipeline success.

### Dry-run with manifest preview

`--dry-run` runs detect → enumerate → window plan and prints the
plan without touching the target. No journal is left on disk.

```bash
crab import \
  --from s3://my-bucket/datasets/v2/ \
  --to   s3://my-bucket/repos/v2 \
  --dry-run
```

Output includes: file count, total source bytes, extension
histogram, LFS-pointer count, detected versioning mode, planned
commit count, and same-bucket status.

### Native LFS-format source import

If the source prefix holds a `.gitattributes` file declaring
`filter=lfs` for any path, the source is an LFS-format tree — the
objects under those paths are LFS pointer blobs, not their
underlying content. Crab supports three explicit modes:

```bash
crab import --lfs-source fail    # default safety mode
crab import --lfs-source skip    # omit LFS pointer paths
crab import --lfs-source resolve --lfs-objects s3://bucket/lfs/objects
```

`resolve` verifies each LFS object's SHA-256 against the pointer OID before
chunking it as Crab-native large-file content. `skip` imports non-LFS entries
and reports skipped pointer counts. Resume journals include the selected LFS
mode and resolved object identity, so stale resume attempts are rejected.

See [Native LFS Import](native-lfs-import.md) for the capability status and
operator notes.

## Resume workflow

```
first run  ──────────► interrupted (network, signal, crash)
                       │
                       │  .crab/import-journal.db remains
                       ▼
resume     ──────────► crab import --into <dir> --resume
                       │
                       ▼
                   full pipeline success → journal removed
```

Key points:

- The journal is a SQLite database in WAL mode. It tolerates
  mid-operation crashes without corruption.
- `Pending` and `Failed` rows retry on resume. `Staged` and
  `Skipped` rows are honored as-is.
- The plan checksum includes source/target URLs, filters, branch,
  versioning mode, window, history range, and `--dest-prefix`. A
  subset of the original args (just `--jobs` / `--fail-fast`) is
  always valid.

## Troubleshooting

- **Nothing matches the filters.** The import errors out rather
  than creating an empty repo. Relax or drop the `--include` /
  `--exclude` globs.
- **Target directory not empty.** `CRAB-E0123`. Point `--into`
  at an empty directory, clean out the existing one, or pass
  `--force`. An empty, freshly-initialized git repo with zero
  commits is always accepted.
- **Target already has a repo.** `CRAB-E0111`. Pass `--force`
  to overwrite `origin`, or remove the existing `origin` remote
  first.
- **Source prefix is already a Crab repo.** `CRAB-E0112`.
  Use `crab clone` instead — or pass `--force` if you really
  mean to re-import the raw bytes.
- **Source collides with target layout.** `CRAB-E0117`. The
  source prefix overlaps the target `.crab/` path in the same
  bucket. Pick non-overlapping prefixes. No `--force` override.
- **Large-import confirmation.** Imports over 1 M objects or
  1 TiB prompt interactively in text mode and require `--yes`
  in `--json` / `--jsonl` modes.
- **Missing git identity.** `CRAB-E0116`. Configure with
  `git config --global user.name 'Your Name'` and
  `git config --global user.email 'you@example.com'`, or pass
  `--author-template`.

## Related errors

| Code           | Summary                                                 |
|----------------|---------------------------------------------------------|
| `CRAB-E0111` | Import target repo already has an origin remote        |
| `CRAB-E0112` | Import source prefix is already a Crab repo          |
| `CRAB-E0113` | Import source uses Git LFS format                      |
| `CRAB-E0114` | Import plan mismatch (resume args disagree)            |
| `CRAB-E0115` | Import journal missing (`--resume` without a journal)  |
| `CRAB-E0116` | Git identity not configured for import                 |
| `CRAB-E0117` | Import source prefix collides with target layout       |
| `CRAB-E0118` | Import source must be a raw cloud URL                  |
| `CRAB-E0119` | Import source and target schemes disagree              |
| `CRAB-E0120` | Import versioning unavailable                          |
| `CRAB-E0121` | Import commit ceiling exceeded                         |
| `CRAB-E0122` | Import history range is invalid (`--since` > `--until`)|
| `CRAB-E0123` | Import target directory is not empty                   |

Look up long-form remediation with:

```bash
crab errors CRAB-E0121
```

## Related commands

- [`crab clone`](crab-clone.md) — clone an existing Crab repo
  (use this if the source is already a Crab repo).
- [`crab init`](crab-init.md) — initialize an empty Crab repo.
- [`crab hydrate`](crab-hydrate.md) — materialize pointer blobs
  into full file content after cloning the imported repo.

## JSON output

Supports `--json` (single terminal summary) and `--jsonl` (streams
`enumerate.event` / `stage.event` / `assemble.event` /
`publish.event` followed by a final `import.summary`).

See [Structured Output](structured-output.md) for envelope details,
event types, and error handling. Field-level docs for the
`ImportSummary` schema live next to the command — `schemars`
generates them from the Rust type.
