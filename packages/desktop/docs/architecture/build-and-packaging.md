# Build System and Packaging

This document describes how Crab Desktop is built, tested, and
packaged for distribution across macOS, Linux, and Windows.

Electron 45 embeds Node 24. Build scripts run on Node 22.12+ within 22.x.
Native Node modules use the Electron ABI; better-sqlite3 13 uses N-API, while
the declared SSH addon dependency uses NAN 2.30 for current V8 APIs. The native
installer calls electron-rebuild's public API and retains the macOS PTY source
build that provides an executable spawn helper. The packaged macOS minimum is
13.0, inherited from Electron rather than overridden in builder configuration.

## Build Pipeline

The build process has three independent stages that can run in
parallel, followed by a packaging step:

```
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│ Build Agent  │  │ Build        │  │ Build        │
│ (Rust)       │  │ Renderer     │  │ Electron     │
│              │  │ (Vite)       │  │ (TypeScript) │
└──────┬───────┘  └──────┬───────┘  └──────┬───────┘
       │                 │                 │
       └─────────────────┼─────────────────┘
                         │
                    ┌────▼─────┐
                    │ Package  │
                    │ (e-build)│
                    └──────────┘
```

## Build Commands

| Command | Description |
|---------|-------------|
| `npm run build:agent` | Build, verify, and stage the CLI, NFS helper, Git helper, and sidecar |
| `npm run build:agent -- --test` | Test the native integration using the build's toolchain and CLI |
| `npm run build:renderer` | `vite build` (React app → `dist/`) |
| `npm run build:electron` | `tsc -p tsconfig.electron.json` + rename to `.cjs` |
| `npm run build` | All three in sequence |
| `npm run package` | Build + electron-builder |
| `npm run dist` | Generate icons + package |

## Agent Build

The Rust agent and its CLI are built together:

```bash
export CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-desktop"
npm run build:agent
npm run build:agent -- --test
```

### Dependencies

The native payload uses:
- Bundled SQLite and DuckDB — table and analytics previews
- Rustls and system certificate roots — TLS without an external OpenSSL library
- `libc` — Unix statvfs for disk-free queries

Azure and Google SDKs use Rustls while retaining their authentication, API-tag,
and signing features. Reqwest also loads native certificate roots, including
system/private CAs, alongside callers' existing WebPKI roots. This transport
choice is shared with the CLI and server consumers in the Cargo workspace.

### Cross-Compilation

For multi-platform builds:
- macOS: Separate ARM64 and x64 binaries built on matching native runners
- Linux GNU: built on the target architecture against the glibc 2.28/GCC 8
  runtime ABI in AlmaLinux 8, using GCC Toolset 14 for current C++ dependencies.
  Hosts without that libc/compiler combination invoke Docker automatically; tests use the
  same compiler. Cache namespace and workflow contract tests also enter through
  `build-native.mjs --test`; host Cargo commands cannot reuse the producer's
  ARM linker configuration. Staging checks every native binary with `readelf` before
  producing release assets. CI runs real repository RPCs in AlmaLinux 8,
  Debian 12, and Amazon Linux 2023. Alpine/musl and the Electron/native Node
  module ABI remain separate release gates.
- Windows: Built on Windows (MSVC toolchain)

## Renderer Build

Vite builds the React application:

```bash
npx vite build
# Output: dist/index.html, dist/assets/*.js, dist/assets/*.css
```

### Configuration (vite.config.ts)

- **Entry**: `index.html` (references `src/main.tsx`)
- **Output**: `dist/` with content-hashed filenames
- **Code splitting**: Dynamic imports for Monaco, xterm, xyflow
- **CSS**: Tailwind CSS processed via PostCSS
- **Source maps**: Generated for development, stripped for production

### Key Optimizations

- **Tree shaking**: Unused exports eliminated
- **Chunk splitting**: Large dependencies (Monaco, Three.js) in
  separate chunks loaded on demand
- **CSS purging**: Tailwind removes unused utility classes
- **Asset inlining**: Small images/fonts inlined as data URIs

## Electron Build

The main process TypeScript is compiled separately:

```bash
tsc -p tsconfig.electron.json
node scripts/rename-to-cjs.mjs
# Output: dist-electron/*.cjs
```

The rename step converts `.js` → `.cjs` because Electron's main
process uses CommonJS modules while the project's `tsconfig` targets
ES modules.

## Packaging

`electron-builder` packages the app for distribution:

### Configuration (electron-builder.yml)

```yaml
appId: com.crab.desktop
productName: Crab Desktop
directories:
  output: release
files:
  - dist/**/*
  - dist-electron/**/*
extraResources:
  - from: ../target/release/crab-desktop-agent
    to: agent
mac:
  target: [dmg, zip]
  category: public.app-category.developer-tools
  hardenedRuntime: true
  entitlements: build/entitlements.mac.plist
linux:
  target: [AppImage, deb, rpm]
  category: Development
win:
  target: [nsis, portable]
```

### Platform-Specific Notes

**macOS**:
- Code signed with Apple Developer ID
- Notarized via `notarytool`
- Hardened runtime with entitlements for:
  - Network access (forge APIs)
  - File system access (repo operations)
  - Process spawning (agent, terminals, git)

**Linux**:
- AppImage for universal distribution
- `.deb` and `.rpm` for package managers
- Desktop file and icon registration

**Windows**:
- NSIS installer with optional per-user install
- Portable `.exe` alternative
- Authenticode code signing

## Development Mode

```bash
npm run dev          # Vite dev server with HMR
npm run dev:electron # Build electron + launch with dev renderer
```

In development:
- Vite serves the renderer with hot module replacement
- Electron loads from `http://localhost:5173` instead of `dist/`
- The agent binary is loaded from `target/release/` (or `target/debug/`)
- Source maps are available for all three layers

## Testing

| Command | Framework | Scope |
|---------|-----------|-------|
| `npm run test` | Vitest | Unit tests (renderer utilities) |
| `npm run test:a11y` | Playwright + axe-core | Accessibility |
| `npm run build:agent -- --test` | Cargo | Native agent integration with the built CLI |

### Test Infrastructure

- **Vitest**: Fast unit tests for renderer utilities (patch parsing,
  format helpers, state reducers)
- **Playwright**: E2E tests that launch the full Electron app
- **axe-core**: Automated accessibility audits
- **proptest**: Property-based tests for the agent (hunk anchoring,
  conflict detection)
- **fast-check**: Property-based tests for renderer (patch parsing)

## Continuous Integration

The CI pipeline runs on every pull request:

1. **Lint**: `eslint` (renderer) + `clippy` (agent)
2. **Type check**: `tsc --noEmit` (renderer + electron)
3. **Unit tests**: Vitest + cargo test
4. **Build**: Full production build (all platforms)
5. **Package**: electron-builder (smoke test, no signing)
6. **Accessibility**: Playwright a11y suite

## Release Process

1. Bump version in `package.json`
2. Update `CHANGELOG.md`
3. Tag the release: `git tag v0.1.0`
4. CI builds and signs for all platforms
5. Upload artifacts to release page
6. Auto-update feed updated (Electron's `autoUpdater`)
