# crab-http-server

Crab's repository HTTP application: one Rust server embeds the React UI and
serves repositories from the operator's object storage.

**Development status:** production qualification is incomplete. Read the
[completion matrix](REFERENCE.md#completion-requirements) before deployment.

## Architecture

```text
Browser                    Native Git / Git LFS
   │                              │
   └──────────────┬───────────────┘
                  ▼
          crab-http-server
       auth · routes · lifecycle
                  │
       remote-git / read / write
                  │
             object storage
```

The server owns HTTP policy and application workflows. Shared crates own Git
reading, validation, publication, and storage mechanics. The server uses
writable temporary space for pack/index preparation; it creates no Git checkout
or local Git object database.

## Build and run

Run from the repository root. You need Node 22.12+, Rust, an existing bucket,
and storage credentials. Build the frontend first: `build.rs` embeds its output.

Choose a unique Cargo target directory for this checkout on the mounted workspace
volume, following root `AGENTS.md`. This example uses `/Volumes/Workspace`:

```sh
export CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-http-server-dev
npm ci --prefix packages/repository
npm run build --prefix packages/repository
cargo build -p crab-http-server --release --locked
```

Create a configuration using the [development example](REFERENCE.md#run-the-current-development-build).
Give each repository a distinct bucket/prefix pair and configure storage
credentials through the environment. Then initialize the prefixes and start:

```sh
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml --initialize
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml
```

The bucket must already exist. Initialization can adopt an existing canonical
repository; it does not convert arbitrary objects into a Crab repository.
Without authentication, the server accepts only loopback listeners and trusts
the local operator. Team deployments need [OIDC configuration](REFERENCE.md#team-sign-in)
and a canonical HTTPS origin. For container deployment, use the
[container instructions](REFERENCE.md#run-the-container) and
[example configuration](deploy/server.example.toml).

## Find the relevant contract

| Task | Read |
| --- | --- |
| Configure repositories, temporary space, and credentials | [Development setup](REFERENCE.md#run-the-current-development-build) |
| Deploy, probe readiness, and drain the service | [Container operation](REFERENCE.md#run-the-container) |
| Understand browser APIs and publication ownership | [Application behavior](REFERENCE.md#repository-browser-and-application-apis) |
| Configure identity, membership, sessions, and protection | [Team sign-in](REFERENCE.md#team-sign-in) |
| Clone and fetch with scoped Git tokens | [Git HTTP reads](REFERENCE.md#git-http-reads) |
| Transfer LFS objects | [Git LFS](REFERENCE.md#git-lfs-transfers) |
| Push, handle uncertain publication, and inspect limits | [Native push](REFERENCE.md#native-git-push), [write design](WRITE-DESIGN.md) |
| Work on issues, pull requests, reviews, or merges | [Collaboration](REFERENCE.md#issues-pull-requests-and-reviews) |
| Report CI results or enforce merge checks | [Statuses and checks](REFERENCE.md#commit-statuses-and-required-checks) |

## Work on this crate

Start with [AGENTS.md](AGENTS.md) for entry points, ownership, and invariants.
The main source path is `src/main.rs` → `src/server.rs` → request handlers;
publication continues into `crab-write`. The frontend lives in
[`packages/repository`](../../packages/repository).

Build the frontend before Rust checks. Focused HTTP authentication coverage:

```sh
cargo test -p crab-http-server --locked --lib server::auth_tests
cargo clippy -p crab-http-server --locked --all-targets -- -D warnings
```

Keep `CARGO_TARGET_DIR` set to the checkout's external directory for each shell
invocation. Broader protocol, browser, live-storage, and container qualification
belongs in CI or a dedicated test environment. See the
[verification commands and evidence](REFERENCE.md#current-verification);
component tests alone do not establish production readiness.
