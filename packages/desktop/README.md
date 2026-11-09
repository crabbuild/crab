# Crab Desktop v2

Electron desktop application in the Crab monorepo. The React renderer talks to
Electron over context-isolated IPC. Electron supervises `crab-desktop-agent`,
which uses native v2 SDK snapshots for remote reads, Git for local browsing, and
the matching Crab CLI for writes, hydration, and workflow runs.
The package includes the CLI, Git remote helper, NFS helper, and agent. An older
CLI on the user's PATH cannot override the bundled runtime.

## Navigation

The primary rail contains **Chat**, **Files**, **Git**, **Large Files**,
**Data Workflows**, **SSH Connections** (when enabled), and **Terminal**.
Files switches between Explorer and Search; Git switches between Changes,
History, Branches, Worktrees, and Pull Requests (when a forge is detected),
in that order. Branches and Worktrees each have their own sidebar and selection.
Settings stays at the bottom. Click the repository name in the title bar for
its Overview; the adjacent arrow switches workspace repositories.
Forge discovery probes Git network remotes; local paths, file URLs, and
object-store remotes open without forge network requests.

Skills live inside Chat. Health & Diagnostics lives in Settings → Diagnostics.
Terminal opens as a full page from its rail entry or the title-bar control.
Ctrl+` switches between Terminal and the previous view. Navigating away keeps
its sessions alive; agent chat terminals remain scoped to their tasks.
Terminal layouts, scrollback, and logs stay inside the app profile. Layouts
are isolated by repository or task, including when using a separate test profile.

The repository toolbar orders Open Repository, Open Workspace, Clone Repository,
then More actions. Filter repositories by name or path; Escape clears the filter.
Each workspace has a single actions menu for dashboard, clone, refresh, settings,
and close. Pin repositories from their row or context menu. Pinned repositories
appear first, followed by five recent repositories and the alphabetical remainder;
worktrees stay nested. Workspace collapse, recent access, pins, and sidebar scroll
survive restarts. Changed-file badges are neutral counts; errors retain error color.

Repository-local desktop state lives under `.crab/desktop/` at Git's worktree
root, including when opening a subdirectory. Its scoped ignore file keeps
generated state out of Git changes; existing tracked files and ignore rules remain.

## File comparisons

Use the file comparison selector for HEAD → Working tree, Index → Working tree,
or HEAD → Index. History compares a commit with its first parent; an initial
commit compares with an empty tree.

Notebook comparisons show source, outputs, cell details and notebook metadata.
Show unchanged cells to inspect them, and use the page controls to reach any
cell or long text/output page. Outputs are displayed as text; comparison does
not execute notebook code. Cancel stops the current read or comparison, and
Retry starts it again. Read and parse failures stay visible.

Structured notebook comparison supports nbformat 4, with a 16 MiB combined
input limit and 100,000 combined cells. A file beyond either limit reports an
error instead of comparing truncated content. Open or materialize the files to
inspect their complete content. Other format and transfer qualification remains
tracked in [the large-file acceptance matrix](docs/large-file-support.md).

## Build

Requirements: Node 22, npm, Rust 1.97+, Git, and the native C/C++ toolchain used
by Cargo and Electron native modules. Build each OS and architecture on its
native runner. The desktop CLI enables NFS and the release provider/workflow
features; it does not require a FUSE driver.
Windows native modules require Visual Studio 2022 with the C++ desktop workload;
the current Electron rebuild tooling does not recognize Visual Studio 2026.
Native installation rebuilds node-pty from source on macOS because its prebuilt
spawn helper lacks executable permissions. The matching helper must be executable
in development and in the packaged application.

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

Run native integration tests after building the runtime, with `CRAB_BIN` set to
`$CARGO_TARGET_DIR/release/crab` (`crab.exe` on Windows). Worktree tests invoke
the real CLI, so building only the sidecar is insufficient.

The desktop logo comes from `packages/web/crab.optimized.svg`. Run
`npm run build:icons` after changing it; this requires `rsvg-convert` and uses
macOS `iconutil` to regenerate the checked-in `.icns` file.

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
build. The PTY daemon loads from ASAR; native terminal modules and helpers
remain unpacked for execution.
`build:electron` also copies the agent manifests and verifies the daemon entry.

The `Desktop v2` GitHub Actions workflow builds macOS arm64/x64, Linux x64, and
Windows x64 artifacts. Pull requests build unsigned installers and test the
unpacked application from the same build. The Crab release workflow calls it
again for the tagged source. All release channels, including alpha, require
signed and notarized macOS builds and a signed Windows installer. `scripts/package.mjs` selects the matching update channel for all
packaging commands. Publication runs only through the release workflow.

Configure these repository Actions secrets before creating a signed Crab release:

- `MAC_CSC_LINK` and `MAC_CSC_KEY_PASSWORD`: Developer ID Application signing
  certificate (base64-encoded PKCS#12 or an electron-builder supported link)
  and its password.
- `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`, and `APPLE_TEAM_ID`: credentials
  for notarization with the same Apple developer team.
- `WIN_CSC_LINK` and `WIN_CSC_KEY_PASSWORD`: Windows code-signing certificate
  (base64-encoded PKCS#12 or an electron-builder supported link) and its password.

Signed releases must use the same macOS signing identity and Windows publisher
identity as installed builds so their platform updaters accept the replacement.
If a signing secret is missing, a signed release fails before publication.
Previously published unsigned alpha builds require one manual installation of a
signed release on macOS before automatic installation can work. Local unsigned
builds can check the release feed but cannot verify macOS update installation.

## Desktop updates

The app checks [Crab's open-source releases](https://github.com/crabbuild/crab/releases).
Desktop updates require Electron's platform metadata (`latest-mac.yml`,
`latest.yml`, or `latest-linux*.yml`) and the installers referenced by it. CLI
archives are not desktop updates. A release without desktop metadata displays a
short informational notice and a link to releases; network or malformed metadata
errors display a concise failure. Detailed diagnostics stay in the updater log.

Every packaged build, including `package:dir`, includes `app-update.yml` and
checks the same GitHub release feed. Installer builds also generate matching
`latest*.yml` files beside the installers. The release workflow verifies their
versions and hashes, combines both macOS architectures into `latest-mac.yml`,
and publishes them with the desktop installers and `SHA256SUMS-desktop.txt` in
the same release as the CLI. Prereleases use their own `alpha*.yml`, `beta*.yml`,
or `rc*.yml` channels;
alpha releases do not replace stable downloads or update Homebrew. The
version-bump script accepts versions such as `2.0.0-alpha.1` and updates the
desktop package,
lockfile, and agent along with the other shipped products. Public update checks
do not require a GitHub authentication token. Existing CLI-only releases remain
unchanged; desktop updates become available from a subsequent Crab release.

The Linux channel includes both AppImage and DEB updates. Installing a DEB
update can require an administrator authorization prompt.

## Built-in storage authentication

Open **Settings → Storage & Cache → Storage connections**. Add a named connection,
choose its provider, enter credentials, and select **Test connection**. The test
performs one real, read-only bucket/container listing with a 30-second deadline;
it reports provider failures without retrying and verifies read/list access,
not write permission. Ordinary data operations retain retries. Save the
connection, then choose **Use connection** for the open repository. Clone and Crab
setup use the same picker and connection test.

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

On macOS and Windows, `python3 scripts/qualify-v2.py` starts an owned native
RustFS fixture and runs the native tests and packaged smoke against the same
bundled CLI. Set `CRAB_PACKAGED_APP` to qualify a package outside `release/`.

Windows daemon terminals use named pipes and the Windows command processor
when no shell executable is supplied. Live descriptor handoff and POSIX file
mode checks run on macOS and Linux. Windows still runs the common terminal
tests and packaged storage qualification.

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
  The editor Output panel retains the current run's command logs and stage
  events, and displays execution errors even when the CLI fails before a stage
  starts. Structured CLI stdout and live command logs on stderr stay separate.
- Renderer RPC waits for the native startup ping and surfaces startup errors.
- Native capability discovery returns `protocol: 1`, the runtime version, and
  registered RPC method names from the dispatcher itself. SSH negotiation uses
  the attached channel during deployment; feature gates require the methods
  actually used by each operation, without inferred minimum versions.
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
