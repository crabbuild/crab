# Crab Desktop v2

Electron desktop application in the Crab monorepo. The React renderer talks to
Electron over context-isolated IPC. Electron supervises `crab-desktop-agent`,
which uses native v2 SDK snapshots for remote reads, Git for local browsing, and
the matching Crab CLI for writes, hydration, and workflow runs.
The package includes the CLI, Git remote helper, NFS helper, and agent. An older
CLI on the user's PATH cannot override the bundled runtime.

## Build

Requirements: Node 22, npm, Rust 1.97+, Git, and the native C/C++ toolchain used
by Cargo and Electron native modules. Build each OS and architecture on its
native runner. The desktop CLI enables NFS and the release provider/workflow
features; it does not require a FUSE driver.

On macOS, mount the Workspace volume and give the **Codex process running the
commands** access to it. Each checkout needs its own Cargo target directory.
From the repository root:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-eace"
mkdir -p "$CARGO_TARGET_DIR"
cd packages/desktop
nvm use
npm ci
npm run build
npm run start
```

Use a distinct target directory when building a different checkout. Do not use
an in-checkout Cargo `target/` directory. Hosted CI uses its isolated runner
scratch directory instead of the developer's Workspace volume.

`build:agent` builds and stages the complete Rust runtime in
`resources/native/<os>-<arch>/`. `build:renderer` emits `dist/` and
`build:electron` emits `dist-electron/`. `build` runs all three.

For live renderer development, run `npm run build:agent` once, then `npm run dev`.
This compiles the Electron entry point and starts Vite and Electron together.

## Installers

```bash
npm run package:dir    # unpacked application for runtime smoke testing
npm run package:mac    # DMG and ZIP on macOS
npm run package:linux  # AppImage and DEB on Linux
npm run package:win    # NSIS installer on Windows
```

Artifacts go to `release/`. Packaging checks the native runtime's version,
platform, architecture, and SHA-256 hashes. Missing or mixed artifacts fail the
build. The PTY daemon and its native dependencies ship together outside ASAR.
`build:electron` also copies the agent manifests and verifies the daemon entry.

The `Desktop v2` GitHub Actions workflow builds macOS arm64/x64, Linux x64, and
Windows x64 artifacts. It runs the Rust and JavaScript tests and a packaged
Electron smoke before producing unsigned installers. CI artifacts are not signed
or notarized public releases; release signing credentials and evidence must be
provided by the release environment. Packaging commands never publish.

## Built-in storage authentication

Open **Settings → Storage & Cache → Storage connections**. Add a named connection,
choose its provider, enter credentials, and select **Test connection**. The test
performs a real, read-only bucket/container listing with a 30-second deadline;
it verifies read/list access, not write permission. Save the connection, then
choose **Use connection** for the open repository. Clone and Crab setup use the
same picker and connection test.

| Provider | Built-in credentials | Repository URL |
| --- | --- | --- |
| AWS / S3-compatible, including R2 and MinIO | Access key, secret key, optional session token; signing region and optional endpoint | `s3://bucket/repository` or `crab://bucket/repository` |
| Google Cloud Storage | Service-account JSON; Signed authentication tokens refresh through the provider | `gs://bucket/repository` or `crab://bucket/repository` |
| Azure Blob | Storage account plus account key **or** SAS query string | `az://account/container/repository` or `crab://container/repository` |

Credentials are encrypted with Electron's OS-backed `safeStorage` in the app's
user-data directory. Only metadata and a connection ID return to the renderer.
The OS keyring must be available; Linux's plaintext `basic_text` backend is
refused. Unsaved form values clear on save, cancel, or provider change. Credentials
never belong in repository URLs. Connections and local checkout selections
survive restart; cloning records the selection for the destination checkout.
Two endpoints with the same bucket name retain separate identities.

The selected connection supplies explicit SDK credentials and an isolated CLI
subprocess environment. It does not change global shell credentials. Repository
URLs must match its provider and bucket/container. **Forget** closes bound remote
SDK sessions and leaves an actionable missing-connection state; choose another
connection or explicitly select **Use default credentials**. Reconnect failures
never silently revert to the previous identity. To rotate credentials, add a new
connection, select it, then forget the old connection.

Default credentials retain the core environment chain. Use a configured
`crab://` remote with `CRAB_STORAGE_PROVIDER` and the provider's environment
credentials. Explicit core auth policy, including OIDC, remains authoritative;
saved static credentials do not bypass it. Local connections are device-local
and are never forwarded to SSH hosts. Forge tokens remain separate. Re-enter legacy desktop cloud-keychain credentials
in the connection form; v1 `.crab/config.toml` is no longer read. Repository
configuration uses v2 `crab.toml`.

## Checks

Run `npm run build:agent` first so JavaScript subprocess tests use the matching
staged v2 CLI. Tests run under Electron to match the installed native-module ABI.

```bash
npm run typecheck
npm run test:build
npm test -- --maxWorkers 2 --minWorkers 1
cargo test -p crab-desktop-agent --locked
npm run package:dir
npm run test:e2e-smoke
```

Keep `CARGO_TARGET_DIR` set for all Cargo invocations. Run broad tests, packaged
smoke, and cross-platform qualification in CI or a dedicated test environment.
The packaged smoke opens a real Git repository through the real v2 sidecar and
checks renderer-visible repository state. The Rust browse tests also exercise a
native v2 object-store snapshot against an isolated S3-compatible fixture. Set
`CRAB_SDK_TEST_S3_BUCKET` and provider environment variables to enable the
packaged add/push/reconstruction/clone/hydrate test. The fixture must own its
credentials, bucket, cache, and user-data directory.

## v2 behavior

- Remote repository authority and verified content come from the current SDK;
  v1 manifests are not consulted as an alternate authority.
- Structured add and push consume the current CLI JSONL `type`/`data` contract.
  A flow completes only after a successful terminal result. Cancellation uses
  the same operation identity in the renderer and agent.
- Push UI reports the real publication operation. The v1 fourteen-step manifest
  pipeline is retired. Transfer totals absent from the v2 terminal contract
  remain unavailable, rather than displaying invented successful counts.
- Workflow parsing, graph construction, templates, and DVC conversion use
  `crab-workflow`; status, queue mutation, and execution use the bundled CLI.
- Renderer RPC waits for the native startup ping and surfaces startup errors.
- Closing a remote session drains its SDK client. Agent shutdown cancels and joins outstanding requests
  before closing its SDK clients and SlateDB index.

The Storage view reports local usage and the verified v2 capsule generation,
refs, capsules, and checkpoint state through the current CLI diagnostics. The
retired v1 physical-xorb listing is removed. Desktop reconstruction retains at
most 512 MiB per repository beneath the shared Crab cache root (`CRAB_CACHE_DIR`
or `~/.cache/crab`). Remote metadata browsing inherits the SDK's bounded raw-blob
read limits. SSH, agent orchestration, previews, and
credentialed cloud workflows require their corresponding qualification evidence.

See [VALIDATION.md](VALIDATION.md) for local proof and remaining launch gates.
