# Plan 020: Harden Crab v2 attribution, history, and content reads

> **Executor instructions:** Read this plan completely. Execute one packet at a
> time, honor its dependencies, and retain source-bound proof before changing its
> status. Source existence or passing fixture tests do not establish production
> performance. Do not implement the conditional storage-format work until its
> decision gate passes. Follow root and nearest scoped `AGENTS.md` instructions.
>
> **Drift check, from repository root:**
> `git status --short`, then
> `git diff --stat aafb6d256c5e8d3a8e768d31dd887a18f2afd2e4 -- crates/crab-remote-git crates/crab-server crates/crab-sdk crates/crab-read crates/crab-metadata crates/crab-remote/examples packages/ui crab/scripts/e2e .github/workflows plans/020-crab-v2-read-hardening.md`
> Inspect relevant untracked files separately. Reconcile expected changes from
> completed packets; stop on unexpected contract or ownership drift. Never
> overwrite another task's changes to satisfy an excerpt.

## Status and scope

- **Priority:** P1 read-path hardening; correctness gates precede tuning.
- **Effort:** L, delivered through separately reviewable packets.
- **Risk:** HIGH overall; cursor, authority, cancellation, and budget semantics
  are more sensitive than the individual speedups.
- **Category:** performance, correctness, resource ownership, qualification.
- **Planned at:** `aafb6d256c5e8d3a8e768d31dd887a18f2afd2e4`, 2026-11-17.
- **Target:** `crab-v2`; revalidate the target before execution.
- **Delivery status:** IMPLEMENTED; ACCEPTANCE OPEN. R0–R5 are implemented.
  Source-bound Git-read/session smoke and historical enforced minimum-profile
  functional proof pass; frozen performance and cold Cellar recovery gates remain
  failed. R7's measured failures and remaining lifecycle cells are recorded,
  not waived. See the
  [combined qualification report](../crab/docs/benchmarks/v2-read-hardening-rustfs-2026-11-19.md).
- **Depends on:** existing read/authority contracts and the applicable resource
  ownership work in `plans/017-local-cache-read-hardening.md`.

This plan covers repository-wide first-parent history, file/path history,
directory/file last-change attribution, first-parent line blame, ordinary Git
file previews/downloads, and the shared runtime they consume. It strengthens
correctness and throughput together. It does not claim to harden every Crab v2
subsystem or complete the broader product/release roadmaps.

`plans/017-post-launch-product-hardening.md` and
`plans/018-product-readiness-roadmap.md` remain product-level owners.
`crab/docs/architecture/indexed-remote-reads.md` remains the existing indexed-read
evidence ledger. Reuse its implemented indexes and retained failures; do not
restart that initiative or mark its outstanding gates complete from this plan.

## Outcome and decision summary

After the first delivery wave, late first-parent history pages do page-sized
work, oversized file inspection avoids full payload reconstruction, identical
cold blame requests share one producer, and expensive reads cannot consume all
interactive capacity. Verification and authorization remain authoritative on
cache hits. Every claimed improvement has deterministic work counters plus
real HTTP/provider measurements.

| Packet | Deliverable                                                              | Effort | Risk | Depends on             | Status      |
| ------ | ------------------------------------------------------------------------ | ------ | ---- | ---------------------- | ----------- |
| R0     | Freeze contracts, add missing measurements, retain the baseline          | M      | MED  | —                      | MEASURED    |
| R1     | Resume first-parent history without prefix rewalking                     | M      | HIGH | R0                     | IMPLEMENTED |
| R2     | Add a truthful metadata-first file-preview representation                | M      | MED  | R0                     | IMPLEMENTED |
| R3     | Coalesce blame with independent waiter budgets and fair HTTP admission   | L      | HIGH | R0                     | IMPLEMENTED |
| R4     | Bound and offload cache verification and diff CPU work                   | M      | HIGH | R0; coordinate with R3 | IMPLEMENTED |
| R5     | Add authorized conditional reads for selected representations            | M      | HIGH | R2                     | IMPLEMENTED |
| R6     | Decide whether persistent attribution or a new range representation pays | L      | HIGH | R1–R5 measurements     | DEFERRED    |
| R7     | Qualify the combined source and publish the supported envelope           | L      | HIGH | R1–R5                  | GATES OPEN  |

Recommended order: R0; R1 and R2; R4; R3; R5; R7. R1 and R2 can use independent
worktrees after R0 freezes their contracts. R3 and R4 share runtime/budget code;
integrate them sequentially. R6 is not required for accepting the first wave.

```mermaid
flowchart LR
    A[Authenticate and authorize] --> B[Capture repository read view]
    B --> C[Resolve pinned commit and path]
    C --> D{Requested representation}
    D --> E[Bounded history page or metadata preview]
    D --> F[Admitted content or attribution producer]
    F --> G[Verified caches and batched origin reads]
    G --> H[Bounded CPU and complete result]
    E --> I[Per-caller limits and response]
    H --> I
```

## Domain and invariants

Use `CONTEXT.md` vocabulary. A **Repository read view** is an immutable generation
plus its validated committed-journal overlay. A **Repository handle** owns a
repository lifetime but does not itself imply a pinned generation. A
**Checkpoint** is the stored immutable view; **Hydration** materializes pointer
content into a working tree and is not a synonym for every range read.

Preserve these rules throughout:

1. Authorize the current repository UUID and prove snapshot/path visibility
   before serving data, a cached result, or a `304`.
2. A cache is an accelerator, never an authority. Keep repository identity,
   placement, generation/snapshot binding, and negative-cache isolation.
3. A selected advertised index that is missing or corrupt fails closed. An
   absent optional index and a corrupt advertised index are different states.
4. Every operation/session closes on success, failure, cancellation, and
   abandonment. Producer shutdown must drain admitted work before releasing
   permits, read pins, and reservations.
5. Every waiter independently honors its limits, deadline, cancellation, and
   response budget. Sharing computation does not share authorization or waive
   admission. Depth is a maximum, not an additive work counter.
6. Blame is complete or errors. Preserve ordinary UTF-8, first-parent semantics,
   deletion boundaries, Myers line-slider behavior, and unsupported-content
   errors. Do not add merge-parent, rename, move, whitespace-ignore, or Xet-line
   blame semantics here.
7. Ordinary Git, LFS, and Xet content have different verification/range costs.
   Never release unverified ordinary Git bytes merely to improve TTFB.
8. No new environment variables, tuning modes, fallback readers, or dependency
   patches. Prefer existing resource authority, runtime limits, and canonical
   owners. Prove a missing resource contract before proposing a new option.

## Current state and evidence map

The following source was inspected at the planned revision. Line numbers are
navigation aids; symbols and excerpts are the drift checks.

| Surface        | Entry/caller and owner                                                                                                              | Callee/sibling proof                                                                                                 | Existing tests/evidence                                                                                                             |
| -------------- | ----------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| History        | `crates/crab-server/src/api/query.rs`, `Action::Commits`; `crates/crab-remote-git/src/snapshot.rs:72`, `RemoteGitSnapshot::history` | `traversal.rs:152`, history codec; `operation.read_commit`; SDK `Snapshot::history`; ahead and path-history siblings | `history_pages_are_deterministic_and_bound_to_start_and_mode`; indexed/raw/native-Git comparison                                    |
| File preview   | `query.rs:262`, `Action::File`; `representation.rs:96`, `content_json`                                                              | `snapshot.rs:1347`, `blob_metadata`; SDK `file_metadata`; `content_json` is also used by Diff                        | `api::tests::file_metadata_keeps_large_utf8_available_without_embedding_it_in_json`; metadata fixtures; UI repository browser tests |
| Content ranges | `representation.rs:12`, `blob_response`; `query.rs:258`, `Action::Blob`                                                             | `snapshot.read_blob`; SDK `read_blob_with_options`; logical Xet and LFS readers                                      | `blob_ranges_*`; `crates/crab-sdk/tests/remote_blob/ranges.rs`                                                                      |
| Blame          | `query.rs:309`, `Action::Blame`; `snapshot.rs:731`, `blame`                                                                         | `path_state.rs::last_change`; `git_line_matches`; runtime result/comparison LRU; SDK `Snapshot::blame`               | `immutable_blame_cache_is_bounded_and_replays_semantic_budgets`; stricter comparison-budget test; native Git oracle                 |
| Shared work    | `runtime/read_flight.rs:38`, `ReadFlights::run`; `budget/shared.rs:113`, late joining                                               | existing object/index flights; `budget.rs:159`, `charge_cached`; operation finalization                              | last-waiter cancellation, sibling cancellation, budget, and close-fault tests                                                       |
| Cache CPU      | `reader.rs:2172`, `verified_cached_object`                                                                                          | single reads, batched reads, delta bases, admitted packed reads; `runtime.rs` owns caches and tracked blocking work  | `corrupt_cached_object_is_discarded_and_reported`; `admitted_packed_read_rechecks_verified_cache_and_caller_limits`                 |
| HTTP capacity  | `server/mod.rs:27`, `READ_ADMISSION_CAPACITY = 16`; `api/mod.rs:245`, admission before opening                                      | `repository/mod.rs:321`, pinned opening; shared runtime origin/decode admission                                      | server browse/authorization tests; new mixed-load proof required                                                                    |
| Qualification  | `crates/crab-remote/examples/qualify_remote.rs`; `crab/scripts/e2e/run_remote_read_scaling_rustfs.py`                               | `verify_remote_browse.py`; support counters; SDK/architecture/container CI                                           | current isolated cold/warm and native Git oracles; no existing mixed-HTTP/deep-page qualification                                   |

