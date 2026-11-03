# Agent Sidecar

The `crab-desktop-agent` is a Rust binary that serves as the data
plane for Crab Desktop. It runs as a child process of Electron,
communicating over stdin/stdout with newline-delimited JSON-RPC 2.0.

## Design Philosophy

- **Concurrent dispatch** — each RPC request runs in its own tokio
  task, so a 10-minute hydrate never blocks a stat or list call
- **SDK for reads, CLI for writes** — read operations go through
  `crab-sdk` for zero-overhead; write operations shell out to the
  `crab` CLI so the desktop app inherits identical behavior
- **Streaming progress** — long operations emit notifications as they
  progress, keeping the UI responsive
- **Crash isolation** — the agent runs in a separate process; if it
  panics, the main process restarts it without losing UI state

## Entry Point

```rust
#[tokio::main]
async fn main() {
    // 1. Initialize tracing (CRAB_DESKTOP_LOG env var)
    // 2. Create shared state (open repos, cancel tokens)
    // 3. Enter NDJSON read loop on stdin
    // 4. For each request: spawn tokio task → dispatch → write response
    // 5. On stdin EOF: graceful shutdown
}
```

## Module Structure

```
agent/src/
├── main.rs              Entry point, RPC dispatch loop
├── rpc.rs               JSON-RPC 2.0 framing (Request, Response, Notification)
├── handlers.rs          Top-level dispatch (100+ method match arms)
├── shellout.rs          Streaming CLI wrapper (spawn crab, stream lines)
├── cancel.rs            Cooperative cancellation via CancellationToken
├── git_branches.rs      Branch CRUD, checkout, upstream tracking
├── git_commit.rs        Commit creation with message/amend/sign
├── git_hunks.rs         Hunk-level staging, unstaging, discard
├── git_worktree.rs      Worktree add/remove/lock/move
├── git_remote.rs        Fetch, pull, push per-branch
├── conflicts.rs         Merge conflict detection and resolution
├── forge/               Forge provider abstraction
│   ├── mod.rs           Provider trait
│   ├── auth.rs          OAuth token management
│   ├── github.rs        GitHub REST API
│   ├── gitlab.rs        GitLab REST API
│   └── bitbucket.rs     Bitbucket REST API
├── workflow.rs           YAML workflow parsing + DAG validation
├── preview.rs            File format previews
│   ├── parquet           Apache Parquet (schema + sample rows)
│   ├── safetensors       HuggingFace SafeTensors (header only)
│   ├── numpy_npy         NumPy .npy arrays
│   ├── numpy_npz         NumPy .npz archives
│   ├── zip_archive       ZIP file listing
│   ├── notebook_meta     Jupyter notebook metadata
│   └── torch_checkpoint  PyTorch checkpoint keys
├── indexer.rs            Background file indexer (SlateDB-backed)
├── watcher.rs            Filesystem change detection (notify crate)
├── storage_listing.rs    Xorb storage browser (cursor-paginated)
├── ops/                  Long-running operation management
│   └── handlers.rs       Merge/rebase/cherry-pick state machine
├── audit.rs              Operation audit log
├── experiments.rs        Experiment tracking integration
├── credential_store.rs   Credential caching
└── worktree.rs           Worktree metadata persistence
```

## State Management

The agent maintains per-session state:

```rust
struct State {
    /// Open repositories keyed by repo_id.
    repos: DashMap<String, RepoState>,
    /// Cancel tokens for in-flight operations.
    cancels: DashMap<String, CancellationToken>,
}

struct RepoState {
    /// Path to the working directory.
    work_dir: PathBuf,
    /// SDK handle for read operations.
    sdk_handle: crab_sdk::Repo,
    /// Detected forge provider.
    forge: Option<Box<dyn ForgeProvider>>,
}
```

## Error Handling

All errors flow through `AgentError`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SDK error: {0}")]
    Sdk(#[from] crab_sdk::Error),

    #[error("crab CLI exited with status {status}: {stderr}")]
    Cli { status: i32, stderr: String },

    #[error("unknown method: {0}")]
    UnknownMethod(String),

    #[error("repository not open: {0}")]
    RepoNotOpen(String),

    #[error("operation cancelled: {op_id}")]
    OpCancelled { op_id: String },
    // ...
}
```

Errors are converted to JSON-RPC error responses at the dispatch
boundary. The agent never panics on user input — all paths return
`Result<Value>`.

## Streaming Operations

Write operations (clone, push, hydrate, dehydrate) use the shellout
module to stream progress:

```rust
async fn hydrate(params: HydrateParams, out: SharedStdout) -> Result<Value> {
    let mut child = Command::new("crab")
        .args(["hydrate", "--all"])
        .current_dir(&params.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Stream each line as a JSON-RPC notification
    let reader = BufReader::new(child.stdout.take().unwrap());
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        emit_notification(&out, "progress/line", json!({
            "op": "hydrate",
            "line": line,
        })).await;
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(AgentError::Cli { status: status.code().unwrap_or(-1), stderr });
    }
    Ok(json!({ "ok": true }))
}
```

## Preview System

The agent handles file format previews server-side to keep the
renderer lightweight. Supported formats:

| Format | Crate | What's Extracted |
|--------|-------|-----------------|
| Parquet | `parquet` + `arrow` | Schema + first N rows as JSON |
| SafeTensors | `safetensors` | Header JSON (tensor names, shapes, dtypes) |
| NumPy .npy | `npyz` | Shape, dtype, first elements |
| NumPy .npz | `npyz` + `zip` | Archive listing + per-array metadata |
| ZIP | `zip` | File listing with sizes |
| SQLite | `rusqlite` | Table list + sample query |
| Jupyter | serde_json | Cell count, kernel info, language |
| PyTorch | `safetensors` | State dict keys |
| ONNX | `prost` | Graph nodes, inputs, outputs |

## Background Indexer

The agent runs a background indexer that builds a local cache of file
metadata using SlateDB (LSM-tree over local filesystem). This enables
instant search and symbol lookup without re-scanning the repository.

The indexer watches for filesystem changes via the `notify` crate and
incrementally updates its cache.

## Build

```bash
cargo build --release -p crab-desktop-agent
# Binary: target/release/crab-desktop-agent
```

The agent binary is bundled into the Electron app during packaging
via `electron-builder`'s `extraResources` configuration.
