# Security Model

Crab Desktop follows Electron security best practices with defense
in depth across all three tiers.

## Renderer Sandboxing

The renderer process runs with maximum isolation:

```javascript
// BrowserWindow creation
{
  webPreferences: {
    contextIsolation: true,    // Separate JS contexts
    nodeIntegration: false,    // No Node.js in renderer
    sandbox: true,             // OS-level sandboxing
    preload: "preload.js",     // Only bridge to main
  }
}
```

The renderer cannot:
- Access the filesystem directly
- Spawn processes
- Make network requests (except to load bundled assets)
- Access Node.js APIs

## IPC Whitelist

The preload script exposes a narrow API surface via `contextBridge`:

```typescript
const ALLOWED_METHODS = new Set([
  "ping", "open_local", "list", "stat", ...
  // ~200 explicitly listed methods
]);

contextBridge.exposeInMainWorld("crab", {
  rpc: (method: string, params: unknown) => {
    if (!ALLOWED_METHODS.has(method)) {
      return Promise.reject(new Error(`Method not allowed: ${method}`));
    }
    return ipcRenderer.invoke("agent:rpc", method, params);
  },
  // ... event listeners, dialogs
});
```

Adding a new RPC verb requires updating three places:
1. Agent `handlers.rs` (implementation)
2. Preload `ALLOWED_METHODS` (whitelist)
3. Renderer `ipc.ts` (typed wrapper)

## Credential Management

### Forge Tokens (GitHub, GitLab, etc.)

- Stored in the OS keychain (macOS Keychain, Windows Credential
  Manager, Linux Secret Service)
- Never written to disk in plaintext
- Never logged or included in crash dumps
- Accessed via `electron/keychain.ts` using Electron's `safeStorage`

### AWS/Cloud Credentials

- Inherited from the user's environment (AWS_PROFILE, config files)
- The agent reads `~/.aws/config` and `~/.aws/credentials` as needed
- Credentials are never cached in the agent's memory beyond the
  lifetime of a single operation

### Repository Secrets

- `.env` files are never read by the desktop app
- The agent does not parse or forward environment secrets
- Git credential helpers are invoked by the CLI, not the agent

## Agent Process Isolation

The Rust agent runs as a separate process with:
- No network access except through the crab CLI (which handles its
  own TLS and auth)
- No GUI access
- stdin/stdout only communication with the main process
- Crash isolation — a panic in the agent doesn't crash the UI

## Input Validation

### Renderer → Main

- Method names checked against whitelist
- Parameters are JSON-serialized (no prototype pollution)
- File paths are validated before forwarding to agent

### Main → Agent

- JSON-RPC framing validated by `rpc.rs`
- Method dispatch uses explicit match arms (no dynamic dispatch)
- Parameters deserialized into typed structs with `serde`
- Invalid params return error code -32602

### Agent → Filesystem

- Path traversal prevented by resolving against the repo root
- Symlinks followed only within the repo boundary
- File operations use the repo's working directory as the root

## Code Signing

Production builds are code-signed:
- macOS: Apple Developer ID + notarization
- Windows: Authenticode signing
- Linux: AppImage with embedded signature

## Content Security Policy

The renderer loads only bundled assets:

```html
<meta http-equiv="Content-Security-Policy"
  content="default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'">
```

No external scripts, fonts, or resources are loaded at runtime.

## Threat Model

| Threat | Mitigation |
|--------|-----------|
| Malicious repo content | Agent validates all pointer/xorb data; renderer never executes repo content |
| IPC injection | Whitelist + typed params; no eval or dynamic dispatch |
| Credential theft | OS keychain; never in localStorage or logs |
| Supply chain | Pinned dependencies; Electron auto-update with signature verification |
| Local privilege escalation | Renderer sandboxed; agent has no elevated privileges |
| Crash data leakage | Crash dumps exclude credentials; stderr ring buffer capped |
