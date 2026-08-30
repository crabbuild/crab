# Project Configuration (`crab.toml`)

The `crab.toml` file is a project-level configuration that travels with your
repository. It tells Crab how the repo is configured so collaborators don't
repeat setup steps.

## Synopsis

Place a `crab.toml` file in your repository root. It's generated automatically
by `crab init` and should be committed to version control.

## Full Schema

```toml
version = 1

# Required: Crab Git remote URL
[remote]
url = "crab://my-bucket/my-repo"

# Optional: file patterns to track with Crab
# If omitted, crab setup can auto-detect patterns.
[track]
patterns = ["*.bin", "*.safetensors", "*.parquet", "datasets/**"]

# Optional: hydration behavior on clone/checkout
[hydrate]
# "lazy" = leave as pointers, hydrate on access (default)
# "eager" = hydrate everything immediately
default = "lazy"
# Always hydrate these patterns regardless of default mode
auto_patterns = ["*.py", "*.rs", "*.toml", "README*", "LICENSE*"]

# Optional: mirror mode (GitHub + Crab coexistence)
[mirror]
# Name of the git remote pointing to GitHub/GitLab
origin_remote = "origin"
# Crab remote name (added by crab init --mirror)
crab_remote = "crab"

# Optional: auth hints (rarely needed — credential discovery handles most cases)
[auth]
# Object-store backend for this Crab remote: "s3" | "gcs" | "azure" | "auto"
storage_provider = "s3"

# Optional: shared hydration sets
[prefetch.profiles.always]
paths = ["README.md", "src/**"]

# Optional: shared workflow policy
[workflow]
enabled = true
discover = "root"
parallelism = 4
```

## Sections

### `[remote]` (required)

The only required section. Specifies the Crab Git remote URL. The URL uses the
`crab://<bucket>/<repo-path>` form even when the backing storage is S3.

| Field | Type | Description |
|-------|------|-------------|
| `url` | String | Remote URL (e.g. `crab://bucket/repo`) |

### `[track]` (optional)

Declares which file patterns Crab manages. These patterns are synced to
`.gitattributes` with the `filter=crab` attribute.

| Field | Type | Description |
|-------|------|-------------|
| `patterns` | Array of strings | Glob patterns for files to track (e.g. `["*.bin", "datasets/**"]`) |

If omitted, `crab setup` uses auto-detection to find large files and well-known
binary extensions.

### `[hydrate]` (optional)

Controls hydration behavior when cloning or checking out.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default` | `"lazy"` or `"eager"` | `"lazy"` | Whether to hydrate all files on clone |
| `auto_patterns` | Array of strings | `[]` | Patterns to always hydrate regardless of default mode |

### `[mirror]` (optional)

Configures mirror mode for GitHub/GitLab + Crab coexistence. When present,
`crab init` (re-apply mode) installs hooks and configures the crab remote.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `origin_remote` | String | `"origin"` | Git remote pointing to GitHub/GitLab |
| `crab_remote` | String | `"crab"` | Git remote pointing to Crab storage |

### `[workflow.remotes.<name>]` (optional)

Names workflow remotes. When a workflow output declares `remote: <name>`, the
URL must be a Crab URL and artifact bytes are routed there while stage
manifests and remote cache refs stay on the primary `[remote]` URL. The same
name can also back DVC-style `remote://name/path` external deps or outs; those
aliases are live-hashed when the configured URL expands to HTTP(S), `file://`,
S3, GCS, or Azure storage.

| Field | Type | Description |
|-------|------|-------------|
| `url` | String | Crab URL for artifact xorbs, or a supported external URL/local absolute path for `remote://` aliases |

### `[auth]` (optional)

Explicit credential hints. Rarely needed — Crab's credential discovery chain
finds credentials automatically in most environments. `storage_provider` is
the common field to commit for non-S3 repositories so collaborators use the
same backend.

