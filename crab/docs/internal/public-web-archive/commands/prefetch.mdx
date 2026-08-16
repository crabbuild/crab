# Prefetch Profiles

Always-materialized files after clone, IDE-ready working trees, and named
hydration sets for CI and development workflows.

## Overview

A lazy clone gives you pointer stubs instead of full content — great for speed,
but your editor can't open a pointer. Prefetch profiles solve this: you declare
which files should be hydrated automatically so the working tree is usable the
moment a clone finishes.

Profiles live in `.crab/prefetch.toml`, committed alongside the code. The
special `always` profile is auto-hydrated on every clone and protected from
`crab dehydrate --all`. Named profiles (`ci`, `dev`, etc.) are opt-in and
hydrated on demand.

## `.crab/prefetch.toml` Format

The file uses TOML with a `version` key and one or more `[[profile]]` entries:

```toml
version = 1

[[profile]]
name = "always"
paths = [
  "README.md",
  "docs/**/*.md",
  "*.toml",
  "src/**/*.rs",
]

[[profile]]
name = "ci"
paths = [
  "tests/fixtures/small/**",
  "scripts/*.sh",
]

[[profile]]
name = "dev"
paths = [
  "src/**/*.rs",
  "*.toml",
  "Cargo.lock",
  ".vscode/**",
]
```

### Schema

| Key | Type | Required | Description |
|-----|------|----------|-------------|
| `version` | integer | Yes | Schema version. Currently `1`. |
| `[[profile]]` | table array | Yes | One or more profile entries. |
| `profile.name` | string | Yes | Profile identifier. `"always"` is special. |
| `profile.paths` | array of strings | Yes | Glob patterns relative to repo root. |

Paths follow the same glob syntax as manifest files: `*` matches within a
directory, `**` matches across directories, `?` matches a single character.
Blank lines and `#` comments are not supported in TOML arrays — use TOML's
own comment syntax outside the array.

## The `always` Profile

The `always` profile has two special behaviors:

1. **Auto-hydrated on clone.** After `crab clone` finishes, files matching
   the `always` profile are hydrated automatically. No extra flags needed.
2. **Protected from dehydrate.** `crab dehydrate --all` skips files that
   match the `always` profile, keeping them materialized on disk.

This makes `always` the right place for files your editor, IDE, or build
system expects to find on disk immediately: READMEs, config files, source
code you actively edit.

### Example

```toml
[[profile]]
name = "always"
paths = [
  "README.md",
  "*.toml",
  "src/**/*.rs",
]
```

After cloning, `README.md`, all `.toml` files, and all Rust source files are
full content on disk. Everything else remains as pointers.

## Named Profiles

Named profiles define sets of files for specific workflows. They are not
auto-hydrated — you activate them explicitly with `--profile`.

```toml
[[profile]]
name = "ci"
paths = [
  "tests/fixtures/small/**",
  "scripts/*.sh",
]

[[profile]]
name = "ml-train"
paths = [
  "data/train/**/*.parquet",
  "configs/training/*.yaml",
]
```

Hydrate a named profile:

```bash
crab hydrate --profile=ci
```

This is equivalent to running `crab hydrate --manifest` with the profile's
paths as the manifest content. The same shard-batched resolver is used, so
hydrating hundreds of files is fast.

Requesting an unknown profile name produces a clear error:

```
error: prefetch profile 'staging' not found in .crab/prefetch.toml
  available profiles: always, ci, ml-train
```

## CLI Usage

### Hydrate a specific profile

```bash
crab hydrate --profile=ci
```

Hydrates all files matching the `ci` profile's paths. Combines with other
hydrate flags — you can add `--jsonl` for machine-readable progress:

```bash
crab hydrate --profile=ci --jsonl
```

### Clone with auto-hydration

```bash
crab clone crab://my-bucket/my-repo
```

