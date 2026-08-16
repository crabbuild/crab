# crab optimize xorbs

`crab optimize xorbs` rewrites content-addressed xorbs to a target size
and grouping profile for cost and performance optimization. This is not
`crab optimize packs` or `crab repack`, which consolidate Git pack files.

## Profiles

| Profile | Target xorb | Max xorbs/file | Group by | Compression |
|---------|-------------|----------------|----------|-------------|
| `ml` | 256 MiB | 4 | File | Zstd(3) |
| `dataset` | 64 MiB | unlimited | Directory | Zstd(5) |
| `code` | 16 MiB | unlimited | Hash | Zstd(9) |

When `--profile` is omitted, Crab scans the file index and selects a
profile from median file size:

- p50 > 100 MiB: `ml`
- p50 >= 1 MiB: `dataset`
- otherwise: `code`

## Custom Profiles

Custom profiles live in `.crab/config.toml`:

```toml
[restripe.profiles.my-profile]
target_xorb_bytes = 134217728   # 128 MiB
max_xorbs_per_file = 8
group_by = "file"
compression = "zstd:5"
```

The config section still uses `[restripe.profiles]` because it stores
the xorb rewrite engine profile configuration.

## Usage

Dry-run first:

```bash
crab optimize xorbs --profile ml --dry-run
crab optimize xorbs --profile ml --dry-run --json
```

Apply the rewrite:

```bash
crab optimize xorbs --profile ml --apply
```

Resume an interrupted run:

```bash
crab optimize xorbs --resume
```

Abort and clean up:

```bash
crab optimize xorbs --abort
crab gc
```

Drop a corrupt or abandoned journal:

```bash
crab optimize xorbs --drop-journal --yes-really
```

## Tier-Aware Optimization

Archive-class source xorbs are restored before processing when included:

- `--include-cold=false`: skip archive xorbs.
- `--restore-tier=<tier>`: restore tier for archive sources.
- `--output-class=<class>`: storage class for destination xorbs.

## Structured Output

- `--json` emits `optimize.xorbs.plan` or `optimize.xorbs.event`.
- `--jsonl` emits streaming `optimize.xorbs.event` lines.

## Concurrent Operation Safety

- Two `crab optimize xorbs` runs: second fails with `CRAB-E0332`.
- `crab gc` + `crab optimize xorbs`: `ConcurrentMaintenance [E0333]`.
- `crab push` + `crab optimize xorbs`: safe; reconciliation leaves concurrent-push xorbs untouched.
