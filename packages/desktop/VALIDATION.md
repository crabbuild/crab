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
- Native runtime staging, packaging, and release assembly checks pass (15 tests).
- Before the provider-auth changes, the full JavaScript suite passed 4,048 tests
  in 221 files. Auth integration runs cover 4,074 tests; each latest broad run
  passed 4,073 and exposed one of the existing issues recorded below.
- All 27 focused storage-connection tests pass, including a regression that
  failed before fixing credential redaction during an in-flight forget.
- 252 Rust agent tests pass, including a real native-v2 repository on private
  RustFS and real Azure HTTP listings with account-key and SAS authentication.
- SDK configuration tests (9), provider Debug redaction tests (2), Azure CLI
  placement, provider option tests (6), and the minimal SDK feature build pass.
- Packaged app qualification covers local browsing, add, push, byte-identical
  remote reconstruction, clone, dehydrate/hydrate, remote refs, verified v2
  diagnostics, and real argv workflow execution after load/save.
  The UI smoke completes the fresh profile’s welcome and telemetry prompts,
  selects a file in the rendered tree, and verifies its editor text.
  Storage auth is exercised through the rendered form: failed and successful
  read/list tests, OS-backed encrypted save, per-repository selection, Git fetch,
  SDK reads and CLI writes with deliberately invalid ambient credentials, then
  a real app restart that recovers the cloned checkout's saved connection.
- `scripts/qualify-v2.py` provides the disposable storage fixture used by CI;
  it uses the existing SDK qualification tool's pinned RustFS binary and checksum.
  Both native test consumers use the CLI from the selected packaged app.
- A rebuilt unsigned macOS arm64 package passes all three packaged smoke tests
  against that fixture, including cloud writes, restart, clone, and hydration.
- A Linux Electron reproduction traced shutdown hangs to Chromium descriptors
  inherited by the persistent PTY daemon. Closing them before daemon exec keeps
  the terminal daemon alive while app shutdown completes and browser files are
  released. All 23 Linux spawner tests and 46 macOS spawner/mode tests pass.
- The three Linux live handoff failures reproduced in isolated Electron 33.
  A readiness-driven reader now waits on inherited nonblocking descriptors;
  it closes the descriptor after borrowed writes finish. All 33 handoff tests
  pass in the Linux fixture and on macOS.
- A native Windows runner reproduced filesystem socket failures. The named-pipe
  transport passes real command execution and the 50-session stress check.
  POSIX permission modes and live descriptor transfer are checked on Unix;
  authentication, snapshot handling, and common session tests still run on Windows.
- A direct Windows sidecar request reproduced `git remote get-url` hanging
  with the agent's RPC stdin inherited. The native probe passed with stdin
  closed; the agent now gives Git a separate input pipe only when needed.
- The packaged Windows updater test confirms the application closes its
  launch pipes while the detached daemon stays alive. All 23 Windows spawner
  tests also passed with the system shell launcher.

## Complex workflow qualification

The macOS arm64 package exercises workflow actions through the production
preload, native agent, bundled CLI, and real repository files. Four packaged
scenarios cover sidebar CRUD and save-before-navigation, cross-file dependencies,
command output and failures, foreach/matrix expansion, named groups, explicit
working directories, conditional skips, retries, cache reuse, and cancellation.
The branching fixture runs 64 stages and checks their final artifact; its stage
finder must leave the selected node fully visible beside the inspector.
Cancellation checks both immediate admission and a running child, proves the
stage PID exits, and starts another run to prove the scheduler lock is released.
A repository containing only a nested named workflow also produces its output
under an explicit repository-relative working directory.

Focused checks pass: 713 `crab-workflow` tests, seven native document/handler
tests, one CLI test covering four sole-file discovery forms, and 24 graph tests
including a deterministic 512-node DAG and preserved manual placements.
Renderer/Electron type checks, renderer/Electron builds, native staging,
unsigned macOS packaging, and Rust formatting pass. The sidebar test waits for
the destination graph before checking disk, so its save assertion observes
completed navigation rather than the click event's asynchronous start.

Ownership proof: `yaml::parse_at` supplies template expansion and execution
policy; desktop document conversion retains the authored fields. `Graph::build`
uses the same working-directory paths as the executor/cache. `DagScheduler`
owns pre-committed admission and resource release. Structured commands reuse
`crab_git::process::OwnedChild`; Tokio's `from_std` adapters retain streaming
pipes. Workflow cancellation waits for process cleanup before its terminal RPC
response. Add/push retain their command/credential configuration and JSONL pumps;
the existing native storage CI remains their broader integration gate.

The Desktop v2 workflow runs the complex packaged scenarios and workflow contract
tests across macOS, Linux, and Windows. Local proof covers macOS only; it does
not qualify cloud artifact stores, SSH workflow sessions, signed distribution,
or every possible workflow. ESLint remains blocked by the existing missing flat
configuration, independently of these source changes.

## Built-in storage connections

