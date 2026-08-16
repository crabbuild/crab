# Plan 001: Harden Crab's Git object-store integration

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. Each phase is an independent landing unit; do not combine phases
> into one unreviewable commit. If anything in the STOP conditions occurs,
> stop and report instead of improvising. When all phases are done, update the
> status row in `plans/README.md` unless the reviewer maintains the index.
>
> **Drift check (run first)**:
>
> ```bash
> git diff --stat ea30d7ed..HEAD -- \
>   crab/src/git/remote_helper.rs \
>   crab/src/git/fetch.rs \
>   crab/src/git/fetch_transport.rs \
>   crab/src/git/push_native.rs \
>   crates/crab-git/src/tag.rs \
>   crab/tests/remote_helper_transcript.rs \
>   crab/tests/e2e_add_commit_push.rs \
>   crab/tests/v2_fetch_transport.rs \
>   crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py \
>   crab/docs/architecture/git-integration.md \
>   crab/docs/guides/clone.md \
>   crab/docs/design/technical-design.md
> ```
>
> If an in-scope file changed, compare the current-state excerpts below with
> live code. Any semantic mismatch is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: L, delivered as five independently mergeable phases
- **Risk**: HIGH overall; MED per phase
- **Depends on**: none
- **Category**: bug / tests / tech-debt / docs
- **Planned at**: commit `ea30d7ed`, 2026-08-14

## Outcome

After this plan lands, direct `crab://bucket/repository` remotes backed by S3
or an S3-compatible object store will have a fail-closed, truthfully advertised
Git contract:

1. A missing manifest means a new empty repository; corrupt, unreadable, or
   transiently unavailable state is an error.
2. Every advertised remote-helper option has tested production semantics.
3. `--follow-tags` either publishes the complete requested tag set in the same
   manifest CAS as the branch or rejects the push before publication.
4. The previously ignored v1 helper transcript cases run deterministically
   without real credentials or network access.
5. A manual RustFS matrix proves success and important failure behavior through
   ordinary Git, the Crab CLI, a fresh process, and byte-identical hydration.
6. Documentation distinguishes Git's filter-process protocol v2 from Git wire
   protocol v2 and lists the exact supported clone/fetch/push surface.

This plan does not add another publication path, object-store layout, public
configuration knob, compatibility alias, or fallback. Existing manifest CAS,
lock ownership, immutable upload ordering, and hydration remain canonical.

## Why this matters

The normal object-store workflow is already real: `crab add`/`crab push` and
ordinary `git add`/`git push` both complete against RustFS, and a fresh clone
hydrates byte-identically. The remaining highest-impact defect is a false
success at the repository visibility boundary: malformed manifest bytes cause
`git ls-remote` and `git clone` to report a successful empty repository. That
can hide an existing repository during storage corruption or a read failure.

The helper also advertises `filter` and acknowledges `blob:none` while still
downloading complete packs. Three canonical v1 transcript cases and four
protocol-v2 placeholders are ignored. `--follow-tags` is acknowledged but can
silently drop tags on discovery or resolution errors. These behaviors prevent
a defensible “complete and gap-free” integration claim even though the happy
path works.

## Current state

### Entry point and owner boundaries

- `crab/src/main.rs:2857` dispatches `git-remote-crab <remote> <url>` into
  `run_remote_helper`.
- `crab/src/git/remote_helper.rs:562` owns the helper protocol loop and store
  resolution. Its `Batch::List` branch at `crab/src/git/remote_helper.rs:986`
  advertises repository refs.
- `crab/src/git/fetch.rs:155` installs manifest-selected Git packs and validates
  requested tips.
- `crab/src/git/push_native.rs:247` adapts ordinary Git push discovery into the
  shared publication pipeline in `crab/src/git/push.rs`.
- `crates/crab-storage/src/store.rs:347` owns strict conditional create and
  `crates/crab-storage/src/store.rs:521` owns ETag-guarded update. Do not weaken
  these primitives.
- `crab/src/cmd/clone.rs:156` performs clone-without-checkout, installs Crab
  configuration, then checks out and optionally hydrates through the real
  `ShardHydrator`.

### Gap 1: repository advertisement converts errors into empty state

`crab/src/git/remote_helper.rs:1685` currently returns `ListOutput`, not a
`Result`:

