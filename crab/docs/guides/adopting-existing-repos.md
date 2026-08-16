# Adopting Existing Repositories

If your repository already has large files committed directly in git, `crab adopt`
converts them to Crab pointer blobs without requiring a full history rewrite.

## The Problem

You have a repo with 5 GB of model files committed as regular git blobs. Clones
are slow, `.git` is bloated, and branches are painful. You want Crab's chunking
and deduplication, but the files are already in history.

## The Solution: `crab adopt`

`crab adopt` converts large files in your working tree to pointer blobs and
stages the original content as xorbs in your cloud bucket. By default it only
affects the current HEAD — no history rewriting, no force-push needed.

## Migration Flow

### Step 1: Preview (dry-run)

```bash
crab adopt --dry-run
```

Output:
```
Would convert 12 files (4.7 GB total):

PATH                              SIZE      EXT
models/bert-large.safetensors     1.3 GB    .safetensors
models/gpt2-medium.bin            1.1 GB    .bin
datasets/train.parquet            890 MB    .parquet
datasets/eval.parquet             650 MB    .parquet
assets/texture-pack.zip           420 MB    .zip
...

Would convert 12 files (4.7 GB total)
```

This shows exactly what would be converted without modifying anything.

### Step 2: Adopt

```bash
crab adopt
```

This:
1. Scans the working tree for files matching tracked patterns
2. Chunks each file using CDC (content-defined chunking)
3. Stages chunks to the local staging area
4. Replaces file content with pointer blobs (~128 bytes each)
5. Updates `.gitattributes` with new tracking patterns
6. Stages all changes for commit (but does NOT commit)

### Step 3: Review and Ship

```bash
# Review what changed
git diff --cached

# Commit and push
crab ship -m "adopt large files into crab"
```

## Pattern Resolution

`crab adopt` determines which files to convert using this priority:

1. **Explicit `--pattern` flags**: `crab adopt --pattern "*.bin" --pattern "datasets/**"`
2. **`.crab.toml` `[track]` section**: if no flags provided, reads patterns from config
3. **Auto-detection**: if no config either, scans for files >1 MiB and well-known binary extensions

## HEAD-Only Mode (Default)

The default mode only affects the current working tree and the next commit.
Historical commits retain the original large blobs.

**Pros:**
- Safe — no history rewriting, no force-push
- Fast — only processes current files
- Non-disruptive — collaborators don't need to re-clone
- Reversible — `git checkout -- .` undoes it before committing

**Cons:**
- `.git` directory retains historical large blobs (still bloated)
- `git clone` still downloads historical blobs (slow first clone)
- Only new commits benefit from Crab's deduplication

**When to use:** Most cases. Especially when the repo is shared and you can't
force-push, or when historical bloat is acceptable.

## History-Rewrite Mode

For repos where you need to eliminate historical bloat entirely:

```bash
crab adopt --rewrite-history --force
```

**Requirements:**
- `--force` flag is mandatory (safety gate)
- Working tree must be clean (no uncommitted changes)
- `git-filter-repo` must be installed

**What it does:**
1. Rewrites all commits, replacing matching blobs with pointer blobs
2. Stages original content as xorbs
3. Produces a new history where large files were never committed inline

**After rewriting:**
```bash
git push --force-with-lease origin main
```

**Pros:**
- Eliminates historical bloat completely
- Future clones are fast (only pointer blobs in history)
- Full deduplication across all historical versions

**Cons:**
- Rewrites shared history — all collaborators must re-clone or `git fetch --all && git reset --hard origin/main`
- Requires `--force-push` to remote
- Cannot be undone once pushed
- Requires `git-filter-repo` installed

**When to use:** Only for repos where you control all collaborators and can
coordinate a re-clone, or for repos that haven't been shared yet.

## Examples

### Adopt with explicit patterns

```bash
crab adopt --pattern "*.safetensors" --pattern "*.bin"
```

### Adopt using `.crab.toml` patterns

```bash
# .crab.toml has [track] patterns = ["*.bin", "datasets/**"]
crab adopt
```

### Adopt with auto-detection

```bash
# No patterns specified, no .crab.toml — scans for large files
crab adopt
```

### Dry-run with JSON output

```bash
crab adopt --dry-run --json
```

### Parallel processing

```bash
crab adopt -j 16  # use 16 threads for chunking
```

## Complete Migration Workflow

For a team migrating an existing repo to Crab:

```bash
# 1. Initialize Crab
crab init crab://my-bucket/my-repo

# 2. Preview what would be adopted
crab adopt --dry-run

# 3. Adopt large files (HEAD-only, safe)
crab adopt

# 4. Review staged changes
git diff --cached --stat

# 5. Ship the conversion
crab ship -m "migrate large files to crab"

# 6. Tell collaborators to re-init
# They run: crab init (reads .crab.toml, sets up filter)
```

## Related

- [Project Configuration](project-config.md) — `[track]` patterns used by adopt
- [Getting Started](getting-started.md) — basic setup flow
- [`crab init`](init.md) — initialize before adopting
- [`crab ship`](ship.md) — commit after adopting
