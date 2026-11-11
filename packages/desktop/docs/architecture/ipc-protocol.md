# IPC Protocol

Crab Desktop uses a layered IPC architecture to communicate between
the sandboxed renderer and the privileged agent sidecar.

## Communication Flow

```
Renderer                Main Process              Agent Sidecar
   │                        │                         │
   │  ipcRenderer.invoke    │                         │
   │ ─────────────────────► │                         │
   │  (method, params)      │  NDJSON write (stdin)   │
   │                        │ ──────────────────────► │
   │                        │                         │ tokio task
   │                        │                         │ dispatches
   │                        │  NDJSON response        │
   │                        │ ◄────────────────────── │
   │  Promise resolves      │                         │
   │ ◄───────────────────── │                         │
   │                        │                         │
   │                        │  NDJSON notification    │
   │                        │ ◄────────────────────── │
   │  event emitter         │                         │
   │ ◄───────────────────── │                         │
```

## Physical file bridge ownership

Repository documents use `fsReadFile(repoId, path, maxBytes, nativeSessionId)`
and `fsWriteFile(repoId, path, content, nativeSessionId)`. The renderer supplies
its observed session; preload and main preserve it. SSH sends that expected
identity through credential preparation to the router's final wire-admission
check. A replaced or missing session rejects before dispatch. The identity is
removed before native JSON-RPC serialization, so it is not a native RPC field.

Workspace read-modify-write actions retain one session across both calls.
Editor retries capture their newly observed runtime identity. Local Node reads
and publications do not depend on native reader availability; local workspace
documents use a stable null owner. The task viewer's separate one-path local
read remains an explicit task-worktree/SSH ownership gap in the large-file
acceptance matrix; it is not a repository-document overload.

## JSON-RPC 2.0 Wire Format