```rust
match read_remote_refs(store, router, hidden_ref_patterns).await {
    Ok(output) => output,
    Err(CrabError::NotFound { path }) if path == router.manifest_path().as_ref() => {
        ListOutput { refs: Vec::new(), head_symref: None }
    }
    Err(e) => {
        tracing::warn!(error = %e, "failed to read remote refs");
        ListOutput { refs: Vec::new(), head_symref: None }
    }
}
```

The final branch is unsafe. A live RustFS probe with an empty/corrupt manifest
made `git ls-remote` exit 0 with no refs and made `git clone` exit 0 with an
“empty repository” warning.

The correct local pattern already exists in push:
`crab/src/git/push.rs:5162` treats only `CrabError::NotFound` as a first push and
propagates every other error.

### Gap 2: the helper advertises an unapplied partial-clone filter

`crab/src/git/remote_helper.rs:856` replies `ok` to `option filter blob:none`
and stores `FilterSpec::BlobNone`. `format_capabilities` advertises `filter`
when a commit graph is present at `crab/src/git/remote_helper.rs:1631`.

`crab/src/git/fetch.rs:170` selects shallow versus full fetch solely from
`fetch_options.depth`; `fetch_options.filter` does not affect pack selection.
The comment at `crab/src/git/fetch.rs:194` explicitly says complete packs are
still downloaded. The partial-clone design remains open at
`crab/docs/design/technical-design.md:2278`.

### Gap 3: acknowledged follow-tags behavior is best effort

`crab/src/git/push_native.rs:397` treats a failed remote manifest read as an
empty remote. `crab/src/git/push_native.rs:428` warns and drops synthesized tag
specs when ref resolution fails. `collect_followtag_specs` returns an empty list
on tag-store errors at `crab/src/git/push_native.rs:680`.

`crates/crab-git/src/tag.rs:50` is the tag-discovery owner. It distinguishes
repository/ref-store open errors, but currently skips unreadable refs, missing
objects, malformed tag objects, and excessive tag nesting. Lightweight tags
and tags targeting non-commits are legitimate non-candidates; damaged
annotated tags are errors when the caller explicitly requested follow-tags.

The positive structural pattern is
`crab/tests/e2e_add_commit_push.rs:185`, which proves an annotated tag and its
branch land together in one in-memory manifest.

### Gap 4: helper protocol coverage is incomplete

`crab/tests/remote_helper_transcript.rs:23` calls the public production helper,
which constructs a real S3 store from `crab://bucket/repo`. Consequently these
tests are ignored:

- `fetch_batch_transcript` at `crab/tests/remote_helper_transcript.rs:105`;
- `push_batch_transcript` at `crab/tests/remote_helper_transcript.rs:114`;
- `multi_command_session_transcript` at
  `crab/tests/remote_helper_transcript.rs:140`.

Tests inside `crab/src/git/remote_helper.rs` already use in-memory stores and
private dispatch helpers. The production function needs one narrow internal
context seam so the complete protocol loop and the same dispatcher can be
tested without publishing a test-only API or contacting S3.

### Gap 5: protocol-v2 scaffold is not a production path

`crab/src/git/fetch_transport.rs:1` says the module is a scaffold. Its
`StdioTransport::handshake` and `request` return
`AuthenticationUnsupported` at `crab/src/git/fetch_transport.rs:393`.
`format_capabilities` correctly does not advertise `connect` or
`stateless-connect` at `crab/src/git/remote_helper.rs:1611`.

Four placeholder tests are ignored in `crab/tests/v2_fetch_transport.rs:45`.
The `gix-transport` Cargo feature and public module exist in released tags, so
do not remove or make them private without an explicit shipped-API decision.
For this plan, replace aspirational ignored tests with executable tests of the
current explicit-unsupported contract; do not create a second fetch path.

### Existing verification and design constraints

- `crab/tests/e2e_add_commit_push.rs` exercises both add routes, native push,
  follow-tags, hydration, and in-memory object storage.
- `crab/tests/e2e_fetch_fsck.rs` exercises fetch/fsck integration.
- `crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py:595` exercises both
  `crab add`/`crab push` and `git add`/`git push`, verifies pointer staging,
  conditional S3 writes, xorb/shard creation, fresh clone, hydration, and
  byte identity.
- `openspec/changes/optimize-add-push-performance/design.md` explicitly keeps
  that RustFS runner manual. Do not add a Make target or GitHub workflow.
