# Structured Output (`--json` / `--jsonl`)

Crab supports machine-readable output for automation, scripting, and UI
integration. Two modes are available:

- `--json` — emit a single JSON object with the full result.
- `--jsonl` — emit a stream of newline-delimited JSON objects (progress events
  followed by a terminal result).

Default (text) output is unchanged. Structured output is opt-in.

## Mode Selection

| Flag | Use Case | Output |
|------|----------|--------|
| _(none)_ | Human terminal | Text + progress bars on stderr |
| `--json` | Single result | One JSON object on stdout |
| `--jsonl` | Streaming progress | One JSON object per line on stdout |

`--json` and `--jsonl` are mutually exclusive. Where `--porcelain` exists
(e.g. `crab status`), it also conflicts with `--json`.

For streaming commands (`add`, `hydrate`, `push`, etc.), `--json` suppresses
all intermediate events and emits only the final result.

## Envelope Format

Every `--json` response is wrapped in a consistent envelope:

```json
{
  "schema": "status",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "total_tracked": 1420,
    "hydrated": 312,
    "pointer": 1108,
    "modified": 2,
    "files": [
      { "path": "models/bert.bin", "state": "hydrated", "bytes": 440401920 }
    ]
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `schema` | string | Canonical command name. Dots for subcommands: `"daemon.list"`, `"lfs.status"`. |
| `version` | string | `"<major>.<minor>"`. Minor bumps add optional fields; major bumps are breaking. |
| `timestamp` | string | RFC 3339 UTC, millisecond precision: `"YYYY-MM-DDTHH:MM:SS.mmmZ"`. |
| `data` | object | Command-specific payload. Present on success. |
| `error` | object | Present on failure (see [Error Envelope](#error-envelope)). Mutually exclusive with `data`. |


## Error Envelope

When a command fails in `--json` or `--jsonl` mode, the envelope contains
`error` instead of `data`. The process exits with a non-zero code.

```json
{
  "schema": "hydrate",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.456Z",
  "error": {
    "code": "CRAB-E0001",
    "category": "transient",
    "message": "network transient error: connection reset by peer",
    "retryable": true,
    "details": {},
    "source_chain": [
      { "message": "connection reset by peer" },
      { "message": "broken pipe" }
    ]
  }
}
```

### Error Fields

| Field | Type | Description |
|-------|------|-------------|
| `code` | string | Stable error code (`CRAB-E####`). See `crab errors` for the full catalog. |
| `category` | string | One of: `transient`, `conflict`, `integrity`, `permanent`, `config`, `transport`, `staging`, `lfs`, `internal`, `cancelled`. |
| `message` | string | Human-readable error message. |
| `retryable` | bool | `true` for `NetworkTransient`, `Throttled`, `CasConflict`, `StagingLocked`. |
| `details` | object\|null | Structured fields from the error variant (snake_case). `null` for variants with no extra data. |
| `source_chain` | array | Cause chain from `std::error::Error::source()`, capped at depth 8. Empty if no cause. |

### Error Categories

| Category | Meaning | Example Codes |
|----------|---------|---------------|
| `transient` | Temporary failure, retry likely helps | `E0001`, `E0002` |
| `conflict` | Concurrent write conflict | `E0010`, `E0011`, `E0012`, `E0017` |
| `integrity` | Data corruption detected | `E0020`, `E0021`, `E0082`–`E0084` |
| `permanent` | Unrecoverable without user action | `E0030`, `E0031`, `E0040`–`E0043` |
| `config` | Configuration or format error | `E0050`–`E0052` |
| `transport` | I/O, protocol, or storage error | `E0060`, `E0070`, `E0071`, `E0110` |
| `staging` | Local staging area issue | `E0080`, `E0081` |
| `lfs` | Git LFS-specific error | `E0100`–`E0105` |
| `internal` | Bug in crab | `E0099` |
| `cancelled` | User cancelled (SIGINT) | `E0090` |

### Exit Codes

Exit codes are preserved from the existing `CrabError::exit_code()` mapping:

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General / user error |
| 2 | Non-fast-forward |
| 3 | CAS / ref conflict |
| 4 | Data corruption |
| 5 | I/O or storage |
| 6 | Configuration |
| 7 | Credentials |
| 8 | Incompatible format |
| 9 | Internal bug |
| 10 | Cancelled |


## JSONL Streaming (`--jsonl`)

Long-running commands emit a stream of newline-delimited JSON objects. Each
line is a complete JSON object (ndjson / RFC 7464). No trailing commas, no
wrapping array.

### Event Envelope

```json
{
  "schema": "add.event",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "type": "file_done",
  "data": {
    "path": "data/train.parquet",
    "bytes": 52428800,
    "duration_ms": 340,
    "status": "ok"
  }
}
```

The `schema` field uses the `.event` suffix (e.g. `"hydrate.event"`,
`"push.event"`). The `type` field identifies the event kind.

