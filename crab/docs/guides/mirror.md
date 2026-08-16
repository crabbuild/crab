# crab mirror

Mirror a Git repository into a Crab remote backed by object storage.

## Synopsis

```bash
crab mirror [OPTIONS] <SOURCE> <DESTINATION>
```

## Description

`crab mirror` copies the full Git namespace from any Git remote or local Git
repository into a `crab://` remote. It preserves commit SHAs and refs exactly by
using Git's mirror clone, fetch, and push behavior. It does not rewrite history
or convert repository contents into Crab-native tracked files.

On later runs, the same command syncs the existing mirror. Crab updates the
bare cache from `SOURCE`, compares the cache refs with `DESTINATION`, and skips
the Git push when the refs already match. In that no-op case Crab also skips the
LFS scan by default.

Relative local source paths are resolved from the invocation directory before
the persistent cache is created, so later cache updates keep using the same
repository regardless of the cache directory's location.

When refs changed, Crab mirrors Git LFS payloads before publishing refs. It
fetches LFS objects for changed source tips and uploads only the changed LFS
object IDs that are missing from `DESTINATION`.

`crab clone --eager` detects those LFS pointers and configures Crab's LFS
transfer agent before checkout, so mirrored LFS files are materialized from the
destination object store during the first checkout. Lazy clones retain pointer
files until `crab lfs pull` or a later LFS checkout.

This is different from `crab init --mirror`, which configures a normal
developer repository to keep GitHub or GitLab for code review while Crab stores
large-file content.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--cache-dir <DIR>` | Crab cache root | Exact bare mirror cache directory to use |
| `--no-atomic` | `false` | Push refs without Git's `--atomic` option |
| `--skip-lfs` | `false` | Skip Git LFS object mirroring |
| `--force-lfs-check` | `false` | Verify all LFS objects even when refs are already in sync |
| `--json` | `false` | Emit one structured result envelope |
| `--jsonl` | `false` | Emit a JSONL stream with the terminal result |

## Behavior

On the first run, Crab creates a persistent bare mirror cache with:

```bash
git clone --mirror -- <SOURCE> <CACHE>
```

On later runs, Crab updates that cache with:

```bash
git remote set-url origin <SOURCE>
git remote update --prune origin
```

It then points a `crab` Git remote at `DESTINATION` and compares source refs
with destination refs:

```bash
git show-ref
git ls-remote --refs crab
```

If the refs match, Crab skips the Git push. Pass `--force-lfs-check` to run a
full LFS verification anyway.

If refs differ, Crab mirrors changed LFS objects unless `--skip-lfs` is set,
then pushes Git refs and objects with:

```bash
git push --mirror --atomic crab
```

`--mirror` is destructive by design: destination refs that no longer exist in
the source are deleted.

## Examples

Mirror GitHub to Crab:

```bash
crab mirror https://github.com/org/repo.git crab://my-bucket/mirrors/org/repo
```

Reuse an explicit cache directory:

```bash
crab mirror \
  --cache-dir /var/tmp/crab-repo-mirror.git \
  git@gitlab.com:org/repo.git \
  crab://my-bucket/mirrors/org/repo
```

Mirror Git refs only:

```bash
crab mirror --skip-lfs https://github.com/org/repo.git crab://my-bucket/org/repo
```

Repair or verify LFS content without changing already-mirrored refs:

```bash
crab mirror --force-lfs-check https://github.com/org/repo.git crab://my-bucket/org/repo
```

## Requirements

- `git` must be installed.
- `git-remote-crab` must be on `PATH`; run `make install` or `crab install` if
  Git cannot find the Crab remote helper.
- `git-lfs` must be installed unless `--skip-lfs` is used.

## Related Commands

- [`crab clone`](clone.md) - clone a Crab repository.
- [`crab lfs`](lfs.md) - inspect or transfer Git LFS objects.
- [`crab init --mirror`](mirror-mode.md) - configure GitHub/GitLab coexistence
  for a developer repository.