- Rust errors use `CrabError`, preserve their sources, and propagate with `?`.
  Match `crab/src/git/push.rs:5162`; do not stringify storage errors.
- Production Rust must not use `unwrap`, `expect`, or `panic`.

## Commands you will need

Create a unique target directory for the executor's checkout. Never reuse the
directory named below across worktrees.

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Workspace preflight | `test -d /Volumes/Workspace && mkdir -p /Volumes/Workspace/crabbuild-target/crab-001-git-integration && test -w /Volumes/Workspace/crabbuild-target/crab-001-git-integration` | exit 0 |
| Format check | `cargo fmt --all --check` | exit 0, no diff |
| Focused helper tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration cargo test -p crab --locked --test remote_helper_transcript --test e2e_add_commit_push --test e2e_fetch_fsck` | all tests pass, zero ignored in `remote_helper_transcript` |
| Feature contract | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration cargo test -p crab --locked --features gix-transport --test v2_fetch_transport` | all tests pass, zero ignored |
| Crab Git library tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration cargo test -p crab --locked --lib git::` | all selected tests pass |
| Tag owner tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration cargo test -p crab-git --locked tag` | all selected tests pass |
| Clippy | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration cargo clippy -p crab -p crab-git --all-targets --locked -- -D warnings` | exit 0, no warnings in touched code |
| Release build | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration cargo build -p crab --release --locked` | release binary exists under the selected external target |
| Live qualification | `python3 crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py --root /Volumes/Workspace/CrabCLI --run-id git-integration-<commit> --crab-bin /Volumes/Workspace/crabbuild-target/crab-001-git-integration/release/crab --size-mib 64` | report status `passed`, no failed checks |

The live command assumes ignored runtime AWS credentials and endpoint variables
are already set. It must not print credentials or pass secret values as command
arguments. Build/install rules still apply: use `make install` when installing
for a user-facing test, not `cargo install` or manual binary copies.

## Scope

**In scope — modify only when required by the corresponding phase:**

- `crab/src/git/remote_helper.rs`
- `crab/src/git/fetch.rs`
- `crab/src/git/fetch_transport.rs`
- `crab/src/git/push_native.rs`
- `crates/crab-git/src/tag.rs`
- `crab/tests/remote_helper_transcript.rs`
- `crab/tests/e2e_add_commit_push.rs`
- `crab/tests/e2e_fetch_fsck.rs`
- `crab/tests/v2_fetch_transport.rs`
- `crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py`
- focused tests for the smoke runner, if such a test file already exists;
  otherwise create `crab/scripts/e2e/test_run_add_commit_push_rustfs_smoke.py`
- `crab/docs/architecture/git-integration.md`
- `crab/docs/guides/clone.md`
- `crab/docs/design/technical-design.md`
- `plans/README.md` status only

**Out of scope — do not touch:**

- Manifest schema or paths, Git pack format, pointer syntax, xorb/shard format,
  or storage layout.
- Manifest CAS, distributed-lock, staging-retirement, GC, or hydration
  algorithms except to preserve and test their existing contracts.
- Managed-service auth, protected-push, active-active replication, cache-service,
  NFS/FUSE, desktop, SDK, or web code.
- New config keys, environment variables, compatibility aliases, fallback
  readers, or legacy protocol paths. The only compatibility retention allowed
  is a time-bounded deprecation for a public Rust API proven present in a
  release tag.
- New dependencies or Cargo dependency patches. A required dependency change is
  a STOP condition and needs explicit approval plus upstream contract proof.
- Git submodules, Git SHA-256 repositories, LFS import, smart HTTP, or SSH.
- Make/GitHub automation for the manual RustFS smoke.

Shared helper changes must still run existing managed/protected tests because
those callers share `remote_helper.rs`; do not change their authorization or
store-resolution behavior.

## Git workflow

- Suggested branch: `advisor/001-harden-crab-git-integration`.
- Use one conventional commit per phase, for example
  `fix(git): fail closed on manifest advertisement errors`.
- Run `cargo fmt --all` before each commit.
- Check `git diff --numstat` after each phase. Remove superseded branches and
  tests; do not leave a second production path.
- Do not push or open a PR unless the operator explicitly asks.

## Phase 1: Make repository advertisement fail closed

### Step 1.1: Characterize all three manifest outcomes

In the private test module for `crab/src/git/remote_helper.rs`, add tests using
`object_store::memory::InMemory` for:

