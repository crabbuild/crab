# crab clone

Clone a crab repository in one step.

## Synopsis

```
crab clone [OPTIONS] <url> [directory]
```

## Description

`crab clone` wraps `git clone` with automatic crab git driver setup, lazy
checkout configuration, and optional post-clone hydration. It replaces the
manual sequence of `git clone` → `crab init` → `crab hydrate`.

By default, clones are lazy: files are checked out as lightweight pointer stubs
rather than full content. This makes cloning multi-GB repositories nearly
instant when most repository weight is in Crab-managed payloads. You can then
selectively hydrate only the files you need. Lazy checkout is not Git partial
clone: the wrapper does not request `--filter=blob:none`, so it installs the
complete Git packs while deferring Crab-managed payload bytes.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `url` | Yes | Remote URL (e.g. `crab://bucket/repo`) |
| `directory` | No | Target directory (defaults to the repo name extracted from the URL) |

## Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--branch` | `-b` | | Branch to check out after cloning |
| `--depth` | | | Shallow clone depth (number of commits) |
| `--lazy` | | `true` | Leave files as pointers (default). Use `--no-lazy` for full hydration |
| `--include` | | | Glob patterns to hydrate immediately after clone |
| `--exclude` | | | Glob patterns to exclude from post-clone hydration |

## How It Works

1. Runs `git clone` with the crab filter driver injected via `--config` flags.
2. Creates the `.crab/` directory for local checkout settings.
3. Registers the filter and diff drivers in the cloned repo's local git config.
4. If `--lazy` (default), configures lazy checkout so files remain as pointers.
5. If `--include` patterns are provided, runs selective hydration on matching
   files after clone.
6. If `--no-lazy`, all files are fully hydrated during checkout.

For lazy clones, `git clone --no-checkout` is used first, then lazy mode is
configured, and finally `git checkout HEAD` populates the working tree with
pointer stubs.

After checkout, an existing `crab.toml` remains authoritative and byte-identical.
If the selected revision has none, Crab creates a minimal `crab.toml` with the
explicit clone URL before post-clone hydration. This makes later `crab hydrate`
and configured LFS reads resolve the same storage location without a temporary
environment override. The new file is not staged or committed automatically;
review it before adding it to the project. No endpoint credentials or resolved
managed placement are stored in it.

A malformed existing project file is an error, not permission to replace its
policy. Ordinary Git URLs and local Git paths still delegate to Git without
creating Crab configuration. This initialization is specific to `crab clone`;
an existing unconfigured checkout needs `crab configure <REMOTE>`.

When a committed project file names a different primary than the clone URL,
that primary still controls payload hydration. To deliberately relocate the
project, clone lazily, run `crab configure <REMOTE> --no-auto-track` in the new
checkout, review its staged project configuration, then hydrate. Clone does
not silently rewrite committed storage policy.

## Examples

### Basic lazy clone (default)

```bash
crab clone crab://my-bucket/my-repo
```

Files are pointer stubs. Hydrate selectively later:

```bash
cd my-repo
crab hydrate '*.safetensors'
```

### Clone into a specific directory

```bash
crab clone crab://my-bucket/my-repo ./local-copy
```

### Clone a specific branch

```bash
crab clone --branch feature/experiment crab://my-bucket/my-repo
```

### Shallow clone

```bash
crab clone --depth 1 crab://my-bucket/my-repo
```

### Clone with selective hydration

Hydrate only model files immediately, leave everything else as pointers:

```bash
crab clone --include '*.safetensors' --include '*.bin' crab://my-bucket/my-repo
```

### Clone with exclusions

Hydrate everything except large training data:

```bash
crab clone --include '*' --exclude 'data/train/*' crab://my-bucket/my-repo
```

### Full (non-lazy) clone

```bash
crab clone --no-lazy crab://my-bucket/my-repo
```

All files are fully hydrated during checkout. This can be slow for large repos.

