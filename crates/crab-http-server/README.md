# crab-http-server

Product work in progress. The objective is a self-hosted GitHub replacement:
one Rust HTTP server, a React application, and repositories backed by the
operator's own object storage. A read-only browser is not the completion bar.

## Run the current development build

Requires Node 22.12+ for building the React app, Rust, and an existing Crab
repository. Runtime needs the resulting Rust binary, object-storage credentials,
and writable temporary space for catalog sidecars and incoming pack preparation. Use the standard
`TMPDIR` on a suitable volume when the system temporary directory is small or
read-only. The server does not clone repositories, execute Git, or write a local
Git object database.

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

## Run the container

The checked-in image builds the locked React application and Rust server from
digest-pinned base images, embeds the assets in one binary, and runs as UID/GID
10001 with a dedicated temporary directory. Its runtime layer installs no
packages. Build it from the repository root so both workspace and frontend
inputs are available:

```sh
docker build \
  --file crates/crab-http-server/deploy/Dockerfile \
  --tag crab-http-server:local \
  .
```

Copy `crates/crab-http-server/deploy/server.example.toml` outside the checkout,
replace the identity, repository and bucket values, and keep the OIDC client
secret in a separate read-only file. Put storage credentials in a private
environment file. A team deployment can then run behind its HTTPS reverse proxy:

```sh
docker run --rm --name crab-http-server \
  --publish 127.0.0.1:8788:8788 \
  --env-file /secure/crab-storage.env \
  --mount type=bind,src=/secure/server.toml,dst=/etc/crab/server.toml,readonly \
  --mount type=bind,src=/secure/oidc-client-secret,dst=/run/secrets/crab-oidc-client-secret,readonly \
  crab-http-server:local
```

The binary's `--healthcheck` mode calls the configured listener's `/readyz`, so
healthy means every configured repository can be opened from object storage.
`SIGTERM` starts the same graceful drain as Ctrl-C. `/var/lib/crab/tmp` holds
bounded transient receive/index files;
mount a larger writable temporary volume there when expected pushes exceed the
container filesystem budget. Repository and application state remain in the
configured bucket.

The browser provides repository selection and a searchable branch/tag picker
with default-branch identification and keyboard navigation, raw-byte path navigation, lazy
Pierre Trees, paginated directories and first-parent history, highlighted files,
exact Git blob downloads, commit changes, Pierre split/unified diffs and first-parent blame.
The root view groups the selected commit and file table beside repository details.
Opening a file shows the tree sidebar; Browse files toggles it. Directories appear
first within each page, and the Code menu provides the Git URL. Request timing
is expandable. Light and dark themes support desktop and narrow screens.
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

The repository cache reopens the authoritative snapshot after two seconds,
including journal commits that have not changed the manifest ETag. Git fetch
always opens a fresh snapshot. Reads remain bounded to 30 seconds and 8 MiB per
operation/JSON response; 16 requests may run
concurrently. `Server-Timing` reports repository open, read, and total handling
milliseconds. It excludes HTTP transmission; the browser also measures the
complete fetch/JSON round trip. Cached reads are not cold-storage measurements.
Ctrl-C cancels requests, drains publication jobs and shuts down the shared runtime.
On Unix, `SIGTERM` follows the same drain path for containers and service managers.

`GET /healthz` reports process liveness. `GET /readyz` opens every configured
repository within one shared ten-second deadline and returns 503 with
`Retry-After: 5` if object storage or repository metadata is unavailable. Health
probes accept the listener's loopback Host even when the browser origin uses a
different canonical hostname; other requests retain strict Host validation.

Unborn default branches are exposed through the refs API and Git protocol v2's
explicit `unborn` advertisement, including repositories that already contain tags.
The older remote-helper `list` format omits unresolved HEAD: Git otherwise treats
it as an all-zero object ID. Concrete tags remain available; no tag is substituted
as the default branch. Use protocol v2 to retain the remote's unborn branch name.

When a read finds committed refs awaiting indexing, the server runs the shared
read-readiness pass under the generation-owner lease and global/repository GC
writer fences, then retries the read. Requests for one repository share that
job; at most two repositories publish concurrently per server. A disconnected
request leaves the job owned by the server. Its 60-second cooperative budget
includes admission wait; cleanup can extend beyond that budget. This work is
included in `Server-Timing`'s open duration, separately from the read budget.