### Event Types

| Type | When Emitted | Payload Fields |
|------|-------------|----------------|
| `progress` | Periodic throughput update (≤ 1 per 250ms) | `operation`, `current`, `total`, `bytes`, `total_bytes`, `rate_bytes_per_sec` |
| `file_done` | A single file completes processing | `path`, `bytes`, `duration_ms`, `status` (`"ok"` / `"failed"` / `"skipped"`) |
| `xorb_done` | A xorb uploads or downloads | `hash`, `bytes`, `compressed_bytes`, `status` |
| `warning` | Non-fatal issue encountered | `code`, `message`, `path` (optional) |
| `result` | Terminal summary (always last) | Full command result object |

The final event is always `type: "result"`. Its `data` field matches what
`--json` would emit for the same operation.

### Example: `crab add --jsonl`

```
{"schema":"add.event","version":"1.0","timestamp":"2026-04-24T18:32:17.100Z","type":"progress","data":{"operation":"staging","current":3,"total":10,"bytes":15728640,"total_bytes":52428800,"rate_bytes_per_sec":45000000.0}}
{"schema":"add.event","version":"1.0","timestamp":"2026-04-24T18:32:17.450Z","type":"file_done","data":{"path":"data/train.parquet","bytes":52428800,"duration_ms":340,"status":"ok"}}
{"schema":"add.event","version":"1.0","timestamp":"2026-04-24T18:32:17.460Z","type":"file_done","data":{"path":"data/val.parquet","bytes":10485760,"duration_ms":120,"status":"ok"}}
{"schema":"add.event","version":"1.0","timestamp":"2026-04-24T18:32:17.500Z","type":"result","data":{"files_staged":10,"files_skipped":0,"files_failed":0,"chunks_staged":42,"bytes_processed":524288000,"lock_wait_duration_ms":1,"chunking_worker_duration_ms":180,"remote_lookup_duration_ms":20,"compression_worker_duration_ms":150,"payload_write_duration_ms":55,"staging_duration_ms":300,"planning_duration_ms":0,"flushing_duration_ms":20,"indexing_duration_ms":80,"duration_ms":400}}
```

### Rate Limiting

`progress` events are rate-limited to at most one per 250ms (wall-clock).
Completion events (`file_done`, `xorb_done`, `warning`, `result`) are never
rate-limited. No heartbeat is emitted during idle periods.

### Cancellation

On SIGINT, the command attempts to emit a final `result` event with
`"status": "cancelled"` and error code `CRAB-E0090` before exiting with
code 10. A second SIGINT forces immediate exit and may truncate the stream.
Consumers should tolerate incomplete final lines.


## Command Table

Every command's structured output support at a glance.

### Read Commands (`--json`)

| Command | Schema Name | Notes |
|---------|-------------|-------|
| `crab status` | `status` | Conflicts with `--porcelain` |
| `crab env` | `env` | |
| `crab doctor` | `doctor` | |
| `crab stat` | `stat` | |
| `crab stat perf` | `stat.perf` | |
| `crab stat push-plan` | `stat.push-plan` | |
| `crab du` | `du` | Migrated to envelope (v1.1) |
| `crab ls-files` | `ls-files` | Migrated to envelope (v1.1) |
| `crab diff` | `diff` | Migrated to envelope (v1.1) |
| `crab lock` | `lock` | Migrated to envelope (v1.1) |
| `crab unlock` | `unlock` | Migrated to envelope (v1.1) |
| `crab locks` | `locks` | Migrated to envelope (v1.1) |
| `crab config get` | `config.get` | |
| `crab version` | `version` | Includes schema registry |
| `crab errors` | `errors` | Catalog or single-code lookup |
| `crab lfs status` | `lfs.status` | Migrated to envelope (v1.1) |
| `crab lfs locks` | `lfs.locks` | Migrated to envelope (v1.1) |
| `crab audit log` | `audit.log` | Local audit events |
| `crab audit verify` | `audit.verify` | Non-zero exit when invalid events are found |
| `crab audit export` | `audit.export` | Portable JSON array export |
| `crab release create` | `release.create` | Manifest plus optional publish location |
| `crab release verify` | `release.verify` | Local manifest verification |
| `crab release export` | `release.export` | Local manifest bundle export |
| `crab release list` | `release.list` | Published manifests when `--remote` is provided |
| `crab recover plan` | `recover.plan` | Non-mutating repair plan |
| `crab recover show` | `recover.plan` | Saved repair plan |
| `crab recover apply` | `recover.apply` | Idempotent verified restore plus optional shard, xorb, pack, remote, and file-index repair result |
| `crab daemon list` | `daemon.list` | |
| `crab daemon status` | `daemon.status` | |
| `crab daemon commit` | `daemon.commit` | Overlay publish result |
| `crab mount diff` | `mount.diff` | Overlay mutation summary |
| `crab mount export` | `mount.export` | Overlay export summary |
| `crab mount commit` | `mount.commit` | Overlay publish result |
| `crab mount reset` | `mount.reset` | Overlay discard summary |
| `crab staging stats` | `staging.stats` | |
| `crab track` | `track` | List mode only |

