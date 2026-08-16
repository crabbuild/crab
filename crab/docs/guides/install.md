# crab install / crab uninstall

Install or remove the crab git drivers in git config.

## Synopsis

```
crab install [OPTIONS]
crab uninstall [OPTIONS]
```

## Description

`crab install` registers the crab filter driver, diff driver, and git hooks in your git
configuration. This is what enables git to route crab-tracked files through
the crab clean/smudge pipeline during checkout/commit and the chunk-level diff
driver during `git diff`.

`crab uninstall` removes the driver configuration and hooks.

Normally you don't need to run these commands directly — `crab init` and
`crab clone` handle installation automatically. Use `crab install` when you
need to repair a broken configuration or install at a different scope.

## Install Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--global` | | `false` | Install globally (all repos for this user) |
| `--system` | | `false` | Install system-wide (all users) |
| `--force` | `-f` | `false` | Overwrite existing configuration |
| `--skip-smudge` | | `false` | Skip the smudge filter (defer hydration) |

Without `--global` or `--system`, installation is local to the current
repository.

## Uninstall Options

| Option | Default | Description |
|--------|---------|-------------|
| `--global` | `false` | Remove from global config |
| `--system` | `false` | Remove from system config |

## What Install Does

### Git Config

Sets the following in the appropriate git config scope:

```ini
[filter "crab"]
    process = crab filter-process
    clean = crab filter-process
    smudge = crab filter-process
    required = true

[diff "crab"]
    command = crab diff-driver
```

With `--skip-smudge`:

```ini
[filter "crab"]
    process = crab filter-process
    clean = crab filter-process
    smudge = cat
    required = true

[diff "crab"]
    command = crab diff-driver
```

The `--skip-smudge` option is useful for CI environments where you want fast
checkouts without hydrating files. Files remain as pointers until explicitly
hydrated.

### Git Hooks

Installs a `pre-push` hook that ensures staged chunks are uploaded before the
push completes. If a pre-push hook already exists, crab appends its line
rather than overwriting.

## Installation Scopes

| Scope | Flag | Config File | Applies To |
|-------|------|-------------|------------|
| Local | (default) | `.git/config` | Current repository only |
| Global | `--global` | `~/.gitconfig` | All repositories for the current user |
| System | `--system` | `/etc/gitconfig` | All users on the system |

## Examples

### Install locally (default)

```bash
cd my-repo
crab install
```

### Install globally

```bash
crab install --global
```

### Install with skip-smudge for CI

```bash
crab install --skip-smudge
```

### Force reinstall

```bash
crab install --force
```

### Uninstall from local config

```bash
crab uninstall
```

### Uninstall from global config

```bash
crab uninstall --global
```

## Verifying Installation

After installing, verify with:

```bash
git config --local filter.crab.process
# Should output: crab filter-process
git config --local diff.crab.command
# Should output: crab diff-driver
```

Or run `crab doctor` for a full health check.

## Related Commands

- [`crab init`](crab-init.md) — initialize a repository (includes install).
- [`crab clone`](crab-clone.md) — clone a repository (includes install).
- [`crab doctor`](crab-doctor.md) — verify the installation is healthy.
- [`crab env`](crab-env.md) — print git driver configuration.