The agent speaks newline-delimited JSON-RPC 2.0 over stdio.

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 42,
  "method": "list",
  "params": { "repo_id": "abc123", "path": "/", "rev": null }
}
```

### Success Response

```json
{
  "jsonrpc": "2.0",
  "id": 42,
  "result": { "entries": [...] }
}
```

### Error Response

```json
{
  "jsonrpc": "2.0",
  "id": 42,
  "error": {
    "code": -32601,
    "message": "unknown method: foo_bar"
  }
}
```

### Notification (no id — fire-and-forget)

```json
{
  "jsonrpc": "2.0",
  "method": "progress/line",
  "params": { "op": "hydrate", "line": "Downloading chunk 42/100..." }
}
```

## Error Codes

The Electron `crab:rpc` channel carries `{ ok: true, value }` or
`{ ok: false, error: { message, code?, data? } }`. Electron serializes thrown
Errors as messages only. Preload preserves the plain rejection data across
the context bridge, and `src/ipc.ts` reconstructs a renderer `Error` with its
classification. Renderer callers use that common client; the agent NDJSON
contract remains JSON-RPC 2.0. Cancellation requests do not settle work;
the original native response supplies its acknowledged terminal result.

| Code | Meaning |
|------|---------|
| -32700 | Parse error (malformed JSON) |
| -32601 | Method not found |
| -32602 | Invalid params |
| 1000 | Other agent failure |
| 1001 | SDK failure |
| 1002 | CLI exited with non-zero status |
| 1003 | I/O failure |
| 1004 | Repository not open or path not found |
| 1005 | Stale hunk anchor |
| 1006 | Patch apply failed |
| 1010 | Operation cancelled |
| 1011 | Managed-service failure with remediation |
| 1012 | Native content read or integrity failure |

## IPC Whitelist (Preload)

The preload script (`electron/preload.ts`) exposes a `window.crab`
object via `contextBridge`. Only methods in the `ALLOWED_METHODS` set
can be invoked from the renderer — unknown calls are rejected before
reaching the main process.

Categories of allowed methods:

### Repository Management
- `open_local`, `open_url`, `close`
- `auth_status`, `about`, `agent_health`, `cache_stats`, `cache_clean`

Cache inspection and cleanup run the matching native `cache stats --json` and
`cache clean --json` operations. An optional `repo_id` selects repository
configuration and routes SSH requests to their host. `cache_clean` accepts
`dry_run`; reports retain native cleanup counts. Inspection preserves partial
reports and their issues. Cleanup failures are errors, with no filesystem
deletion fallback; retained, unknown and busy state stays with its owner.

### File Operations
- `list`, `stat`, `file_open`, `file_chunks`, `file_close`, `file_materialize`, `file_diff`
- `read_text`, `read_bytes`, `read_range`
- `preview_parquet`, `preview_safetensors`, `preview_npy`, `preview_npz`
- `preview_zip`, `preview_notebook_meta`, `preview_torch_checkpoint`
- `preview_sqlite`, `file_query`

`file_query {repo_id, path, rev?, op_id?, sql, max_rows?}` captures a Parquet or
SQLite source. Active working SQLite journals use an isolated read-only
transaction. Private snapshots include any WAL from the selected SQLite source.
The RPC returns columns, rows, row count and a truncation flag. `max_rows`
defaults to 500 and is capped at 1,000. The query reports cancellation after engine
shutdown and snapshot cleanup. The desktop bridge validates the observed repository
session before admitting the operation.

`file_open {repo_id, path, rev?, op_id?}` captures a file and returns an opaque
`file_id`, logical `size`, `kind`, `representation`, `can_materialize`, and available identities.
Working files never claim an uncomputed content hash. `file_chunks {repo_id,
file_id, start, limit, op_id?}` returns at most 512 ordered occurrences and a
next cursor; repeated hashes retain their positions. Local recipes do not claim
published xorb placement. Git and LFS files have no Crab recipe.

`read_range {repo_id, file_id, offset, length, op_id?}` reads up to 8 MiB from
that retained source. Byte ranges and chunk pages use independent query scopes;
reopening a pathname is not part of range delivery.

Remote LFS content reads allow the captured logical size plus the SDK's finite
metadata/retry fetch allowance, because a cold partial read verifies the whole
object. Checked arithmetic, unchanged response/decode limits and the read
deadline still apply. Other content kinds retain SDK defaults.

`file_close {repo_id, file_id}` cancels and drains active queries before releasing
the native source. Repository close drains every retained file. Queries have
independent cancellation; cancelling a page does not cancel its file. At most
128 files are retained per repository. These stateful operations are rejected
during SSH reconnection and are never served from stale cache or replayed.

Native reader opening and queries share cancellable staging-writer admission.
Closing a file while its reader is opening drains that admission before the
helper acknowledges `Closed`, including when `Ready` has not yet been emitted.

`file_materialize {repo_id, file_id, op_id}` writes a captured working Crab or
LFS pointer through 1 MiB ranges to a temporary file on the target filesystem. It verifies
the complete content identity and rechecks the working pointer before atomic
replacement, preserving permissions and indexed pointer identities. Historical, remote and
symlink captures cannot be materialized. The result contains `written_bytes`
only after publication; complete-response preview limits do not apply.

`files_materialize {repo_id, paths, op_id}` applies the same writer to exact
working-tree paths. It captures the selection before writing and serializes
publication with other materialization in that repository session. For a
missing path with an existing parent, it captures a resolved index pointer and
rechecks the index before a no-clobber publication. A batch can publish some
files before a cancellation or error stops it. Activity retains the count
already published.

Hydration, dehydration and index restoration share a per-worktree publication
workspace. A native filesystem lease serializes owners across app profiles. Its
Git-control SQLite journal uses WAL/FULL and records filesystem identities before
payload writes. The next admitted publication recovers only exact recorded
objects; replaced objects and unlisted children remain a visible cleanup failure.
Cleanup never recursively deletes the workspace. Publication counts survive that
failure. A workspace's recorded private Git ignore file prevents partial payloads
from entering ordinary status, Stage All and dirty counts, including after a
native restart. No global filename-prefix ignore rule is installed. These
records authorize scratch cleanup, not transfer replay.

`files_dehydrate {repo_id, paths, op_id}` verifies the exact selection before
replacing each working file with its committed Crab or Git LFS pointer bytes.
The index entry must match the current commit. Both the working file and the
normal reconstruction source must verify against that pointer. The reader's
`Open` request includes `dehydrate_path` so the CLI applies the existing
`crab.toml` always-profile protection. Ordinary reader opens omit that field.
The reconstruction reader and its staging pin stay owned through publication.
The writer checks the index, commit and working-file metadata again before
atomic replacement, preserves permissions, and retains index object IDs and modes.

Verified publication refreshes the Git stat cache through the same locked
writer as the CLI. The writer uses the captured output stat and expected index
blob, skips changed identities and preserves sibling staging. It never stamps
a later external edit as clean. A cache-refresh failure is advisory after
verified bytes have been published; it does not reverse that publication.

The operation has a repository-owned scope. Closing its view cancels ordinary
queries and waits for materialization before releasing the native source;
repository close cancels and drains the write. `rpc/cancel {repo_id, op_id}`
signals cancellation, and the original reply establishes the terminal outcome.
A cancellation that loses to publication returns success. Failed or cancelled
writes explicitly remove their temporary output. Detected external edits survive.

`progress/transfer {op, job_id, repo_id, written_bytes, total_bytes}` reports
logical bytes actually written to the temporary file, not network traffic.
Batch progress includes `files_published` and `files_total`.
Dehydration reports `checked_bytes` for the two full verification passes.
Its `written_bytes` result counts the pointer bytes published. These checks use
bounded ranges and apply the same cancellation and cleanup ownership as hydrate.
`progress/complete` includes `outcome` for materialization: `completed`, `failed`
or `cancelled`. Activity owns progress and cancellation across view changes.
A transport error without a terminal result leaves the outcome unknown;
reopening the working file establishes its representation. Resume/restart and
long SSH request deadlines still require separate qualification.

The main process owns `media_open {repo_id, path, rev?, op_id?}` and
`media_close {media_id}`. It captures a native file and exposes an opaque
`crab-content://media/<id>` capability to the application window. Audio/video
GETs stream 1 MiB windows with backpressure; single byte ranges support seeking,
and HEAD reads no payload. Invalid ranges return 416. Native failures emit
`media/error {media_id, error}` with the structured RPC failure to that window.
Tab cleanup cancels and drains active queries before closing the captured file;
repository close, navigation, renderer failure and application exit also drain
owned media. URLs expire on close and never contain a file path or credentials.