1. missing `{repo}/manifest` returns an empty `ListOutput`;
2. a valid manifest advertises its HEAD and refs;
3. empty or malformed manifest bytes return an error;
4. an injected non-NotFound store read error returns the original sourced
   `CrabError` rather than empty refs.

Name tests by behavior, for example
`corrupt_manifest_rejects_ref_advertisement`. Do not assert only on log text.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration \
  cargo test -p crab --locked --lib read_remote_refs_for_advertisement
```

Expected: the new corrupt/error cases fail before the source fix and pass after
Step 1.2; the missing-manifest case remains green.

### Step 1.2: Return `Result<ListOutput>` and propagate errors

Change `read_remote_refs_for_advertisement` to return `Result<ListOutput>`.
Retain exactly one empty-repository branch: `CrabError::NotFound` whose path is
the routed manifest path. Propagate all other errors unchanged with their
sources.

Update `dispatch_batch`'s `Batch::List` branch to use `?` before
`filter_list_for_push`. If the helper has no store because store setup failed,
preserve the existing setup error rather than synthesizing empty refs. Inspect
the surrounding resolved-store error path before editing; one failed store
construction must not be converted into successful `list` output.

Apply the same fail-closed rule to the follow-tags base-manifest read in
Phase 3 rather than adding a new fallback here.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration \
  cargo test -p crab --locked --lib git::remote_helper
```

Expected: all remote-helper unit tests pass. Missing manifest still advertises
an empty repository; malformed manifest and injected storage failure return
errors.

## Phase 2: Make the complete helper protocol deterministically testable

### Step 2.1: Split resolution from the protocol loop at one internal seam

Refactor `run_remote_helper` into two ownership stages without changing its
public signature:

1. production setup resolves the repository/store, caching store, staging,
   config, progress mode, and managed repository exactly once;
2. a private runner owns stdin/stdout batching and calls the existing
   `dispatch_batch` with that resolved context.

Use one private context struct only if it reduces the current argument list and
is consumed by both production and tests. Do not add a public constructor,
test-only production feature, global mutable override, environment-variable
hook, or duplicate dispatcher.

Keep resolution/auth tests around the public `run_remote_helper`; use the
private runner only for deterministic protocol transcript tests inside the
module's test subtree.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration \
  cargo test -p crab --locked --lib git::remote_helper
```

Expected: existing production setup tests and private dispatch tests pass.

### Step 2.2: Replace the three ignored v1 transcript cases

Move only the cases needing private context into a child test module of
`remote_helper.rs`; a separate `crab/src/git/remote_helper/transcript_tests.rs`
is acceptable because the owner module is already large. Keep public pure
formatting snapshots in `crab/tests/remote_helper_transcript.rs`.

Build a temporary real Git repository and in-memory store. Exercise:

- capabilities followed by list in one session;
- fetch of a real advertised ref and installation of its pack;
- push of a real local branch and a second ref in one batch;
- a multi-command capabilities/list/push session;
- per-ref rejection output for malformed, non-fast-forward, and atomic-abort
  cases;
- cancellation and store failure exiting non-zero without protocol success.

The in-process harness proves exact helper bytes and returned errors. The real
`git ls-remote`/clone process regression remains in the isolated RustFS matrix
in Phase 5 because a spawned Git process cannot consume the in-memory store
without adding a test-only public transport.

Delete the three ignored placeholder tests once equivalent deterministic cases
exist. Do not merely change snapshots to the current failures.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration \
  cargo test -p crab --locked --test remote_helper_transcript
rg -n '#\[ignore' crab/tests/remote_helper_transcript.rs crab/src/git/remote_helper
```

Expected: transcript test exits 0; the `rg` command returns no ignored v1 helper
transcript cases.

## Phase 3: Make advertised options truthful

### Step 3.1: Stop advertising and accepting unapplied `blob:none`

Remove `filter` from `format_capabilities`. Make `option filter blob:none`
reply `unsupported`, matching other unsupported filter specs. Retain shallow
`depth` semantics unchanged.

`FilterSpec` and `FetchOptions.filter` are public under the `crab` library and
exist in release `v1.0.14`. Preserve that shipped Rust API for the current major
version with an explicit deprecation/removal note, but make
`run_fetch_batch` reject `filter.is_some()` before creating directories or
downloading packs. Remove internal logging and branches that imply the filter
was applied. A future major version may delete the deprecated types; do not add
an alias or second field.

