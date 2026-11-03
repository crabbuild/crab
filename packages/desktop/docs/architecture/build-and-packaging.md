# Build System and Packaging

This document describes how Crab Desktop is built, tested, and
packaged for distribution across macOS, Linux, and Windows.

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
| `npm run build:agent` | `cargo build --release -p crab-desktop-agent` |
| `npm run build:renderer` | `vite build` (React app → `dist/`) |
| `npm run build:electron` | `tsc -p tsconfig.electron.json` + rename to `.cjs` |
| `npm run build` | All three in sequence |
| `npm run package` | Build + electron-builder |
| `npm run dist` | Generate icons + package |

## Agent Build

The Rust agent is compiled as a standalone binary:

```bash
cargo build --release -p crab-desktop-agent
# Output: target/release/crab-desktop-agent
```

### Dependencies

The agent links against several native libraries:
- `better-sqlite3` (bundled) — for DuckDB/SQLite previews
- `openssl` (via rustls) — TLS for forge API calls
- `libc` — Unix statvfs for disk-free queries

### Cross-Compilation

For multi-platform builds:
- macOS: Universal binary (aarch64 + x86_64) via `lipo`
- Linux: Built on the target architecture (no cross-compile)
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
| `cargo test -p crab-desktop-agent` | Cargo | Agent unit tests |

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
