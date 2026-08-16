# Mirror Mode (GitHub + Crab Coexistence)

Mirror mode lets you keep GitHub (or GitLab) for code review and pull requests
while Crab handles large-file storage transparently. Code goes to GitHub,
large files go to your cloud bucket — your team's PR workflow stays unchanged.

## How It Works

```
┌─────────────┐     git push origin     ┌──────────────┐
│  Developer  │ ──────────────────────── │    GitHub    │
│  Workstation│                          │  (code only) │
└──────┬──────┘                          └──────────────┘
       │
       │  pre-push hook: crab push --remote crab
       │
       ▼
┌──────────────┐
│  S3 Bucket   │
│ (large files)│
└──────────────┘
```

When you `git push origin`:
1. The `pre-push` hook fires first
2. It pushes large-file xorbs to the Crab remote
3. Then git pushes pointer blobs to GitHub

When a collaborator pulls from GitHub:
1. Git delivers pointer blobs
2. The `post-checkout`/`post-merge` hook fires
3. It hydrates pointers from the Crab remote

## Setup: Repository Owner

```bash
cd my-project

# Initialize with mirror mode (origin must already exist)
crab init --mirror=origin crab://my-bucket/my-repo
```

This does:
- Adds a `crab` git remote pointing to your bucket
- Installs `pre-push`, `post-checkout`, and `post-merge` hooks
- Writes `[mirror]` section to `.crab.toml`
- Auto-tracks large file patterns

Commit and push:

```bash
crab ship -m "enable crab for large files"
git push origin main
```

The `.crab.toml` is now on GitHub. Collaborators will inherit the mirror config.

## Setup: Collaborator

After cloning from GitHub:

```bash
git clone git@github.com:team/project.git
cd project
crab init
```

Running `crab init` with no URL detects the `[mirror]` section in `.crab.toml`
and automatically:
- Adds the `crab` remote
- Installs the mirror hooks
- Hydrates files according to `[hydrate]` settings

## Hook Behavior

### `pre-push` (runs before `git push origin`)

```bash
#!/bin/sh
# Crab mirror: push xorbs before refs go to origin
crab add . --skip-git-add 2>/dev/null
crab push --remote crab --quiet 2>/dev/null
```

Ensures all large-file content is in the bucket before pointer blobs reach
GitHub. If the crab push fails, the git push is aborted (preventing dangling
pointers on GitHub).

### `post-checkout` (runs after `git checkout`, `git switch`, `git clone`)

```bash
#!/bin/sh
# Crab mirror: hydrate pointer files after checkout
crab hydrate . --quiet 2>/dev/null || true
```

Materializes pointer files so you see real content after switching branches.
Failures are non-fatal (you can always hydrate manually).

### `post-merge` (runs after `git pull`, `git merge`)

```bash
#!/bin/sh
# Crab mirror: hydrate after merge/pull
crab hydrate . --quiet 2>/dev/null || true
```

Same as post-checkout but triggers on pulls and merges.

### Existing Hooks

Crab appends its hook lines to existing hooks rather than overwriting them.
If you already have a `pre-push` hook, Crab's lines are added at the end.
Installation is idempotent — running `crab init` again won't duplicate the
hook content.

## Checking Mirror Status

```bash
crab status
```

When mirror mode is active, `crab status` shows an additional section:

```
Mirror: origin ↔ crab (crab://my-bucket/my-repo) | healthy
  Crab remote: reachable
  Pending push: 0 files
```

## `.crab.toml` Mirror Section

```toml
[mirror]
origin_remote = "origin"    # your GitHub/GitLab remote
crab_remote = "crab"        # the Crab storage remote
```

## When to Use Mirror Mode

Mirror mode is ideal when:
- Your team uses GitHub/GitLab for code review and CI
- You have large files (models, datasets, assets) that don't belong in git
- You want the PR workflow unchanged — reviewers see pointer diffs, not binary blobs
- You need deduplication and lazy checkout for large files

If you don't need GitHub/GitLab integration (e.g. Crab is your only remote),
standard `crab init <url>` without `--mirror` is simpler.

## Troubleshooting

**Hooks not firing**

Check that hooks are installed:
```bash
cat .git/hooks/pre-push | grep "Crab mirror"
```

If missing, re-run `crab init` to reinstall them.

**Hydration fails after pull**

Verify the crab remote is configured and credentials are valid:
```bash
git remote get-url crab
crab doctor
```

**Large files showing as pointers**

Run hydration manually:
```bash
crab hydrate .
```

## Related

- [Project Configuration](project-config.md) — `.crab.toml` reference
- [Getting Started](getting-started.md) — basic setup without mirror mode
- [`crab status`](status.md) — includes mirror health reporting
