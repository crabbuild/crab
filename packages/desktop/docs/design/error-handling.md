# Error Handling Strategy

Crab Desktop employs a layered error handling strategy that ensures
errors are surfaced appropriately at each tier without crashing the
application or losing user work.

## Design Goals

1. **Never lose user work** — a crash in one subsystem must not
   corrupt data in another
2. **Informative errors** — users see actionable messages, not stack
   traces
3. **Graceful degradation** — partial failures don't block unrelated
   functionality
4. **Debuggable** — developers can trace errors through all three
   tiers

## Error Flow

```
Agent Error (Rust)
    │
    ├─ Converted to AgentError enum variant
    ├─ Logged via tracing::error! at the boundary
    ├─ Serialized as JSON-RPC error response
    │
    ▼
Main Process (Electron)
    │
    ├─ Parsed from JSON-RPC error frame
    ├─ Wrapped as JavaScript Error with code + message
    ├─ Forwarded to renderer via IPC rejection
    │
    ▼
Renderer (React)
    │
    ├─ Caught by the calling component's try/catch
    ├─ Displayed as inline error state or toast notification
    └─ Never propagates to crash the React tree
```

## Agent Layer (Rust)

### Error Type

All agent errors flow through a single enum:

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

    #[error("invalid params for {method}: {source}")]
    InvalidParams { method: String, source: serde_json::Error },

    #[error("repository not open: {0}")]
    RepoNotOpen(String),

    #[error("stale hunk anchor")]
    StaleHunk { path: String, expected_old_start: u32, expected_old_lines: u32 },

    #[error("patch apply failed: {stderr}")]
    PatchApply { stderr: String },

    #[error("operation cancelled: {op_id}")]
    OpCancelled { op_id: String },

    #[error("{0}")]
    Other(String),
}
```

### Conversion to JSON-RPC

Each variant maps to a numeric error code:

```rust
impl From<AgentError> for RpcError {
    fn from(e: AgentError) -> Self {
        let code = match &e {
            AgentError::UnknownMethod(_) => -32601,
            AgentError::InvalidParams { .. } => -32602,
            AgentError::RepoNotOpen(_) => 1001,
            AgentError::Cli { .. } => 1002,
            AgentError::Sdk(_) => 1003,
            AgentError::StaleHunk { .. } => 1004,
            AgentError::PatchApply { .. } => 1005,
            AgentError::OpCancelled { .. } => 1010,
            _ => -32603,
        };
        RpcError { code, message: e.to_string(), data: None }
    }
}
```

### Logging Rules

- `tracing::error!` — only at the boundary that decides to surface
  the error (the dispatch loop). Never at intermediate layers.
- `tracing::warn!` — for degraded behavior that self-recovers
  (e.g., cache miss, retry succeeded)
- Source errors are preserved via `#[source]` / `#[from]` for full
  chain visibility in logs

### Panic Prevention

The agent binary enforces `#![deny(clippy::unwrap_used)]`. All code
paths return `Result<T, AgentError>`. A panic in the agent would
corrupt the stdio stream, so it is treated as a critical defect.

## Main Process Layer (Electron)

### Agent Communication Errors

```typescript
// In agent.ts — handling RPC responses
if ("error" in frame) {
  const err = new Error(frame.error.message);
  (err as any).code = frame.error.code;
  pending.reject(err);
}
```

### Agent Crash Handling

```typescript
child.on("exit", (code, signal) => {
  // Reject all pending requests
  for (const p of this.pending.values()) {
    p.reject(new Error(`agent exited before response (code=${code})`));
  }
  this.pending.clear();

  // Auto-restart with debounce
  if (code !== 0) {
    this.handleCrash(code, signal);
  }
});
```

### Crash Debounce

The main process tracks crash timestamps in a sliding window:
- Window: 30 seconds
- Max auto-restarts: 1
- If exceeded: show error dialog, don't restart

### Uncaught Exception Handler

```typescript
process.on("uncaughtException", (err) => {
  dialog.showErrorBox("Crab Desktop — unexpected error", err.stack);
});
```

This prevents silent crashes — the user always sees what went wrong.

## Renderer Layer (React)

### Error Boundary

The top-level `AppErrorBoundary` catches render errors:

```typescript
class AppErrorBoundary extends Component {
  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    if (this.state.error) {
      return <CrashRecoveryUI error={this.state.error} />;
    }
    return this.props.children;
  }
}
```

The recovery UI offers:
- **Reload** — refresh the window
- **Clear state & reload** — purge localStorage and reload
- **Try again** — dismiss the error boundary

### Stale Chunk Detection

Dynamic import failures (after a rebuild) are detected and
auto-recovered:

```typescript
const CHUNK_LOAD_PATTERNS = [
  "Failed to fetch dynamically imported module",
  "error loading dynamically imported module",
];

// Auto-reload once on stale chunk error
if (isStaleChunkError(error)) {
  reloadOnce();
}
```

### Per-View Error Handling

Each view handles errors locally:

```typescript
const [error, setError] = useState<string | null>(null);

try {
  const result = await rpc.someOperation(params);
  // success path
} catch (e) {
  setError(e instanceof Error ? e.message : String(e));
}

// Render inline error state
{error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}
```

### Toast Notifications

Non-critical errors surface as toast notifications:

```typescript
publishToast({
  type: "error",
  title: "Push failed",
  message: "Network timeout — check your connection",
  duration: 5000,
});
```

## Error Categories

| Category | User Impact | Handling |
|----------|-------------|----------|
| Network timeout | Operation fails | Retry button + toast |
| Auth expired | Forge features unavailable | Re-auth prompt |
| Stale hunk | Stage/unstage fails | Refresh + retry |
| Agent crash | All operations fail briefly | Auto-restart |
| Renderer crash | Blank screen | Error boundary + reload |
| CLI not found | Write operations fail | Settings prompt |
| Disk full | Write operations fail | Warning + cleanup suggestion |
| Merge conflict | Commit blocked | Conflicts view |

## Retry Strategies

| Operation | Strategy | Max Retries |
|-----------|----------|-------------|
| S3 upload | Exponential backoff | 3 |
| Forge API | Linear backoff (1s) | 2 |
| Agent RPC | No retry (user-initiated) | 0 |
| File watch | Restart watcher | 1 |
| LSP server | Restart with debounce | 1 |

## Debugging

### Agent Logs

Set `CRAB_DESKTOP_LOG=debug` (or `trace`) in Settings → Orchestrator
to increase agent verbosity. Logs are written to stderr and captured
by the main process.

### Renderer DevTools

`Cmd+Shift+I` opens Chrome DevTools. Console errors include the full
RPC error chain.

### Diagnostics Export

The Health view's "Copy Diagnostics" button exports:
- Agent version and health status
- CLI version
- Cache statistics
- Recent warnings
- Last 500 log lines