Relevant current code:

```rust
// snapshot.rs::history: each request starts at the original commit.
let mut pending = vec![self.commit_oid];
let mut traversed = 0u64;
// Commits before `skip` are still read and traversed.
let commit = operation.read_commit(oid).await?;
```

```rust
// query.rs: /file fully reads before representation policy runs.
Action::File => content_json(&snapshot.read_blob(&path, &operation).await?),
```

```rust
// representation.rs: size omission currently follows content inspection.
let text = decoded.filter(|_| blob.bytes.len() <= MAX_INLINE_TEXT_BYTES);
```

```rust
// reader.rs::verified_cached_object: full synchronous checksum on a hit.
gix_object::Data::new(&object.data, object.kind, gix_hash::Kind::Sha1)
    .verify_checksum(&object.oid)
```

The HTTP server also charges the full `read_blob` result against its response
budget before `/file` omits large text. Its configured response budget is 8 MiB;
the inline preview policy is 1 MiB. This is both unnecessary payload work and an
important large-file inspection test case.

## Released contracts: decisions that are not optional cleanup

The reviewers checked tag `v2.0.0-alpha.1`, peeled commit
`7e8aa458cc5356c1f9040e50b4feb075bbd33130`.

| Contract                                                                                     | Decision and proof obligation                                                                                                                                                                                                                                                                                                                                                                                                               |
| -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Lower-level `crab_remote_git::PageCursor::{from_bytes,as_bytes}` and version-1 history bytes | Preserve decoding and meaning of the released offset cursor. Issue a distinctly versioned seek cursor for new first-parent pages. Never reinterpret old bytes. Retire its reader only through an explicit breaking-release/restart-pagination contract. Doctor cannot rewrite cursor bytes held by remote clients.                                                                                                                          |
| Untrusted cursor exposure                                                                    | `crab-remote-git/README.md:83` requires callers to sign cursors. HTTP already MACs bytes in `api/representation.rs:179`; SDK `read_types.rs:167` uses opaque in-process wrappers bound to context. Preserve both. Raw import is not authentication. Audit every raw cursor importer before relying on a resume OID.                                                                                                                         |
| Cursor signing across replicas                                                               | Production already derives the key from the shared `auth.state_key_file`; non-loopback authentication requires it. `server/mod.rs:281`, `auth/mod.rs:243`, `config/mod.rs:248`, and `DESIGN.md:219` establish the contract. Same-key replicas/restarts must work; key rotation rejects old signatures. Loopback without a configured key has process-local cursors by design. No new signing-key configuration or sticky routing is needed. |
| Tagged `/file` response                                                                      | `text_truncated=true` means inspected UTF-8 omitted due size; false is not a license to label uninspected large bytes binary. Preserve `/file` and its Diff sibling. Add a separately named canonical first-party preview representation with explicit unknown/omitted state.                                                                                                                                                               |
| Per-hit cache verification                                                                   | Preserve size checks, checksum failures, eviction, `CacheCorrupt`, current limits, and cancellation. First offload/bound verification. A private verified-cache redesign is a separate measured follow-up, not permission to delete checks.                                                                                                                                                                                                 |
| Git/LFS/Xet ranges                                                                           | Raw Git ranges slice a complete verified blob. Cold LFS range tests permit a full download. Xet has independently chunk-verified ranges. Report those costs separately.                                                                                                                                                                                                                                                                     |

The retained `/file` endpoint and v1 cursor decoder are named tagged contracts,
not generic compatibility stacks. New first-party UI code uses only the new
preview endpoint; it must not try the new endpoint and then silently retry the
old endpoint. Removal of the old endpoint requires a declared breaking API
boundary with client recovery documentation, not elapsed time or a test edit.

## Target design

### History continuation

Add a new history-kind cursor version for `FirstParent`. It carries the pinned
start commit, traversal mode, next commit OID, and required context binding.
Do not expose provider/repository namespace strings in the cursor; use existing
opaque context binding/digests where needed. Retain the released version-1
offset reader and its bounded behavior. Preserve current page-size-changing
behavior unless the tagged contract audit proves otherwise.

Only a continuation issued by a trusted caller boundary may authorize resuming
at that OID. HTTP MAC verification and SDK identity/generation/commit validation
must happen before the new cursor is consumed. An arbitrary caller-selected OID
must not become a way to read history outside the pinned start. If any exposed
consumer lacks that trust boundary, stop: general ancestry is not proof of an
exact first-parent position, and replaying the entire prefix would lose the
optimization. Resolve the validation design explicitly.

The existing HTTP MAC authenticates the low-level payload but does not include
a repository UUID. SDK wrappers do bind identity. Add two-repository tests using
same-commit forks, then issue a versioned UUID-bound HTTP envelope for new
cursors, preserving the named tagged envelope reader explicitly. Independent
target-repository authorization already applies; do not describe the current
cross-repository acceptance of equivalent payloads as an authorization bypass.
Do not claim that the current signature alone proves repository identity.

Read page-sized first-parent batches through existing graph and `read_commits`
mechanics when available, preserving raw-object verification and ordering. With
no optional index, a trusted seek cursor still resumes at the verified next
commit and performs page-sized serial work. Do not silently change `AllParents`,
ahead history, or indexed file/path-history ordering or continuations.

### Truthful metadata-first previews

Add `GET /api/repos/{owner}/{name}/file-preview` as the canonical first-party
inspection route. Its server-owned representation contains existing identity,
size, mode, and classification plus one discriminated preview state:

```text
preview = { state: "inline", text: string }
        | { state: "omitted_size" }
        | { state: "binary" }
        | { state: "unsupported_encoding" }
```

The enclosing reply also retains pinned `commit` and `generation`. For objects
over the existing 1 MiB inline limit, return `omitted_size` from authenticated
metadata without inspecting the entire encoding. The UI says “Preview omitted
because of file size,” not “Large text file” or “Binary file.” For smaller
objects, read and verify bytes before selecting inline/binary/encoding state.
Pointer-sized objects still require the existing Crab/LFS inspection. Inline
pointer bytes remain the Git representation, not hydrated payload content.

Keep this product policy in the server; reuse `blob_metadata` rather than adding
a second metadata engine. Migrate all first-party browse consumers and browser
fixtures together. Keep tagged `/file`, raw downloads, assets, and Diff on their
existing explicit contracts. In particular, do not change shared `content_json`
to guess encodings from size.

### Shared attribution and resource ownership

Introduce bounded semantic blame flights in the existing runtime owner, keyed
by repository read-view identity, pinned commit, raw path, blame semantics, and
the reader's `ObjectLimits` admission profile. Different immutable-object limits
must not silently share the more generous reader. Per-operation work limits
remain independently charged waiter constraints rather than part of that key.
Use an immutable shared result plus a work ledger. The producer owns its own
operation/session, cancellation scope, and lifetime; it is not owned by the
first HTTP caller. A joining waiter is authorized separately.

Do not open the producer with the first waiter's `OperationLimits` or silently
use repository defaults as a semantic ceiling. Existing `operation_with_limits`
pins a budget and deadline, so its ownership needs an explicit internal refactor.
The producer's semantic budget owner must charge every live waiter's ledger,
detach exhausted/expired waiters with their own errors, and continue only while
an eligible waiter remains. Its remaining allowance and deadline follow those
live participants; immutable `ObjectLimits` come from the matched flight profile.
Physical CPU/memory/backend reservations remain independently bounded by the
runtime. A later generous waiter must neither inherit an earlier strict ceiling
nor erase already incurred work. Preserve callers that intentionally supply
operation limits above repository defaults. If the existing operation/session
owner cannot support this contract, stop and resolve that refactor before
enabling sharing; do not clamp callers or create an unchecked producer budget.

Existing `ReadFlights` is a lifecycle exemplar, not a complete whole-blame budget
implementation. Its late-join replay currently includes only storage requests,
fetched bytes, and inflated bytes. Implement and test semantic accounting before
enabling shared blame:

- Replay prior work and charge new work independently for every participant.
- Include logical objects, entries, history commits, blame lines/comparisons,
  response bytes, and the maximum traversal depth where applicable.
- Keep physical provider/CPU telemetry counted once per actual producer; do not
  multiply origin bytes by waiter count in performance reports.
- Preserve each waiter's deadline, maximum object/input sizes, and other
  admission constraints; a generous caller cannot sponsor a stricter one.
- Removing one waiter cannot cancel remaining participants. Last-waiter
  abandonment cancels pending admission and drains admitted backend/CPU work.
- Preserve original typed provider/corruption/close failures; cancellation must
  not disguise an independently completed error.

Keep total pending HTTP work bounded. Initial internal policy retains the
existing total of 16 admitted browse requests and allows at most 12 bulk
requests/waiters, leaving four slots for metadata and inline-preview work.
Classify by server-verified operation/representation cost, not a caller's priority
flag. Bulk followers still consume bulk/request capacity; coalescing alone does
not free their HTTP slots. Preserve the existing fail-fast busy response instead
of adding an unbounded queue. Reuse current CPU/resource authority and introduce
no user-facing tuning knobs in this wave.

Bound actual producers independently from HTTP waiters and apply bounded CPU
admission around checksum/Myers work. Never hold the same decode permit across
the entire blame operation and then acquire it again while decoding its blobs.
Account for active working sets, retained result Arcs, serialization buffers, and
response lifetimes separately from LRU map occupancy. If the existing owners
cannot enforce the frozen memory envelope, land the required shared reservation
work from Plan 017 before enabling the affected production packet. Do not claim
an LRU byte cap bounds total RSS or raise concurrency to hide admission failures.

### Verification and cache identity

Run large cache-hit checks and Myers work in tracked blocking jobs with bounded
CPU admission. Retain the exact immutable object while work runs, and keep
permits/reservations until it actually finishes. Preserve pre/post CPU
cancellation checks; do not promise preemption inside the dependency's opaque
diff implementation. Reuse its existing Myers and line-slider semantics.

Keep cache keys unchanged in the first wave. Current hits precede locator
lookup, so removing generation/snapshot identity can bypass current visibility.
Audit single-object, batch, delta-base, and admitted packed insertion paths
before any later private verified-object type is proposed. Keep the public
`RemoteGitObject` DTO unchanged and preserve corruption tests. Broader positive
cache reuse requires a new explicit current-view visibility proof; negative
caches always stay state-scoped.

### Conditional HTTP reads

Opt only the new preview representation and deliberately selected raw content
responses into private revalidation. Keep the global default `no-store` and
unrelated account/discussion/mutation responses unchanged. Authenticate,
authorize, capture the current repository read view, and resolve visible path
metadata before deciding a `304`.

Use a strong representation validator, not one universal blob validator. JSON
identity includes format, preview policy/state, mode/classification, commit, and
generation as well as content identity. Account for selected content coding and
`Vary` wherever negotiation changes the actual bytes. Raw byte identity and JSON identity are
different. Do not serialize secret storage namespaces into validators. For an
opted-in browser representation, `Cache-Control: private, no-cache` requires
revalidation and prevents shared-cache storage. Use HTTP caching semantics, not
another long-lived JavaScript data cache or manual `304` JSON parsing.

Do not promise cold backend range savings for ordinary packed Git or LFS.
Validate ranges against authenticated size before expensive work when possible;
serve warmed ranges from verified content. Keep Xet's existing independently
verified chunk-range path and full-file-versus-range integrity distinction.

## Commands and execution environment

Use a suitable existing worktree unless the operator requests isolation. New
worktrees use the Codex worktree tool when available. Implementation branches use
`codex/v2-read-<packet>`. Commit logical changes with conventional subjects. Do
not push, open issues/PRs, or merge without the operator's authorization.

Before any local compilation, resolve and verify the mounted workspace volume.
Run from repository root; redo this setup in every new shell invocation:

```sh
python3 - <<'PY'
import os
from pathlib import Path
root = Path.home() / "Workspace"
assert root.is_dir(), "Workspace unavailable"
assert root.resolve().stat().st_dev != Path.home().stat().st_dev, "Workspace is not on the separate mounted volume"
PY
READ_WORKTREE_KEY="$(python3 -c 'import hashlib,os; print(hashlib.sha256(os.getcwd().encode()).hexdigest()[:12])')"
READ_TARGET="$HOME/Workspace/crabbuild-target/crab-read-hardening-$READ_WORKTREE_KEY"
mkdir -p "$READ_TARGET"
test -w "$READ_TARGET"
```

Never fall back to a local `target/`, reuse another worktree's directory, or
clean another task's artifacts. CI uses its explicitly configured runner artifact
storage; do not copy a CI-local `target/` path into local verification commands.

| Purpose                    | Existing command, from repository root                                                                                                                                            | Expected result                                                              |
| -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| UI dependencies if missing | `npm install --prefix packages/ui`                                                                                                                                                | Success; inspect any lockfile delta, no unrelated dependency upgrade         |
| Embedded UI prerequisite   | `npm run build --prefix packages/ui`                                                                                                                                              | Success before server compilation                                            |
| History contract           | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-remote-git --locked --test remote_repository history_pages_are_deterministic_and_bound_to_start_and_mode -- --exact`          | Exactly the named test runs and passes                                       |
| Indexed path/native parity | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-remote-git --locked --test remote_repository indexed_path_history_matches_raw_git_changes_and_continuations -- --exact`       | Named test passes                                                            |
| Metadata contract          | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-remote-git --locked --test remote_repository content_metadata_distinguishes_git_and_logical_representations -- --exact`       | Named test passes                                                            |
| Shared-read lifecycle      | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-remote-git --locked --lib runtime::read_flight::tests`                                                                        | Nonzero tests, all pass                                                      |
| Result-cache budgets       | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-remote-git --locked --test remote_repository immutable_blame_cache_is_bounded_and_replays_semantic_budgets -- --exact`        | Named test passes                                                            |
| Comparison-cache budgets   | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-remote-git --locked --test remote_repository retained_blob_comparison_does_not_bypass_a_stricter_blame_budget -- --exact`     | Named test passes                                                            |
| Corrupt warm cache         | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-remote-git --locked --lib reader::tests::corrupt_cached_object_is_discarded_and_reported -- --exact`                          | Original corruption assertion passes                                         |
| HTTP large-file contract   | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-server --locked --lib api::tests::file_metadata_keeps_large_utf8_available_without_embedding_it_in_json -- --exact`           | Original tagged response semantics retained                                  |
| HTTP ranges                | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-server --locked --lib api::tests::blob_ranges_`                                                                               | Existing range tests pass                                                    |
| SDK raw sibling            | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-sdk --locked --features remote --test remote_blob raw_blob_reads_survive_source_removal_and_keep_snapshots_pinned -- --exact` | Named test passes                                                            |
| SDK content sibling        | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-sdk --locked --features files --test remote_blob raw_blob_reads_survive_source_removal_and_keep_snapshots_pinned -- --exact`  | Named test passes                                                            |
| Scaling harness tests      | `python3 -m unittest discover -s crab/scripts/e2e -p test_remote_read_scaling.py`                                                                                                 | Nonzero tests, all pass                                                      |
| Qualifier counters         | `CARGO_TARGET_DIR="$READ_TARGET" cargo test -p crab-remote --locked --features publication --example qualify_remote support::tests`                                               | Counter tests run and pass                                                   |
| UI types                   | `npm run typecheck --prefix packages/ui`                                                                                                                                          | No errors                                                                    |
| Browser wire contracts     | `CARGO_TARGET_DIR="$READ_TARGET" npm run contracts:check --prefix packages/ui`                                                                                                    | Generated declarations match server owners; embedded UI prerequisite applies |
| Browser source regression  | `npm run test:browser --prefix packages/ui -- tests/browser/repository.e2e.ts --grep source --workers=1`                                                                          | Relevant nonzero tests pass; fixture APIs do not prove backend I/O           |
| Browser preview regression | `npm run test:browser --prefix packages/ui -- tests/browser/repository.e2e.ts --grep preview --workers=1`                                                                         | Relevant nonzero tests pass                                                  |
| Browser blame regression   | `npm run test:browser --prefix packages/ui -- tests/browser/repository.e2e.ts --grep blame --workers=1`                                                                           | Relevant nonzero tests pass                                                  |
| Rust format                | `CARGO_TARGET_DIR="$READ_TARGET" cargo fmt --all -- --check`                                                                                                                      | No formatting differences                                                    |

After the required workspace preflight, the existing isolated qualifier can be
built and invoked in a dedicated test environment:

```sh
CARGO_TARGET_DIR="$READ_TARGET" cargo build -p crab-remote --locked --features publication --release --example qualify_remote
"$READ_TARGET/release/examples/qualify_remote" "$QUALIFICATION_BUCKET" "$QUALIFICATION_PREFIX" "$QUALIFICATION_TEXT_PATH" all
```

Bucket, prefix, endpoint, and credentials must be explicitly selected for owned
qualification data. Do not print credentials. `build-indexes` and `checkpoint`
are write-side maintenance selections, not read-only benchmark options.

The existing local scaling runner accepts:

```sh
python3 crab/scripts/e2e/run_remote_read_scaling_rustfs.py \
  --root "$HOME/Workspace/CrabData/read-hardening-qualification" \
  --bucket "$RUSTFS_FIXTURE_BUCKET" --endpoint-url "$RUSTFS_ENDPOINT" \
  --crab-bin "$SOURCE_MATCHED_INSTALLED_CRAB" \
  --qualifier "$READ_TARGET/release/examples/qualify_remote" \
  --run-id "$UNIQUE_RUN_ID" --history 100 1000 10000 --repeats 20