The service account therefore needs conditional writes and deletes for repository
metadata, the locator database, visibility and journal/lock lifecycle, plus the
global GC coordination namespace, in addition to object reads. Another server or
CLI generation owner retains priority. Contention or superseded publication can
return a retryable 503. Missing verified visibility is an explicit 503 failure;
the server does not invent proofs or roll back committed refs. Storage or local
temporary-space failures appear in server logs and can be retried after repair.
Index receipts and reconstruction of missing evidence remain unfinished.

An isolated RustFS qualification exercised cache refresh after a journal commit,
API tree/blob reads, a later journal update repaired by native Git fetch, and
exact commit/tree/blob comparison against native Git. A temporary-directory
permission failure returned 503; restarting with writable temporary space repaired
the partially published generation. The server ran in a sandbox denying source
repository reads and Git execution; only its dedicated temporary index directory
and log were writable. Owner and GC leases were released after both generations,
and graceful shutdown succeeded. Initial API publication took 149 ms server time;
native fetch including the second publication took 153 ms wall time for this
three-object fixture. These shared-cache observations do not qualify production
latency or native HTTP push.

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
protected_branches = [
  { branch = "main", required_approvals = 1, required_checks = ["ci/test"] },
]
members = [
  { subject = "provider-subject-for-alice", name = "Alice", access = "write" },
  { subject = "provider-subject-for-bob", name = "Bob", access = "read" },
]
```

Terminate TLS at your reverse proxy and forward the original canonical `Host` to
the loopback listener. Forwarded headers cannot override the configured origin.
Keep the internal HTTP listener private. For local identity-provider development,
both issuer and public URL may use HTTP loopback addresses, with a loopback
listener. Production identity endpoints require HTTPS. Secret files may have a
single trailing newline; other whitespace is preserved.

`members` assigns each provider’s stable `sub`, display `name` and explicit
`read` or `write` grant. Names are used in assignment controls. Write grants
include reads; duplicate subjects or names and unspecified/unknown
access values are rejected. This unreleased server configuration requires all
three fields. An authenticated account
with no memberships sees an empty catalog and its user ID for requesting access.
All repository read endpoints enforce membership before opening storage; absent
and unauthorized repositories both return 404. Membership changes currently
require a configuration update and server restart, which invalidates every session.
Organization and membership administration in the application remain future work.

`protected_branches` contains exact branch names without the `refs/heads/`
prefix. `required_approvals` accepts 0–20 and defaults to zero when omitted.
`required_checks` accepts at most 50 unique, case-insensitive context names of
1–100 characters. It defaults to an empty list.
Once a repository has a branch, native Git cannot create, update or delete a
protected name; an atomic push containing one is rejected in full. The first
branch can still initialize an empty or tag-only repository. A fast-forward
pull request merge is the supported publication path for a protected branch.

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

The Clone menu exposes `/git/{owner}/{name}.git` for native Git protocol-v2 fetches.
The HTTP transport uses the same framing parser, visibility planner and bounded
transfer profile as the Crab remote helper. `ls-refs`, shallow/deepen requests,
filters and pack streaming are supported. Native push uses the separate
`receive-pack` protocol described below.
Older protocol versions are rejected; use `git -c protocol.version=2` if your
client configuration overrides the modern Git default.

Authenticated users choose one repository under **Git access** and generate a
read token, or a read/write token when their membership permits writes. Supply `crab` as the Git username and the token as its password
using a credential manager. Git requests use Basic authentication independently
of browser cookies. Tokens are restricted to the selected owner/repository and
their requested permission, intersected with the user's configured grant. A read
token cannot gain write access when its owner has a write grant. Basic tokens
also authenticate the commit-status API for their selected repository; other
browser APIs require the signed-in session cookie.

Git credential helpers ignore HTTP paths by default. Enable path matching for
this server before saving tokens, replacing the example origin with yours:

```bash
git config --global 'credential.https://git.example.com.useHttpPath' true
```

The token panel shows the command for the current server. This keeps tokens for
different repository paths separate; other hosts retain their own configuration.
See [Git credential contexts](https://git-scm.com/docs/gitcredentials#_configuration_options).

`POST /api/git-token` requires session CSRF/Origin and a JSON body with `owner`,
`repository` and `access` (`read` or `write`), limited to 2 KiB. Missing fields and
unknown permissions fail; inaccessible targets or permissions return 403 without
opening storage. The response includes the token, target, permission and remaining
session lifetime. The catalog includes the effective `access` for each repository.
The browser defaults to read and offers write only for configured writers.
Changing repository or permission clears the displayed token.

Sign-out, replacement sign-in and server restart invalidate tokens. **Revoke
tokens** revokes every token issued by the current session, including retained
principals on their next authorization check. Revocation does not undo completed
operations. At most ten tokens per session and 4,096 total tokens are retained by
hash in process memory. The browser keeps the generated secret only in component
memory and clears it on a new request or repository selection.

Four Git transfers may run concurrently per server process. Transfer operations
use the shared two-hour profile with bounded objects, storage bytes and response
size, independently of the interactive API's 30-second/8-MiB limits. Disconnects
cancel pack production. Pack generation can use temporary files: set `TMPDIR` to
a writable directory on the workspace volume for large qualification runs. The
server still creates no clone or local Git object database. Multi-instance global
admission and production transfer qualification remain pending.

Native Git qualification against Kubernetes in local RustFS passed ref discovery,
`clone --depth=1 --filter=blob:none --no-checkout`, an exact recursive tree comparison
(31,328 blob entries), lazy retrieval of the 4,236-byte README, and `fetch --deepen=1`
(three reachable commits). Ref discovery took 296 ms, filtered clone 8.6 seconds,
lazy README retrieval 175 ms, and deepening 7.5 seconds. These are individual local
measurements without cache flushing, not throughput or production guarantees.
The qualification client creates its own clone; the HTTP server reads the bucket
from an empty working directory. HTTP responses end with flush and HTTP EOF;
unlike the stdio helper, they must not send a response-end (`0002`) packet.

Git fetch can gzip buffered requests or send an empty authentication probe followed by
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
  --url http://127.0.0.1:8788/git/my-team/my-project.git \
  --source /path/to/read-only-kubernetes \
  --revision FULL_UPLOADED_COMMIT \
  --workdir "$HOME/Workspace/Github/crab-qualification"
```