### Streaming Commands (`--json` and `--jsonl`)

These commands accept both flags. `--json` emits only the terminal result;
`--jsonl` emits the full event stream.

| Command | Schema (result) | Schema (events) |
|---------|----------------|-----------------|
| `crab add` | `add` | `add.event` |
| `crab hydrate` | `hydrate` | `hydrate.event` |
| `crab dehydrate` | `dehydrate` | `dehydrate.event` |
| `crab export` | `export.summary` | `export.plan`, `export.file` |
| `crab fetch` | `fetch` | `fetch.event` |
| `crab push` | `push` | `push.event` |
| `crab gc` | `gc` | `gc.event` |
| `crab fsck` | `fsck` | `fsck.event` |
| `crab repack` | `repack` | `repack.event` |
| `crab prune` | `prune` | `prune.event` |
| `crab clone` | `clone` | `clone.event` |
| `crab mirror` | `mirror` | `mirror.event` |

`crab push --rebase-on-non-fast-forward` result payloads include
`integration_retries` and `integration_retry_limit` so agent orchestrators can
distinguish a first-attempt conflict from an exhausted integration retry budget.
When retries occur, `integration_retry_stages` groups them by bounded stage
name, such as `preflight`, `lock`, `xorb-upload`, or `ref-commit`; its counts
sum to `integration_retries`.
Per-ref push outcomes also include `retryable` and `retry_after_secs` when Crab
can classify the rejection. If Crab's automatic rebase cannot integrate the
agent's commit, the ref status is `integration-failed` with `retryable: false`.

### Remote Helper Exception

The `git-remote-crab` remote helper (invoked by `git push`) cannot use
stdout for structured output because git owns that channel. Instead, set:

```bash
export CRAB_PROGRESS_FORMAT=jsonl
```

When this env var is set, the remote helper emits JSONL events to **stderr**.
The event schema is identical to `crab push --jsonl` (`push.event`).

UI applications spawning `git push` should read JSONL from the child's stderr.
Applications spawning `crab push` directly should read from stdout as usual.

### Commands Without Structured Output

The following are excluded from structured output:

- **Protocol binaries**: `filter-process`, `diff-driver`, `lfs-transfer-agent`
  (git owns their stdout)
- **Stub commands**: `cache stats`, `sync refs`, `bench` (not yet implemented)
- **Daemon control**: `mount` start, `unmount`, `daemon` (foreground), `daemon
  enable/disable/fetch/remount`
- **One-shot actions**: `install`, `uninstall`, `logs clear`


## Schema Versioning

Each command's output has an independent version string (`"<major>.<minor>"`).

- **Minor bump** (e.g. `1.0` → `1.1`): new optional fields added. Existing
  consumers are unaffected.
- **Major bump** (e.g. `1.1` → `2.0`): fields removed, renamed, or type
  changed. Consumers must update.

JSON Schemas (draft-07) for every payload type are committed to
`crab/schemas/`. They are generated from Rust types via `schemars` — not
hand-written. A CI test asserts that emitted output validates against the
committed schemas; drift fails the build.

To see all schema names and versions supported by your binary:

```bash
crab version --json
```

```json
{
  "schema": "version",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "crab_version": "0.15.0",
    "git_sha": "abc1234",
    "build_timestamp": "2026-04-24T12:00:00Z",
    "schemas": {
      "add": "1.0",
      "add.event": "1.0",
      "du": "1.1",
      "hydrate": "1.0",
      "status": "1.0"
    }
  }
}
```

Version changes are recorded in
[structured-output-changelog.md](structured-output-changelog.md).

## Quick Examples

### Parse a single result

```bash
crab du --json | jq '.data.total_bytes'
```

### Stream progress in a script

```bash
crab add --jsonl *.bin | while IFS= read -r line; do
  type=$(echo "$line" | jq -r '.type')
  case "$type" in
    progress)  echo "$(echo "$line" | jq -r '.data.current')/$(echo "$line" | jq -r '.data.total')" ;;
    file_done) echo "Done: $(echo "$line" | jq -r '.data.path')" ;;
    result)    echo "Finished: $(echo "$line" | jq '.data')" ;;
  esac
done
```

### Handle errors programmatically

```bash
if ! output=$(crab hydrate --all --json 2>/dev/null); then
  code=$(echo "$output" | jq -r '.error.code')
  retryable=$(echo "$output" | jq -r '.error.retryable')
  if [ "$retryable" = "true" ]; then
    echo "Retryable error $code, trying again..."
    crab hydrate --all --json
  else
    echo "Fatal error: $code"
    exit 1
  fi
fi
```
