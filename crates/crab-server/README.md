# crab-server

Crab's repository HTTP application: one Rust server embeds the React UI and
serves repositories from the operator's object storage.

The Cargo package and executable are `crab-server`; Rust callers import
`crab_server`. Object-store keys, including `.crab/http-server/v1/`, and
authentication key derivation remain unchanged by the crate rename.

**Development status:** Cellar migration and production qualification are in progress.
Read [the native runtime contract and outstanding release gates](CELLAR.md)
before deployment. Historical runtime receipts do not qualify this implementation.

## Browse indexing

Capsule browse indexing is background work, independent of native Git receive.
Canonical attribution is available without a Cell projection; configured Cells
project one captured snapshot and promote only if its complete source identity
still matches. Missing/stale indexes return HTTP 202 with retry guidance;
corrupt derived records or path metadata return 503 and schedule repair. Native
Git and mutation validation do not load the optional browse-index record.

Browse readers retain verified immutable data while revalidating the root,
transaction-consistent refs, and derived index hashes every two seconds. Ref or
index changes open a new reader. Attribution uses that reader's captured source
identity instead of opening the capsule payloads again for the same request.

Cold browse opens load complete control metadata; Git pack bodies stay in their
authenticated object-store sources and objects use verified bounded range reads.

Repository pages select their metadata with `/api/repos?owner=OWNER&name=NAME`
so unrelated repository Cell reads do not block browsing. The unfiltered catalog
still supplies the repository list and loads Git access scopes when that
selector is opened.

Unchanged browse and background probes share the root/ref/index change probe.
Immutable descriptors are verified on identity changes and at the existing
hourly scrub boundary; explicit repair invalidates the cached proof.

One worker per repository retains completion only for the exact verified source, so idle
polling does not reimport the same Cell epoch. Tree APIs use the verified remote
reader; reconciliation imports only consumed ref, summary and attribution rows.
It streams authenticated path-state pages into provisional Cell epochs without
rereading raw Git commits. Retained summaries cannot authorize a revision: the
foreground reader still checks current ref visibility before querying the Cell.

New repository Cells use one canonical schema containing only consumed commit
summaries and attribution data. Only native Cellar application storage is
supported; no previous-runtime reader or importer is provided.

Projection reads and ingestion borrow runtime-owned Cells like collaboration
requests. Cold read completion does not drain the Cell or wait for ingestion.
Cellar owns retained Cells, idle eviction, pressure policy, and node shutdown.

The current unshipped path-state codec interns raw components per layer and
rejects aggregate writes above the reader budget before publishing any layers.
The full Next.js/RustFS idle and mixed-load qualification remains in progress.

## Architecture