The work directory must already exist on the workspace volume. This verifier
leaves its two native Git client repositories there for inspection.

[Native write design](WRITE-DESIGN.md) records the receive-pack boundaries,
including why protected-view commit translation cannot be used unchanged for
native pushes. Quarantine, graph/ref validation, self-contained pack/index
preparation and per-ref visibility planning are qualified against native Git and
Kubernetes/RustFS. Crab pointer content verification is separately qualified
against RustFS, including missing/corrupt backing objects. Captured-snapshot
dependency selection remains pinned after later metadata updates and enforces
per-session scan budgets. Scans and pointer proofs bound CPU work across the
process and retain admission through cancellation. A combined dependency batch
verifies Crab and LFS payloads from validated Git pointer blobs, including one
deadline for selection and content reads. Production write/recovery qualification
remains pending.

## Git LFS transfers

The canonical clone URL is `/git/{owner}/{name}.git`. Git LFS discovers
`/info/lfs/objects/batch` from that URL without a separate `lfs.url` setting.
Update development remotes that used the earlier suffix-free URL with
`git remote set-url origin <URL from the Code menu>`.

The batch API supports SHA-256 `basic` transfers. Read tokens download; write
tokens also upload. Action URLs use the configured origin and the same scoped
Git token. Every transfer rechecks access; upload checks it again after receiving
the body, before storage publication. Revoked tokens cannot start another transfer.
A multipart operation already in progress drains through completion or abort.

Uploads stream to a private temporary file and then use `crab-lfs`'s verified,
bounded-memory multipart publication. Downloads verify size and SHA-256 before
opening a backpressured response. Already verified objects omit upload actions;
missing/corrupt downloads return per-object errors. A successful upload needs no
separate verify request. Git receive independently proves the pointer dependency
before publishing its commit. No server-side Git checkout or LFS client is used.