```

This runner uses fixed local fixture credentials. It is not a hosted-S3 runner.
If an installed Crab binary is needed, inspect the current Make target's
artifact lookup, then use:

```sh
CARGO_TARGET_DIR="$READ_TARGET" make -C crab install PREFIX="$READ_INSTALL_PREFIX" CARGO_BIN="$READ_INSTALL_PREFIX"
```

Never copy binaries manually or use `cargo install`.
Set both Make variables: the installer also writes `CARGO_BIN` even when
`PREFIX` is isolated. Matching them avoids replacing the user's normal binaries.
`READ_INSTALL_PREFIX` is a specific isolated directory on the mounted
workspace volume. Use existing public repositories under
`$HOME/Workspace/Github/<owner>/<repository>` as read-only inputs; generated
fixtures/evidence belong outside tracked source on that volume.

## Execution packets

### Execution record

Implementation branch: `codex/v2-read-hardening`, based on the planned revision.
R0 adds fresh-runtime deep-page measurements, a native Git oracle for those
pages, checksum/uncached-attribution counters, and a real HTTP driver with
source/build receipts, bounded concurrency, private authentication input, failed
request accounting, and frozen-profile comparisons. The HTTP driver labels
cache state as the case's initial state; later requests are not cold samples.
Cold deep-page HTTP cases require cursors prepared before the measured process
starts, so cursor preparation cannot warm the measured runtime.

R0 remains IN PROGRESS. The initial RustFS smoke passed all 41 checks. The
completed native 20-repeat baseline over 100/1,000/10,000 commits passed 1,347
checks across 663 retained rows. Frozen debug binaries/source precede R1. On
the 10,000-commit fixture, cold page 100 made a median 10,055 storage reads,
with 45.55-second query p95; page 1 p95 was 0.478 seconds. These are local debug
measurements, not hosted or release latency claims. The HTTP driver's 13 tests,
qualifier counters, five server metrics tests, and embedded UI build pass.

The user selected local RustFS for verification. Initially, a task-owned Colima
profile ran the pinned RustFS image with one CPU and 1 GiB memory inside a
2-CPU/4-GiB VM. Fixture data, VM disks, and receipts reside on the mounted
Workspace volume. Other Docker profiles and normal installed binaries are
untouched. Complete HTTP/resource/replica baselines, deliberate server-side
cancellation/drain proof, and the remaining frozen matrix are outstanding.
The release HTTP control passes 15 RustFS checks: real fixture login, same-key
replica continuation, file/raw/blame agreement with Git, private mTLS metrics,
and zero-exit shutdown of both native servers. The servers use host resources
without per-process CPU/RAM quotas; minimum-profile qualification remains open.
An earlier debug control aborted with worker stack overflow on its first file
request; investigate default debug read-future stack during R4. Release proof
does not erase that failure. Python's strict X.509 verifier also rejected initial
fixture certificates; corrected key identifiers and CA key usage pass strict
offline server/client verification and live scraping. TLS verification remains
enabled. These receipts prove selected local S3-protocol behaviors, not complete
conformance, hosted latency or whole-product readiness.

R1 source adds versioned first-parent seek cursors, page-bounded indexed/raw
walks, parsed-commit batch reuse with existing size checks, and UUID-bound HTTP
envelopes. Tagged offset bytes and signed envelopes retain explicit readers.
New cold-runtime/tagged-cursor tests and existing history/ahead/indexed-path
tests pass. All 14 API tests pass, including replica/restart/rotation/fork checks
using independent process-owned repository handles. Both SDK raw and hydrated
checks pass with opaque first-parent cursors and generation rejection. The
snapshot module moves to `snapshot/mod.rs` under the scoped module convention.
The matched 20-repeat native R1 candidate also passes 1,347 checks across 663
rows. Cold page 100 at 10,000 commits falls to 0.147-second p95 and a median
54 storage reads. Seventeen untargeted p95 cells exceed the frozen 10% regression
gate, so acceptance remains open. A diagnostic interleaves twenty old/new pairs
on each of two identical existing RustFS fixtures: 1,000-commit object-batch
medians differ by less than 1%, and 10,000-commit blame medians are effectively
equal. Warm blame p95 still exceeds the gate. These controls do not replace the
original failed receipts or the complete integrated qualification matrix.

R2 adds typed metadata previews and generated browser contracts. First-party
inspection consumers use the discriminated state; tagged `/file` and Diff stay
explicit. UI type/build checks and 28 focused transport/preview unit tests pass.
All 15 focused server API tests pass. The new preview test proves large plain
and delta representations remain omitted below their reconstruction budget,
while tight fetched-byte and response budgets still reject. Alpha.1 metadata
verifies complete encoded entries by CRC; it can fetch a large compressed base
without fully inflating it. This is not prefix-only or zero-I/O inspection.
Two omission/encoding browser tests and four existing inspection/editor consumer
tests pass. Rendered mobile/desktop views pass both themes, accessibility and
root-overflow checks. A source-bound debug server passes 30 RustFS checks over
the native Git preview corpus, cursor/replica, raw budget and shutdown paths.

R4 routes checksum/Myers work through the existing tracked CPU admission.
Decode permits now belong to actual workers, including batch decode and index
siblings. A controlled large-cache verification test proves async progress,
retained buffers after caller cancellation, pending-work cancellation and actual
shutdown drain. Original corrupt-cache assertions now cover small and large
entries; the three Myers cancellation/repeated-line checks pass. The retained
macOS crash report identifies recursive delta polling as the debug stack failure;
an iterative traversal and exact 64-delta regression now pass on default worker
stacks, including the point and batch paths. The integrated remote-Git library
suite passes all 188 tests. R6 defers persistent formats; its decision is below.

R3 owns complete semantic flights with dynamic live-waiter budgets rather than
the first caller's/default ceiling. All fourteen dimensions replay, depth is a
high-water mark, and 256 bounds both live leases and departed ledgers. Eleven
native Git blame regressions pass, including sixteen concurrent callers,
strict-first/generous-first limits, late depth/line replay, independent deadline
and cancellation, abandoned shutdown and distinct immutable object profiles.
The HTTP 16-total/12-bulk test retains permits through the final transport slice.
The affected Plan 017 ownership prerequisite is implemented through one runtime
resident owner: native buffers, parsed indexes/commits/trees, comparison work,
managed blame results and response encoding retain reservations through actual
workers and final owners. Explicit caller-owned DTO copies and dependency/RSS
overhead remain separate. Minimum-profile resource derivation has focused proof;
production/provider resource acceptance still requires the combined qualification.

R5 opts only preview/raw blob representations into private revalidation. The
real HTTP authorization test passes mode, changed commit, checkpoint generation,
removed path, revoked membership and reused-UUID cases. Conditional opens force
current control metadata validation inside the normal browse freshness window;
the same canonical pinned-reader path is reused. Native Git supplies complete
visibility evidence for checkpoint fixtures. Entity-tag lists/weak comparison,
separate raw identity and strong If-Range behavior have focused proof. The real
Chromium check sees wire 200/304/401, receives the retained JSON body through
ordinary Fetch, and refuses cached success after authentication is removed.

The branch is rebased to `040916ccd056162dca9d2962a8b97843c1a14007` on
`origin/crab-v2`; the already merged Symbols commit drops cleanly. Source-bound
combined debug/release builds and the dedicated RustFS qualification are in
progress. The first combined debug smoke verifies sixteen real HTTP blame
callers with exactly one producer per replica and native Git parity. Admitted
CPU/origin/blame work and response bytes drain to zero; native server RSS peaks
are 325/205 MB, below the predeclared 1 GiB observation ceiling. It retains a
failed short-run RSS-growth receipt: the first observer incorrectly applied a
steady-state slope test during 27 seconds of startup/warming. Its exact source
hash is archived. The observer now requires the plan's 600-second soak before
claiming a plateau; the 1.10 growth threshold and failed receipt remain intact.
This corrects the measurement definition rather than waiving a measured soak
failure. Functional runs label plateau proof unavailable.

The first matched release load attempt loses its local RustFS container at
32 clients: Docker records an OOM kill at the task-selected 1 GiB backend limit.
Its partial request journal and backend exit state are retained. The matched
retry declares 3 GiB for RustFS on both sides, retaining the one-CPU backend and
2-CPU/4-GiB VM. This is a backend fixture change, not minimum-server or hosted
qualification. No latency, throughput, failure or RSS threshold is loosened.

Both retry variants complete all 22 HTTP cells, including their ten-minute
soaks, with zero Git-oracle mismatches, zero non-rejection request errors and
clean replica exits. Both RSS plateau checks pass. The old control fails the
soak's interactive-latency target; the pre-fix candidate fails mixed-load
latency/rejection gates. Its retained comparison contains 91 p95/throughput
regression entries. These are failed engineering receipts, not accepted results.
The observer now preserves its report when a replica disappears, and the
comparator distinguishes baseline starvation from invalid measurements without
waiving candidate targets. Fixture commit and compiler must match as well.

The measured preview regression identifies repeated encoded-entry metadata reads
despite a verified cached blob. Metadata and pointer-body readers now reuse that
cache with full checksum verification, existing identity/generation keys and
pointer size limits. Metadata still reports oversized cached objects without
reconstruction. Warm history likewise continues through verified parsed parents
without reselecting graph records; cold batching and selected-index validation
remain. These changes pass 189 library tests, 19 focused native Git regressions,
three qualifier-counter tests, strict affected-crate Clippy and 20 qualification
tool tests. Source-bound rebuilds and the final matched qualification follow;
the earlier failures remain in the evidence set.

**Final combined qualification, 2026-11-19:**

The final production tree at `c5b6d40c459` completes 1,440 native Git query
checks across all 72 cold/warm cells, with twenty interleaved pairs per cell.
Cold page 100 at 10,000 commits uses a median 54 backend reads versus 10,055,
with p95 0.131 versus 28.893 seconds. Fourteen native p95 controls still fail.
The final release HTTP matrix completes all 22 cells per variant, including
600-second mixed soaks, with zero Git mismatches and zero non-rejection request
errors. Both RSS plateau checks pass and both variants exit zero. The candidate
retains 41 p95, 34 throughput and three interactive-latency gate failures; its
provider observer also retains one upstream connection failure. An alternating
short-run diagnostic varies in both directions and does not clear these gates.

Final combined debug HTTP proof passes 43 checks, including a real mid-blame
socket abort with one producer and all admitted work drained. An exact-source
Linux image additionally passes 25 checks with enforced 1-CPU/1-GiB cgroups:
startup detects those limits, eight cold callers share one producer, managed
capacity is at most 256 MiB, cgroup memory peaks at 418,279,424 bytes, no OOM
occurs and shutdown exits zero. The amd64 image runs emulated on the arm64 VM;
this proves minimum-profile functionality, not native latency or a sustained
minimum-profile soak. Failed fixture setup attempts remain retained.

The old catalog develops a non-contiguous Cellar LTX restore plan after earlier
runs. The frozen R0 binary fails on the same preserved state; no dependency
patch or data repair is applied. Final matched/functional runs use a previously
unused catalog that adopts the identical original Git prefixes. Their restarts
pass, without claiming recovery of the old epoch. The portable report binds all
build/source inputs, thresholds, successful and failed cells, provider counts,
resource limits and retained failures. It also names the remaining lifecycle
and resource proof; R7 acceptance remains open. Local RustFS replaces hosted S3
for this execution, without establishing hosted latency.

The mounted volume's root is not writable by this user; its existing
`crabbuild-target/` and `CrabData/` directories are writable. The preflight above
therefore checks the mount identity and the actual per-worktree target rather
than incorrectly requiring permission to create a sibling at the volume root.

**Subsequent qualification, 2026-11-19:** revision `8ba88f5a452` adds bounded
64-KiB JSON chunks, overlapping fresh session/identity-marker reads, complete
shutdown proof checks and bounded quiet proxy reuse. Source-bound debug/release
builds, focused tests and strict Clippy pass. Its 46-check two-replica RustFS
Git-read/session smoke passes, including durable logout and rejection of the
original cookie/validator by both replicas. Both log/local-durability shutdown
proofs complete with no pending work and zero exits.

The newer Directory catalog also develops a missing transaction 2 between 1 and
3. Frozen R0 fails on an isolated copy with the original format and ended
authority/session records; six short R0 restarts on a healthy fixture pass.
A narrower R0 reproducer creates the same gap after a read-only foreign restore,
70 seconds idle, graceful stop, local reload and another stop. Maintenance can
stage another transaction between the final capture/upload cut and database
close, leaving the clean-reload certificate ahead of the uploaded position.
A current-source cold Repository restore also fails after the next matched run;
its exact transaction 2 remains staged locally. The 1-CPU/1-GiB image starts with
verified cgroup limits but exits on that Repository verification failure, without
OOM; its soak is not reached. Dependency patch approval is pending. No dependency
patch, invented transaction or state repair has been applied.

The preceding `03ecadc5e33` release matrix completes all 22 cells and both
600-second soaks, but retains 30 p95, 18 throughput and one interactive failure,
plus one observed upstream disconnect. An actual subsequent RustFS CopyObject
fails with `Disk full` on the shared workspace volume. Offline migration to the
dedicated VM disk verifies all 19,711 file hashes, retains the original data and
makes the same copy succeed; the missing transaction remains missing. The next
matched run completes all 22 cells and both ten-minute soaks on that provider
profile with unchanged thresholds and original Git inputs. Both variants have
zero Git mismatches/non-rejection request errors/proxy errors and pass RSS plateau
and shutdown proof. Baseline engineering gates pass; candidate retains eleven
p95, ten throughput and one interactive failure. Its targeted one-client late
history p95 improves by about 81%, but the mixed soak regresses. The native rerun
on the VM disk completes 1,440 Git-oracle queries over 72 cold/warm cells and
retains twelve timing failures; the older fourteen failures remain retained.
Cold page 100 at 10,000 commits falls from 18.325 s to 0.217 s p95 and from
10,055 to 54 median backend reads. Current-source CI container and three-node
Kubernetes recovery jobs pass. The auth-only and warm-stage diagnostics retain
fresh authority and isolate cleanup overhead without replacing frozen gates.
Acceptance remains open.


A subsequent ready-close optimization (`37002f85ce2`) polls locator cleanup once
inline, transferring a pending close to tracked runtime ownership before yielding.
The red/green regression, ten operation tests, three archive cleanup/cancellation
tests and strict all-target Clippy pass. Forty diagnostic processes against RustFS
match native Git. Across their warm samples, the median per-process cleanup p50
falls from 31.5 to 8 microseconds. This isolates cleanup cost; the full frozen
acceptance matrix has not been rerun for this follow-up.


The branch is rebased onto `crab-v2` at `464d943a045`, preserving the newer file
workspace layout and preview-state migration. The rebased code (`10994bae022`)
builds as a release; UI type checking, seven API transport tests and eighteen
browser tests pass. Its release SHA-256 is
`f8c9a0b0d2d0412bc79f35180a6bfa617eb53c4a0dd925fabb211174b550f1d5`.
The new HTTP smoke fails before public serving: the retained Repository epoch 9
still contains 1 and 3 without 2. It exits 1 after a complete shutdown proof and
reaches none of the 46 read checks. The earlier successful 46-check run remains
historical; it does not qualify this restart. Dependency approval remains pending,
and the full frozen timing matrix has not been rerun for the ready-close change.

### R0 — Freeze the evidence contract and establish a baseline

**Scope:** `crates/crab-remote/examples/qualify_remote.rs`, its `support/` owners;
`crab/scripts/e2e/run_remote_read_scaling_rustfs.py`,
`test_remote_read_scaling.py`, `verify_remote_browse.py`; create
`crab/scripts/e2e/qualify_v2_read_service.py` and
`crab/scripts/e2e/test_qualify_v2_read_service.py`; bounded
counter/timing additions in remote-Git/server metrics; the existing indexed-read
ledger. No production read behavior, codecs, limits, or cache policy changes.

1. Record source revision/patch, binary hashes, compiler/profile, fixture
   identities, backend/region, quotas, and report schema. Readiness/capture,
   query, serialization, and client timing must remain separate.
2. Extend the existing qualifier to request history pages 1/10/100 at a fixed
   page size. Its current `history` selection measures only the first 1,000
   commits. Extend the native Git oracle with the new result shape and negative
   cases; do not make the oracle accept missing rows or measurements.
3. Add a real HTTP mixed-load driver: metadata/previews, history, raw content,
   and blame; same-key bursts and distinct keys; cold, warm, post-push,
   restarted-runtime, and co-resident expensive-load cases. Include two server
   replicas in cursor/authority qualification. Record 429s, other failures,
   successes, TTFB, completed-request latency, throughput, and cancellation.
4. Add explicit semantic-producer counts, checksum bytes/time, per-class
   admission occupancy, cache occupancy, active-result lifetime, physical
   provider operations/bytes, and process/cgroup memory. Use existing bounded
   metrics and avoid repository/path/OID labels with unbounded cardinality.
5. Freeze the matrix and acceptance thresholds below before optimization runs.
   Record unsupported workloads as gaps; never invoke invented flags or claim
   the current sequential RustFS runner already measures them.

The new driver's CLI contract is an R0 deliverable, **not an existing command**:

```sh
python3 crab/scripts/e2e/qualify_v2_read_service.py \
  --base-url "$READ_SERVICE_URL" --repository "$READ_REPOSITORY" \
  --revision "$READ_COMMIT" --text-path "$READ_TEXT_PATH" \
  --profile "$READ_PROFILE_JSON" --output "$READ_EVIDENCE_DIR" \
  --headers-file "$READ_AUTH_HEADERS_FILE"