After the clone completes, the `always` profile is auto-hydrated by default.
No extra flags required. The output shows what was hydrated:

```
Cloning into 'my-repo'...
Configuring crab...
Hydrating prefetch profile 'always'...
  README.md                 4 KB  ✓
  Cargo.toml                2 KB  ✓
  src/main.rs               8 KB  ✓
  ... (12 more files)
Clone complete. 15 files hydrated, rest are pointers.
```

### Dehydrate respects the `always` profile

```bash
crab dehydrate --all
```

Files matching the `always` profile are preserved. Everything else is
dehydrated:

```
Dehydrating 42 files...
  models/weights.bin       1.2 GB → 128 B  ✓
  data/eval.safetensors    300 MB → 128 B  ✓
  README.md                skipped (prefetch: always)
  src/main.rs              skipped (prefetch: always)

Dehydrated 40 files, freed 3.1 GB in 0.6s
  Skipped: 2 files (prefetch profile)
```

### Override profile protection

To dehydrate everything including `always`-protected files:

```bash
crab dehydrate --all --ignore-profiles
```

This is useful when you need to reclaim all disk space, for example before
archiving a working copy or on a CI runner that's done with the repo.

## Configuration

### Disable auto-hydration on clone

By default, `crab clone` auto-hydrates the `always` profile. To disable
this (useful on bandwidth-metered connections or when you want full control):

Add to `.crab/config.toml`:

```toml
[hydrate]
auto_prefetch = false
```

With this setting, `crab clone` produces a fully lazy working tree. You can
still hydrate the `always` profile manually:

```bash
crab hydrate --profile=always
```

## Examples

### CI pipeline

A CI job that only needs test fixtures and build scripts:

```toml
# .crab/prefetch.toml
version = 1

[[profile]]
name = "always"
paths = ["README.md", "*.toml", "Cargo.lock"]

[[profile]]
name = "ci"
paths = [
  "src/**/*.rs",
  "tests/**",
  "scripts/*.sh",
]
```

```bash
# In CI
crab clone crab://bucket/repo    # auto-hydrates 'always'
cd repo
crab hydrate --profile=ci          # hydrate test fixtures + scripts
cargo test
```

### IDE / editor setup

Keep source code and config always on disk so your editor works immediately:

```toml
version = 1

[[profile]]
name = "always"
paths = [
  "README.md",
  "*.toml",
  "Cargo.lock",
  "src/**/*.rs",
  ".vscode/**",
  ".editorconfig",
]
```

After cloning, your editor can index and navigate the codebase without
triggering on-demand hydration for every file open.

### Monorepo with multiple services

In a monorepo, different teams need different subsets:

```toml
version = 1

[[profile]]
name = "always"
paths = ["README.md", "*.toml"]

[[profile]]
name = "frontend"
paths = [
  "services/web/**",
  "packages/ui/**",
  "package.json",
  "yarn.lock",
]

[[profile]]
name = "backend"
paths = [
  "services/api/**",
  "packages/shared/**",
  "Cargo.lock",
]

[[profile]]
name = "ml"
paths = [
  "models/**/*.py",
  "configs/training/**",
  "data/eval/**",
]
```

```bash
# Frontend developer
crab clone crab://bucket/monorepo
cd monorepo
crab hydrate --profile=frontend

# ML engineer
crab clone crab://bucket/monorepo
cd monorepo
crab hydrate --profile=ml
```

Each developer hydrates only the slice they work on. The `always` profile
ensures shared config files are available to everyone.

## Related Commands

- [`crab hydrate`](crab-hydrate.md) — materialize pointer files into full content.
- [`crab dehydrate`](crab-dehydrate.md) — replace hydrated files with pointers.
- [`crab clone`](crab-clone.md) — clone with automatic prefetch hydration.
- [`crab config`](crab-config.md) — read/write crab configuration.
- [`crab status`](crab-status.md) — see which files are hydrated vs. pointers.