Batches accept at most 200 objects and 64 KiB of JSON, with a 30-second body
budget. Files are limited to 512 MiB, matching the existing receive dependency
limit; a push permits at most 2 GiB of dependency content. LFS byte transfers share
the four Git transfer slots. Verification and byte transfer use a five-minute
budget; started multipart work retains its slot and temporary file while draining,
which can extend shutdown beyond that budget. Use a suitable `TMPDIR` for uploads.
Downloads currently restart rather than resume ranges. The optional HTTP locking
API returns 501; lock creation and enforcement across native HTTP pushes remain
unfinished. Browser blob downloads still return exact Git pointer bytes.

Native Git LFS qualification uploads a 10 MiB file, publishes its pointer commit,
and checks exact hydrated bytes in an independent clone. HTTP tests cover invalid
hashes/sizes, repeated uploads, missing objects, access scope, token revocation
before publication and disconnect cleanup.

An isolated RustFS run also pushed a 32 MiB LFS file, removed the source, and
verified exact bytes in an independent clone. After removing that client and
gracefully restarting the server, a second independent clone matched the same
SHA-256. The local push took 491 ms; the first and restarted clones took 352 ms
and 378 ms. These shared-cache localhost measurements verify the path and expose
its observed latency; they are not production or Internet benchmarks.

## Native Git push

`POST /git/{owner}/{name}.git/git-receive-pack` accepts exact native Git commits,
branch/tag creation, fast-forward updates and deletions in one atomic batch.
Non-fast-forward updates are rejected, including forced refspecs. Existing Crab
repositories must be initialized before serving them. The local loopback operator
can push; authenticated team pushes require a repository-scoped write token.
Read tokens are never upgraded implicitly.

The server streams requests to private temporary files, validates full/thin packs,
resolves bases only from the repository's committed visibility, proves graph and
pointer dependencies, and publishes through the shared journal. It uses sorted
ref leases, the shared namespace lease for creates/deletes, and both GC fence
domains. It preserves submitted object IDs and uses no Git executable or clone.
Requests share the four transfer slots with fetch. Each receive has a five-minute
cooperative deadline, a 2-GiB wire/prepared-pack limit, one million incoming objects,
64-MiB individual objects and an 8-GiB inflation budget. Graph and dependency reads
have further bounds. Temporary disk needs can exceed the wire limit. Buffered and
chunked native pushes are supported; receive requires identity content encoding.

Shallow Git clients can push normally. Crab accepts the protocol's bounded
`shallow <oid>` declarations before the first ref command, but does not trust them
as proof of connectivity or create a shallow server repository. Every omitted
parent must still resolve from the committed remote object graph or the push is
rejected as incomplete.

The current symbolic HEAD cannot be deleted through HTTP push. Rejection applies
to the entire atomic batch and Git reports `deletion is prohibited`; other branches
and tags remain deletable. This follows the repository's recorded default rather
than a hardcoded branch name. Exact names listed in `protected_branches` also
reject direct creation, update or deletion after repository initialization and
report `protected branch requires a pull request`. Default-branch and protection
administration in the application remain pending.

Disconnects and deadlines signal cancellation; owned workers retain admission
and renew their GC fences until cleanup finishes. A known commit is never reported
as a ref rejection due to later cleanup or indexing failure. If the response is lost or returns 503,
inspect remote refs before retrying: publication may have committed. Subsequent
reads run catalog repair for committed journal work. This is not yet durable
application-level push receipt or complete disaster-recovery support.

Native Git integration and isolated RustFS runs cover initial branch and
annotated-tag pushes, a depth-one client's branch push, updates, atomic rewrite
rejection, existing-object tags, deletion, API visibility and independent fetch.
Remote objects are compared byte for byte after removing client repositories.
The deletion flow also exposed and fixed a shared catalog bug: removed tips must
be looked up even when no surviving ref or new evidence mentions them. These
small-repository tests are not Kubernetes push throughput or production
qualification. Detailed check runs, broader protection rules, protected-view and active-active
publication coexistence, LFS HTTP locking, and process-crash qualification remain unfinished;
use this development server with standard Crab repository publication only.