```

Implement and test `--help` and this interface before invoking it. The profile
contains the report schema version, resource/provider identity, replica URLs,
workload paths, page depth/size, client counts, duration/warm-up, and frozen thresholds. The
headers file is an owner-private qualification fixture containing authorized
HTTP headers; never include its values in logs, reports, process arguments, or
source. Permit omission only for an explicitly loopback/local-trust fixture.
The output directory is a fresh task-owned path on the mounted workspace volume.

Process restart, replica selection, and fault orchestration belong to the owned
test environment, not a new production cache-reset endpoint. Bind cold/restart
receipts to the actual process/start identity and cache counters; repeating a
request against an already warmed server is not a cold sample. Hosted runs use
the same workload/report schema with real scoped credentials and a separately
declared provider profile; the fixed-credential RustFS runner is not repurposed.

**Verify:** run the scaling-harness and qualifier-counter commands in the table.
After creating the driver's tests, also run:

```sh
python3 -m unittest discover -s crab/scripts/e2e -p test_qualify_v2_read_service.py
```

Expected:
nonzero tests reject incomplete/incorrect reports, native Git differences,
missing failures, missing source binding, and unclassified cache states.

**Exit:** source-matched baseline receipts exist for every required cell or the
cell is explicitly blocked; no later packet may relabel a missing baseline a
performance improvement. Local fixture setup and hosted-provider measurement
have separate receipts.

### R1 — Make first-parent continuation page-sized

**Scope:** remote-Git `snapshot.rs`, `traversal.rs`, `commit_graph.rs`, relevant
operation batch helpers and tests; HTTP cursor representation/tests; SDK cursor
conversion/tests and SDK docs. Do not change all-parent/ahead algorithms or
persistent repository formats in this packet.

1. Add cold-runtime continuation tests at pages 1/10/100, EOF, different page
   sizes, missing optional indexes, force-pushed refs with pinned snapshots,
   malformed/cross-context cursors, and authenticated HTTP/opaque SDK callers.
   Retain the tagged v1 codec fixtures and existing history tests.
2. Add the distinct seek cursor and first-parent path. Obtain the next OID from
   verified parent data, enforce existing trust/context validation, batch
   page-sized commits, and delete new-path offset rewalking. Retain the v1
   reader only for the named tagged contract, with a comment explaining why.
3. Reuse the existing shared state-key signer. Qualify replica A to B and restart
   continuation with the same configured key, rotation rejection, and the
   separate loopback/no-key restart boundary. Add the versioned UUID-bound HTTP
   envelope and same-commit-fork context test from the target design; preserve
   the explicitly named tagged envelope reader. No new environment secret,
   sticky routing requirement, or unsigned-token path is permitted.
4. Document seek versus retained-offset behavior and the lower-level caller's
   signing responsibility. Preserve SDK private wrapper checks and public
   native ordering. Keep advertised graph corruption observable.

**Verify:** run the history, indexed-path, SDK sibling, and relevant existing
ahead-history tests. Add semantic tests named
`resumed_first_parent_pages_do_not_read_the_skipped_prefix` and
`history_seek_cursors_preserve_context_and_tagged_offset_cursors` under the
existing `remote_repository` test target, then run each with `-- --exact` and
the external Cargo target. Also run the HTTP tamper/replica cases from R0.

**Exit:** a new seek page reads at most `page_limit + 1` raw history commits,
excluding separately counted snapshot-opening/authority work. That work does
not grow with the skipped prefix. Results equal native Git; v1 semantics and
all-parent/ahead siblings remain intact. Do not claim v1 offset performance
improved merely because the new cursor is fast.

### R2 — Inspect oversized files without reconstructing them

**Scope:** server `api/mod.rs`, `api/query.rs`, `api/representation.rs`, a focused
preview owner if warranted, API tests and actual Serde wire owners; UI `src/lib/api.ts`, repository file/
preview consumers and browser fixtures/tests; server/UI documentation. Reuse
remote-Git `blob_metadata` and its tests. Raw and logical SDK contracts remain
unchanged; do not change shared Diff serialization.

1. Add real HTTP tests for oversized UTF-8, binary, and non-UTF-8 objects; an
   ordinary object over the server's 8 MiB response budget; pointer-sized Crab
   and LFS content; symlinks; bad metadata; strict byte limits; cancellation.
   Meter full packed-entry fetches and complete blob inflation, not only JSON
   response size. Preserve the old `/file` test unchanged.
2. Implement the explicit `file-preview` action and discriminated states from
   the target design. Charge returned metadata/inline bytes appropriately;
   do not charge an omitted body as response bytes. Metadata work still obeys
   normal object, delta-depth, storage, time, and cancellation limits.
3. Move every first-party file-inspection consumer to that endpoint and state.
   Follow `packages/ui/README.md`'s server-owned wire-contract rules: update the
   actual serialized type, export it through `api/contracts.rs`, and generate
   browser declarations; never hand-edit generated `src/lib/contracts.ts`.
   Use neutral omitted-size copy. Adapt symbol source, editor availability,
   copy/download controls, and preview-engine entry points to actual inline
   availability. Do not silently fetch full content just to recover a label.
4. Keep explicit asset/download/preview-engine reads functional. A user opening
   a media/data preview can deliberately request content; the initial metadata
   call must not reconstruct the same large object first. Preserve original
   `/file`, Diff, and raw contracts and document their compatibility boundary.

**Verify:** run metadata, HTTP large-file/range, SDK sibling, UI type/build,
browser wire-contract, and focused browser commands. Generate changed contracts
with `CARGO_TARGET_DIR="$READ_TARGET" npm run contracts:generate --prefix packages/ui`
after the embedded UI prerequisite, then rebuild the UI and check the contracts.
Add
`file_preview_omits_large_payloads_without_guessing_encoding` under
`crab-server`'s API tests and invoke its full test name with `-- --exact`.
Browser mocks verify UI behavior only; require real HTTP storage counters too.

**Exit:** no full body reconstruction/inflation for oversized ordinary base
blob inspection; delta metadata incurs only its independently metered bounded
descriptor/header work. Small-object text/binary decisions remain verified.
Old `/file` and Diff tests pass unchanged, and oversized unknown encoding is
never reported as verified text or binary.

### R3 — Share blame safely and preserve interactive capacity

**Scope:** remote-Git runtime/flight/budget/operation and blame owners with their
tests; server read admission and bounded metrics/tests. Do not move HTTP product
policy into shared crates or create a second backend/read session stack.

1. Implement budget/lifetime characterization before whole-blame sharing.
   Exercise simultaneous and late waiters with different line, comparison,
   history, depth, bytes, object-size, response, and time limits. Verify maximum
   versus additive dimensions and no double charging across nested object
   flights. Include strict-first/generous-late and the reverse order, above-default
   custom limits, and same-identity repository handles with different immutable
   `ObjectLimits`. Use `charge_cached` and shared physical-budget code as exemplars.
2. Create the bounded semantic flight keyed as designed. The runtime owns its
   producer operation/session; a caller dropping its future only removes that
   waiter. Finish the producer operation exactly once and preserve close-error
   precedence. A cache hit still replays semantic budgets independently.
3. Enforce bounded producer and working-set admission before expensive
   allocation. Retain reservations through computation, retained result use,
   and response serialization where ownership requires it. Reuse the shared
   resource owner from Plan 017; if it cannot cover these lifetimes, close that
   prerequisite instead of adding an unaccounted cache/pool or new env knob.
4. Add the bounded bulk/request policy without raising the total 16. Metadata
   and inline-preview traffic retain four slots during bulk pressure; count
   followers too. Keep product classification server-owned, reject excess work
   explicitly, and do not add unbounded pending queues.

**Verify:** run existing shared-read lifecycle and both blame-budget tests;
add semantic-flight tests under the same owners. Required named regressions:
`concurrent_blame_has_one_producer_and_independent_waiter_limits`,
`late_blame_waiters_replay_semantic_work_and_maximum_depth`, and
`cancelled_blame_waiter_does_not_cancel_remaining_readers`. Run the real HTTP
mixed-load driver from R0 as well as last-waiter/shutdown drain cases.

**Exit:** exactly one producer for a synchronized same-key cold burst; strict
waiters reject independently; physical work is counted once; no abandoned
backend/session/permit remains; interactive requests are admitted during bulk
saturation. Report successes and failures so extra rejection cannot masquerade
as improved latency. The declared memory envelope must pass before acceptance.

### R4 — Keep heavy verification off async executor threads

**Scope:** remote-Git `reader.rs`, runtime blocking/decode admission, snapshot
line matching, and their tests. Audit the four cache insertion families and
all affected consumers. No cache-key broadening or public object DTO change.

1. Add warm large-object verification counters and an unrelated async-progress
   test using controlled blocking work. Retain corrupt-cache and independent
   caller-limit tests. Measure checksumming separately from cache lookup.
2. Move large checksum verification into bounded tracked blocking work. Keep
   limit checks before dispatch; retain the exact object Arc and the permit
   until actual completion. Last-waiter cancellation must not release resources
   while an admitted CPU task still runs. Preserve `CacheCorrupt` and eviction.
3. Apply bounded CPU admission to Myers work without changing the algorithm or
   sliders. Avoid recursive acquisition of the same semaphore through object
   decoding. Integrate with R3's producer/session lifetime rather than adding
   an independent untracked task pool.
4. If counters show repeated hashing dominates after offloading, record a
   separate private verified-object design decision with all insertion paths,
   corruption semantics, and release proof. Do not implement skipped checksum
   checks, global OID-only caching, or new positive-cache scope in this packet.

**Verify:** original corrupt-cache test; admitted packed recheck test; concurrent
cold/warm blob test; snapshot cancellation/repeated-line tests; SDK siblings.
Add `large_cached_object_verification_preserves_async_progress_and_drains` and
run it under its declared owner. R0's warm checksum/mixed-load measurements
must show bounded CPU work and responsive unrelated reads.

**Exit:** all original integrity assertions remain; checksum CPU is metered;
blocking-job occupancy stays inside the configured/derived envelope; cancellation
and shutdown wait for actual task completion. No speedup claim may rely on
removing checksum bytes from a workload without a separately approved contract.

### R5 — Revalidate pinned content without retransmitting it

**Scope:** selected preview/raw HTTP response handling, auth/read admission,
representation validators and tests; necessary browser transport tests/docs.
No global weakening of `no-store`, public CDN caching, or application-state
response caching. Follow RFC 9110/9111 conditional and private-cache semantics.

1. Define raw and JSON validator identity separately, including every field
   affecting a serialized representation. Retain current UUID/path visibility
   validation before a matching validator can return `304`.
2. Add full-response GET conditional handling. For matching validators, avoid
   full payload reconstruction when authenticated metadata is sufficient.
   Emit quoted strong validators; evaluate `If-None-Match` using weak comparison,
   lists, and wildcard semantics. Keep `If-Range`'s strong comparison distinct.
   Cover selected content coding and `Vary` before reusing a validator.
3. Exercise real browser HTTP-cache revalidation; do not implement a manual
   `304` JSON parser without an owned retained response. Keep raw Range behavior
   and existing error statuses intact. Do not advertise cold range-I/O savings.

**Verify:** HTTP tests for unchanged body, changed mode/state/commit/generation,
permission revocation, owner/name reuse with another UUID, inaccessible path,
tampering, corrupted current metadata, and range siblings. Add
`conditional_file_reads_authorize_before_not_modified`. Run UI browser tests
against the real response-cache scenario in addition to ordinary fixtures.

**Exit:** authorized unchanged GETs return the right validator/304 and no full
body; revoked users receive the normal denial, not a cached success. JSON
validators change when representation metadata changes even if the blob OID
does not. Unrelated response cache policy is unchanged.

### R6 — Gate larger persistent-read designs on measured cold cost

**Scope now:** design/evidence only in this plan and the indexed-read ledger.
Production metadata formats, GC, and write/publication changes require their
own reviewed execution packet and applicable scoped guides. This is not a
partially implemented production feature.

Trigger only if R7 identifies cold attribution or cold range cost as a remaining
dominant bottleneck. Compare these options with existing indexed change points,
bounded batching, and a verified cache; choose the smallest design that meets
the frozen workload target:

- Persist line-comparison mappings keyed by repository identity, ordered
  parent/current blob IDs, and semantics version.
- Persist complete line-origin ranges keyed by pinned commit, raw path, and
  semantics; identical content at different paths/history can have different
  owners. Bound pages by output and authenticate their identity.
- For ordinary Git or LFS cold random access, investigate independently hashed
  blocks or an existing canonical verified representation. Do not present
  compressed pack offsets or an HTTP range as proof of partial Git-object
  integrity. Reuse Xet range mechanics where its actual representation applies.

Any accepted design must name the producer/maintenance owner, publication point,
reader pin and GC roots, quota/cost budget, corruption behavior, replication,
restore, incomplete-index behavior, format/version contract, and release-tag
upgrade/refusal policy. Background preparation must not block native push
acknowledgement or install a mandatory new data server. Additional write/storage
cost must be measured against eliminated read work.

**Exit:** a source-bound decision document accepts or rejects each option with
actual cold measurements and a lifecycle proof plan. No new persistent reader
or format lands under a “performance experiment” without that contract.

**Decision record, 2026-11-19:** defer all three persistent representations.
The retained 20-repeat native RustFS comparison binds R0 qualifier
`2ff8649df3b8c740d6d12aac580a8bfa336199bf7f73addcdce9d56f53c19ae3`
and R1 qualifier
`5a7944c05a158586de13bfe4ae9d0a1ba31123a0d4ffe085b7f73f0bee28b7c7`.
These are debug diagnostic inputs, not the combined release acceptance result.
The exact candidate hash is recorded in `native-comparison.json`; no persistent
format is accepted from these measurements.

| Option                                | Measured cold evidence                                                                                                                                                                                                                                      | Decision and next proof                                                                                                                                                                                       |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Persist comparison maps               | On 10,000 commits, R0/R1 blame query p95 is 1.953/3.523 seconds with the same median 667 backend reads. An interleaved repeat improves cold timing but retains a warm p95 failure. This does not isolate Myers as the dominant cost.                        | Defer. First isolate comparison CPU from ancestry, verified object reads and output. Measure map bytes and preparation CPU per eliminated comparison.                                                         |
| Persist complete line origins         | The same blame baseline is expensive, but repository page 100 already falls from 45.55 seconds/10,055 reads to 0.147 seconds/54 reads without a new format. Shared producers and retained results must be qualified before adding another persistent cache. | Defer. Measure remaining cold blame after the combined changes, varying unrelated history independently from file changes. Compare origin-index publication/storage cost with the existing change-point path. |
| Independently verified Git/LFS ranges | The real 9 MiB plain/delta preview corpus proves bounded metadata output and preserved full-response rejection. It does not establish a cold random-range latency or provider-byte baseline. Alpha.1 CRC validation can still fetch a large encoded base.   | Defer; missing range baseline. Measure authenticated cold range amplification separately for Git and LFS. Existing Xet chunk integrity does not prove either representation.                                  |

Before revisiting any option, its separate design must bind a repository UUID,
immutable snapshot/input IDs and semantics version; publish only complete
authenticated descriptors through the existing publication owner. Reader pins
and GC roots must retain every referenced page through cancellation and cleanup.
Qualification must interrupt preparation/publication, race readers with GC and
force push, recreate paths, corrupt/miss an advertised page, enforce quota, and
restore/replicate the index before accepting it. Missing optional preparation
uses the existing canonical computation; an advertised corrupt index fails.
Measure write acknowledgement, maintenance CPU/bytes and retained storage, and
name the release-tag refusal/upgrade contract before changing stored formats.
No preparation command, background writer, GC root or compatibility reader is
introduced by this decision.

### R7 — Qualify the combined source and supported resource envelope

**Scope:** R0 tooling, CI qualification integration, existing native Git/provider
oracles, supported operator/SDK/UI documentation, and retained evidence. Reuse
existing architecture, SDK, container, and provider gates; do not edit their
baseline/ignore/expected-failure files to obtain a pass.

1. Run the focused command table on the integrated revision, then appropriate
   affected-consumer, broad, cross-platform, and real HTTP/provider proof in CI
   or the dedicated test environment. Rebuild embedded UI before server builds.
2. Run cold/warm/post-push/restart/replica cells with exact source/binary hashes.
   Keep cache state explicit; an object-cache-only warm case and a complete
   blame-result hit are different measurements.
3. Verify force push, path recreation, mode-only changes, deletion, indexed/raw
   parity, corruption, stricter late budgets, cancellation, shutdown, and GC/
   reader retention where a changed owner requires it. Preserve native Git
   oracles and byte-identical content checks.
4. Publish a bounded support report containing thresholds, results, failures,
   active backend count after each run, capacity/resource limits, and remaining
   gaps. A passing local RustFS fixture is not hosted-S3 or all-cloud evidence.

**Exit:** the first-wave acceptance table passes on the same integrated source.
If hosted-provider or lifecycle cells are unavailable, mark those cells pending
and make no production-readiness claim. R6 may remain gated/rejected without
blocking a separately accepted first-wave improvement.

## Acceptance matrix and proposed engineering gates

These are proposed acceptance gates, not measured performance claims. R0 freezes
the machine-readable profile before tuning; any later change requires a stated
product/measurement reason and retained results against the old profile. Never
loosen a threshold merely because the implementation fails it.

| Axis              | Required cells                                                                                                                          |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| History           | 100/1,000/10,000 commits; fixed file changes; repository pages 1/10/100 where present; separate file/path history                       |
| Repository growth | Independently grow unrelated commits, refs, source inventories, and compacted/uncompacted frontiers                                     |
| Content           | Small inline source; oversized UTF-8/binary/non-UTF-8; plain/delta Git blobs; pointer files; Xet and LFS ranges reported separately     |
| Cache             | Fresh process; warm objects only; warm comparison cache; warm complete result; restart; ordinary push; force push                       |
| Load              | 1/8/16/32/64 offered clients; same-key burst and distinct keys; small-read-only, expensive-only, and mixed traffic                      |
| Deployment        | Dedicated local RustFS correctness; same-region hosted S3 latency; at least two server replicas; affected native platforms in CI        |
| Resources         | Declared CPU/memory/disk limits including the supported minimum profile; active buffers/responses and LRU occupancy measured separately |

Deterministic gates:

- New seek continuation consumes no skipped-prefix raw commit reads. Its query
  work is page-sized; opening/index proof work is counted independently.
- Oversized metadata previews do no full blob reconstruction. Pointer and delta
  metadata work is explicitly bounded and counted, not mislabeled zero I/O.
- Same-key cold blame has one producer. Every strict waiter and corrupt input
  preserves its independent failure; no session or backend task remains active.
- Total HTTP work remains bounded; bulk traffic cannot occupy the four reserved
  metadata/preview slots. All released corruption and byte-identity tests pass.
- Result/working-set/serialization ownership fits the frozen resource envelope;
  memory does not grow with completed request count during the soak.

Timing and throughput gates for the matched resource/provider profile:

- At least 20 independent cold runs per cell for p50/p95. Report p99 only with
  at least 1,000 completed observations per declared workload/load cell.
- Run a minimum ten-minute mixed-load measurement after warm-up. Record offered
  rate, successes, 429s, other errors, and latency together.
- Target at least 20% lower p95 in the specifically targeted, baseline-proven
  bottleneck cell. Untargeted matched controls must not regress p95 or successful
  throughput by over 10% without an explicit product decision.
- At the frozen mixed-load operating point, target interactive p95 within 2×
  its small-read-only baseline. Failure/rejection rates must remain inside the
  frozen service envelope; dropping more work cannot satisfy this target.
- Record peak and steady-state RSS against the predeclared byte envelope.
  Late-window resident ownership must plateau after queues and caches stabilize;
  an unexplained rise is a failure even if the LRU reports a bounded size.

Extend the existing evidence validator to enforce these fields/gates, reject
missing cells, and retain failed receipts. Relative thresholds are engineering
targets to qualify; absolute user-facing latency SLOs require the hosted-provider
baseline and a declared deployment/resource profile.

## Review and completion checklist

- [x] Source/release contract audit and drift checks recorded.
- [x] Every packet has a commit/evidence receipt or an explicit pending reason.
- [ ] R0–R5 and R7 pass their stated gates on the combined source.
- [x] Original cursor, `/file`, Diff, checksum, and range assertions remain.
- [x] HTTP authorization precedes cache/validator success on every touched path.
- [x] SDK, browser/server, and shared-reader consumers have appropriate proof;
      first-party browser mocks are never offered as provider I/O evidence.
- [x] No new environment knob, unapproved dependency change, hidden fallback,
      or unsupported production format was introduced.
- [x] Local Cargo targets and qualification data stayed on the mounted volume.
- [x] Documentation includes measured limits and unresolved provider/lifecycle
      gates; Plan 020 and its index status distinguish implementation from proof.

## STOP conditions

Stop and report the exact gap rather than inventing compatibility or weakening
tests if:

- A source excerpt/owner changed unexpectedly or a tagged contract was missed.
- A resume OID can reach an untrusted caller without the documented signer/
  opaque wrapper, or bounded validation cannot preserve first-parent semantics.
- A preview optimization requires claiming an uninspected encoding, changing
  Diff implicitly, or silently reconstructing omitted content.
- Sharing blame requires borrowing a departing caller's operation/session,
  bypasses a late limit, double-counts depth, or loses source errors on close.
- A blocking job outlives its reservation/permit, or minimum-profile memory
  cannot be bounded by the canonical resource owner.
- A cache change requires weaker verification, state-independent negative
  caching, or serving an object without current-view visibility proof.
- A required provider/replica fixture, credential scope, mounted volume, or
  source-matched binary is unavailable. Do independent work and mark that proof
  missing; never redirect a qualification command at production data.
- A reasonable fix and one retry still fail the packet's focused verification.
  Preserve the failing receipt and diagnose it; do not edit the expected result.

## Considered and deferred/rejected approaches

- **Smaller pages by default:** rejected as a general tuning rule. The retained
  4 KiB page experiment reduced bytes but increased requests and latency; optimize
  measured serial round trips and coalescing instead.
- **Higher global request concurrency:** deferred until isolation, producer
  ownership, and resource proof pass. The same 16 requests can perform much less
  duplicate work without admitting an unbounded additional working set.
- **Skip checksums or change Myers to Histogram:** rejected in this wave because
  that changes integrity or attribution semantics rather than qualifying the
  same operation more efficiently.
- **OID-only caches across generations/tenants:** rejected without separate
  current-view authority proof; current cache hits precede locator checks.
- **One HTTP cache validator for all blob-related JSON:** rejected because mode,
  preview policy, commit, and generation can change without a different blob.
- **Declare range support as backend range efficiency:** rejected. Git, LFS,
  and Xet have distinct representation/verification contracts.
- **Persistent blame before profiling:** gated behind R6, because storage writes,
  maintenance, GC/reader pins, replication, and format contracts must pay rent.

## Maintenance and reference notes

R0 tooling and thresholds must evolve together with cursor/representation/schema
changes. New query shapes need native oracles before timing acceptance. Any new
index format or stronger blame semantics requires new semantics-version keys
and a separate lifecycle/compatibility review. No task/packet IDs belong in
production comments or test names; name the invariant being protected.

Relevant primary references:

- `crates/crab-remote-git/{README.md,REFERENCE.md,AGENTS.md}`.
- `crates/crab-server/{DESIGN.md,REFERENCE.md,CELLAR.md,AGENTS.md}`.
- `crab/docs/architecture/indexed-remote-reads.md`, especially the retained
  page-size failure, comparison-cache results, and remaining provider gates.
- `crab/docs/architecture/crab-sdk.md` and `crab-sdk-api.md` for SDK delivery.
- `packages/ui/README.md` for server-owned browse state and UI ownership.
- [RFC 9111, HTTP caching](https://www.rfc-editor.org/rfc/rfc9111.html) and
  [RFC 9110, conditional requests](https://www.rfc-editor.org/rfc/rfc9110.html).
- [Amazon S3 performance guidance](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance-guidelines.html): measure requests/bytes/CPU
  and use a same-region deployment for the declared S3 latency profile.
