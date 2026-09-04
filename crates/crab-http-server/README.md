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

Without authentication, the server accepts only loopback listeners. This mode
trusts the local operator and exposes every configured repository. Team deployments
must configure OIDC and a canonical HTTPS origin as described below. `/healthz`
is process liveness, not storage readiness.

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

## Team sign-in

Register an OpenID Connect authorization-code client with your identity provider.
Set its redirect URI to `https://git.example.com/auth/callback` and enable PKCE
with S256. Use the provider's exact issuer string, including its path and trailing
slash policy. The server discovers metadata and signing keys; it does not implement
password storage or use email addresses as authorization identifiers.

```toml
listen = "127.0.0.1:8788"

[auth]
issuer = "https://identity.example.com/realms/team"
client_id = "crab-browser"
public_url = "https://git.example.com"
# Omit for a public PKCE client. For confidential clients, supply a private file:
client_secret_file = "/run/secrets/crab-oidc-client-secret"

[[repositories]]
owner = "my-team"
name = "my-project"
bucket = "my-git-bucket"
prefix = "my-project"
members = ["provider-subject-for-alice", "provider-subject-for-bob"]
```

Terminate TLS at your reverse proxy and forward the original canonical `Host` to
the loopback listener. Forwarded headers cannot override the configured origin.
Keep the internal HTTP listener private. For local identity-provider development,
both issuer and public URL may use HTTP loopback addresses, with a loopback
listener. Production identity endpoints require HTTPS. Secret files may have a
single trailing newline; other whitespace is preserved.

`members` contains the provider's stable `sub` values. An authenticated account
with no memberships sees an empty catalog and its user ID for requesting access.
All repository read endpoints enforce membership before opening storage; absent
and unauthorized repositories both return 404. Membership changes currently
require a configuration update and server restart, which invalidates every session.
Organization and membership administration in the application remain future work.

Sign-in verifies browser-bound state, PKCE, nonce, signature, issuer, audience,
authorized party, expiry, issuance time and an access-token hash when supplied.
Callbacks reload provider keys to handle rotation. HTTP identity requests reject
redirects, time out after 10 seconds each, and cap responses at 1 MiB. Login
transactions expire after 10 minutes; at most 512 transactions and eight callbacks
are admitted. At most 4,096 sessions are retained in process memory.

Session cookies are HttpOnly and SameSite=Lax, with Secure and the `__Host-` prefix
on HTTPS deployments. Sessions expire at the earlier of ID-token expiry or eight
hours; refresh tokens are not retained. Restarting logs everyone out. Logout
requires the canonical Origin and the session's CSRF token, and removes the local
session. It does not sign the user out of the identity provider. Account revocation
at the provider does not invalidate an already issued Crab session until expiry
or a server restart; back-channel logout is not yet implemented.

`GET /api/session` exposes the current account and session CSRF token to the
same-origin frontend. Anonymous repository APIs return 401. Failed callbacks
return to a sign-in error state without exposing provider response bodies or tokens.
No cloud credentials are sent to the browser.

## Git HTTP reads

The Clone menu exposes `/git/{owner}/{name}` for native Git protocol-v2 fetches.
The HTTP transport uses the same framing parser, visibility planner and bounded
transfer profile as the Crab remote helper. `ls-refs`, shallow/deepen requests,
filters and pack streaming are supported; `receive-pack` (push) is not implemented.
Older protocol versions are rejected; use `git -c protocol.version=2` if your
client configuration overrides the modern Git default.

Authenticated users generate a read-only token from **Git access**. Supply `crab`
as the Git username and the token as its password using a credential manager.
Git requests use Basic authentication and do not require browser cookies. Tokens
inherit repository membership and session expiry; sign-out, replacement sign-in
and server restart invalidate them for subsequent requests. **Revoke tokens**
revokes every token issued by the current session. At most ten tokens per session
and 4,096 total tokens are retained as hashes in process memory. The browser keeps
the generated token only in component memory. Tokens cannot authenticate browser
read APIs or authorize writes.

Four Git transfers may run concurrently per server process. Transfer operations
use the shared two-hour profile with bounded objects, storage bytes and response
size, independently of the interactive API's 30-second/8-MiB limits. Disconnects
cancel pack production. Pack generation can use temporary files: set `TMPDIR` to
a writable directory on the workspace volume for large qualification runs. The
server still creates no clone or local Git object database. Multi-instance global
admission, write hosting and production transfer qualification remain pending.