Tag-only initialization preserves an unborn default branch. The refs API returns
`head: null` and its symbolic name in `unborn_head`; native protocol-v2 clone
preserves that name while fetching tags. The browser selects an available ref for
browsing without changing HEAD. HTTP receive, CLI publication and protected
publication choose only branches when establishing a default.

Deploy the updated CLI and services together before using this state. Tagged
releases v1.0.1 and v1.1.0 reject a nonempty repository with an unborn HEAD. Existing
resolved-HEAD repositories and serialized fields are unchanged; no automatic
migration or tag-to-branch conversion is performed.

Injected storage faults pass against both memory storage and RustFS: lost marker
replies, rejected marker writes, and cancellation before and after the commit
boundary. A fresh server instance reads the exact committed state; an explicit
retry publishes uncommitted requests and clears their prepared recovery evidence.
Blob bytes match after the client directory is removed. These tests exercise
cooperative shutdown and fresh instances, not abrupt process termination.

To repeat the isolated receive qualification, source your private RustFS environment
and choose a fresh prefix (the test creates a manifest and will not overwrite one):

```sh
QUALIFICATION_BUCKET=my-test-bucket \
QUALIFICATION_PREFIX=qualification/http-receive-my-run \
TMPDIR="$HOME/Workspace/Github/crab-qualification/scratch" \
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo test -p crab-http-server --locked native_http_push_rustfs -- --ignored --nocapture
```

Create the temporary directory first and use this checkout's own target directory.
The test removes its Git client directories and retains the isolated object-storage
repository for inspection.

Repeat with another fresh prefix and replace `native_http_push_rustfs` with
`receive_faults_rustfs` to exercise the four injected failures and GC fence renewal
while a cancelled writer drains beyond its initial lease expiry.

## Issues, pull requests and reviews

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

Pull requests use the corresponding `/pulls` collection and
`/pulls/{number}` item routes. Creation records exact `refs/heads/*` base and head
names plus their commit IDs. Detail reads refresh both IDs from live branches, so
new head commits appear without rewriting the pull request. The original IDs
remain immutable review evidence. If either branch is deleted, the conversation
and original IDs remain available while live file comparison is disabled. Pull
comments use `/pulls/{number}/comments` and the same conditional edit contract.
Authors can close or reopen their own pull requests; repository writers can also
change their state. Reviews use `/pulls/{number}/reviews` and
`/pulls/{number}/reviews/{review}`. A review records `commented`, `approved` or
`changes_requested` against the exact current head commit. Pull request authors
can comment but cannot approve or request changes on their own changes. Advancing
the head leaves prior reviews visible with `current: false`; the state and commit
of an existing review are immutable while its author can conditionally edit its
body. For a protected base branch, only each reviewer's latest decision on the
exact current head counts. Stale approvals do not count, and a current request
for changes blocks the merge until that reviewer submits a current approval.
Recording a decision advances the pull version so a concurrent stale merge or
edit cannot claim the pull after that decision.

Repository labels use `GET`/`POST /api/repos/{owner}/{name}/labels` and
`PATCH`/`DELETE .../labels/{id}`. Any repository member can list labels; write
access is required to create, edit, delete or assign them. Names contain 1–50
characters and are unique case-insensitively. Colors are six hexadecimal digits
without `#`, descriptions contain at most 100 characters, and each issue or pull
request accepts at most 20 distinct label IDs. The label catalog is capped at 500
created IDs over the repository lifetime so its conditionally updated document
stays bounded.

Issue and pull `PATCH` requests assign the complete label set with `label_ids`
and the discussion's current `version`. Assignments store stable IDs. Renaming a
label is therefore visible everywhere immediately; deleting it removes it from
every response without rewriting discussions. Create requests use immutable UUID
reservations. Replaying one after an edit returns the current label, while durable
deletion tombstones prevent a replay from resurrecting a deleted label. Deletes
are idempotent when retried with the deleted label's version. The React application
provides repository label management, GitHub-style colored badges and label pickers
on issue and pull detail views.

