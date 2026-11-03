# Desktop v2 validation

Local qualification: macOS arm64, Node 22.20.0, Rust 1.97.0.
Artifacts are unsigned development/release-candidate builds, not notarized releases.

## Scope

The desktop agent now belongs to the Rust workspace and uses the current v2
SDK remote repository API. Its write operations use the matching bundled v2
CLI. Native runtime checks reject a different version, platform, architecture,
or binary hash at packaging time. Electron preserves ESM dependency imports,
ships its required protobuf peer, and unpacks the PTY daemon with its native
modules. JavaScript tests use Electron's native-module ABI.

Add/push consume the CLI's current JSONL envelope. Storage diagnostics use
`du` and `metadb diagnose`. Workflow discovery, event schemas, cancellation
identity, argv boundaries, and shell command lists follow the current core
contracts. The relevant owners are `crates/crab-sdk/src/builder.rs`,
`crates/crab-sdk/src/content/crab.rs`, `crab/src/core/output/jsonl.rs`, and
`crates/crab-workflow/src/yaml.rs`.

## Evidence

- TypeScript renderer and Electron type checks pass.
- Native runtime staging and packaging checks pass (3 build tests).
- Full JavaScript suite passes: 4,048 tests in 221 files, zero failures or skips,
  using `npm test -- --maxWorkers 2 --minWorkers 1`.
- 246 Rust agent tests pass, including a real native-v2 repository on private RustFS.
- Packaged app qualification covers local browsing, add, push, byte-identical
  remote reconstruction, clone, dehydrate/hydrate, remote refs, verified v2
  diagnostics, and real argv workflow execution after load/save.
  The UI smoke completes the fresh profile’s welcome and telemetry prompts,
  selects a file in the rendered tree, and verifies its editor text.
- `scripts/qualify-v2.py` provides the disposable storage fixture used by CI;
  it uses the existing SDK qualification tool's pinned RustFS binary and checksum.

## JavaScript failure resolution

The 38 reproduced failures are resolved. Per the accepted product model, local
and remote workspaces remain separate. The retired hybrid reducer/actions,
`repos.added`, and deleted remote-source store no longer have speculative
compatibility tests. Current coverage exercises the production workspace reducer
through the same function used by React, current configuration persistence, and
host-scoped discovery/polling/disconnect/reconnect behavior.

Iteration waits for its pre-turn checkpoint before invoking local adapters or
remote RPC; initial local spawn follows the same ordering. Remote iteration emits
its persisted states. Human-review nodes validate schema-bound output before
unblocking mapped child workflows. New delayed-checkpoint and invalid-review
regressions failed against the original source and pass with these fixes.
Selected-file apply tests inspect the actual committed diff; discard fixtures use
the product's worktree creation and metadata contract.

The full run also exposed Codex protocol failures and a misplaced placeholder
Playwright test. Codex frames now follow the [official app-server wire contract](https://learn.chatgpt.com/docs/app-server#protocol),
and resume loads the stored thread before starting a turn. Resume errors surface
without silently creating a new conversation; response fixtures use `thread.id`.
File edits remain members of rendered tool groups. The placeholder LSP test is
replaced with a real TypeScript-server completion test through production
`LspServer`. It found and verifies the fix for advertising configuration requests
that the renderer cannot answer. This is subprocess integration proof, not a
packaged editor LSP UI qualification.

The changed owners and regression evidence are:

| Owner | Evidence |
| --- | --- |
| `src/state.tsx` → `src/lib/workspace-reducer.ts` | `workspace-state`, `separate-workspaces`, `workspace-config`, `remote-workspace-discovery-integration` |
| `electron/orchestrator.ts` → `orchestrator-checkpoints.ts` | Local/remote delayed-checkpoint regressions; diff/worktree and SSH iteration suites |
| `electron/agent-workflows/runner.ts` → `output.ts` | Structured review rejection and mapped child-workflow output |
| `electron/codex-app-server-adapter.ts` → `codex-rpc.ts` | RPC, approval, shutdown, resume, error, death, iterate and backpressure suites; official wire contract above |
| `electron/lsp-server.ts` | Real installed `typescript-language-server` 5.1.3 and TypeScript 5.9.3 completion; dependency waits for configuration only when that client capability is advertised |

## Launch gates still open

`npm run lint` is blocked by the imported package's missing ESLint 9 flat
configuration. Installation reports 63 dependency audit findings, including
28 high and 3 critical findings; dependency remediation remains a launch gate.

The macOS x64, Linux, and Windows workflow is authored but has not run here.
Public-cloud AWS/GCS/Azure, managed-service credentials, SSH deployment, NFS
mounting, and external agent integrations still need their platform/account
qualification. Signing and notarization need release-environment credentials.
CI uploads build artifacts and then runs the broad JavaScript regression gate.
