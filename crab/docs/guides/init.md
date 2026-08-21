# crab init

Initialize a new crab repository at a remote URL.

## Synopsis

```
crab init [OPTIONS] <url>
```

## Description

`crab init` sets up a new crab-managed repository by connecting a local
directory to a cloud object storage backend. It creates the `.crab/`
configuration directory, writes the remote URL, installs the git filter and
diff drivers, and prepares the repo for `crab setup`.

If no git repository exists in the current directory, `crab init` automatically
runs `git init` — no need to initialize git separately.

After init, run `crab setup` to scan the working tree for large files and write
the matching `.gitattributes` rules.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `url`    | Yes      | Remote URL to initialize (e.g. `crab://my-bucket/my-repo`) |

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--storage-provider <provider>` | `auto` | Storage backend: `s3`, `gcs`, `azure`, or `auto` |
| `--gc-list-profile <profile>` | `adaptive` | Local bucket-GC policy: `adaptive`, `cost`, or `latency` |
| `--mirror <remote>` | — | Configure mirror mode with an existing Git remote |
| `--log-level` | — | Set log verbosity |

## URL Format

Crab URLs follow the pattern:

```
crab://<bucket>/<repo-path>
```

- `<bucket>` — the bucket or container name for your selected storage backend.
- `<repo-path>` — a path prefix within the bucket that isolates this repository's data. Can be nested (e.g. `org/team/repo`).

Use the `crab://` scheme for Git remotes. `crab init` also accepts
provider-prefixed repository URLs and normalizes them to `crab://` in local
config, `.crab.toml`, and the Git remote:

```bash
crab init s3://my-s3-bucket/my-repo
crab init gs://my-gcs-bucket/my-repo
crab init gcs://my-gcs-bucket/my-repo
crab init azure://my-container/my-repo
```

After init, use the printed `crab://...` URL with Git and Crab commands. Git
would otherwise look for helpers such as `git-remote-s3` instead of Crab's
`git-remote-crab` helper.

Use `--storage-provider` to choose the object-store backend:

```bash
crab init --storage-provider s3    crab://my-s3-bucket/my-repo
crab init --storage-provider gcs   crab://my-gcs-bucket/my-repo
crab init --storage-provider azure crab://my-container/my-repo
```

Bucket administrators can also choose the local GC listing policy during
initial setup:

```bash
crab init --gc-list-profile adaptive crab://my-bucket/my-repo
```

`adaptive` uses one low-cost recursive stream for smaller namespaces and
switches to parallel hash partitions after a bounded provider-aware probe.
`cost` always minimizes LIST streams. `latency` immediately scans populated
partitions concurrently. This preference is stored only in
`.crab/config.toml`; it does not alter the bucket-global object layout and can
safely differ between operators.

For Azure, the URL host is the Blob container; the storage account comes from
Azure credentials, user config, or environment variables.

## What It Does

1. Runs `git init` if no `.git` directory exists.
2. Validates the provided URL.
3. Creates the `.crab/` directory in the repository root.
4. Writes the remote URL to `.crab/remote`.
5. Creates `.crab/config.toml` with `[remote]` and optional `[auth]` sections.
6. Registers the crab git drivers in the local `.git/config`:
   - `filter.crab.process` — the long-running filter process command.
   - `filter.crab.clean` — the clean filter fallback.
   - `filter.crab.smudge` — the smudge filter fallback.
   - `filter.crab.required = true` — ensures git fails if the filter is unavailable.
   - `diff.crab.command` — the external diff driver for `diff=crab` files.
7. Prints the next `crab setup` and `crab ship` steps.

## Auto-Tracking

By default, `crab setup` scans the working tree and tracks extensions that meet
either criterion:

- **Size threshold**: Any file above 1 MiB triggers tracking for its extension.
- **Well-known binary formats**: Extensions like `.safetensors`, `.bin`, `.onnx`,
  `.pt`, `.h5`, `.parquet`, `.arrow`, `.fbx`, `.blend`, `.mov`, `.mp4`, `.db`,
  `.tar`, `.gz`, `.zip` are always tracked when found.

To disable this behavior:

```bash
crab setup --no-auto-track
```

## Examples

### Initialize a new project (no git repo needed)

```bash
mkdir my-project
cd my-project
crab init --storage-provider s3 crab://my-bucket/my-project
# Initialized git repository in /home/user/my-project
# Remote 'origin' → crab://my-bucket/my-project
# Storage provider → s3 (Amazon S3 or S3-compatible)
# Next: crab setup
```

### Initialize an existing git repo

```bash
cd my-existing-repo
crab init --storage-provider s3 crab://my-bucket/my-repo
crab setup
```

### Re-initialize (idempotent)

Running `crab init` again on an already-initialized repository is safe. It
updates the git drivers and local config without overwriting tracking rules.
Run `crab setup` again whenever you want to rescan for new large-file patterns.

```bash
crab init --storage-provider s3 crab://my-bucket/my-project  # first time
crab init crab://my-bucket/my-project                        # safe to repeat
```

## Prerequisites

- The `crab` binary must be on your `PATH`.
- Cloud credentials must be configured for the selected storage backend.

Note: `git init` is no longer a prerequisite — `crab init` handles it
automatically.

## Configuration Files Created

| File | Purpose |
|------|---------|
| `.git/` | Git repository (created if missing) |
| `.crab/remote` | Stores the remote URL |
| `.crab/config.toml` | Local crab configuration |
| `.git/config` (modified) | Filter and diff driver registration |
| `.crab.toml` | Committed project configuration |

## Related Commands

- [`crab install`](install.md) — install the git drivers without initializing a remote.
- [`crab clone`](clone.md) — clone a repository with automatic crab setup.
- [`crab track`](track.md) — manually add tracking patterns.
- [`crab ship`](ship.md) — one-shot add + commit + push.
- [`crab doctor`](doctor.md) — verify the crab setup is healthy.

## Troubleshooting

**"invalid URL" error**
Ensure the URL follows the `crab://<bucket>/<path>` format. The bucket name
must not contain slashes.

For S3, `crab://my-bucket/my-repo` is the correct Crab remote URL. Configure
AWS credentials separately with `AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY`, web identity, ECS task credentials, or EC2 instance
credentials. The current provider does not read `AWS_PROFILE` or shared AWS
configuration files.

For GCS or Azure, keep the `crab://` URL and add `--storage-provider gcs` or
`--storage-provider azure`.

**Filter driver not registered**
If `git config --local filter.crab.process` or
`git config --local diff.crab.command` returns empty after init, check
that the crab binary is accessible. Run `crab doctor` for a full health
check.