`GET /api/repos/{owner}/{name}/assignees` lists the repository member directory.
Issue and pull `PATCH` requests replace the complete assignment with `assignees`
and the discussion's current `version`. The server accepts at most 10 distinct,
configured member subjects, and only repository writers can change the set. The
stored subjects remain stable while responses resolve current configured display
names. Removing a member from configuration removes that person from subsequent
responses. Local loopback repositories expose the single local operator. The
React issue and pull conversation views present assignees and labels together in
a responsive metadata rail.

Repository writers can `POST /pulls/{number}/merge` with the displayed
pull version and exact base/head IDs. The only accepted method is
`fast_forward`: the server revalidates ancestry, pointer dependencies and Git
visibility under the base-ref lease and both GC fences, then publishes through
the same journal path as native Git push. A persisted merge marker blocks
concurrent pull edits and lets the same request resume after a lost response or
restart. A completed merge retains its pre-merge comparison even if the source
branch is deleted. Protected base branches can be updated only through this merge
path. The configured current-head approval requirement is checked before the
merge reservation is created. Merge commits are not implemented.

## Commit statuses and required checks

External CI can `POST /api/repos/{owner}/{name}/statuses/{sha}` with a
repository-scoped write token using Basic authentication. The JSON body contains
a UUID `request_id`, `context`, `state` (`pending`, `success`, `failure` or
`error`), and optional `description` and `target_url`. The target must use HTTPS,
or loopback HTTP for local development. A signed-in writer may use the same route
with the normal Origin and CSRF headers. Repository readers can
`GET /api/repos/{owner}/{name}/commits/{sha}/status` to inspect the latest status
for every context and the combined state.

The commit must be reachable in the repository. Context matching is
case-insensitive, so a later `CI/Test` status replaces `ci/test`. Every status
submission has an immutable object-storage reservation and monotonically ordered
number. Replaying an older request returns its original result without replacing
a newer status for the same context. Each commit accepts at most 1,000 status
submissions and retains the latest result for at most 128 contexts; descriptions
are limited to 140 characters and target URLs to 2 KiB.

Protected pull requests evaluate configured required contexts only against the
exact current head. Missing, pending, failed and errored checks block merge;
every required context must report `success`. A head advance starts with missing
statuses for that new commit. The merge panel mirrors GitHub's checks summary,
with expected, pending, successful and unsuccessful states plus safe detail links.
Merge admission uses one latest-status snapshot; an update that completes after
admission applies to later merge attempts, while an existing merge reservation
remains recoverable.

CI integrations can also create detailed check runs with
`POST /api/repos/{owner}/{name}/check-runs`, list them at
`GET /api/repos/{owner}/{name}/commits/{sha}/check-runs`, inspect one at the
same path plus `/{id}`, and update it with `PATCH`. Create and update bodies use
UUID `request_id` values. Updates also include the displayed `version`; only the
writer identity that created a run can update it. Runs advance from `queued` to
`in_progress` to `completed`. A completed run requires one of `success`,
`neutral`, `skipped`, `failure`, `cancelled`, `timed_out` or
`action_required`, and cannot be reopened. Queued and in-progress runs cannot
carry a conclusion. `details_url` follows the status target URL policy.

Each report contains an output title, Markdown summary, optional Markdown text,
up to 50 steps with bounded plain-text logs, and up to 50 file/line annotations.
Encoded output is limited to 192 KiB and requests to 256 KiB. A commit retains
at most 100 runs. List pages use the same `limit` and exclusive `before` cursor
contract as other app lists. The pull request Checks tab presents run outcomes,
annotations, rendered output and expandable escaped logs, with links to older
runs and external details.

For a required context, the newest report by timestamp wins across commit
statuses and check runs; a check run wins a same-millisecond tie. Queued and
in-progress runs satisfy `pending`. Completed `success`, `neutral` and `skipped`
satisfy `success`; all other conclusions block merge. Check creation and every
update reserve immutable request and output objects before a conditional catalog
write. Retrying an old request returns that version without rolling back the
visible run.

