# crab optimize xorbs

`crab optimize xorbs` rewrites content-addressed xorbs to a target size
and grouping profile for cost and performance optimization. This is not
`crab optimize packs` or `crab repack`, which consolidate Git pack files.

## Profiles

| Profile | Target xorb | Max xorbs/file | Group by | Compression |
|---------|-------------|----------------|----------|-------------|
| `ml` | 256 MiB | — | — | LZ4 |
| `dataset` | 64 MiB | — | — | LZ4 |
| `code` | 16 MiB | — | — | LZ4 |

When `--profile` is omitted, Crab scans the live xorb inventory and selects a
profile from median source-object size:

- p50 > 100 MiB: `ml`
- p50 >= 1 MiB: `dataset`
- otherwise: `code`

## Custom Profiles

Custom profiles live in `.crab/config.toml`:

```toml
[optimize.xorbs.profiles.my-profile]
target_xorb_bytes = 134217728   # 128 MiB
compression = "lz4"
```

Custom profiles live under the same command namespace as `crab optimize xorbs`.

## Usage

Dry-run first:

```bash
crab optimize xorbs --profile ml --dry-run
crab optimize xorbs --profile ml --dry-run --json
```

`--dry-run` lists the live source xorbs, obtains their sizes and storage
classes, and produces an estimate without writing remote objects. The
inventory is disk-backed and bounded; malformed manifests fail closed before
unbounded download or HEAD fan-out.

Apply the rewrite:

```bash
crab optimize xorbs --profile ml --apply
```

Apply writes immutable destination xorbs, records progress in a WAL journal,
verifies source and destination size/hash, and reconciles file-index and shard
metadata through a manifest CAS. Candidate indexes are published before the
manifest becomes visible, and old roots remain protected until reconciliation
completes. If the process is interrupted, rerun with `--resume`; uploaded
immutable objects are safe to reuse and old objects remain eligible for normal
garbage collection.

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

## Structured Output

- `--json` emits `optimize.xorbs.plan` or `optimize.xorbs.event`.
- `--jsonl` emits streaming `optimize.xorbs.event` lines.

## Concurrent Operation Safety

- Two `crab optimize xorbs` runs: second fails with `CRAB-E0332`.
- `crab gc` + `crab optimize xorbs`: `ConcurrentMaintenance [E0333]`.
- `crab push` + `crab optimize xorbs --dry-run`: safe; dry-run performs no writes.
