# crab config

Manage crab configuration.

## Synopsis

```
crab config get <key>
crab config set <key> <value>
```

## Description

`crab config` reads and writes Crab settings. Most keys are local settings in
`.crab/config.toml`; repository-sharing keys, such as `auth.storage_provider`,
are written to `.crab.toml` so collaborators inherit them.

## Subcommands

### crab config get

Read a configuration value.

```bash
crab config get <key>
```

If the key exists, its value is printed to stdout. If the key does not exist,
nothing is printed (exit code 0).

### crab config set

Write a configuration value.

```bash
crab config set <key> <value>
```

Creates the key if it doesn't exist, or overwrites the existing value.

## Key Format

Keys use dotted notation: `<section>.<key>`. For example:

- `checkout.lazy` — boolean, whether to use lazy checkout.
- `hydrate.include` — array, default include patterns for hydration.
- `hydrate.exclude` — array, default exclude patterns for hydration.

## Value Types

| Type | Examples | Notes |
|------|----------|-------|
| Boolean | `true`, `false` | Case-insensitive |
| String | `us-east-1` | Stored as-is |
| Enum | `recursive`, `split` | Validated by the key being set |
| Array | `*.bin` | Appended to existing array values for `include`/`exclude` keys |

For array keys (`include`, `exclude`), each `set` call appends to the array
rather than replacing it.

## Examples

### Check if lazy checkout is enabled

```bash
crab config get checkout.lazy
```

```
true
```

### Enable lazy checkout

```bash
crab config set checkout.lazy true
```

### Set the repository storage backend

```bash
crab config set auth.storage_provider gcs
```

This writes the committed `.crab.toml` project config, not just local state.

### Set default hydration patterns

```bash
crab config set hydrate.include '*.safetensors'
crab config set hydrate.include '*.bin'
crab config set hydrate.exclude 'archive/*'
```

### Enable the workflow layer

```bash
crab config set workflow.enabled true
crab config set workflow.discover recursive
crab config set workflow.lockfile split
```

### Read a missing key

```bash
crab config get nonexistent.key
# (no output)
```

## Configuration Files

Local configuration lives at `.crab/config.toml` in the repository root. It uses
TOML format:

```toml
[checkout]
lazy = true

[hydrate]
include = ["*.safetensors", "*.bin"]
exclude = ["archive/*"]

[workflow]
enabled = true
discover = "recursive"
lockfile = "split"
```

Project configuration lives at `.crab.toml` and is safe to commit. Use it for
settings the whole team must share, such as `auth.storage_provider`.

## Available Configuration Keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `auth.storage_provider` | string | `auto` | Object-store backend: `s3`, `gcs`, `azure`, or `auto` |
| `checkout.lazy` | bool | `false` | Leave files as pointers on checkout |
| `hydrate.include` | array | `[]` | Default include patterns for `crab hydrate` |
| `hydrate.exclude` | array | `[]` | Default exclude patterns for `crab hydrate` |
| `hydrate.auto` | bool | `false` | Automatically hydrate matching files |
| `workflow.enabled` | bool | `false` | Enable workflow, stage, experiment, metric, plot, and queue commands |
| `workflow.discover` | enum | `root` | Discover only root `crab.yaml`, or `recursive` for nested `crab.yaml` and `*.workflow.yaml` files |
| `workflow.lockfile` | enum | `single` | Use a single `crab.lock`, or `split` for per-workflow lockfiles |
| `workflow.parallelism` | int | `4` | Maximum concurrent stage executions unless overridden by `--parallelism` |
| `workflow.graceful_shutdown_timeout_secs` | int | `10` | Seconds between terminating and killing an over-time stage process |
| `workflow.max_outs_per_stage` | int | `10000` | Maximum declared outputs per stage |
| `workflow.max_out_bytes` | int | `1099511627776` | Maximum bytes per declared output |
| `workflow.lock_timeout_secs` | int | `600` | How long a second workflow run waits for the scheduler lock |
| `workflow.remote_cache_readonly` | bool | `false` | Reject workflow cache pushes while allowing cache reads |
| `hydra.enabled` | bool | `false` | Compose Hydra config groups for experiment parameter overrides |
| `hydra.config_dir` | string | `conf` | Hydra config root used by experiment runs |
| `hydra.config_name` | string | `config.yaml` | Hydra root config file name |
| `download_concurrency` | int | | Number of concurrent downloads |
| `max_retries` | int | | Maximum retry attempts for transient errors |
| `operation_timeout` | string | | Timeout for remote operations |
| `repack_auto_threshold` | int | | Pack count threshold for auto-repack |

## Related Commands

- [`crab init`](init.md) — creates the initial config file.
- [`crab env`](env.md) — print current configuration state.
- [`crab doctor`](doctor.md) — verify configuration is valid.

## JSON Output

`crab config get` supports `--json`.

```bash
crab config get checkout.lazy --json
```

```json
{
  "schema": "config.get",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "key": "checkout.lazy",
    "value": "true",
    "source": "local"
  }
}
```

The `source` field indicates where the value came from: `"default"`, `"env"`,
`"local"`, or `"remote"`.

See [Structured Output](structured-output.md) for envelope details and error handling.