Lists accept `limit` (1–50, default 30) and an exclusive numeric `before` cursor.
Issue and pull lists accept `state=open|closed|all` plus an optional case-insensitive
`q` search across title, description and author. Queries are limited to 256
characters. Results are newest first. Each page scans at most 200 allocated
numbers; an empty filtered page can still have `next`. Clients must follow that
cursor. Titles allow 1–256 characters; bodies allow 64 KiB. Eight discussion
operations run concurrently, with a 30-second deadline and an 80-KiB HTTP body
limit. `Server-Timing: app` reports handling latency.

Data lives under `<repository-prefix>/app/v1/issues`, `app/v1/pulls`,
`app/v1/labels`, `app/v1/statuses` and `app/v1/check-runs`,
independently of Git refs, packs and metadata. Each JSON document has
`schema_version: 1`; unknown versions are rejected. Conditional counter updates
allocate numbers, immutable
`requests/{uuid}.json` reservations make creation retries converge, and ETag
updates protect visible issue, comment, review and merge documents. Interrupted
allocation can leave numbering gaps. A retry completes an existing reservation
without overwriting a later edit. Comments and reviews have their own counters
and reservations under their parent discussion. Each pull also retains the
latest decision for at most 1,024 reviewer identities so merge admission does
not depend on a paginated review list. Merge reservations and pending state
preserve the exact actor, method, pull version and commit IDs.

The service account needs reads and conditional writes to this app prefix in
addition to existing Git read permissions. Preserve the entire app prefix,
including counters and reservations, in backups; restoring only visible records
loses numbering and retry guarantees. Restart preserves discussions but invalidates
sessions. Markdown renders without raw HTML; external images appear as links.
Discussion deletion/moderation, edit history, notifications and merge commits
remain unimplemented. Production
backup/restore qualification is pending.

The local authenticated Kubernetes/RustFS qualification created an issue and comment,
replayed both creation requests, edited content, closed/reopened the issue and
rejected stale edits. Both edited records survived a process restart; replaying
the original submission IDs recovered the same records. Git refs were unchanged.

The detailed-check qualification created a queued run on a real RustFS-backed
commit, advanced it through in-progress to successful with two step logs and a
file annotation, and recovered byte-identical detail after restart. Create took
99.12 ms, updates took 32.63–36.95 ms, and warm list/detail reads took
27.39–31.86 ms locally. A native HTTP push supplied a live pull head; light,
dark and 390-pixel browser runs displayed its stored result without horizontal
overflow.

A separate RustFS qualification used a real depth-one Git client to push a pull
request branch, created a pull request and comment, and compared the exact added
file. Both records survived a process restart. After deleting the branch and
removing the client checkout, the conversation retained its original commit IDs
and reported that live comparison was unavailable. Warm detail, comment-list,
pull-list and exact-change reads took 14.2, 1.3, 0.9 and 8.4 ms from the client;
idempotent pull and comment replays took 552.7 and 581.5 ms. These localhost,
shared-cache timings expose observed latency rather than production performance.
An authenticated two-user integration test uses native Git pushes: one member
opens a pull request that cannot merge before approval, another member's exact-head
approval unlocks it, and self-approval is rejected. A later push makes the first
approval outdated; a current request for changes blocks merging until that same
reviewer submits a current approval.

The RustFS qualification also created a commit-bound review, advanced the head,
and verified that the first review became outdated while a second review bound
to the new head. Both survived server restart; deleting the source branch kept
both review records and marked both outdated. Initial pull and review writes took
38.2 and 16.3 ms, the post-restart list took 33.1 ms, and idempotent replay took
560.8 ms on localhost with shared caches. These are observations, not production
latency guarantees.

A fast-forward merge qualification pushed a dedicated branch with native Git,
created pull request 3, and merged its exact head through the HTTP API into the
same RustFS-backed repository. An independent depth-one clone read the merged
commit and file from `main`. After deleting the source branch and restarting the
server, the pull remained merged and its exact one-file comparison remained
available. Pull creation, merge and idempotent merge replay took 34.1, 391.2 and
1.0 ms; after restart, pull detail and exact changes took 25.4 and 55.8 ms of
server work on localhost with shared caches. These are observations, not
production latency guarantees.