Do not reinterpret `fetch.object_level_filtering` as partial clone: it currently
operates after whole-pack selection and is a separate optimization surface.
If internal filter-only helpers become dead, delete them. The deprecated public
field is the sole allowed compatibility retention and must never silently
degrade to a full fetch.

Add tests proving capabilities never contain a line exactly equal to `filter`
and both `blob:none` and another filter spec receive `unsupported`.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration \
  cargo test -p crab --locked --lib git::
rg -n 'caps\.push_str.*filter|options\.fetch_options\.filter =|filter = \?fetch_options\.filter' \
  crab/src crab/tests
```

Expected: tests pass and `rg` returns no matches. Separate tests prove the
deprecated public `FetchOptions.filter` fails before I/O.

### Step 3.2: Prove or reject `include-tag`

Add a real-Git in-memory integration case with an annotated remote tag pointing
at a fetched commit. Drive the same helper option sequence Git uses and assert
the tag object and expected local tag ref behavior.

- If whole-manifest pack installation plus Git's own ref update already
  satisfies `include-tag`, keep a stateless `ok` acknowledgement with the
  regression test. `HelperOptions.include_tag` is public in `v1.0.14`; mark the
  unused stored field deprecated for removal in the next major rather than
  removing it here.
- If the tag ref does not materialize, change the option response to
  `unsupported` and deprecate the stored field. Do not add bespoke tag-ref
  mutation in this phase.

**Verify**: focused helper and fetch integration tests pass with a meaningful
tag assertion, not just an `ok` protocol line.

## Phase 4: Make follow-tags atomic and explicit

### Step 4.1: Give tag discovery a strict mode

In `crates/crab-git/src/tag.rs`, retain the current tolerant discovery API only
if an existing non-follow-tags caller needs it. Add the smallest strict API or
typed policy needed for follow-tags to distinguish:

- legitimate non-candidates: lightweight tags, symbolic tag refs, and
  annotated tags whose final target is not a commit;
- errors: unreadable tag refs, missing/corrupt tag objects, decode failures,
  object-read failures, and recursion-limit exhaustion.

Preserve the original `TagPeelError` source chain. Do not stringify gix errors
or hardcode tag names from tests.

If no caller needs tolerant behavior, replace it with one strict canonical
path and delete the tolerant branches.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-001-git-integration \
  cargo test -p crab-git --locked tag
```

Expected: tests distinguish lightweight/non-commit exclusion from damaged-tag
errors.

### Step 4.2: Propagate follow-tags discovery and resolution failures

Change `collect_followtag_specs` to return `Result<Vec<PushSpec>>`. In
`run_native_push`:

- propagate base-manifest read errors instead of treating the remote as empty;
- propagate strict tag-discovery errors;
- propagate synthesized-ref resolution errors;
- construct the complete effective spec set before acquiring its final lock
  set;
- keep branch and synthesized tag publication in one existing manifest CAS;
- release any pre-acquired leases on every new error path.

Do not weaken protected/service-owned push's existing explicit rejection of
follow-tags. Do not let a branch publish when the requested synthesized tag set
could not be determined.

Add tests for successful annotated tag, lightweight tag exclusion, malformed
tag failure, manifest-read failure, cancellation, and lock release. Assert that
the manifest is unchanged on every failure.

**Verify**: run `e2e_add_commit_push`, helper transcript tests, tag owner tests,
and the push-native library slice.

Expected: a successful follow-tags result contains both branch and tag; every
discovery/resolution error causes zero manifest publication.

## Phase 5: Replace aspirational protocol tests and strengthen live evidence

### Step 5.1: Make the shipped `gix-transport` scaffold contract executable

Do not advertise `connect` or `stateless-connect`. Replace the four ignored
placeholder tests in `crab/tests/v2_fetch_transport.rs` with non-ignored tests
that prove the current released contract:

- normal helper capabilities omit both keywords with and without the feature;
- `StdioTransport` reports only protocol v2 and stateless request lifetime;
- `handshake` and `request` return the documented explicit unsupported error;
- typed ref formatting remains byte-identical to the canonical list formatter;
- gix refspec parsing matches supported force, update, and delete forms.

Rename tests and module prose so they do not claim an end-to-end v2 session
exists. Do not expose a new capability or add a second pack-install pipeline.

Before removing any public scaffold type, check released tags and stop for an
explicit compatibility decision. The `gix-transport` feature and module exist
in `v1.0.14`.

