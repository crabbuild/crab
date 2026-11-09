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
- `list`, `stat`, `file_open`, `file_chunks`, `file_close`, `file_materialize`
- `read_text`, `read_bytes`, `read_range`
- `preview_parquet`, `preview_safetensors`, `preview_npy`, `preview_npz`
- `preview_zip`, `preview_notebook_meta`, `preview_torch_checkpoint`

`file_open {repo_id, path, rev?, op_id?}` captures a file and returns an opaque
`file_id`, logical `size`, `kind`, `representation`, `can_materialize`, and available identities.
Working files never claim an uncomputed content hash. `file_chunks {repo_id,
file_id, start, limit, op_id?}` returns at most 512 ordered occurrences and a
next cursor; repeated hashes retain their positions. Local recipes do not claim
published xorb placement. Git and LFS files have no Crab recipe.

`read_range {repo_id, file_id, offset, length, op_id?}` reads up to 8 MiB from
that retained source. Byte ranges and chunk pages use independent query scopes;
reopening a pathname is not part of range delivery.

`file_close {repo_id, file_id}` cancels and drains active queries before releasing
the native source. Repository close drains every retained file. Queries have
independent cancellation; cancelling a page does not cancel its file. At most
128 files are retained per repository. These stateful operations are rejected
during SSH reconnection and are never served from stale cache or replayed.

`file_materialize {repo_id, file_id, op_id}` writes a captured working Crab or
LFS pointer through 1 MiB ranges to a same-directory temporary file. It verifies
the complete content identity and rechecks the working pointer before atomic
replacement, preserving permissions and the Git index. Historical, remote and
symlink captures cannot be materialized. The result contains `written_bytes`
only after publication; complete-response preview limits do not apply.

The operation has a repository-owned scope. Closing its view cancels ordinary
queries and waits for materialization before releasing the native source;
repository close cancels and drains the write. `rpc/cancel {repo_id, op_id}`
signals cancellation, and the original reply establishes the terminal outcome.
A cancellation that loses to publication returns success. Failed or cancelled
writes explicitly remove their temporary output. Detected external edits survive.

`progress/transfer {op, job_id, repo_id, written_bytes, total_bytes}` reports
logical bytes actually written to the temporary file, not network traffic.
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

### Git Operations
- `git_status_porcelain`, `git_diff_file`, `git_grep`
- `git_log`, `git_commit`, `git_commit_changes`
- `git_add_paths`, `git_reset_paths`
- `git_stage_hunks`, `git_unstage_hunks`, `git_discard_hunks`
- `git_checkout`, `git_branch_create`, `git_branch_delete`, `git_branch_rename`
- `git_fetch_branch`, `git_pull_branch`, `git_push_branch`
- `git_merge`, `git_rebase`, `git_cherry_pick`, `git_revert`
- `git_stash_list`, `git_stash_save`, `git_stash_apply`, `git_stash_pop`

### Crab Operations
- `clone`, `push`, `add`, `hydrate`, `dehydrate`, `status`
- `crab_add_auto`, `crab_add_structured`, `crab_push_structured`
- `crab_diff_file`, `hydration_status`
- `staging_stats`, `staging_clean`

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