### Captured file comparisons

`file_diff {repo_id, path, old_path?, before, after, op_id?}` uses explicit
versions: `{kind:"working"}`, `{kind:"index"}`, `{kind:"revision", rev}` or
`{kind:"empty"}`. Root commits use the empty version; a proven unborn symbolic
HEAD also resolves to empty. Invalid revisions and failed metadata reads remain
errors. Renames can select a distinct old path. Working/index/committed Crab
and LFS identity capture does not prepare or download payloads merely to classify
them. Changed working bytes are streamed when their content identity is needed.

The result is `{kind:"git", path, patch}` for ordinary Git, preserving Git's
patch and textconv handling. Untracked additions use bounded `--no-index` output.
Each output pipe is capped at 8 MiB and the process has a five-minute deadline;
limits fail explicitly. Cancellation stops the owned process group/job and
joins both pipes before replying.

Large-file results use `kind:"large_file"`, content `status`, before/after
identities and presence/sizes, nullable `chunk_metrics`, and `metrics_note`.
Different Crab content resolves actual ordered chunk pages (or streams the
captured working file through the same Gear chunker as clean). Recipes must
cover all bytes and occurrences. Each version is limited to 262144 chunks;
the shared comparator independently bounds matching work. Ordered reuse and
added/removed chunk bytes are not network/upload cost or size delta. Identical
content identities need no recipe fetch. LFS identity changes do not invent Crab
chunk metrics. Unsupported mixed representations retain unknown metrics.

Cancellation drains blocking hashing/comparison and native/SDK reads before
closing both sources. SSH comparisons are live captures: they are neither
replayed nor served as stale cache results during reconnection.

### Git Operations
- `git_status_porcelain`, `git_grep`
- `git_log`, `git_commit`, `git_commit_paths`, `git_commit_changes`
- `git_add_paths`, `git_reset_paths`
- `git_stage_hunks`, `git_unstage_hunks`, `git_discard_hunks`
- `git_checkout`, `git_branch_create`, `git_branch_delete`, `git_branch_rename`
- `git_fetch_branch`, `git_pull_branch`, `git_push_branch`
- `git_merge`, `git_rebase`, `git_cherry_pick`, `git_revert`
- `git_stash_list`, `git_stash_save`, `git_stash_apply`, `git_stash_pop`