**Verify**: run the feature-contract command from the command table.

Expected: all feature tests pass with zero ignored tests.

### Step 5.2: Extend the manual RustFS qualification matrix

Extend the existing runner rather than creating a competing script. Preserve
its unique prefix, no bucket-wide deletion, secret redaction, structured report,
conditional-write probes, fresh clone, and hash verification.

Add cases for:

1. missing manifest: `git ls-remote` succeeds with no refs;
2. malformed manifest under a unique test prefix: `git ls-remote` and clone
   fail non-zero;
3. ordinary branch update followed by fetch in a second existing clone;
4. branch deletion and fresh clone/list visibility;
5. force push and non-fast-forward rejection;
6. atomic two-ref rejection with neither ref changed;
7. `--follow-tags` success with tag-object SHA verification;
8. shallow clone/fetch and unshallow;
9. missing/corrupt immutable pack or index fails before ref update or checkout;
10. concurrent push CAS conflict with exactly one coherent winning manifest.

Use deterministic small Git objects for protocol cases and retain the 64 MiB
tracked duplicate payload for multipart/dedup/hydration proof. Every destructive
fault must target only the run's unique prefix. Restore nothing by overwriting a
valid repository; create isolated fault fixtures instead.

Change credential defaults to read standard ignored runtime AWS environment
variables before development-only defaults, so credentials never need to be
passed on the command line. Continue redacting report environment and command
records. Add Python unit tests for credential selection and redaction without
using live values.

Do not add Make or GitHub workflow wiring.

**Verify**:

```bash
python3 -m unittest crab/scripts/e2e/test_run_add_commit_push_rustfs_smoke.py
python3 crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py \
  --root /Volumes/Workspace/CrabCLI \
  --run-id git-integration-<commit> \
  --crab-bin /Volumes/Workspace/crabbuild-target/crab-001-git-integration/release/crab \
  --size-mib 64
```

Expected: unit tests pass; live report status is `passed`; every case records a
real Git/Crab command, expected S3 side effect, and visible result. Negative
cases must record the expected non-zero command status rather than aborting the
runner as an unexpected failure.

### Step 5.3: Publish an exact support matrix in docs

Update `crab/docs/architecture/git-integration.md` and
`crab/docs/guides/clone.md` with one table covering:

- list, clone, fetch, push, force, delete, atomic, follow-tags, include-tag,
  shallow, unshallow, lazy checkout, hydration, and connectivity checks;
- whether each is supported for direct S3-compatible remotes;
- its focused test and live RustFS evidence;
- explicit unsupported status for Git partial clone filters and remote-helper
  `connect`/`stateless-connect`.

Clarify that the “filter protocol v2” in the architecture guide is Git's
long-running clean/smudge filter-process protocol, not Git wire protocol v2.
Update `crab/docs/design/technical-design.md:2278` to retain partial clone as a
future design item without claiming the helper currently honors it.

Do not claim GCS/Azure/provider parity from RustFS proof. Those providers need
their own conditional-write/ETag release evidence.

**Verify**:

```bash
rg -n 'blob:none|stateless-connect|filter protocol v2|follow-tags' \
  crab/docs/architecture/git-integration.md \
  crab/docs/guides/clone.md \
  crab/docs/design/technical-design.md
```

Expected: every term has an explicit, non-contradictory support statement.

## Test plan

New or strengthened deterministic tests must cover:

- valid, missing, malformed, and read-error manifest advertisement;
- process-level clone/list false-success regression;
- complete v1 capabilities/list/fetch/push/multi-command transcripts;
- protocol error, cancellation, per-ref rejection, and atomic-abort output;
- absence of `filter`, `connect`, and `stateless-connect` advertisements;
- `option filter` returns `unsupported`;
- `include-tag` has proved semantics or is rejected;
- strict follow-tags success and zero-publication failure cases;
- public `gix-transport` scaffold returns explicit unsupported errors;
- live RustFS success, corruption, CAS, deletion, force/non-fast-forward,
  atomic, tag, shallow/unshallow, and fresh-process reconstruction.

Use these existing tests as structural patterns:

- in-memory store and real Git fixture:
  `crab/tests/e2e_add_commit_push.rs:22`;
- helper duplex protocol capture:
  `crab/tests/remote_helper_transcript.rs:23`;