The same RustFS repository was then configured with protected `main`. A native
atomic push that combined a fast-forward of `main` with creation of another
branch was rejected in 126.7 ms and neither ref changed. Pull request 4 published
that exact head through the merge path in 259.6 ms, and a fresh depth-one clone
read the exact commit and file in 163.5 ms. After source-branch deletion and a
server restart, native ref discovery still returned the merged `main`; pull
detail and its exact one-file comparison took 46.8 and 40.6 ms from the client.
These localhost timings are functional observations rather than production
latency guarantees.

A required-check qualification then pushed a new head with native Git and opened
pull request 5 against protected `main`. Missing and pending `ci/test` statuses
blocked merge; a success reported through the repository token unlocked it. An
older pending request replay returned its original response without replacing the
newer success. The merge took 297.9 ms, and an independent depth-one clone read
the exact merged commit and file in 723.1 ms after the source checkout and branch
were removed. After server restart, pull detail, combined status, exact changes
and native ref discovery took 45.8, 42.4, 21.1 and 42.7 ms from the client. The
check panel was also inspected in light, dark and 390-pixel layouts without
horizontal overflow. These shared-cache localhost timings are functional
observations rather than production latency guarantees.

The rebuilt server also searched that persisted pull collection without an
index or local checkout. A case-insensitive title/description query returned
pulls 6 and 5 in 8.4 ms, an uppercase author query returned all six records in
2.0 ms, and a missing query returned an empty page in 1.5 ms from the client.
Light, dark and 390-pixel pull-list screenshots were compared with the saved
GitHub issue-list reference; the search input, state controls and results had no
horizontal overflow. These small shared-cache reads validate behavior and
layout, not production search throughput.

The same RustFS-backed pull collection qualified repository labels without a
local checkout. Two labels were assigned to persisted pull request 2; editing a
label propagated immediately, deleting the other removed it from the pull, a
repeated delete returned 204, and replaying the original create returned the
edited label without resurrection. The final assignment survived a server
restart. Warm label edit, delete and replay requests took 1.8, 1.5 and 0.8 ms
round trip; pull assignment took 37.9 ms. The label catalog and pull picker were
inspected in light, dark and 390-pixel layouts with no browser errors or
horizontal overflow. These localhost shared-cache timings are functional
observations rather than production throughput evidence.

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
of first-parent blame against native Git. The latest mixed first/repeated local
run measured median tree reads of 11 ms, diffs of 34 ms, and one blame request
of 1.5 seconds. Caches were not flushed; these measurements are not a production
latency guarantee. Forty-one Rust server tests and eight frontend navigation,
model and Markdown tests passed. Identity integration tests exercise real HTTP redirects and signed
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
| Single-server deployment | Built React assets and every application API served by one Rust binary; documented bucket setup, health checks, graceful shutdown, reproducible package/container | Complete: locked multi-stage image with digest-pinned inputs, non-root runtime, binary readiness probe and Linux CI build/runtime inspection |
| Repository browsing | Repository selector, refs/tags, byte-preserving paths, paginated history, file views, blame, downloads, deep links, freshness and empty/error states against real repositories | In progress |
| Diff and tree UI | Actual `@pierre/diffs` and `@pierre/trees` React integration; accurate additions/deletions/modes/binary handling; large-file/tree performance and keyboard navigation | In progress |
| GitHub-quality design | Primer tokens, light/dark/system themes, accessible controls, responsive layouts, navigation and loading/error behavior verified in browser | In progress |
| Team identity and authorization | Real sign-in, sessions, organizations/repositories/membership and permissions; isolation, revocation, CSRF and unauthorized-access tests | In progress: OIDC, sessions and configured read/write grants and repository-scoped Git tokens; administration and provider revocation pending |
| Git hosting | Authenticated smart HTTP fetch/push, branch and tag lifecycle, protected branches, metadata publication and Git CLI round-trip proof | In progress: authenticated fetch, large request encodings and native Git qualification pass; native atomic push, tag lifecycle and exact protected branches have scoped proof; administration pending |
| Collaboration | Persisted issues, pull requests, comments, reviews, labels, assignees, merge/conflict handling, activity and notifications | In progress: issues, pull requests, comments, commit-bound reviews, repository labels and assignment, commit statuses, detailed check runs/logs, required checks and recoverable fast-forward merge with canonical ref publication; merge commits and remaining workflows pending |
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