| Field | Type | Description |
|-------|------|-------------|
| `storage_provider` | String | Object-store backend: `"s3"`, `"gcs"`, `"azure"`, or `"auto"` |
| `provider` | String | Optional auth provider hint |

AWS profile selection is machine-specific. Set it in `.crab/local.toml` with
`crab config set auth.aws_profile <name>`, pass `crab configure --aws-profile
<name>`, or use `AWS_PROFILE`.

## Precedence

Configuration is resolved in this order (highest priority first):

1. **CLI flags** — e.g. `--pattern`, `--eager`, `--mirror`
2. **`crab.toml`** — project-level defaults committed to the repo
3. **Built-in defaults** — lazy hydration, auto-detection for patterns

Example: if `crab.toml` sets `default = "lazy"` but you run
`crab clone --eager`, the clone will hydrate everything.

## `crab.toml` vs `.crab/local.toml`

These two files serve different purposes:

| | `crab.toml` | `.crab/local.toml` |
|---|---|---|
| **Location** | Repo root | `.crab/` directory |
| **Purpose** | Shared project policy | Machine-local settings |
| **Committed** | Yes (travels with repo) | No (`.crab/` is added to `.git/info/exclude`) |
| **Audience** | Collaborators | Crab internals |
| **Contains** | Remote URL, patterns, hydration/prefetch/workflow policy | AWS profile, local cache paths, GC tuning |
| **Edited by** | Users and Crab commands | Users and Crab commands |

Think of `crab.toml` as the "what should this repo do" declaration, and
`.crab/local.toml` as the "how this machine accesses and runs it" settings.
Other uncommitted operational data—staging, caches, journals, and locks—also
lives under `.crab/`. Staging can contain unpublished work and is not a
throwaway cache.

The retired `.crab.toml` filename is not read. Rename it to `crab.toml` and
commit the rename before running Crab commands.

## How It's Used

### By `crab init`

Generated automatically with a `[remote]` section. If it already exists,
`crab init` updates the URL and warns about the change.

### By `crab init` (no URL, re-apply mode)

Reads `crab.toml` and re-applies the configuration: installs the git drivers,
syncs `.gitattributes`, configures remotes, and installs mirror hooks if
`[mirror]` is present.

### By the global git drivers

When `crab install --global` is active and a repo has `crab.toml` but no
`.crab/local.toml`, the filter auto-configures from `crab.toml` on first
activation. This makes "clone from GitHub → files just work" possible.

### By `crab clone`

After cloning, reads `crab.toml` to determine hydration behavior. If
`[hydrate]` specifies eager mode or auto-patterns, those files are hydrated
automatically.

### By `crab adopt`

If no `--pattern` flags are provided, reads `[track].patterns` to determine
which files to convert.

## Examples

### ML Repository

```toml
[remote]
url = "crab://ml-artifacts/my-model"

[track]
patterns = ["*.safetensors", "*.bin", "*.onnx", "datasets/**"]

[hydrate]
default = "lazy"
auto_patterns = ["*.py", "*.yaml", "requirements.txt"]
```

### Monorepo with Large Assets

```toml
[remote]
url = "crab://company-assets/monorepo"

[track]
patterns = ["assets/**/*.psd", "assets/**/*.fbx", "builds/**"]

[hydrate]
default = "lazy"
auto_patterns = ["*.ts", "*.tsx", "*.json", "*.md"]
```

### Mirror Mode (GitHub + Crab)

```toml
[remote]
url = "crab://team-bucket/our-project"

[track]
patterns = ["*.bin", "*.parquet"]

[mirror]
origin_remote = "origin"
crab_remote = "crab"

[hydrate]
default = "lazy"
auto_patterns = ["*.py", "*.rs"]
```

## Related Commands

- [`crab init`](init.md) — generates `crab.toml`
- [`crab doctor`](doctor.md) — validates configuration
- [`crab adopt`](adopting-existing-repos.md) — uses `[track]` patterns
- [`crab clone`](clone.md) — reads `[hydrate]` settings