- follow-tags manifest assertions:
  `crab/tests/remote_helper_transcript.rs:2655`;
- tag-owner fixture: `crates/crab-git/src/tag.rs:207`;
- manual object-store report/check shape:
  `crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py:595`.

Avoid tests that only snapshot warnings, private implementation ordering, or
obsolete fallback paths. Assert repository-visible state, exact ref outcomes,
manifest immutability on failure, installed Git objects, and reconstructed
bytes.

## Done criteria

All conditions must hold:

- [ ] Only a missing routed manifest can produce an empty ref advertisement.
- [ ] Corrupt/unreadable manifest tests make `git ls-remote` and clone fail.
- [ ] `remote_helper_transcript` has zero ignored tests.
- [ ] `v2_fetch_transport` has zero ignored tests and documents the explicit
      unsupported stateless-connect contract.
- [ ] Helper capabilities do not advertise `filter`, `connect`, or
      `stateless-connect`.
- [ ] `option filter blob:none` returns `unsupported`; direct library use with
      a filter returns an error before I/O; only the explicitly deprecated
      `v1.0.14` public API shape remains until the next major version.
- [ ] Follow-tags discovery, manifest read, and tag resolution errors publish
      neither branch nor tag.
- [ ] Successful follow-tags publishes branch and annotated tag through one
      existing manifest CAS.
- [ ] Focused helper/fetch/push/tag tests pass.
- [ ] `cargo fmt --all --check` passes.
- [ ] Clippy passes for `crab` and `crab-git`, or any unrelated baseline failure
      is identified with a clean touched-file proof; no touched warning remains.
- [ ] Manual 64 MiB RustFS report passes every success and expected-failure
      case with no credential disclosure.
- [ ] User docs contain the exact support matrix and distinguish the two
      meanings of “protocol v2.”
- [ ] No durable format, storage layout, config key, dependency, or second
      production fetch/push path was added.
- [ ] `git diff --numstat` is reviewed; source growth is justified by deleted
      fallback/dead-option complexity and behavior-level tests.
- [ ] No files outside the in-scope list are modified by this plan.
- [ ] `plans/README.md` status is updated.

## STOP conditions

Stop and report instead of improvising if:

- The current code no longer matches a current-state excerpt or another branch
  has already changed the same helper contract.
- A missing manifest cannot be distinguished from another storage error at the
  `Store`/`CrabError` boundary without changing a shared serialized error/API.
- Testing the full protocol loop appears to require a public test-only API,
  global mutable store override, or live S3 dependency.
- Git's observed `include-tag` behavior differs across supported Git versions;
  report the version matrix before selecting a behavior.
- Strict follow-tags needs a change to the manifest schema, lock key layout, or
  push pipeline CAS ownership.
- Any public symbol under the shipped `gix-transport` feature would need removal
  or incompatible signature change.
- Protocol-v2 implementation requires a new dependency, dependency patch, or a
  second pack-install path. Split it into a separate proposal with upstream
  source/type proof.
- The partial-clone fix would require promisor object storage, lazy Git blob
  retrieval, or a new server. Keep it unsupported and write a separate design.
- Live qualification cannot use isolated prefixes or would require bucket-wide
  cleanup.
- A verification command fails twice after a reasonable scoped correction, or
  the fix requires an out-of-scope file.
- `/Volumes/Workspace` is unavailable or the selected external Cargo target is
  not writable. Do not fall back to a local `target/` directory.

## Maintenance notes

- Reviewers should scrutinize every `NotFound` match: only the exact routed
  manifest absence means “empty repository.” Pack, index, shard, credential,
  parse, and transport errors must remain failures.
- Capability lines are product contracts. A future option may be advertised
  only after a real Git test and object-store E2E prove its semantics.
- If partial clone becomes worthwhile, design the promisor/on-demand Git object
  contract together with Crab's pointer smudge behavior; do not reuse
  `fetch.object_level_filtering` by name alone.
- If stateless-connect is pursued, create a separate plan grounded in the
  shipped gix dependency versions and decide whether it can reuse the canonical
  S3 manifest/pack installer. Do not let it become a parallel source of ref or
  pack truth.
- Keep RustFS evidence manual until the existing performance design is amended.
  Provider-specific S3/GCS/Azure evidence remains a separate release concern.
- Future changes to manifest parsing, remote store selection, tag peeling, or
  helper option handling must update the support matrix and the corresponding
  process-level regression test in the same change.