## Output

```
Cloning into 'my-repo'...
Configuring crab...
Clone complete (lazy). Files are pointer stubs.
Hydrate selectively:  crab hydrate '*.safetensors'
Hydrate everything:   crab hydrate --all
```

When `--include` patterns are provided:

```
Cloning into 'my-repo'...
Configuring crab...
Hydrating matching files...
Clone complete. Matched files hydrated, rest are pointers.
```

## Prerequisites

- The `crab` binary must be on your `PATH`.
- `git` must be installed (version 2.27+ recommended).
- AWS credentials must be configured for the target bucket.

## Performance Tips

- Lazy clones are nearly instant regardless of repository size — only git
  packs and pointer files are downloaded; Crab-managed payload bytes remain
  remote until hydration.
- Use `--include` to hydrate only the files you need right away.
- Combine `--depth 1` with `--lazy` for the fastest possible clone.
- After cloning, use `crab fetch` to pre-warm the cache for files you plan
  to hydrate later.

## Git feature support

Direct S3-compatible remotes support ordinary clone, fetch, shallow clone,
deepen/unshallow, lazy pointer checkout, full hydration, and connectivity
checks. A missing, malformed, or unreadable canonical layout descriptor or
manifest is an error and clone stops. New empty repositories exist only after
`crab init` publishes the generation-0 manifest.

Shallow traversal uses a bounded remote commit-graph summary. If a requested
tip or deepen operation is outside that retained window, Crab safely downloads
the complete Git pack set and removes the shallow boundary instead of treating
truncated metadata as complete history.

Depth-based shallow operations (`--depth`, `--deepen`, and `--unshallow`) are
supported. Git's date and ref-exclusion selectors (`--shallow-since` and
`--shallow-exclude`) are not yet supported and fail explicitly; Crab does not
silently replace either request with a full clone.

The current development-line helper can run a proof-gated partial clone with
the supported `--filter=...` forms: `blob:none`, `blob:limit=<n>[kmg]`,
`tree:<depth>`, `object:type={tag,commit,tree,blob}`, full-SHA-1
`sparse:oid`, and bounded repeated/combine intersections. It records the
standard Git promisor state and fetches omitted blobs lazily. RustFS
qualification is green, while provider and released-artifact qualification
remain; do not treat this as a released support claim yet. If the remote lacks
the generation-bound locator or visibility proof, the v2 capability is withheld
and the filtered operation fails rather than silently becoming a complete
clone. See [Git Integration](../architecture/git-integration.md) for the exact
semantics and unsupported v2 extensions.

## Related Commands

- [`crab init`](crab-init.md) — initialize crab in an existing repository.
- [`crab hydrate`](crab-hydrate.md) — materialize pointer files into full content.
- [`crab dehydrate`](crab-dehydrate.md) — replace hydrated files with pointers.
- [`crab status`](crab-status.md) — see which files are hydrated vs. pointers.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` streams clone and hydration progress followed by a terminal
  `result` event.

### crab clone --json crab://my-bucket/my-repo

```json
{
  "schema": "clone",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:35.200Z",
  "data": {
    "url": "crab://my-bucket/my-repo",
    "directory": "my-repo",
    "lazy": true,
    "files_hydrated": 0,
    "duration_ms": 4500
  }
}
```

### crab clone --jsonl crab://my-bucket/my-repo

```
{"schema":"clone.event","version":"1.0","timestamp":"2026-04-24T18:32:31.000Z","type":"progress","data":{"operation":"cloning","current":0,"total":1,"bytes":0,"total_bytes":0,"rate_bytes_per_sec":0.0}}
{"schema":"clone.event","version":"1.0","timestamp":"2026-04-24T18:32:35.200Z","type":"result","data":{"url":"crab://my-bucket/my-repo","directory":"my-repo","lazy":true,"files_hydrated":0,"duration_ms":4500}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