Native Git qualification against Kubernetes in local RustFS passed ref discovery,
`clone --depth=1 --filter=blob:none --no-checkout`, an exact recursive tree comparison
(31,328 blob entries), lazy retrieval of the 4,236-byte README, and `fetch --deepen=1`
(three reachable commits). Ref discovery took 296 ms, filtered clone 8.6 seconds,
lazy README retrieval 175 ms, and deepening 7.5 seconds. These are individual local
measurements without cache flushing, not throughput or production guarantees.
The qualification client creates its own clone; the HTTP server reads the bucket
from an empty working directory. HTTP responses end with flush and HTTP EOF;
unlike the stdio helper, they must not send a response-end (`0002`) packet.

Git can gzip buffered requests or send an empty authentication probe followed by
a chunked request when a batch exceeds its HTTP buffer. Both forms are supported.
Authentication and membership checks precede the probe response; probes do not
open repository storage. A transfer slot is acquired before buffering the body,
with a 30-second body deadline and a 4.25-MiB limit on both transmitted and decoded
bytes. The shared parser independently limits command payload to 4 MiB. Unknown
content encodings return 415, invalid gzip returns 400, and oversized bodies return
413. Gzip decoding runs on a blocking worker with an expansion limit.

The large-batch qualification below fetched 1,600 distinct Kubernetes blobs through
both native Git request modes and compared every byte with the source repository.
Git reported an 80,122-byte request compressed to 39,574 bytes in the buffered run,
and a chunked request in the second run. Local fetch times were 2.58 seconds and
0.81 seconds respectively; these were sequential, cache-sharing runs, so the timing
difference does not establish that one encoding is faster.

Configure your credential helper or `GIT_ASKPASS` with a Git access token first:

```sh
python3 crates/crab-http-server/tests/verify_git_transport.py \
  --url http://127.0.0.1:8788/git/my-team/my-project \
  --source /path/to/read-only-kubernetes \
  --revision FULL_UPLOADED_COMMIT \
  --workdir "$HOME/Workspace/Github/crab-qualification"
```

The work directory must already exist on the workspace volume. This verifier
leaves its two native Git client repositories there for inspection.

[Native write design](WRITE-DESIGN.md) records the remaining receive-pack boundary,
including why protected-view commit translation cannot be used unchanged for
native pushes. Quarantine, graph/ref validation, self-contained pack/index
preparation and per-ref visibility planning are qualified against native Git and
Kubernetes/RustFS. Crab pointer content verification is separately qualified
against RustFS, including missing/corrupt backing objects. Captured-snapshot
dependency selection remains pinned after later metadata updates and enforces
per-session scan budgets. Scans and pointer proofs bound CPU work across the
process and retain admission through cancellation. A combined dependency batch
verifies Crab and LFS payloads from validated Git pointer blobs, including one
deadline for selection and content reads. Complete receive deadlines, LFS HTTP
transfer, atomic publication and HTTP receive wiring remain pending;
these checks do not establish working pushes.

## Issues and comments

Repository members can create issues, write Markdown comments and browse open or
closed discussions. Authors can edit their own content and close or reopen their
issues. Ownership uses the exact OIDC issuer and subject, not a display name.
The local loopback mode uses a single trusted operator identity. Browser writes
require the existing session, canonical Origin and CSRF token.

`GET`/`POST /api/repos/{owner}/{name}/issues` lists or creates issues;
`GET`/`PATCH .../issues/{number}` reads or edits one. Comments use
`.../issues/{number}/comments` and `.../comments/{comment}` with the same methods.
Creation requires a UUID `request_id`, a `body`, and an issue `title`. Reuse the
same UUID and original content after a lost response to recover the completed
write. Reusing it with different content returns 409. Edits require the current
`version`; stale updates return 409 without replacing another writer's changes.

Lists accept `limit` (1–50, default 30) and an exclusive numeric `before` cursor;
issues also accept `state=open|closed|all`. Results are newest first. Each page
scans at most 200 allocated numbers; an empty filtered page can still have `next`.
Clients must follow that cursor. Titles allow 1–256 characters; bodies allow
64 KiB. Eight discussion operations run concurrently, with a 30-second deadline
and an 80-KiB HTTP body limit. `Server-Timing: app` reports handling latency.

Data lives under `<repository-prefix>/app/v1/issues`, independently of Git refs,
packs and metadata. Each JSON document has `schema_version: 1`; unknown versions
are rejected. Conditional counter updates allocate numbers, immutable
`requests/{uuid}.json` reservations make creation retries converge, and ETag
updates protect visible issue/comment documents. Interrupted allocation can leave
numbering gaps. A retry completes an existing reservation without overwriting a
later edit. Comments have their own counter and reservations under each issue.