The main process owns encrypted saved connections and checkout bindings through
Electron 33 `safeStorage`. The renderer receives metadata only. S3 access keys
and optional session tokens, inline GCS service-account JSON, and Azure account
keys or SAS tokens feed the same explicit provider constructors used by SDK
reads. CLI subprocesses use the SDK's scoped environment policy rather than
mutating process-wide credentials. Repository URL checks prevent a selected
identity from crossing provider or bucket/container boundaries.

The shared form replaces cloud credential forms in clone, setup, settings, and
the Remote Manager. Its connection test performs a real read-only listing,
reports a bounded failure in the UI, and does not claim write permission.
Forgotten connections close SDK sessions and preserve missing-connection bindings
rather than silently switching to ambient credentials. Explicit selections are
persisted before opening/cloning so a failed encrypted write creates no cloud
session or unbound checkout. Native stderr, persisted logs, progress lines, and
public IPC errors pass through main-process credential redaction. In-flight
operations retain redaction coverage when their connection is forgotten.
Renderer RPC waits for the native startup ping rather than failing while shell
and PTY initialization is still running.

Dependency proof: locked `object_store` 0.14.1 accepts inline
`GOOGLE_SERVICE_ACCOUNT_KEY`, refreshes service-account self-signed JWTs, and
accepts Azure account keys through `with_access_key`. SAS strings use its parser,
which decodes signatures once. Electron 33.4.11 provides the synchronous
`safeStorage` methods; Linux `basic_text` is explicitly rejected. Default core
auth policy remains authoritative, including OIDC; explicit desktop credentials
are not an override for that policy.

Focused coverage includes encrypted round trips, restart recovery, concurrent
saves and requests, provider input validation, all credential forms, matching
URL targets, failed reconnects, removal, persistence failures, and redaction.
Live public-cloud GCS/Azure accounts and OS keyrings on Linux/Windows remain
qualification gates; saved configuration tests do not substitute for them.

## Desktop update feed

The packaged feed uses `crabbuild/crab`, matching the package's source repository.
The immutable v1.2.4 release currently has CLI archives and no desktop metadata;
the retired `crabbuild/crab-release` v1.0.14 feed likewise lacks `latest-mac.yml`.
Changing only the repository would therefore still produce a missing-metadata
failure. The app now distinguishes unpublished desktop updates from other
failures without showing provider stack traces, headers, or misleading token
advice in the renderer.

The locked `electron-updater` 6.8.3 GitHub provider fetches channel metadata before
comparing versions. A 404 for that metadata produces
`ERR_UPDATER_CHANNEL_FILE_NOT_FOUND`; the provider also defines
`ERR_UPDATER_NO_PUBLISHED_VERSIONS` when no release tag is selected. Only those cases become an informational
unavailable state. Other failures remain errors. Manual check/download responses
use the same safe messages, and shutdown clears both scheduled checks.

Eight focused updater tests cover the publishing repository, typed failure
classification, IPC responses, genuine updates, download/downgrade policy and
shutdown. Seven failed against the old source before the fix. The packaged E2E
uses the real GitHub provider and Electron HTTP session with a local release
fixture, covering absent metadata, a valid newer desktop version and corrupt
metadata through the production preload and rendered banner. Linux's unpacked
smoke skips this scenario because it does not represent an installed package.

Release assembly checks the actual installer hashes and update metadata, combines
both macOS architectures, and requires AppImage and DEB update entries. Tests
exercise electron-updater's real macOS architecture selection, including Rosetta,
and its Linux file selection. A local assembly also checked the existing macOS
arm64 DMG, ZIP, and blockmaps; other platform inputs were fixtures. A rebuilt
unsigned macOS arm64 app passed the packaged updater and repository smoke tests.
All hosted release channels, including alpha, now require
signed macOS and Windows artifacts before publication.
Actual signed cross-platform installation and updates still require configured
signing credentials and qualification in the release environment.

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

Two broad JavaScript runs exposed unchanged pre-auth test issues:
`daemon-legacy-adapter` can restore an exited mock session when its 50 ms poll
races its 50 ms assertion wait; its focused 24-test rerun passes.
`orchestrator-settings` can generate `__proto__` as an agent type, then lose its
manually assigned configuration during JSON serialization (seed `494924895`,
path `13:1:0:0`). Both source/test pairs are unchanged from pre-auth commit
`c39ff89dfccb9da02727c2204b26f155b84252b9`. These remain broad-suite launch gates;
no baseline, ignore list, or expected-failure file was changed to hide them.

`npm run lint` is blocked by the imported package's missing ESLint 9 flat
configuration. Installation reports 63 dependency audit findings, including
28 high and 3 critical findings; dependency remediation remains a launch gate.

Cross-platform publication remains gated on the hosted builds and packaged smoke.
Public-cloud AWS/GCS/Azure, managed-service credentials, SSH deployment, NFS
mounting, and external agent integrations still need their platform/account
qualification. Signing and notarization need release-environment credentials.
CI uploads build artifacts and then runs the broad JavaScript regression gate.