Native Git commands inherit the request's selected storage identity, including
status and diff commands that invoke clean filters. They do not change the
agent's process environment. Background work must retain its own request scope.
Inspection suppresses Git's optional index-cache writes, matching the Electron
inspection owner. Mandatory staging/commit locks and verified stat publication
remain enabled.
`git_add_paths` and `git_reset_paths` require a nonempty literal selection;
the explicit `.` selection serves Stage All/Unstage All. Whole-file binary and
pointer actions use these path operations. Hunk patch APIs disable external
diff drivers and text converters: display-only output cannot safely drive
staging or `git apply`.

### Crab Operations
- `clone`, `push`, `add`, `hydrate`, `dehydrate`, `status`
- `crab_track_and_add`, `crab_add_structured`, `crab_push_structured`
- `hydration_status`
- `staging_stats`, `staging_clean`

`staging_stats` returns the CLI envelope's flat `data` object, including `files`
and `lifecycle`. A file recipe with zero local segment chunks still appears:
its Git pointer may need committing even when its payload is reused or prepared
outside those segments. Statistics errors remain visible in Track & Stage.

`crab_track_and_add` accepts exact regular repository paths. New paths receive
anchored literal Crab attributes; existing Crab/LFS routing is retained. Git's
repository-local drivers are configured by the existing SDK `configure_local`
owner with the selected Git and bundled Crab executable paths. Both Crab and
LFS filters use that Crab executable; symlink and wrapper invocation paths stay
intact. The SDK client is closed before staging starts. Its configured clean
filters stage each selection, and the handler verifies the
resulting pointer format before reporting success. The response carries
`files_tracked`, `errors`, `patterns_added` and the exact `commit_paths`, including
pending `.gitattributes` rules after a retry. Partial failures stay visible.

`git_commit_paths` requires a nonempty `paths` array and uses Git's literal
[`commit --only`](https://git-scm.com/docs/git-commit#Documentation/git-commit.txt---only)
operation. It commits the current working contents of those files
and preserves other staged paths. Git owns filters, hooks, signing and index
locking. Its distinct method name makes an older agent reject the operation
instead of ignoring a new optional scope. Ordinary `git_commit` retains the full
staged-index composer behavior. The wizard and Crab staging section supply their
selected files plus the pending `.gitattributes` file. User edits within that
attributes file are included in the selection.

### Merge/Rebase Operations
- `op_start`, `op_continue`, `op_abort`, `op_status`, `op_edit_todo`
- `conflict_list`, `conflict_read_sides`, `git_apply_resolution`

### Worktree Operations
- `worktree_list`, `worktree_add`, `worktree_remove`
- `worktree_lock`, `worktree_unlock`, `worktree_move`
- `worktree_set_meta`, `worktree_status`, `worktree_push_estimate`

### Forge Operations
- `forge_list_prs`, `forge_get_pr`, `forge_create_pr`
- `forge_list_issues`, `forge_get_issue`
- `forge_pr_checks`, `forge_pr_diff`

## Notification Types

The agent emits notifications for long-running operations:

| Method | Purpose |
|--------|---------|
| `progress/line` | Single progress line (clone, push, hydrate) |
| `progress/partial` | Partial result batch (streaming list) |
| `progress/complete` | Operation finished |
| `watcher/changed` | File system change detected |
| `indexer/progress` | Background indexing progress |

## Concurrency Model

Each incoming RPC request is dispatched to its own tokio task. This
means a long-running `hydrate` never blocks a `list` or `stat`.

The renderer is expected to manage ordering dependencies itself — for
example, waiting for `open_local` to resolve before issuing `list_refs`.

Stdout writes are serialized via `Arc<Mutex<Stdout>>` to prevent
NDJSON frame interleaving when notifications race with responses.

## Cancellation

Long-running operations support cooperative cancellation:

1. Renderer calls `cancel(op_id)` via IPC
2. Main process forwards to agent
3. Agent checks `CancellationToken` at suspension points
4. Returns error code 1010 (`OpCancelled`)

The renderer can safely fire-and-forget the cancel — the operation
will complete with an error that the UI handles gracefully.

## Streaming Reads

For large directory listings, the agent uses streaming:

1. Renderer calls `list(repo_id, path, { stream: true })`
2. Agent emits `progress/partial` notifications with batches of entries
3. Final response contains the complete entry list
4. Renderer can render partial results as they arrive

This keeps the UI responsive when listing directories with thousands
of files.
