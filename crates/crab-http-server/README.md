# crab-http-server

Product work in progress. The objective is a self-hosted GitHub replacement:
one Rust HTTP server, a React application, and repositories backed by the
operator's own object storage. A read-only browser is not the completion bar.

## Run the current development build

Requires Node 22.12+ for building the React app, Rust, and an existing Crab
repository with a current committed object locator. Runtime needs only the
resulting Rust binary and object-storage credentials. The server does not clone
repositories, execute Git, or write a local object database.

```sh
npm ci --prefix packages/repository
npm run build --prefix packages/repository
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo build -p crab-http-server --release --locked
"$HOME/Workspace/crabbuild-target/crab-main/release/crab-http-server" \
  --config /path/to/server.toml
```

Use a separate target directory per checkout. Example `server.toml`:

```toml
listen = "127.0.0.1:8788"

[[repositories]]
owner = "my-team"
name = "my-project"
bucket = "my-git-bucket"
prefix = "my-project"
description = "Our project"
```

Configure credentials using Crab's existing S3 environment provider. For local
RustFS, set `AWS_ENDPOINT_URL`, `AWS_ALLOW_HTTP=true`, and
`AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false`, plus credentials in a private environment
file. Never put credentials in server configuration or frontend assets.

The current build intentionally accepts only loopback listeners until browser
identity and team authorization are implemented. Do not put this development
build behind a public proxy. `/healthz` is process liveness, not storage readiness.

The browser provides repository/ref selection, raw-byte path navigation, lazy
Pierre Trees, paginated directories and first-parent history, highlighted files,
exact Git blob downloads, commit changes, Pierre split/unified diffs and first-parent blame.
Crab/LFS pointer downloads contain the pointer Git blob; artifact hydration is
not yet part of the HTTP application. Tree search covers loaded directories.

`GET /api/repos` returns the configured catalog. Repository reads use
`GET /api/repos/{owner}/{name}/{action}`, where `action` is `refs`, `commit`,
`commits`, `tree`, `file`, `blob`, `changes`, `diff`, or `blame`. Parameters:

- `rev`: ref or full commit OID; defaults to HEAD. Responses identify their commit
  and generation. Browser links pin the selected OID.
- `path` or `path_hex`: exactly one optional raw Git path. No filesystem
  normalization; hex supports names that are not UTF-8.
- `limit` (1–200) and signed `cursor`: directory/history pagination.
- `base`: optional comparison base; defaults to the first parent. A root commit
  compares against an empty tree.

The repository cache checks manifest freshness after two seconds. Reads remain
bounded to 30 seconds and 8 MiB per operation/JSON response; 16 requests may run
concurrently. `Server-Timing` reports repository open, read, and total handling
milliseconds. It excludes HTTP transmission; the browser also measures the
complete fetch/JSON round trip. Cached reads are not cold-storage measurements.
Ctrl-C cancels requests and shuts down the shared runtime.

## Current verification

```sh
npm test --prefix packages/repository
npm run typecheck --prefix packages/repository
npm run format:check --prefix packages/repository
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo test -p crab-http-server --locked
python3 crates/crab-http-server/tests/verify_live.py \
  --url http://127.0.0.1:8788 --repository my-team/my-project \
  --source /path/to/read-only-source --revision FULL_UPLOADED_COMMIT
```

The live verifier uses native Git as an independent oracle. It checks paginated
entries, history/messages, exact text/binary blob bodies, every changed file's
old/new diff input, error status, host validation, embedded shell and HEAD
semantics. It reports measured round-trip latency. This is a read-only
qualification check; run against a revision with a parent and `README.md`, or
adapt the file inputs for the fixture.

The local Kubernetes/RustFS run matched 162 directory entries, 10 commits,
three exact blobs including a PNG, six changed files' diff inputs, and one line
of first-parent blame against native Git. The latest mixed first/repeated local run measured median tree reads of 11 ms,
diffs of 34 ms, and one blame request of 1.5 seconds. Caches were not flushed;
these measurements are not a production latency guarantee. Five Rust transport
and envelope tests and five frontend navigation/model tests passed. Dark/light
rendering, highlighted source and an actual split diff were inspected in browser.

Known Vite/esbuild advisories were addressed by updating to Vite 7.3.6 and
esbuild 0.28.2 within Vite's supported dependency range. The final online npm
audit endpoint timed out; a fresh successful audit remains part of release proof.

## Completion requirements

| Surface | Required evidence | Status |
| --- | --- | --- |
| Single-server deployment | Built React assets and every application API served by one Rust binary; documented bucket setup, health checks, graceful shutdown, reproducible package/container | In progress |
| Repository browsing | Repository selector, refs/tags, byte-preserving paths, paginated history, file views, blame, downloads, deep links, freshness and empty/error states against real repositories | In progress |
| Diff and tree UI | Actual `@pierre/diffs` and `@pierre/trees` React integration; accurate additions/deletions/modes/binary handling; large-file/tree performance and keyboard navigation | In progress |
| GitHub-quality design | Primer tokens, light/dark/system themes, accessible controls, responsive layouts, navigation and loading/error behavior verified in browser | In progress |
| Team identity and authorization | Real sign-in, sessions, organizations/repositories/membership and permissions; isolation, revocation, CSRF and unauthorized-access tests | Pending sign-in choice |
| Git hosting | Authenticated smart HTTP fetch/push, branch and tag lifecycle, protected branches, metadata publication and Git CLI round-trip proof | Pending |
| Collaboration | Persisted issues, pull requests, comments, reviews, labels, assignees, merge/conflict handling, activity and notifications | Pending |
| Repository management | Create/import/archive repositories, settings, discoverability and search, audited administration | Pending |
| Production operation | Atomic durable writes/concurrency, restart/recovery and backup/restore proof, observability, safe upgrades, deployment and operator documentation | Pending |
| Quality gates | API and UI regression suites, accessibility, realistic Kubernetes qualification, security boundaries, CI/package smoke and measured latency | Pending |

Keep this matrix truthful as implementation advances. No placeholder navigation,
mock collaboration data, or green narrow test is evidence of product completion.

## Ownership

- `crab-remote-git`: immutable, bounded Git reads and semantic correctness.
- `crab-http-server`: HTTP transport, authentication/authorization, configured
  repository catalog, application workflows, sessions, error contracts and
  static asset delivery. Compose existing Crab Git/storage/write primitives.
- `packages/repository`: React product application, Primer design tokens,
  Pierre Trees and Diffs, URL state, and accessible user interactions.
- Object storage remains the repository authority. Runtime caches are disposable.
  Any new collaboration data needs a documented versioned storage/concurrency
  contract, separate from existing Git objects and metadata namespaces.

## Integration sources

- https://diffs.com/docs and https://diffs.com/llms-full.txt
- https://trees.software/docs and https://trees.software/llms-full.txt
- https://primer.style/product/getting-started/react/

The initial implementation is being qualified locally before replacing the
`browse_http` example. Diagnostic qualification examples remain useful tools;
the old standalone HTTP implementation will be removed when the product server
owns that behavior.