The service account needs reads and conditional writes to this app prefix in
addition to existing Git read permissions. Preserve the entire app prefix,
including counters and reservations, in backups; restoring only visible records
loses numbering and retry guarantees. Restart preserves discussions but invalidates
sessions. Markdown renders without raw HTML; external images appear as links.
Labels, assignees, deletion/moderation, edit history, notifications and pull
requests remain unimplemented. Production backup/restore qualification is pending.

The local authenticated Kubernetes/RustFS qualification created an issue and comment,
replayed both creation requests, edited content, closed/reopened the issue and
rejected stale edits. Both edited records survived a process restart; replaying
the original submission IDs recovered the same records. Git refs were unchanged.
The new discussion UI still needs browser interaction and accessibility qualification.

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

For an authenticated server, add `--cookies /path/to/private-cookies.txt` with a
Netscape-format cookie file from a signed-in session. Keep that file private and
never commit it.

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
these measurements are not a production latency guarantee. Nineteen Rust transport, identity and discussion tests and eight frontend
navigation, model and Markdown tests passed. Identity integration tests exercise real HTTP redirects and signed
Ed25519 tokens, including key rotation, replay, invalid claims, outsider access
and logout CSRF rejection, plus confidential-client secret-file authentication,
Git token scope and revocation. Thirteen shared wire tests and nineteen remote-helper
tests cover the extracted framing/parser path and existing helper contracts.
The local test issuer is not a production identity service. Dark/light
rendering, highlighted source and an actual split diff were inspected in browser.

Discussion keyboard regression checks run in Chromium:

```bash
cd packages/repository
npm ci
npx playwright install chromium
npm run test:browser
```

These seven checks cover issue/comment edit focus, cancellation, close/reopen,
Markdown tab navigation, posting from preview, retained drafts after failure,
preserving focus moved elsewhere during a delayed response, repeated edit
conflicts, and retrying a failed read of newer content. Header checks
exercise light/dark themes at 320–1,280 px, including control hit targets and
Git access panel bounds. They use
in-memory HTTP fixtures to isolate browser behavior; they do not prove storage
persistence or replace authenticated RustFS qualification. CI runs them in a
separate browser job and retains traces/screenshots on failure.

An authenticated browser run against a separate RustFS fixture also exercised
issue creation/edit/cancellation, comment preview/post/edit, close/reopen and
page reload. A concurrent edit returned a conflict while preserving the local
draft and the separately saved content; Git refs remained unchanged. Desktop/mobile screenshots
revealed overlapping account/theme controls; the header now wraps account
actions onto a separate mobile row, with its Git access panel below the header.
The saved title, comment and reopened state also survived a server restart and
fresh sign-in. These checks use a local test issuer, not a production identity
provider, and do not constitute a complete accessibility audit.

When an issue or comment save conflicts, an inline panel loads the saved title
and raw Markdown while retaining the editable draft. Save remains disabled
until the user reviews that version. **Continue with my draft** retains local
content; **Use saved content** replaces the form with the displayed version.
Neither action writes to storage. Users can combine changes in the form before
explicitly saving; another concurrent edit produces another conflict. A failed
read has a retry action and leaves the draft intact. This is manual conflict
resolution, without automatic merging or forced writes.

A real RustFS browser run exercised both choices and two successive edit races
for both issues and comments. Version checks rejected all four stale saves;
review choices left storage unchanged, and explicit combined saves persisted
after reload. Git refs remained unchanged. Light/dark desktop/mobile conflict
panels were inspected, with no browser runtime errors or horizontal overflow.

The authenticated Kubernetes/RustFS run matched the same data checks. Its median
tree request was 8.878 ms, diff request 40.688 ms, and blame request 1,982.136 ms.
These mixed cached measurements do not isolate authentication overhead. Browser
qualification exercised sign-in, a repository/file read, logout, and recovery from
an invalid callback against a local signed-token test issuer.

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
| Team identity and authorization | Real sign-in, sessions, organizations/repositories/membership and permissions; isolation, revocation, CSRF and unauthorized-access tests | In progress: OIDC, sessions and configured read memberships; administration and provider revocation pending |
| Git hosting | Authenticated smart HTTP fetch/push, branch and tag lifecycle, protected branches, metadata publication and Git CLI round-trip proof | In progress: authenticated fetch, large request encodings and native Git qualification pass; push and branch administration pending |
| Collaboration | Persisted issues, pull requests, comments, reviews, labels, assignees, merge/conflict handling, activity and notifications | In progress: issues and comments with author edits and conditional writes; remaining workflows pending |
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

The OIDC dependency includes `rsa` for public-key signature verification.
[RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html)
concerns private-key timing leakage; this relying party neither holds RSA signing
keys nor decrypts RSA ciphertext. The test issuer uses Ed25519 and is compiled only
in the test harness. No advisory suppression or dependency override is added.
A complete dependency audit remains part of production qualification.