The server embeds the published [Cellar 0.1.0](https://github.com/crabbuild/cellar)
native SDK. Crab owns HTTP authorization and product policy; Cellar owns Cell
execution, leases, routing, SQLite/LTX persistence, and output durability.

Issues, comments, labels, checks, branch protections, pull requests, reviews, releases,
and browse projections use one native Cell per repository. Git objects and the
catalog retain their existing Crab storage paths. Private peer traffic crosses
the existing mTLS boundary. See [CELLAR.md](CELLAR.md) for initialization,
shutdown, provider configuration, and the native storage boundary.

Authenticated repository administrators manage the complete member list in
**Settings → Members** or through the conditional CLI replacement command.
Both paths commit the same catalog revision and durable membership audit event.

OIDC providers can also revoke an identity's Crab browser sessions and derived
Git tokens through the documented back-channel logout endpoint. GitHub OAuth
uses the same server-side browser session contract but does not provide signed
OIDC logout tokens. The exact API, catalog, and identity contracts are in
[Team sign-in](REFERENCE.md#team-sign-in).

## Browser navigation

Repository pages use GitHub-style paths. **Code**, **Issues**, **Pull requests**,
**Activity**, and **Settings** appear in that order; commit history lives under Code.
**Go to file** (keyboard `T`) opens file search, and **Add file** offers the
existing create and upload actions.

Pull requests can be created as drafts, converted to drafts, and marked ready
for review. Draft badges appear in lists and details; merge admission rejects
drafts and stale readiness changes. Authors or writers manage readiness, and
conversion is blocked while a merge is pending.

The reviewer sidebar searches current members by public handle or display name,
selects stable actor UUIDs, and links public profiles. Triage or write authority
is required to request, cancel or re-request reviews. Submitted reviews fulfill
matching requests; stale changes require explicit reload. Fulfillment does not
imply approval or current-head validity. The sidebar includes uninvited reviewers,
keeps approval decisions separate from later comments, and distinguishes an
outdated review from a missing head. Existing discussion indexes require
`cells rebuild-search` for format 9 before serving. Branch protection rules stay separate.
The “Review changes” dialog keeps submission stable while the diff loads and
pins a review to the inspected commit. A changed head requires comparison
reload and explicit draft rebasing; retries use the native submission ledger.

Stable qualified account UUIDs now exist independently of public profiles.
Private `cells register-accounts --identities-file` registration creates no
username or permissions; later enrollment keeps the same UUID. Existing native
Directory format 1 needs explicit `cells migrate-directory`, which preserves
profiles and resumes bounded work after interruption. `cells directory-status`
reports preparation state. Update peers together; repository records and the
search index are unchanged. Assignment and native grant migrations remain work.
See [account preparation](REFERENCE.md#prepare-and-use-account-profiles).

Issue/PR authors share public profile links across lists, details, comments,
reviews and inline conversations. Renaming keeps the actor UUID and historical
source attribution. Prepare Directory before serving these responses; missing
preparation produces a visible error, and profiles grant no access.

Issue and PR lists include a searchable Labels menu with colors, descriptions,
multiple selections, exclusions and a no-label option. Choosing labels requires
all selected labels; Alt+click or Alt+Enter toggles exclusion. The menu stays
open across result refreshes, while stale counts and rows clear. Native choices
preserve other filters and conditional boolean groups. Recent personal label
suggestions remain unfinished.

Issue and PR lists include a searchable Milestone filter with open/closed
choices and a no-milestone option. It preserves boolean queries, sort and
pagination semantics and searches the complete native catalog. The search box
also accepts `milestone:"title"`, `-milestone:"title"` and `no:milestone`, with
native suggestions. Existing indexes need explicit `cells rebuild-search`
preparation for discussion index format 3; canonical collaboration data remains
unchanged.

Search also accepts `author:username`, `-author:username` and `author:@me`.
Usernames resolve through current public profiles; `@me` works before enrollment.
Created by me and Authored by me are native filtered views with URL/reload,
state and sort support. Author suggestions search indexed public handles; names
never act as identities or grants. Format-1/2 indexes need an explicit rebuild
before serving the new lists; update fleet peers together.

Repository members can receive Triage access in Settings → Members. It permits
issue/PR state changes, existing metadata assignment and review requests while
code publication, merging, catalog definitions and administration retain their
separate permissions. Issue state controls use `can_close` independently of
`can_edit`. Existing schema/index data needs no rebuild; deploy peers together
for the updated native edit input. Full role and organization authority parity
remains tracked in the collaboration plan.

Pull request conversations show merge requirements above the comment editor.
Choose a merge method, review the commit message, and confirm the merge; an
uncertain publication can be retried using the original submission.

Request timing widgets are hidden by default. Add `?powerbar=1` to a browser
page (or `&powerbar=1` when it already has a query) to show component timings.
Each timing badge outlines its complete widget; hover the badge to highlight
that boundary. Nested outlines identify nested requests contributing to a view.
Expand a widget to inspect its API URL and the server's `Server-Timing` header.

The displayed duration includes network transfer and JSON parsing, not React
rendering. In-app navigation retains the flag; `powerbar=0` disables it.

| Page | Path after `/{owner}/{name}` |
| --- | --- |
| Repository default branch | Root |
| Directory or file | `/tree/{ref}/{path}`, `/blob/{ref}/{path}` |
| File blame | `/blame/{ref}/{path}` |
| Commit history or detail | `/commits/{ref}/{path?}`, `/commit/{oid}` |
| Issues or new issue | `/issues`, `/issues/{number}`, `/issues/new` |
| Pull requests or detail | `/pulls`, `/pull/{number}` |
| Pull request tabs | `/pull/{number}/commits`, `/pull/{number}/files`, `/pull/{number}/checks` |
| Repository activity | `/activity` |
| New pull request | `/compare/{base}...{head}` |
| Releases or release detail | `/releases`, `/releases/tag/{tag}` |
| Release creation or editing | `/releases/new`, `/releases/edit/{tag}` |
| Branches, tags, or labels | `/branches`, `/tags`, `/labels` |
| Settings | `/settings`, `/settings/branches`, `/settings/access` |
| File creation, upload, editing, deletion | `/new/{ref}/{directory?}`, `/upload/{ref}/{directory?}`, `/edit/{ref}/{path}`, `/delete/{ref}/{path}` |

Account pages live outside repository routes: `/-/profile` edits the signed-in
account's public profile; `/-/users/{actor-uuid}` shows its stable public URL.
The header's profile icon opens account settings. Initialize the shared directory
before serving profiles or nonempty issue/PR lists, using
`cells initialize-directory` against the running private mTLS listener. Existing assignment data also requires explicit
`cells migrate-assignments` and a current-format `cells rebuild-search`; nonempty records
need qualified-identity mappings. See [assignment preparation](CELLAR.md#assignment-preparation).
Users choose a username and display name; authentication claims are
never published as profile fields. Changing a profile grants no repository or
organization access. `-` is reserved as a server route owner.
Issue/PR list authors link enrolled profiles by stable UUID and show their current
handle; unenrolled authors keep their recorded display name. Detail/comment/
review attribution and additional participant predicates remain unfinished.
Assignee metadata links public profiles; list filters support usernames, `@me`,
`*`, `no:assignee` and explicit `actor/UUID` values. The searchable Assignee menu
and assigned-to-me sidebar use native queries. Existing derived indexes require
a format-8 rebuild; canonical assignments remain format 2.
PR lists also support `review-requested`, `reviewed-by`, `review-involves` and
`user-review-requested` with a public username or `@me`. The Reviews filter and
Review requests sidebar use those native relationships; submitted reviews leave
pending results and retain review participation.
`commenter:username` and `commenter:@me` find conversation comments, submitted
reviews (including empty approvals), inline threads and replies. Opening an issue
or PR alone does not count. Public username completion uses the same current
qualified identity as author/reviewer filters. Mentions and `involves` remain unfinished.
Text searches include titles, descriptions and all five contribution types. Use
`in:title`, `in:body`, `in:comments`, or `in:title,body` to select fields. Native
FTS updates with comment edits and deletion cascades. Author names require the
explicit `author:` filter.
Issue and PR search also accepts `comments`, `reactions`, and `interactions`
counts: `comments:>=3`, `reactions:0`, `interactions:5..20`, `comments:10..*`,
or `-reactions:>0`. Interactions sum conversation comments and reactions on the
issue/PR itself. Inline review replies and reactions to individual comments are
not included. These filters use the existing transactional index without a new
preparation step; search completion offers examples.

Generated links encode a slash-containing ref as one URL segment. Pasted paths
with unescaped ref slashes resolve against the longest known ref. Raw Git
filename bytes are percent-encoded without UTF-8 replacement; literal percent
signs remain distinct from escaped bytes. A tag sharing a branch name retains
its full ref namespace.

Named-ref navigation keeps its branch or tag while data
reads remain pinned to the resolved commit OID. On a file page, keyboard `Y`
replaces the ref with that OID, preserving the path and line fragment, following
[GitHub's permanent file link convention](https://docs.github.com/en/repositories/working-with-files/using-files/getting-permanent-links-to-files).

Deep links survive direct reloads through the embedded asset handler; service
and asset namespaces retain their own 404 behavior. List filters and pagination
stay in query parameters.

Browser navigation query selectors (such as `view`,
`rev`, and `path`) and numeric release pages are rejected with “Page not found”.
Release pages address tags directly. The release tag API uses the existing
unique tag index and the same repository permissions and draft visibility as
numbered reads.

## Build and run

For a new S3-backed Cell deployment that requires every acknowledged mutation
to have an exact object root, set `durability = "object"` in `[cells]` on every
node. This uses the existing object publication proof and does not recruit
durability-log followers. The default `fleet` profile can acknowledge a
follower-fsynced cut before the object root is published.

Switching a running fleet requires draining its old owner sessions, proving object coverage of
their accepted cuts, and retiring their logs before relying on the all-node-loss
contract.

Reads use the authoritative owner; configurable replicas and explicit replica
reads are retired in the Cellar migration.

For the fastest local start, use Docker Engine with Compose v2:

```sh
docker compose --file crates/crab-server/deploy/compose.yaml \
  up --detach --build --wait
```

Then open <http://127.0.0.1:8788/demo/hello>. The stack creates a persistent
local object store and demo repository automatically. See the
[Compose operations guide](deploy/README.md#start-locally-with-docker-compose)
for repository commands, configuration overrides, and safe teardown.

To build outside Docker, run from the repository root. You need Node 22.12+,
Rust, an existing bucket, and storage credentials. Build the frontend first:
`build.rs` embeds its output.

Choose a unique Cargo target directory for this checkout on the mounted workspace
volume, following root `AGENTS.md`. This example uses `/Volumes/Workspace`:

```sh
export CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-server-dev
npm ci --prefix packages/ui
npm run build --prefix packages/ui
cargo build -p crab-server --release --locked
```

Create a configuration using the
[development example](REFERENCE.md#run-the-current-development-build). Select
one `s3://`, `gs://`, or `az://` storage root and configure ambient workload
credentials. Then create a cataloged repository and start:

```sh
"$CARGO_TARGET_DIR/release/crab-server" --config /path/to/server.toml \
  repository create --owner team --name project --prefix team/project
"$CARGO_TARGET_DIR/release/crab-server" --config /path/to/server.toml \
  storage-probe
"$CARGO_TARGET_DIR/release/crab-server" --config /path/to/server.toml \
  cells capacity --json
"$CARGO_TARGET_DIR/release/crab-server" --config /path/to/server.toml serve
```

`repository create` publishes the initial native Cell before marking its catalog
entry ready. `repository adopt` initializes an empty collaboration Cell for an
existing Git repository; it does not import application data from another runtime.
`cells capacity --json` reports admission limits. `cells runtime` reports the
native runtime version. Existing native repositories require
`cells rebuild-search --owner OWNER --name NAME` against the running private
mTLS listener before admitting traffic with the indexed discussion lists. The
command resumes an interrupted rebuild without changing domain records.
Before serving account profiles or nonempty issue/PR lists, run
`cells initialize-directory` against that same running listener. It initializes
a separate native Directory and leaves
repository records unchanged; users enroll their own profiles afterward.
The supported management surface and retired commands
are listed in [CELLAR.md](CELLAR.md).

The bucket or container must already exist. `repository adopt` can publish an
existing canonical repository; it does not convert arbitrary objects into a
Crab repository. Every replica discovers catalog changes without a restart.

Without authentication, the server accepts only loopback listeners and trusts
the local operator. Team deployments need [OIDC or GitHub OAuth configuration](REFERENCE.md#team-sign-in)
and a canonical HTTPS origin. For container deployment, use the
[deployment profiles](deploy/README.md),
[container instructions](REFERENCE.md#run-the-container), and
[example configuration](deploy/server.example.toml).

## Find the relevant contract

Read [CELLAR.md](CELLAR.md) for the native runtime, storage, and lifecycle
boundary. [DESIGN.md](DESIGN.md) covers Git publication ownership.

| Task | Read |
| --- | --- |
| Configure catalog, storage root, temporary space, and credentials | [Development setup](REFERENCE.md#run-the-current-development-build) |
| Start locally with Docker Compose | [Local Compose stack](deploy/README.md#start-locally-with-docker-compose) |
| Deploy on EKS, GKE, AKS, or ECS | [Deployment profiles](deploy/README.md) |
| Probe readiness and drain the service | [Container operation](REFERENCE.md#run-the-container) |
| Scrape metrics and define alerts | [Operations runbook](deploy/operations.md#observe-requests-and-capacity) |
| Understand browser APIs and publication ownership | [Application behavior](REFERENCE.md#repository-browser-and-application-apis) |
| Configure identity, membership, sessions, and protection | [Team sign-in](REFERENCE.md#team-sign-in) |
| Clone and fetch with scoped Git tokens | [Git HTTP reads](REFERENCE.md#git-http-reads) |
| Transfer LFS objects | [Git LFS](REFERENCE.md#git-lfs-transfers) |
| Push, handle uncertain publication, and inspect limits | [Native push](REFERENCE.md#native-git-push), [write design](DESIGN.md) |
| Work on issues, pull requests, reviews, or merges | [Collaboration](REFERENCE.md#issues-pull-requests-and-reviews) |
| Report CI results or enforce merge checks | [Statuses and checks](REFERENCE.md#commit-statuses-and-required-checks) |

Pull request Files changed views support durable inline review threads in
addition to the existing summary review. Members can anchor a comment to an
old or new line range, reply and edit their own messages, resolve or reopen a
conversation, and keep exact-commit conversations visible when a later push
makes them outdated. New-side suggestions use the existing exact-head
`/contents` mutation, so applying one creates the normal Git commit and keeps
branch/blob conflicts authoritative.

## Trace issue creation

Use `POST /api/repos/{owner}/{name}/issues` to follow a collaboration request.
Read these sources in order:

| Source | Responsibility |
| --- | --- |
| [server/http/mod.rs](src/server/http/mod.rs) | Route composition |
| [server/http/boundary.rs](src/server/http/boundary.rs) | Host/session checks and mutation protection |
| [api/issues/mod.rs](src/api/issues/mod.rs) | Route body limit, request extraction, HTTP contract, and typed command/query invocation |
| [app.rs](src/app.rs) | Admission timeout, repository access, input validation, and HTTP error mapping |
| [cells/client.rs](src/cells/client.rs) | Typed requests through native ingress |
| [cells/application.rs](src/cells/application.rs) | Transaction, replay receipts, and native output gate |
| [cells/repository/issues.rs](src/cells/repository/issues.rs) | SQLite transaction, submission reservation, number allocation, and typed outcome |
| [Cellar SDK](https://github.com/crabbuild/cellar) | Native ownership, persistence, recovery, and output durability |

Example JSON body, subject to the server's authentication and mutation checks:

```json
{
  "request_id": "01931b9e-4b3c-7b2a-b9f0-0123456789ab",
  "title": "Document the repository setup",
  "body": "Include prerequisites and a minimal configuration."
}
```

Generate a fresh submission ID for a new issue. If the response is lost, retry
with the same ID and original title/body: the reservation preserves the issue
number. Reusing that ID with different content or another author is a conflict.

The Cell command commits and publishes before the handler loads label and
assignee presentation data. A lost or failed HTTP response therefore does not
prove that the write failed. Retry with the same submission ID and payload; a
new ID represents a new mutation. Unknown Cell outcomes remain distinguishable
from unavailable routing and contract failures in the HTTP error mapping.

## Admission ownership

A handler returning a response and a client finishing its response body are
different lifecycle events:

| Surface | Ownership |
| --- | --- |
| Collaboration middleware in [app.rs](src/app.rs) | Holds an application slot while producing the response; applies a 30-second handler timeout |
| Release download in [releases.rs](src/api/releases/mod.rs) | Moves a separate transfer permit and cancellation guard into the response stream |

Keep body-stream resources with the stream. The middleware timeout cannot stand
in for transfer deadlines or worker cleanup after a response has been returned.
The production server initializes eight process-local application slots and
four deployment-wide Git/LFS/archive/release transfer slots in
[server/mod.rs](src/server/mod.rs); test fixtures use smaller limits.

Archive downloads in [archive.rs](src/api/archive.rs) use a channel-backed ZIP body.
Traversal cancellation must fail that body: finalizing ZIP state for cleanup
must not turn an incomplete archive into a successful download.

## Work on this crate

Start with [AGENTS.md](AGENTS.md) for entry points, ownership, and invariants.
The main source path is `src/main.rs` → `src/cli/mod.rs` → `src/server/mod.rs` → request handlers;
publication continues into `crab-write`. The frontend lives in
[`packages/ui`](../../packages/ui).

API, Git, and authentication owners assemble their own routes;
`server/http/mod.rs` applies the common security boundary after merging them.

Components with children use a single `<component>/mod.rs` entry point. Tests
live with their owner, including the HTTP authorization suites under
`api/<feature>/tests.rs` and `auth/<feature>/tests.rs`. Shared setup lives in
`test_support/`; HTTP fixtures and their deterministic identity provider are
under `test_support/http/`. Tests never import another feature's test suite.

The internal owners are:

| Owner | Responsibility |
| --- | --- |
| `cli/` | Command parsing/dispatch, management client, and ECS address discovery |
| `server/mod.rs` | Startup, shared application state, and ordered shutdown |
| `server/background.rs` | Catalog refresh and fair projection scheduling |
| `server/management/mod.rs`, `server/capacity.rs`, `server/storage.rs` | Private management, resource policy, and storage preflight |
| `repository/` | Pinned Git snapshots, maintenance, browse projections, and atomic catalog installation |
| `catalog/` | Durable catalog records and membership audit |
| `auth/` | Identity, durable sessions, UUID-bound current grants in `principal/`, login endpoints, and bounded provider transport |
| `git_import/mod.rs`, `git_import/source.rs`, `git_import/git.rs` | Import jobs/workflow, source validation, and isolated Git processes |
| `api/` | Browse HTTP orchestration, feature handlers, Git queries, and wire encoding |
| `git/` | Native Git protocol, object, receive, and LFS endpoints |
| `peer/`, `peer/tls/` | Private transport and mutual TLS |
| `app.rs` | Collaboration admission, authorization, mutation identity, and error mapping |
| `cells/` | Native ingress and durable transaction/replay boundary |
| `cells/repository/` | Typed domain facade and shared repository invariants |
| `cells/repository/pulls/` | Pull records, comments, reviews, merge commands, and review threads |
| `cells/repository/search/` | Query grammar, completion, transactional search projection, sorting/counts, and explicit index preparation |
| `cells/repository/{checks,settings,releases,labels,statuses,projection}/` | Feature commands, local persistence/validation, and tests |

Authentication always uses shared durable state, including in-memory object-store
test fixtures; there is no separate process-local authentication mode. Repository
construction initializes private cache and maintenance state. Settings
policy stays in `repository/settings.rs`; repository reads do not forward HTTP
settings operations. HTTP mutation families share one request identity policy in
`app.rs`; projection retains its own non-HTTP error contract.

Build the frontend before Rust checks. Focused HTTP authentication coverage:

```sh
cargo test -p crab-server --locked --lib auth::http::tests
cargo clippy -p crab-server --locked --all-targets -- -D warnings
```

Keep `CARGO_TARGET_DIR` set to the checkout's external directory for each shell
invocation. Broader protocol, browser, live-storage, and container qualification
belongs in CI or a dedicated test environment. See the
[verification commands and evidence](REFERENCE.md#current-verification);
component tests alone do not establish production readiness.
