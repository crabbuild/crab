# Rust crate source quality

Status: in progress. Scope: all 21 crates under `crates/`.
Review: [PR #159](https://github.com/crabbuild/crab/pull/159).

The agent-guide work is complete in PR #158. This follow-up addresses source
correctness, ownership, diagnostics, readability, and executable documentation.
A crate is not qualified merely because its guide exists or its code formats.

Verified source batches:

- pointer error sources.
- ordered large-term diff matching.
- auth/storage credential Debug redaction.
- shared Git delta instruction decoder.
- refresh adapter API and retry-boundary documentation.
- LFS upload file error causes.
- cache credential Debug redaction.
- retry-policy validation before execution.

These commits cover specific invariants, not completion of the all-crate goal.

## Completion evidence

For each changed behavior, record its entry point, owner, callers, callees,
sibling implementations, existing tests, and the behavior on `main`.

- Reproduce correctness defects before fixing them; retain regression coverage.
- Preserve error causes, integrity checks, cancellation cleanup, and durability.
- Refactor only when the result reduces concepts or gives an invariant one owner.
- Explain non-obvious contracts at the code site; remove misleading commentary.
- Verify affected feature combinations and consumers. Broad suites and platform
  qualification belong in CI or a dedicated test environment.
- Keep README examples short, accurate, and linked to runnable examples/tests.

## Coverage ledger

“Pending” means source qualification remains; these are investigation targets,
not claims that the named code is defective.

| Crate | Next source inspection | State |
| --- | --- | --- |
| crab-types | Pointer causes and checked timestamps; broader contract qualification remains | Pointer and timestamp slices verified |
| crab-git | Shared delta decoder; discovery and process contracts remain | Delta slice verified |
| crab-diff | Large term comparison: ordered matches and duplicate counts | Comparison slice verified |
| crab-xet | Coverage count simplification; parser and reconstruction qualification remain | Coverage slice verified |
| crab-storage | Broader retry/error classification and cancellation cleanup remain | Diagnostics, multipart cleanup, and stream framing verified |
| crab-metadata | Remote writer selection and close contract; catalog lifecycle remains | Writer selection slice verified |
| crab-staging | Recovery lookup errors; flush/publication and scale qualification remain | Recovery slice verified |
| crab-coordination | Renewal control flow; provider and GC fencing contracts remain | Renewal slice verified |
| crab-lfs | First-verification cost and lock ownership remain | Upload cleanup, identity, and shared stream framing verified |
| crab-cache | Credential diagnostics; cache keys and invalidation remain | Diagnostic slice verified |
| crab-cache-store | Startup outcomes; origin authority and range qualification remain | Startup slice verified |
| crab-read | Term cancellation cleanup; hydration and source-chain qualification remain | Batch cleanup slice verified |
| crab-write | Shared cleanup error precedence; commit-graph coverage remains | Maintenance cleanup slice verified |
| crab-remote-git | Finish/shutdown docs; range and consumer qualification remain | Lifecycle documentation verified |
| crab-vfs | Mount teardown and shared FUSE/NFS lifecycle invariants | Pending |
| crab-auth | Key-source policy, power-loss durability, non-Unix locking remain | Diagnostic, load-outcome, and key-publication slices verified |
| crab-auth-store | Shared bounded auth retry; provider concurrency and gateway qualification remain | Unary retry slice verified |
| crab-auth-server | Shared output classification; receive/view cleanup qualification remains | Output slice verified |
| crab-cache-server | Eviction concurrency, shutdown, request validation | Hex input guards verified; broader lifecycle proof pending |
| crab-http-server | Request validation, embedded assets, service errors | Pending |
| crab-workflow | Async lock waiting, cache/resume, native qualification remain | Retry parsing, lock readability, metadata identity, and default API docs verified |

## Pointer diagnostic change

Owner: `crab-types/src/pointer.rs`. `Pointer::parse` calls UTF-8 and `u64`
parsers. Previously both failures became strings, so `Error::source()` could
not expose their typed cause. Keep the public error struct and its traits;
store typed causes privately without adding a crate dependency.

Consumer map:

- `crab-git/src/pointer_detect.rs` classifies parse failures; no error-chain use.
- `crab-read/src/error.rs` and `crab-vfs/src/error.rs` retain pointer errors.
- `crab/src/core/error.rs` converts them to `Protocol(String)`.
- `crab-auth-server/src/error.rs` converts them to `CorruptObject` text.

The last two conversions remain follow-up work: changing them requires checking
their error codes, response policy, and all variant consumers. This change does
not claim end-to-end source retention through those boundaries.

Proof: test UTF-8 failure position, integer failure kinds (empty, invalid,
negative, overflow), and absence of a synthetic source for wire-format errors.
Existing round-trip, version, size-limit, and detector tests protect parsing
behavior. Admission ledger and dependency budget must remain unchanged.

Validation: all 41 crate tests and admission/dependency checks pass. Strict
Clippy passes for all targets in the four source crates changed so far.
`crab-git` and `crab-coordination` also compiled as auth dependencies; full
read/VFS/product consumer qualification remains for CI.

## Large term diff investigation

`crab-diff/src/chunk_comparator.rs` previously switched to set membership above
8,192 combined terms. That path discarded occurrence counts and ordering,
while `build_segment_details` paired unchanged entries in order.

Both new large-input regressions fail against that implementation: one repeated
insertion produces 40,970 new bytes instead of 40,980, and reversing distinct
terms pairs different hashes as unchanged. The replacement shares the existing
ordered greedy matching algorithm with `chunk_sequence.rs`. Exact comparison
and each caller's work budget remain with their respective owners.

All 27 diff library tests pass after replacement, including both regressions
and the existing repetitive-chunk case that exercises the shared greedy path.

Sibling: `chunk_sequence.rs` already uses ordered greedy matching when exact
matching exceeds its work budget. The CLI calls `compare_sequences`;
`compare_terms` is exported and documented but has no workspace production
caller found. Do not claim a CLI regression from the term-only path.

## Credential diagnostic investigation

Derived `Debug` exposes credential fields in `crab-auth` (`CloudCredentials`,
`AzureToken`, `CachedTokens`, `OidcTokens`, `CrabAuthCredentialResponse`, and
`PushPrepareResponse`)
and `crab-storage` (`ObjectStoreCredentials`, `AzureAuthorization`).

Boundary: `crab-auth-store/src/lib.rs` converts resolved credentials to storage
credentials; `managed_repository.rs` consumes cached/OIDC tokens for refresh.
Managed transfer `SecretString` already redacts Debug, and provider objects
already use selective Debug output. Fix the typed payloads on both sides of
the conversion without changing their fields, serialization, or validation.

New integration regressions use synthetic values and check normal and pretty
Debug. They do not construct a token cache, access the keychain, or contact a
provider. All five initial auth regressions fail against derived Debug. Added
protected-push response coverage during sibling inspection. Selective Debug
implementations now retain provider/lifetime/operation metadata, and omit
credential payloads.

Validation: six auth redaction tests pass with `oidc-client`; five pass with
no default features. All 116 auth library tests pass with `oidc-client`,
including token serialization, encrypted persistence, and protocol validation.
Both storage redaction regressions fail before the fix and pass after it.
Strict all-target Clippy also passes after removing three unused lint
expectations from storage tests; assertions remain unchanged.

## Manifest documentation

Six READMEs (`crab-auth`, `crab-auth-store`, `crab-cache`, `crab-coordination`,
`crab-metadata`, `crab-vfs`) prescribed version-1 registry dependencies despite
their manifests declaring `publish = false`. Examples now use `workspace = true`
and explicitly apply to workspace members. Checked every named feature and
workspace dependency against the TOML manifests; no dependency changes.

## Local verification environment

User authorized `/Volumes/Workspace` because `$HOME/Workspace` is absent.
The checks above ran in worktree `089c` with these directories. Other
checkouts must use their own target directory, as required by root AGENTS.md:

```sh
RUSTC_WRAPPER= \
CARGO_HOME=/Volumes/Workspace/crabbuild-target/crab-089c-cargo-home \
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-089c-quality \
cargo test -p crab-types --locked
```

The separate Cargo home avoids pre-existing dangling registry cache/source
symlinks into another checkout. Do not repair or reuse that checkout's paths.
The wrapper override bypasses the shared `sccache` process, which stalled the
first compilation attempt. Other checkouts' processes are left untouched.

## Shared Git delta decoder

`crab-git/src/delta.rs` now gives validation and reconstruction one private
instruction walker. Previously each decoded copy/insert commands separately.
The refactor removes that duplication while retaining output allocation limits,
base-length checks, per-instruction cancellation, and typed corruption errors.

Callers: incoming-pack quarantine applies deltas before retaining objects;
`crab-remote-git/src/reader.rs` applies them for object reads and validates them
for metadata-only inspection. The locked `gix-pack` delta implementation confirms
copy-field decoding and the implicit 64 KiB copy size. No public API changes.

Proof: seven delta tests pass before and after the refactor, including malformed
instruction parity and implicit-copy-size coverage. Nine incoming-pack tests
pass, including native Git full/thin reconstruction and cleanup. Remote-reader
REF_DELTA, OFS_DELTA, and metadata-only tree-listing tests pass. Strict all-target
crab-git Clippy passes. Production decoding shrinks by removing the second loop;
tests retain the shared integrity contract.

## LFS upload error context

`LfsObjectStore::put_stream_with_size` and `stream_file_parts` both annotate local
file failures. The old wrapper converted the OS error to text. A private typed
context now retains that cause and the filename inside the existing I/O variant.
CLI and auth-server conversions both retain the I/O error; download fallback
classification remains unchanged. No dependency or public enum change.

The public upload regression fails against the old wrapper because no original
I/O source is available. All 31 object-store tests pass with the replacement,
including upload error sources, multipart round trips, corruption rejection,
and abort behavior. Strict all-target Clippy passes for crab-lfs and crab-git.

## Refresh adapter contract documentation

`crab-auth-store/src/refreshing_store.rs` exposes backend parts and the refresh
constructor without describing their consistency requirements. Rustdoc now
records shared state, target binding, constructor preconditions, and capability
queries that do not themselves refresh. The README distinguishes unary retries,
stream bodies, raw upload handles, and stable-ID multipart calls.

Evidence: traced the CLI builder in `crab/src/auth/mod.rs`, all three unary
retry helpers, list/delete adapters, and raw multipart creation. Existing tests
cover proactive refresh, one authentication retry, fresh permission denial,
shared handles, and target/destination rejection. The locked object_store 0.14.1
MultipartUpload contract confirms that its returned handle owns subsequent
part/completion/abort calls. This is documentation of current behavior; no
refresh policy was changed or newly qualified against a live identity service.

## Workflow retry validation investigation

The previous semantic validator checked `backoff_multiplier < 0.0`, missing NaN
and positive infinity. The locked serde_yaml parser explicitly accepts `.nan`
and `.inf`. Retry planning then normalizes NaN through `max(1.0)` and handles
infinity through its non-finite backoff branch, hiding invalid configuration.

The semantic-validation regression fails on the baseline: `.nan` is accepted.
Caller inspection found that `crab run --validate` invokes semantic validation,
while execution paths parse separately. A validator-only patch would therefore
be incomplete. `RawRetry::into_policy` is shared by stage-local and default
retry declarations. It now uses the same private range validator as semantic
validation. Both reject invalid policies; finite nonnegative multipliers retain
their existing behavior. All 91 YAML tests pass, covering stage/default policies,
programmatic invalid policies, templates, overrides, and existing parsing rules.
Strict all-target Clippy passes. The parser returns the first invalid policy
error; aggregate validation still collects errors for programmatically
constructed workflows. Product execution proof remains for CI.

## Cache credential diagnostics investigation

Before the fix, three derived Debug surfaces exposed cache credentials: `CacheServiceAuth`,
`ActiveProbeAuth`, and `CacheClient`'s stored header string. Redacting only the
first enum would leave the other two exposed. Config Debug in crab-cache-store
and the CLI nests CacheServiceAuth; doctor and cache-server onboarding use the
borrowed active-probe enum. Header construction consumes the original values.

Added synthetic integration regressions for normal and pretty Debug. Coverage
is feature-gated to the base contract, active probe, and remote client. The
client constructor builds configuration without issuing requests. Baseline
execution fails for all three credential surfaces. Selective Debug implementations
now pass all three regressions; four existing request-header tests also pass,
confirming PSK/bearer values still reach requests and no-auth/mTLS add no headers.
The base credential regression also passes without default features. Strict
all-target Clippy passes for crab-cache (remote-client) and crab-workflow.

## Publication cleanup ownership

`crab-write` repeated the same outcome selection five times: catalog planning
reader close, catalog writer close, catalog lease release, and owner/reader
journal compaction release. A private `finish_after_cleanup` now owns that rule:
surface cleanup failure after success; preserve the primary operation error and
log the cleanup cause when both fail. Callers still await close/release first.
No public API or lifecycle ordering changed.

Caller map: CLI push owner/reader wrappers and metadb generation maintenance;
HTTP maintenance enters through `generation::make_readable`. Callees remain
metadata reader/writer close and coordination's holder-checked lease release.
The namespace sibling intentionally preserves a known committed outcome even
after lease failure and does not use this maintenance helper.

All 18 existing generation/journal integration tests pass before and after the
refactor. Four library tests (including typed I/O error precedence) and the
native-Git catalog reconstruction test also pass, for 23 focused tests. Strict
all-target Clippy passes. Production line count across the three source modules
is unchanged while five outcome decisions become one. The README now has a
publication-flow diagram, subsystem headings, and a compact test evidence table
instead of uncited historical timing paragraphs.

## Lease renewal control flow

`crab-coordination::while_renewing` duplicated primary-error selection inside
its pending-renewal branch. That branch is guarded by `renewal_error.is_none()`;
when work completes there, no renewal failure has been recorded. It now returns
the operation result directly. The outer path still drains work after renewal
failure and preserves its primary error. No polling priority, timer, lease
release, or public API changed.

Callee: `PushLock::renew` performs holder-checked CAS and updates the stored
ETag only after success. Callers in CLI metadb and crab-write journal/catalog
await the wrapper and release explicitly. Namespace publication separately
records a committed outcome before processing lease errors. Existing main
behavior includes both primary-error draining and an operation winning over a
stalled backend retry; these remain the intended contracts.

The lost-lease test passes before and after simplification. All 108 coordination
library tests with `object-store-lock` and strict all-target Clippy pass locally.
The CLI `completed_owner_does_not_wait_for_stalled_renewal` consumer test also
passes. Its test binary emitted an Apple linker warning about the size of the
DWARF unwind section; compilation and execution succeeded. README now gives an outcome table and distinguishes coordinator
commit from durable regional projection. CLI push and protected receive both
persist that projection before acknowledging regional materialization.

## Term-resolution cancellation ownership

Owner: `crab-read/src/term_resolver.rs`. Both term and sequence batching on
current main can return on cancellation while spawned workers remain detached,
bypassing `close_file_index_lookup`. Locked Tokio 1.52.1 `JoinHandle` docs
explicitly describe detach-on-drop. The callee's `FileIndexLookupSession::close`
consumes and closes its SlateDB reader; dropping worker handles is insufficient.

Both batch paths now stop spawning on cancellation, drain all existing handles,
and then close the lookup session before selecting the returned error. Workers
waiting for admission observe cancellation directly. A shared drain routine
retains strict first-error behavior and cancellation precedence. Already admitted
I/O is allowed to finish; this is orderly cleanup, not an I/O timeout or an
asynchronous drop guarantee. README and scoped guidance state that callers must
await batch futures through cancellation.

Callers: the CLI diff facade forwards terms and sequences; `cmd/diff.rs` and
`cmd/diff_driver.rs` compose it. The strict sequence method is exported but has
no current workspace production caller. Both sequence policies use the same
cleanup path. Normal per-file best-effort omission remains unchanged.

The drain regression failed with the original early-return branch extracted
into the common routine, then passed after retaining cancellation until all
workers joined. It proves shared ownership is released before return in both
strict and best-effort modes. Public batch admission cancellation is additionally
checked for all three APIs. Existing origin/cache shard reuse and corrupt-range
repair tests remain the integrity regression coverage. Broader hydration and
real SlateDB cancellation qualification remain separate work.

Validation: all four term-resolution tests and strict all-target Clippy pass.
Production code shrank; added lines are regression tests and lifecycle docs.

## Staging recovery filesystem errors

`crab-staging/src/recovery.rs` used `Path::exists` before current-segment
recovery. Rust's installed standard-library documentation confirms that this
method coerces lookup failures to false. An inaccessible current segment could
therefore be treated as absent, deleting pending rows and resetting its durable
boundary before the subsequent writer open failed.

Recovery now inspects metadata once, permits absence recovery only for
`ErrorKind::NotFound`, and returns other typed I/O causes before changing that
segment's rows. Sealed-segment lookup likewise distinguishes missing data from
an I/O failure. Orphan temp cleanup directly removes the entry and tolerates
only NotFound, covering dangling symlinks and removing the check/delete race.
No serialized format, public error variant, or durability ordering changed.

Entry point: writable `StagingArea::open_with_acquired_lock` calls recovery
under the exclusive process lock, then `SegmentWriter::open_recovered`.
Callees: Index's promoted/pending offsets, deletion of incomplete pending rows,
and durable-boundary update. CLI and protected-server conversions preserve
`StagingError::Io` as their typed I/O variant. Confirmed missing/short sealed
segments remain corruption errors; existing torn-tail and promoted-row tests
protect the destructive recovery rules.

Three Unix regressions fail on main behavior: a looping current symlink loses
its pending row, a looping sealed symlink loses the I/O cause, and a dangling
temporary symlink survives cleanup. The fixtures work without assuming process
permission restrictions. These are local filesystem/SQLite checks, not
power-loss or platform-wide qualification.

Validation: all 14 recovery tests and strict all-target staging Clippy pass.
Production recovery grows nine lines to distinguish three filesystem outcomes;
regression fixtures account for the remaining source growth.

## Remote index writer selection and durability

`crab-metadata::RemoteIndexWriter` can open file and chunk indexes independently,
but `write_opened_entries` silently skipped nonempty entries when their database
was not selected. The writer now checks both selections before buffering either
batch and returns the existing Internal error for a caller-contract violation.
This adds no public variants or dependencies and leaves empty batches valid.
It does not promise cross-database atomicity for later codec or storage errors.

Caller map: protected receive and path-view publication open both indexes;
`write_index_entries` derives selection from nonempty input. Those correct paths
remain supported. Callee: SlateDB 0.15.0 writes use `await_durable: false`; its
close implementation flushes outstanding writes while the database is healthy.
`close_opened_writers` awaits both handles before selecting the first error.
Catalog writers/readers have separate publication/checkpoint policies and keep
their existing close paths.

Rustdoc previously called buffered writes a commit. It now states selection,
partial-failure, durability, and close ownership explicitly. README includes a
writer outcome table, separates snapshot identity from lifecycle, and replaces
a dense lookup-budget paragraph with a field table and a byte-budget example.

A new real-SlateDB/in-memory-store regression rejects mismatched selections in
both directions and checks neither index received a row. It failed against the
old implementation. Existing tests cover flush-on-close, receipt round trips,
fresh indexes, and configured paths.

Validation: all five remote-index tests and strict all-target Clippy with
`remote-index` pass. Added production logic is one preflight loop; the remaining
source growth documents durability and exercises both database selections.

## Reconstruction coverage validation

`crab-xet::validate_term_coverage` allocated a per-file boolean vector to mark a
contiguous prefix and repeated the same checked sum after equality was already
established. The validator now computes the diagnostic position directly from
its checked count. Reversed ranges, count overflow, missing/excess counts, and
existing example-hash diagnostics remain unchanged; repeated Xorb ranges stay
valid. Removed bookkeeping reduces production code and avoids allocation
proportional to the input chunk count on mismatch.

Caller map: CLI exports a forwarding wrapper and has coverage regression tests;
no production invocation of that validator was found. Production recipe
construction instead calls FileTermBuilder push/finish. The latter is unchanged.
Parser siblings separately verify serialized payload digests and decoded chunk
hashes; whole-file identity remains with crab-read. Documentation now makes
these distinct guarantees explicit and corrects the builder's constant-memory
claim: emitted terms and distinct starts can grow with recipe fragmentation.

All five reconstruction tests pass before and after the refactor, including
missing/excess diagnostics, malformed/overflow ranges, and repeated chunks.
Strict all-target Clippy passes with default features. Locked Cargo dependency
trees also show xet-core-structures -> xet-runtime -> reqwest even with no Crab
features; README no longer claims the default transitive graph is runtime-free.
This is a source/API simplification, not a measured production performance claim.

## Remote Git operation documentation

Reviewed OperationContext open/finish/drop, tracked locator close, runtime
shutdown/task tokens, and the qualification example's semantic-result pattern.
Existing cleanup retains both semantic and close failures and drains tracked
contexts. No behavior change was needed in that examined path.

Added a compiled rustdoc example that passes the semantic result to finish
without an intervening early return. README carries the same usage pattern,
a close-outcome table, deadline precedence, and process shutdown ordering.
Runtime rustdoc now states that shutdown waits for live operation contexts,
not just single-flight tasks. Locked tokio-util 0.7.18 tracker code confirms
that tokens count as outstanding work until released. Existing shutdown tests
cover active and dropped contexts.

The README's long performance section now has cache/budget, generated-pack,
source-reuse, traversal, and deployment subsections. Qualification commands
use an explicit external target directory. The operation example compiles as
a doctest. These documentation improvements do not claim complete remote-read
or provider qualification.

## CI observation: repository browser contrast

Run 34103218613, job 101682532999 failed the release browser accessibility
assertion at releases.e2e.ts:275: the delete tooltip measured 1.42:1 contrast
in dark mode. The other 29 browser tests passed. The entire packages/repository
tree is identical to origin/main (tree 4889f8fd8012415770e0d8fa50ef54f8dbb85ace),
as is the job definition. Playwright starts Vite and this test mocks /api/**;
it does not invoke any changed Rust crate. This is an unrelated existing
frontend surface, not a passing gate. Logs were retained by CI. The PR stays
draft; no test, baseline, stylesheet, or workflow was changed to silence it.

Remote Git validation: the finish doctest and both native-fixture shutdown
integration tests pass. Runtime behavior and public signatures are unchanged.

## Cache-store startup outcomes

`CachingStore::try_build_healthy` nested health, capability retrieval, and route
validation inside four levels of control flow. Guard clauses now expose each
exit directly, preserving the same log messages and remote-client disabling
policy. No new fallback or configuration mode was added.

The old rustdoc incorrectly promised Some unconditionally. Construction errors
return None; runtime probe failures retain a local-only wrapper. `new` constructs
the configured client without probing health. An explicit LocalCache keeps its
own limits rather than inheriting CacheConfig::max_bytes. Rustdoc and a README
constructor table now describe those differences.

Caller map: filter-process chooses the origin on None; add, push, and remote
helper also consume the optional wrapper. Callees: CacheClient construction,
its bounded health request, capability decoding, and the shared route-contract
comparison. The canonical read fallback and xorb verification paths are unchanged.
A loopback cache-server test passes before and after for authorized and rejected
credentials. The no-default-features test confirms unsupported service config
returns None. These tests qualify startup decisions, not all cache read behavior.

Strict all-target cache-store Clippy passes with remote-client. The refactor
adds no production API and reduces nesting; added tests protect constructor
behavior across enabled and disabled feature configurations.

## Cache-server hexadecimal input

The config PSK decoder and public cache hash decoder checked only byte length
before slicing two-byte string ranges. A 64-byte value beginning with a
three-byte character panicked at a UTF-8 boundary. Both now reject non-ASCII
input before indexing, preserving their existing error/None contracts and all
ASCII hex decoding behavior.

Entry points: TOML config parsing through parse_auth and the binary's config
loader; cache_store hash decoding for shard verification, persisted object-name
recovery, and storage IDs. The new public-config regression covers multibyte
characters at different alignments. The cache helper has its own regression.
Both panicked before the guards were added. No auth hash format changed.

Sibling search: HTTP api::decode_hex already validates ASCII hex digits before
slicing. HTTP contents::validate_path, workflow stage_runtime::parse_cached_hash,
metadata split_commit_graph::parse_sha1_hex, and macOS auth token_cache::hex_to_key
have similar unchecked byte slicing and require the next cross-crate pass.
Their callers and regressions are not yet qualified; this cache-server change
must not be presented as a completed fix for every hexadecimal parser.

README specifies the PSK input shape and uses external target directories for
service commands. The repeated frontend CI failure on run 34103916186 is again
the release tooltip contrast assertion (29 browser tests passed); the unchanged,
mocked frontend surface remains outside this Rust change.

Validation: 15 config tests and 41 cache-store tests pass; strict all-target
cache-server Clippy passes. Removed unnecessary allocations/borrows and stale
lint expectations in tests, retaining assertions. Moved the request logging
helper before the test module without changing its structured fields.

## Workflow URL digest decoding

Pinned URL input reached two-byte string slicing after a byte-length check,
so a 64-byte multibyte value panicked through DepUrlHashExt. The new public
regression reproduced that panic. Pinned and cached values now share the
private parse_b3_digest decoder, with ASCII validation before slicing.
Prefix, length, and invalid-character configuration diagnostics remain at
the pinned-input boundary; no error includes the supplied digest.

All three cached consumers (HTTP, object-store object, object-store prefix)
receive hashes from ExternalHashIndex::reusable, which already validates
ASCII hex. The helper regression failed in isolation, but that does not prove
a reachable cached-input panic. Index rejection and fresh-content reads stay
unchanged. No persisted shape, dependency, or public signature changed.

HTTP contents, metadata split-commit-graph, and macOS keychain decoding remain
identified follow-up surfaces; their full caller proof is still pending.

All 15 stage-runtime tests pass, including real loopback HTTP reads and index
reuse, object/prefix hashing, and both baseline-failing decoder regressions.
Production parsing shrank by five lines; added tests cover the input contract.
Strict all-target workflow Clippy and formatting pass.

## Split commit-graph hexadecimal IDs

The public CommitGraphTraversal implementation forwarded IDs to a decoder
that checked 40-byte length before slicing UTF-8. A public trait regression
reproduced the panic. The decoder now rejects non-ASCII IDs; membership returns
false, and traversal returns None when an invalid ID is needed for its answer.
The regression covers membership, shallow tips, reachability tips/boundaries,
and roots/wants. Existing successful ancestry and shallow tests remain intact.

Callers inspected: CLI repack graph/manifest identity checks, fetch traversal-root
admission, and shallow pack filtering. Callees use fixed-size OIDs and positional
graph records. The sibling CommitGraphSummary uses string comparisons/maps,
not byte slicing; its distinct completeness semantics are unchanged. No binary
layer format, generation binding, public signature, or provider contract changed.
Rustdoc now states invalid-input outcomes, including short-circuit root traversal.

Eight split-graph tests pass with no default features. That build reports five
pre-existing dead-code warnings in ref_registry and shallow_closure; feature-gate
cleanup remains a separate quality item. HTTP contents and macOS auth keychain
hex decoding still need completion of their caller and regression proof.
Strict all-target metadata Clippy with remote-index passes.

## Auth Keychain decoder

The macOS hex_to_key helper accepted a 64-byte string before two-byte slicing.
A synthetic multibyte regression panicked before the fix. It now returns a
KeyStore error without key material. Valid upper/lowercase hex still decodes.
The caller keychain_load_key reads security CLI output; load_or_create_key
already handles key-store errors through file_based_key. No fallback policy,
keychain command, crypto format, or public signature changed. The sibling
read_key_file checks raw byte length and copies bytes, so it has no UTF-8 slicing.

All 18 token-cache tests pass on macOS, including encrypted persistence and
concurrent store/load. Tests construct synthetic keys and temporary directories;
no live Keychain command was run. HTTP contents decoding remains pending.
Strict all-target auth Clippy with default features passes.

## HTTP content-path decoding

Single-file create/update/delete share validate_input; upload uses
validate_upload. Both call validate_path before opening the repository or
publishing objects, after repository authorization. Their error mapper returns
HTTP 400 for Input. Two regression tests reproduced UTF-8 slicing panics through
these validation boundaries, then passed with the ASCII guard. Tests verify
response mapping directly, not a live HTTP request. The sibling api::decode_hex
already validates ASCII hex digits, so read decoding is unaffected. Valid raw
Git bytes remain representable as hex; no path normalization policy changed.

Frontend build prerequisite passed after installing missing local dependencies.
No dependency or lockfile change retained. Current PR browser checks passed on
head 0bf1d72e99c; broader Rust/platform CI was still running when inspected.
Strict all-target HTTP-server Clippy passes.

## Metadata feature boundaries

Private ref-registry persistence records, their Default implementation, and
root schema constant now use the same storage gate as every consumer. The
shallow-closure descriptor test helper follows its storage-only test. This
removes five minimal-build dead-code warnings without lint suppression or
changing public contracts, serialized fields, or runtime code.

Storage-only qualification found GitCatalogVisibilityRead was compiled despite
its only constructor and re-export requiring remote-index. Its declaration and
import now match that gate. Public availability is unchanged. Verified all
references in ref_registry, shallow_closure, and git_visibility before gating.

Minimal-feature strict all-target Clippy passes. With storage enabled, 26
registry tests and five shallow-closure tests pass, including CAS updates,
conservative roots, isolated partitions, stale descriptors, and corrupt entries.
Strict all-target Clippy also passes with storage and remote-index separately.

## VFS task ownership inspection

Read pipeline worker creation, HydrationService spawn/worker/drain/prefetch paths,
coordinator shutdown, daemon teardown, and NFS shutdown. Queue cancellation
clears pending work after the current synchronous step. Read-window prefetch
spawns independent tasks and discards their handles. The coordinator timeout
owns a future containing worker handles; timeout drops those handles rather
than proving worker completion. Daemon requests abort without joining workers.
NFS manages server/control/refresh separately. These are qualification gaps,
not evidence that teardown is complete or that corruption has occurred.

Corrected rustdoc and README to distinguish queue-worker completion from total
hydration completion, and fixed the constructor's worker-method link. Runtime
behavior is unchanged. Follow-up must consolidate task ownership across all
mount owners and qualify real teardown before claiming resource release.

## VFS shutdown implementation contract

Further owner tracing confirms PipelineOutput retains HydrationService, whereas
RepoRuntime currently retains only worker handles; NfsMountedSession retains the
engine. Production worker startup occurs in pipeline and daemon. Coordinator
also has synchronous shutdown/Drop paths that cannot await. This requires a
cross-owner change, not just replacing one spawn call.

Locked tokio-util 0.7.18 TaskTracker::close explicitly permits subsequent spawns;
wait completes on closed-and-empty. A cancellation check followed by spawn is
therefore insufficient admission control. The implementation must serialize
registration with shutdown admission closure (short synchronous critical section,
no await under its lock), register all queue/prefetch work, cancel, then drain.
No new dependency is needed: crab-vfs already enables tokio-util/rt.

Implementation order and acceptance evidence:

1. Give HydrationService one background-task owner with an admission-closed state.
   Register work before releasing admission protection. Reject queue/prefetch
   scheduling after closure; preserve foreground-read behavior until backend
   unmount has completed. Test shutdown racing registration and a held task:
   completion must wait for task release, and no task may register afterward.
2. Replace production raw worker-handle ownership with retained service ownership
   in pipeline/daemon/coordinator. Keep one shutdown method; remove redundant
   vectors when all callers migrate. Test duplicate startup and shutdown calls.
3. Preserve backend ordering: NFS journal sync and native unmount need a serving
   backend. Stop admission to new backend requests before final hydration drain;
   never cancel reads needed by unmount first. Await refresh/control/server aborts
   before releasing their state. Cover backend-error cleanup as well as success.
4. Coordinator grace-period expiry must retain completion ownership. It must not
   drop JoinHandles and then describe resources as released. Synchronous Drop
   cannot promise async completion; document its limited request-only contract,
   and qualify explicit async shutdown as the supported completion path.
5. Exercise nfs and fuse feature builds and lifecycle fixtures, then native mount
   read/unmount tests in their dedicated environments. Queue tests alone cannot
   prove prefetch or kernel-request teardown.

Open evidence: ownership of foreground read futures during native unmount,
error ordering when unmount fails, and mount-removal races in daemon startup.
Inspect those before changing cancellation timing. This section specifies the
required implementation; it does not claim that runtime shutdown is repaired.

## Daemon task completion before cache release

DaemonService::teardown_runtime now joins its refresh/watcher tasks after abort,
then unmounts the backend, aborts all queue workers and joins them before dropping
snapshot/overlay/resolver/cache-lock ownership. Backend errors are still retained
until cleanup finishes. Tokio 1.52.1 JoinHandle documents abort as a cancellation
request; it does not establish completion. The regression retained four Arc
guards after baseline teardown and only the observer after the fix.

All 36 daemon tests pass with nfs, including the regression. No new runtime
abstraction or dependency. Awaiting cancellation may wait for a synchronous step
to return; no hard shutdown deadline is claimed. This function serves both
backend variants, but fuse compilation/native teardown proof remains pending.
Coordinator, failed startup and NFS task handles still need equivalent ownership
work. Detached read-window prefetch is outside these daemon-owned handles and
still requires the admission/tracking consolidation specified above.

Validation limitation: strict all-target VFS Clippy with nfs failed with 418
lib-test diagnostics, including existing literal/style/test-panic issues across
several modules. Baseline comparison and attribution of new diagnostics are
pending. The daemon runtime change remains uncommitted/unpublished until its
lint impact is isolated; the 36 passing tests are not a clean quality gate.

Clippy attribution completed: structured diagnostics compared by lint code,
message, primary source file, and source text (ignoring shifted line numbers).
The first test version added one no-effect binding warning; the guard now stays
owned until an explicit drop after the pending future. The final current and
committed baseline each have 417 error diagnostics, with zero added or removed
fingerprints. Baseline daemon.rs is identical to origin/main. The corrected
regression passes again. No lint suppression was added. Strict VFS Clippy is
still not clean; the backlog remains part of the all-crate quality work.

## VFS literal readability

Applied 140 Clippy-suggested numeric separators across nine VFS files. Before
writing each edit, checked that removing underscores yields the exact original
literal. After formatting, every changed Rust file remains byte-equivalent to
HEAD after underscore removal; no numeric value, assertion, or control flow
changed. No new tests are needed for separator-only changes.

Strict all-target nfs Clippy now reports 277 diagnostics, down from 417; all
140 unreadable_literal diagnostics disappeared and all other lint counts remain
unchanged. The command still fails, so this is measured backlog reduction rather
than a clean VFS gate. Formatting and diff checks pass.

## VFS helper and locking clarity

Replaced 17 identical poison-error closures with PoisonError::into_inner;
recovery semantics are unchanged. VerifiedSet::len exists only under cfg(test)
and only its child tests call it, so made it private instead of expanding the
API with an unused is_empty helper. Corrected the lock-free read claim: locked
DashMap 6 source obtains a shard read guard in _get. Five verified-set tests
pass. Strict all-target nfs Clippy decreases from 277 to 259 diagnostics: exactly
17 redundant-closure and one public-length-helper warnings removed. Remaining
lint counts are unchanged; the strict gate still fails. No suppressions added.

## VFS timing fixtures

Twelve test durations now use minute units, already used by production VFS code;
all values are unchanged. One test uses checked Instant subtraction with an
explicit test assertion instead of the panicking subtraction operator. Six
backoff tests and the elapsed-retry test pass. Strict all-target nfs Clippy now
reports 246 diagnostics (13 fewer); timing diagnostics are gone. No assertions
or lint settings were weakened, and runtime timing policy is unchanged.

## VFS fixture setup clarity

Five NFS attribute fixtures initialize their changed field together with the
remaining defaults. Snapshot borrowing uses slice::from_ref instead of a clone.
The Unix control test retains its concrete endpoint and wraps it only for the
optional server argument, removing an unnecessary unwrap. Assertions unchanged.
Four attribute tests, four symlink tests, the Unix control socket shutdown test,
and snapshot pointer round-trip pass. Strict nfs Clippy decreases from 246 to
238 diagnostics; remaining categories are test unwrap/panic and match style.
No production behavior or lint policy changed; strict qualification remains open.

## VFS test lint scope

Checked every remaining unwrap/panic diagnostic against its source: all 235
were inside cfg(test) modules. Root policy prohibits production panics and permits
test assertions. Added explicit, reasoned expect annotations only on affected
test modules, matching existing test conventions. Production crate deny rules
remain unchanged. These annotations acknowledge intentional test failures;
they are not runtime fixes or deleted assertions. Expectations become warnings
if no longer fulfilled. Combined the NFS module's existing unwrap expectation
with its test-panic expectation to keep attribute style consistent.

Simplified two error-result matches to let-else and made the source variant
match exhaustive. Both error-path tests and all 24 source tests pass. Strict
all-target crab-vfs Clippy with nfs now passes. This qualifies that feature/lint
surface only: fuse/platform and lifecycle-completion proof remain outstanding.

## VFS FUSE feature lint qualification

Strict all-target Clippy now passes with fuse and with nfs separately. FUSE-only
builds exposed shared async APIs whose awaits exist only with nfs: daemon
shutdown_mount_session/read_status and mount_control::reset_overlay. Added
feature-conditional expectations naming that API contract, rather than changing
public signatures or duplicating backend entry points. read_status mutability
is now scoped to nfs. Test-panic expectations are limited to FUSE test modules;
four test pointer conversions use pointer::cast without changing addresses.
No mounting was performed; this is compile/lint proof, not native teardown proof.

## NFS task joining in progress

Updated native mount failure to join its aborted listener before returning the
original mount error. run_until_cancelled now joins refresh/control tasks before
journal sync, keeps the listener serving through native unmount, then joins it
unless select already consumed its result. The loop returns that ownership state
explicitly to avoid polling a completed JoinHandle twice. Existing drain,
unmount, then server-error precedence is unchanged. Strict nfs Clippy passes.

This runtime change is not yet committed or published: it still needs lifecycle
regressions for cancellation and already-completed server results, plus failed
native mount cleanup proof. No claim of native mounted behavior is made. NFS
listener child connection ownership also requires upstream-source inspection
before claiming all request futures have completed when the listener joins.

## NFS dependency ownership evidence

Inspected locked nfs3_server 0.11.0 tcp.rs and transaction_tracker.rs.
NFSTcpListener::handle_forever spawns Cleaner::run and one process_socket task
per accepted connection, discarding both handles. process_socket additionally
spawns its message handler without retaining a handle. Contexts retain Arc VFS
references. NFSTcpListener::drop notifies Cleaner through Notify; connection
handlers do not consume that stop notification. NFSTcp exposes no drain method.
The socket processor is crate-private except under a test-only re-export feature.

Therefore local listener joining is not whole-backend completion proof. Do not
publish a claim that cache/snapshot ownership is released merely because the
listener joined. The in-progress local joins can still establish parent-task
completion, but full shutdown needs an explicit dependency task-ownership
solution and connection-level regressions. Do not enable test-only exports in
production or patch/vendor the dependency without required authorization.
README now records this limitation. No dependency or lockfile changed.

Local NFS regression evidence: cancellation_joins_listener_task fails against the
previous production function (listener-owned Arc remains alive) and passes with
the join. completed_listener_is_not_joined_twice also passes, covering the select
branch that consumes the join result. Both use the real run_until_cancelled with
synthetic listener tasks, empty engine state, and a nonexistent mount path;
they do not invoke native mounting or qualify dependency child connections.
Strict all-target nfs Clippy passes with these tests. Failed native mount and
journal/unmount error injection remain unqualified; no complete NFS shutdown
claim is made. The joins improve directly owned task release only.

NFS parent-task error qualification: listener_error_survives_cleanup verifies
that a selected listener ConnectionReset retains its typed I/O error after
cleanup. aborted_listener_is_not_joined_twice covers the cancelled JoinError
branch. All 16 nfs_mount tests pass, including the earlier completion/cancellation
regressions; strict all-target nfs Clippy passes. Tests use no native mount and
do not prove concurrent error precedence when cancellation wins select, failed
native unmount, or dependency connection draining. Those gaps remain explicit.

## Auth-server helper output classification

The output boundary now chooses the stderr prefix and exit code together,
removing duplicate conflict classification without changing either helper's
public result mapping. Both binary entry points call emit_json_result; receive
helper Python consumers check the return code before parsing JSON and map
conflict/invalid prefixes, while view consumers preserve a generic error path.
Read the complete output module, both binary entry points, error variants and
receive_helper.py/view_helper.py consumer branches. Existing main behavior is
preserved; no dependency behavior changes. README now documents the mapping,
Clap's separate argument handling, cleanup warnings, and absence of automatic
error-text redaction. Production Rust shrinks by eight lines.

Validation: all four output tests pass; strict all-target crab-auth-server Clippy
passes; cargo fmt --all and git diff --check pass. This proves the scoped output
refactor, not cloud-backed receive/view runtime qualification or full crate
quality completion.


## One owner for auth-store unary retries

Object operations, signing, and stable-ID multipart operations previously each
implemented authentication retry. They now use one private implementation;
backend selection remains in the small signer/multipart adapters. This removes
five production lines and two copies of policy. All three surfaces now use the
existing auth-retry warning; response/error mapping is unchanged. Stream bodies,
listing streams, and returned MultipartUpload handles remain outside replay.

Evidence map: read the complete refreshing_store module, CLI constructor and
handle wiring in crab/src/auth/mod.rs, and locked object_store 0.14.1 Signer and
MultipartStore contracts. The constructor binds all handles to one transport
target; refresh_parts still checks target/multipart identity before publishing
replacement parts. Existing tests exercise proactive refresh, permission denial,
object/multipart success after refresh, and destination rejection. Added one
table-driven public-interface test: persistent authentication failures on get,
signed_url, and create_multipart each return Unauthenticated after exactly one
provider refresh. This protects the documented bound across sibling surfaces;
it is qualification of existing behavior, not a newly reproduced defect.

Validation: all eight refreshing_store tests pass with refreshing-store;
strict all-target Clippy passes with managed-service (which includes the refresh
feature). Formatting and diff whitespace checks pass. Provider refresh races,
live identity-service behavior, and complete gateway qualification remain open.

## HTTP-server documentation entry point

The 1,282-line README mixed setup, API contracts, historical local measurements,
and product completion tracking. It is now a 93-line entry point with an
architecture diagram, build/run commands, and task-oriented links. REFERENCE.md
preserves the complete original content, adds a browser/API section anchor, and
labels historical qualification as distinct from current production readiness.
The native-write design's incoming push link and scoped agent guide now point
to the reference where appropriate. No runtime behavior changed.

Validation: compared the reference body byte-for-byte with the previous README
after removing only the added navigation/title changes; all 21 local Markdown
links/anchors in the four touched documents resolve. CLAUDE.md remains a sibling
symlink to AGENTS.md. Checked quick-start arguments against main.rs, embedding
requirements against build.rs, configuration against the deployment example and
loopback validation, and the focused test route against server.rs. Formatting
and git diff --check pass. This documentation-only change needs no Rust rebuild;
historical verification claims were retained, not re-certified.

## Checked timestamp contract

Status: focused validation complete; ready for draft PR review. Broader crate
qualification remains in the coverage ledger.

The original helper, compiled directly from source, produced invalid RFC 3339:

| Epoch milliseconds | Original output | Checked behavior |
| --- | --- | --- |
| 253402300799999 | `9999-12-31T23:59:59.999Z` | Same string |
| 253402300800000 | `10000-01-01T00:00:00.000Z` | `TimestampError::OutOfRange` |
| 18446744073709551615 | `8354187-08-01T14:25:51.615Z` | `TimestampError::OutOfRange` |

[RFC 3339 section 5.6](https://www.rfc-editor.org/rfc/rfc3339#section-5.6)
requires four-digit years. The old calendar arithmetic also narrowed the day
count, while the wall-clock wrapper silently replaced pre-epoch time with 1970.
Release tag `v1.1.0` contains the helper and required string timestamp field.
Valid wire output stays unchanged; invalid input produces a typed error.

### Ownership and consumer evidence

| Surface | Owner and caller | Failure policy |
| --- | --- | --- |
| Shared formatting | `crab-types/src/time.rs`: `from_epoch_millis`, `from_system_time`, wall-clock wrapper | Validate before narrowing; preserve pre-epoch source; truncate sub-millisecond precision |
| Journal compaction | `crab-write/src/journal.rs`: `compact_ref_journal_until_idle` → metadata compaction | Resolve time before starting the next manifest transaction |
| Protected manifests | `crab-auth-server/src/receive.rs` and `view.rs` | Typed error; view resolves time before segmented-bulk upload |
| Stage execution | `crab-workflow/src/executor.rs`: `run_local` | Resolve start time before Running transition and child execution |
| VFS serialization | `crab-vfs/src/daemon.rs` optional SystemTime serde hook | Return serializer error for unrepresentable dates |
| Mount display | `crab-vfs/src/mounts_registry.rs` → CLI mount registration/display | Shared checked calendar; optional unavailable display remains absent |
| Import commit dates | `crab/src/import/assemble.rs`: `commit_window` → `epoch_to_rfc3339` | Check signed conversion and multiplication; keep whole-second strings |
| JSON/JSONL | `crab/src/core/output/`: envelopes, stream and command presenters | Construct checked timestamp before writing; preserve existing writer-error policy |

All direct callers of the changed shared APIs were searched. Other independent
calendar helpers in CLI auth status, Git push, repack, history recovery and xorb
reconciliation remain separate inspection targets; this migration does not
claim those formatters are qualified.

### Output and cleanup rules

- Terminal success presenters return output errors to the command boundary.
- Informational progress callbacks report output failure and let workers drain.
  Remote-helper stderr summaries follow the same policy: Git has already received
  authoritative per-ref outcomes on stdout, so diagnostics cannot fail the helper.
- Primary push, DAG, dehydration and experiment failures survive secondary
  timestamp/output failure. Error reporting uses direct stderr, never another
  timestamped envelope.
- DAG summary follows stage completion and lockfile persistence. Add stops its
  progress ticker before terminal output; hydration rejects failed machine-mode
  batches before success output.
- Experiment end-time construction follows checkpoint-supervisor joining;
  worktree Drop owns cleanup on early errors. Checkpoint restoration resolves its
  temporary filename timestamp before preparing files.
- The CLI retains typed timestamp causes through its existing I/O error category.
  No nullable timestamp, clamping shim or unchecked formatter was introduced.

### Validation evidence

| Check | Result |
| --- | --- |
| Shared time tests | 8 passed; strict types Clippy passed |
| VFS mount registry | 19 tests passed with NFS |
| VFS timestamp serializer | Pre-epoch and year-10000 regression passed |
| Shared write/workflow/auth-server compile | Passed |
| Default CLI check | Passed without warnings |
| CLI output suite | 26 passed again after lifecycle adjustments |
| Import timestamp boundaries | 2 passed after lifecycle adjustments |
| Formatting / whitespace | Passed |
| Strict default CLI library lint | Baseline debt, detailed below |
| Strict affected shared-crate lint | All targets passed for types, write, workflow, auth-server, and VFS with NFS + FUSE |
| Final VFS timestamp fixture rerun | Passed with NFS + FUSE |
| Final CLI consumer check | Passed without warnings |
| Publication | Prepared for draft PR #159 |

Strict Clippy against current main `4b8b36b1870` reports 494 diagnostics; this
branch reports 489. Of those, 484 match unchanged source lines, codes and
messages exactly. Five are existing experiment/queue large-future findings,
each eight bytes larger, with no new lint location or threshold crossing.
No suppressions were added. This is baseline proof, not a clean CLI lint claim.

The baseline checkout and Cargo target are separate on the mounted workspace.
Local diagnostics are `/tmp/crab-089c-{main,current}-cli-clippy.jsonl`; comparison
is `/tmp/crab-089c-clippy-comparison.json`. CLI test linking reports an unwind-table
size warning for the large test binary; no Rust test failure was reported.

## VFS chunk completion notification

Owner: `crates/crab-vfs/src/hydration.rs`, `InflightEntry` and `fetch_chunk`.
Current main waits unconditionally on `Notify::notified()` after cloning an
occupied DashMap entry. The fetcher can store completion and call notify_waiters
between those steps, leaving the reader waiting for a second notification that
will never occur.

The locked Tokio source guarantees notify_waiters delivery after Notified future
creation, even before polling; it does not retain earlier broadcasts for newly
created futures. `InflightEntry::wait` now creates that future before checking
stored completion. Either the state is already complete or the future observes
the subsequent broadcast. Cache verification and fetch-error behavior stay with
`fetch_chunk`; notification does not substitute for a successful cache read.

Consumer and sibling evidence:

- Engine reads call `HydrationService::read_range`; its chunk path calls fetch_chunk.
- Overlay promotion also calls fetch_chunk while materializing backing files.
- The separate read-window cache uses AsyncMutex acquisition followed by a cache
  recheck, rather than Notify; it does not share this missed-broadcast protocol.
- Background whole-file hydration calls do_fetch_chunk directly, so it does not
  wait on an InflightEntry. Prefetch task ownership remains separate unfinished work.

The completed-before-subscription regression failed against the original wait
logic, with a 100 ms timeout. It covers both successful and failed completion.
A second regression registers two pending readers and checks both are released
on either completion result. Tests use the actual private wait boundary called
by fetch_chunk, without test-only production hooks or native mounting.
All 30 hydration tests passed with NFS and FUSE enabled, including both new
regressions on the multi-thread Tokio runtime. Strict all-target VFS Clippy
passed with NFS and FUSE enabled. The API and dependency graph are unchanged;
native mount and complete hydration shutdown qualification remain open.

## VFS worker startup ownership

Both `MountPipelineBuilder::execute` and daemon `execute_mount_pipeline` started
hydration workers before constructing the fallible ODB reader. An engine setup
error dropped the join handles while tasks retained the hydration service and
its chunk cache. The daemon outer error path requested cancellation but did not
own those handles to prove completion before releasing runtime cache ownership.

The standalone pipeline now prepares hydration, resolver and engine first, then
starts workers immediately before returning their handles in PipelineOutput.
The daemon prepares hydration and refresh state, completes backend setup, then
starts and installs both task groups under the runtime write lock. There is no
await between spawning and handle ownership, and a removed runtime starts neither
group. This removes the abandoned local-worker abort loops for that branch.

Evidence map:

- Owner: pipeline execute/step_create_hydration and daemon execute_mount_pipeline.
- Entry: `crab/src/cmd/mount.rs` and `crates/crab-vfs/src/ipc_server.rs` call
  pipeline.execute; daemon start_repo calls its pipeline and handles errors
  through teardown_runtime.
- Callee: OdbReader::new validates the Git object directory and creates blob cache;
  hydration spawn_workers returns handles whose futures retain the service Arc.
- Siblings: both setup paths now defer workers. Engine foreground reads and
  overlay promotion fetch directly; directory prefetch only enqueues work, so
  native startup does not require these queue workers to be running beforehand.
- Remaining boundaries: backend tasks, detached read-window prefetch, cancellation
  during mount/session installation, and native failure cleanup remain separate.

Two real-Git fixture regressions obstruct the blob cache with a file, then check
that engine failure releases the supplied cache reference. Both failed before
the fix (strong count 2 rather than 1), and both pass after it. The standalone
fixture also removes the obstruction, retries preparation, and checks that the
returned worker handles can be cancelled and joined. No native mount is invoked.
All 14 pipeline tests and 38 daemon tests pass with NFS and FUSE enabled.
Strict all-target VFS Clippy passes with NFS and FUSE. CLI/coordinator failures after successful pipeline
return still require handle ownership review; this change does not qualify those
post-preparation paths.

## Coordinator grace-period ownership

`Coordinator::shutdown_graceful` moved hydration handles into a timeout future.
On expiry, dropping that future dropped the handles and detached the workers;
mount/cache state could then be released before task destruction. The locked
Tokio JoinHandle contract explicitly distinguishes dropping from joining and
supports cancellation-safe waiting through a mutable handle reference.

The grace helper now borrows retained handles, removes each completed handle
before awaiting another, and aborts/joins every unfinished task after timeout.
Removing completed handles avoids a second poll of an already consumed result.
The grace period is a cooperative deadline, not a bound on blocking task exit.
Shutdown must still be awaited to completion.

Evidence map:

- Entry: `crab/src/cmd/coordinator.rs` directly awaits shutdown_graceful after the
  IPC server returns; it does not wrap shutdown in a second timeout/select.
- Owner: `crates/crab-vfs/src/coordinator.rs`, MountHandle/PipelineOutput retain
  cache-backed state until the worker join helper completes.
- Callee: Tokio timeout cancels its inner future; JoinHandle drop detaches, while
  awaiting a join guarantees the task destructor has finished.
- Sibling: daemon teardown already aborts and joins its retained handles without
  a timeout. Synchronous shutdown/Drop, individual unmount, foreground reads,
  refresh task ownership and detached prefetch remain separate open boundaries.

The timeout regression failed before the fix: a pending task retained its Arc
state after the grace helper returned. It now also includes a previously finished
handle to detect accidental double-await. A second test covers ordinary finished
and aborted tasks inside the grace period. These are real Tokio tasks and the
same helper used by production shutdown; no native FUSE mount is involved.
All 15 coordinator tests and strict all-target VFS Clippy pass with NFS and
FUSE enabled. The ten-second cooperative grace value is unchanged; final joining
can take longer if a blocking hydration step is still running. No native mount
or entire-process shutdown qualification is claimed.

## README example presentation

Fourteen crate READMEs exposed rustdoc-only `#` harness lines in GitHub code
blocks. These READMEs are not included by the crate documentation tests.
Their usage snippets now show explicit functions with visible error propagation
and normal Rust indentation; fetched bytes have a visible use. The examples
retain their existing API calls, feature requirements, and storage assumptions.

Validation: extracted all fourteen revised snippets verbatim into separate
modules in a temporary CLI example, then ran `cargo check -p crab --locked
--example codex_readme_quality_probe` with the dedicated external target and
Cargo home. The fixture uses the CLI's enabled dependency features; this checks
API/type compatibility, not isolated minimal feature sets. Removed the temporary
fixture after checking. No cloud requests or filesystem examples were executed.
This pass covers the revised snippets, not every code block in all 21 READMEs.

## Non-resumable multipart cleanup

The storage byte/progress and file paths, plus LFS streaming uploads, returned
completion failures without aborting the multipart session. The callback-free
storage path used object_store 0.14.1 WriteMultipart: `finish` skipped abort when
part draining failed and replaced a completion error when abort also failed.
A fault-injecting integration fixture reproduced nine incorrect outcomes across
four public entry paths before the production edit.

One bounded byte queue now serves both callback modes. Storage owns
`multipart::complete_upload`, shared by the byte/file and LFS upload paths;
completion failure attempts abort and retains the original mapped error. The
helper must be awaited through cleanup. Part errors still use their existing
abort boundary. The removed writer path also eliminates its unbounded task
submission and makes the caller's part size apply consistently in both modes.

Evidence map:

- Entry: CLI LFS transfer/publication/batch/migration call
  LfsObjectStore::put_stream_with_size; CLI Store delegates ordinary byte/file
  multipart upload to crab-storage, including pack and xorb publishers.
- Owner: crab-storage Store owns whole-upload retry and part scheduling;
  crab-lfs owns streaming SHA-256/size verification before completion.
- Callee: locked object_store 0.14.1 MultipartUpload. S3 and GCS complete and
  abort are separate requests; neither reclaims upload parts on handle drop.
  Azure abort is a no-op. Cleanup cannot roll back an already committed object
  when completion's response was uncertain.
- Siblings: journal-owned resumable sessions deliberately retain recoverable
  state, verify uncertain completion, and release the lease for later recovery.
  They must not use this non-resumable completion helper. The raw export upload
  controller already aborts completion errors while preserving the primary
  error. Raw-handle lifetime remains the caller's responsibility.
- Main: origin/main contains the same direct completion returns and upstream
  WriteMultipart delegation reproduced by the regression.

Best-fix assessment: fixing only LFS leaves the same resource leak in storage.
Sharing completion policy and deleting the second byte uploader removes that
inconsistency without new config, provider overrides, or serialized changes.
Production Rust shrinks; the integration fixture covers the four public paths
rather than testing only the new helper. Remote abort failure and caller/process
cancellation still prevent a guarantee of full remote reclamation. Live provider
qualification remains required for that deployment boundary.

Separate integrity follow-up found during this review: receipt-aware LFS stream
verification and opening the served stream are separate reads. The second read
checks size/range but does not compare its ETag/version to the verified metadata.
Receipt recording after upload also performs a fresh HEAD. These identity races
need a dedicated validator/receipt investigation and regression; this cleanup
change does not qualify LFS streaming integrity in full.

Validation: both cross-crate integration regressions pass, including 24
success/part/completion/abort-outcome combinations and multi-part callback parity.
Error checks traverse the typed source chain; successful uploads are read back
byte-for-byte. All 11 selected storage multipart tests and 31 LFS object-store
tests pass. Strict all-target Clippy passes for crab-storage and crab-lfs.
The existing macOS CLI linker unwind-table warning remains; no Rust build or
regression failure is attributed to it.

The six existing CLI multipart-retry integration tests also pass with the
`testing` feature (transient retry, exhaustion, cancellation, progress, byte
round-trip, Xet hash validation). Clippy checks the new cross-crate integration
target without diagnostics; the CLI library still reports its previously
recorded 489 warnings. No new lint suppressions were added to production code.

CI observation on head 26d28d68338: repository-browser job 101723107249 fails
its release-page tooltip contrast assertion (3.87 versus required 4.5). The
entire packages/repository tree and the browser workflow match origin/main;
this Node-only job does not build or execute Rust. The latest main run skipped
that job, so no main runtime reproduction is claimed. Rust CI remained running
at observation. This is an outstanding PR check, not a green qualification.

## LFS verification identity

Three public-entry regressions failed before the fix: a same-size replacement
between verification and serving was returned successfully; a later HEAD could
produce a receipt accepting corrupt replacement bytes after upload; and a stream
without any validator reused an earlier hash. A fourth regression showed an old
`crab-lfs/1` receipt could continue accepting that unverified HEAD metadata.

Receipts now use verifier `crab-lfs/2`, keeping the same optional receipt encoding
and key layout. Earlier verifier receipts miss and require fresh body hashing.
All receipt writers receive metadata from a verified read; the fresh-HEAD writer
and its callers are removed. Repair verification records its own read metadata.
New uploads create no receipt until a subsequent verifier hashes stored bytes.
This avoids expanding Store's write API or guessing a write validator, at the
cost of one full read for the first post-upload verification. Repeated valid
receipt checks remain available; measuring that first-read cost and designing
exact write-result propagation remain performance follow-ups.

Stream admission requires a nonempty object version or nonempty, non-weak ETag,
then compares both validator fields on the served response with the verified
metadata. Same-size replacements are rejected before returning the stream. No
validator produces StorageError::NotSupported; download_to_file still hashes one
read to a local destination and works without validators. Existing primary
fallback can retry a rejected replica against its own independently verified
object. Receipt admission and creation use the same validator predicate.

Evidence map:

- Entry: HTTP lfs::download calls get_stream before constructing the response;
  integrity rejection maps to HTTP 422, unsupported storage to HTTP 503. CLI
  transfer/publication/migration call put_stream_with_size and verify_size.
- Owner: crab-lfs owns SHA-256/size checks and receipt trust. crab-storage owns
  response transport and preserves each GET's ObjectMeta with its body stream.
- Callee: locked object_store 0.14.1 ObjectMeta defines ETag as the unique object
  identifier and version as its version indicator. RFC 9110 section 8.8.3.2
  excludes weak ETags from strong comparison:
  https://www.rfc-editor.org/rfc/rfc9110.html#section-8.8.3.2.
- Siblings: verify_origin continues to ignore receipts and hash the same response
  whose metadata it returns; download_to_file hashes the downloaded stream itself.
  Both remain usable with no validator. Repair and existing-object upload receipt
  paths now retain their verified metadata rather than re-reading HEAD.
- Main: origin/main contains the unchecked second GET and fresh-HEAD receipt
  writer reproduced above. Old receipts cannot be grandfathered into new trust.

Best-fix assessment: checking only stream validators leaves a poisoned receipt
able to bless the same corrupt version. Removing unsafe receipt provenance,
invalidating old verifier claims, and binding served responses address both ends
of that contract. No new storage mode, runtime config, or unbounded body buffer
is introduced. Provider validators and response-body consistency remain dependency
contracts; this does not prove arbitrary faulty/custom providers safe.

Validation: the old-receipt regression failed against the original verifier and
passes with verifier 2. All 35 selected LFS object-store tests pass, including
origin verification and upload/repair behavior. Six cross-crate integration tests
cover full/range replacement races, ordinary and streamed upload receipt races,
missing/weak validators, version-only metadata, primary fallback, and the real
local object-store backend. The existing HTTP batch/upload/download test passes
with its request router and actual object storage. Strict all-target LFS Clippy
and the new integration target pass; the CLI library retains its recorded 489
warnings. No native/cloud E2E result is claimed. Formatting and diff checks pass.
Production Rust grows by seven lines for validator admission and response identity
checks while deleting the unsafe HEAD receipt path. The earlier ledger's two LFS
identity follow-ups are addressed by this batch; framing and first-read cost
remain qualification work. Before publication, CI for head eb2605ff9a2 had 12
successful, 12 running, and 12 skipped checks, with no failed check reported yet.

## Storage stream framing

Store::get_stream previously forwarded body chunks without validating their
range or total length. A public-entry regression reproduced six successful
malformed responses: short/excess full and ranged bodies, a partial response to
a full read, and a shifted requested range. Extending the same fixture to file
downloads reproduced fourteen accepted responses across the three public paths.

get_stream now validates its response against the requested range using locked
object_store GetRange::as_range for EOF clamping. Its stream owns a remaining-byte
counter: oversize chunks fail before exposure, EOF with bytes missing fails, and
provider body failures keep their original mapped source. No whole-body buffer,
background task, or additional retry layer is introduced. A consumer must poll
through EOF to establish complete framing.

The unbounded file downloader now delegates to download_to_path_bounded with the
maximum u64 limit. The bounded path additionally validates a full response range
and rejects bytes beyond the advertised size before writing them. Its existing
error cleanup removes the partial destination; this also covers the unbounded
entry point instead of leaving two download implementations with different
failure guarantees. Byte accounting remains at the existing boundaries.

Evidence map:

- Owner: crab-storage Store::get_stream and download_to_path_bounded.
- Entry/callers: LFS get_verified_stream_at returns the storage stream to HTTP;
  LFS origin verification and download_to_file consume it while hashing. Remote
  Git reader.rs verified pack download and pack.rs artifact download consume it
  under their admission/deadline and content-checksum contracts.
- Callee: object_store 0.14.1 GetResult couples ObjectMeta, range, and payload;
  GetRange::as_range validates bounded requests and clamps only the end at EOF.
- Siblings: materialized get_with_etag, bounded reads, and get_version already
  check full-body size. File downloads now share one canonical implementation.
  Remote Git and LFS keep content hashing; framing alone cannot prove identity.
- Main: the same unchecked chunk mapping and independent unbounded download loop
  occur on origin/main. Re-running the new fixture against the original production
  section reproduced the failures; the corrected production section was restored.

Best-fix assessment: checking only LFS would leave remote Git and file consumers
with the same transport hole. Validation belongs in the storage stream; deleting
the duplicate downloader closes its sibling gap without changing content formats,
provider dependencies, or retry configuration. Exact hash/validator checks remain
with higher layers. This is transport framing proof, not cloud-service or full
HTTP connection qualification.

Validation: all 76 storage Store tests, 35 LFS object-store tests, five remote Git
single-pack tests, three generated-pack corruption tests, and six LFS identity
integration tests pass (125 total). The transport regression checks all three
public paths; positive cases cover multiple/empty chunks, empty objects and EOF
clamping. A late provider ConnectionReset retains its typed cause after an
already-delivered prefix. Strict all-target Clippy passes for storage and LFS.
The existing macOS CLI linker warning remains. No cloud E2E run is claimed.

Production section grows by 21 lines for response-range and byte-count guards
while deleting the duplicate downloader. Returned download errors attempt file
removal; failures of removal and dropping the operation future are still caller/
OS cleanup boundaries, not guarantees established by these tests. The earlier
LFS framing follow-up is addressed here. Before publication, CI for head
2caa1315d26 reported 11 successful, 13 running and 12 skipped checks, with no
failures yet; it was not complete.

## Workflow lock readability and API documentation

SchedulerLock converted fs4's contention boolean to an artificial WouldBlock
I/O error, then immediately classified it back into contention. Both acquisition
methods now match the dependency result directly. Remove the two conversion
helpers and the stale private CrabError alias; genuine filesystem errors still
map to WorkflowError::Io. Lock lifetime, polling delays, PID writes, timeout
values, and release order are unchanged.

Evidence map:

- Owner: crab-workflow scheduler_lock.rs, acquire/try_acquire/Drop.
- Callers: async CLI run_inline_single_stage, run_yaml_single_stage and run_dag
  retain the acquire guard; workflow journal GC uses immediate try_acquire.
- Callee: Cargo.lock pins fs4 0.13.1. FileExt::try_lock_exclusive returns
  Result<bool>; Unix maps WouldBlock to Ok(false), Windows maps IO_PENDING and
  LOCK_VIOLATION to Ok(false). Other failures remain errors. Direct matching
  preserves these contracts without a second classification layer.
- Sibling: staging's shared/exclusive adapter still performs the same conversion.
  It has different multi-reader acquisition paths; this refactor does not change
  contention semantics or require a staging behavior change. Simplifying that
  separate adapter remains a readability opportunity.
- Main: contains the same synthetic-error conversion and legacy lock commentary.
- Tests: all twelve existing scheduler-lock tests pass, including real same-process
  handle contention, timeout, release/reacquisition, zero-wait and retained fork-like
  duplicate descriptors. No assertion-only duplicate tests were added.

Module docs now distinguish advisory ownership from best-effort PID diagnostics,
explain the Windows sidecar and retained inode, and state that acquisition blocks
the caller. The README adds a short resource/contract table. Public method links
use Self:: targets, and PID sync comments no longer claim fsync establishes read
visibility or ownership.

A strict rustdoc check then found seven old intra-doc links into the monolithic
CLI layout or incorrect dependency/type paths. Fixed the graph/error targets;
reworded discovery, parameter output, and experiment-reader docs around actual
library ownership instead of inventing product dependencies. The only found
ExperimentMetaRead implementation is its test mock, so the old production-adapter
claim was removed. API docs now build using cargo doc -p crab-workflow --no-deps
with RUSTDOCFLAGS='-D warnings'. Strict all-target workflow Clippy also passes.
Native Windows/Linux runtime qualification remains with workflow-native.yml CI.

Best-fix assessment: use the existing dependency's three outcomes directly;
no new lock abstraction, fallback, or timeout configuration is needed. Source and
documentation shrink. Correctness follow-ups found while tracing callers: the
three async CLI paths invoke blocking acquire directly, read_holder_pid has an
unbounded diagnostic read, and read_experiment_metadata ignores its referenced
content hash. These are not resolved or qualified by this readability batch.

### Experiment metadata identity: consumer investigation

The unchecked hash in the exported workflow reader is a library-contract gap,
not evidence that the CLI accepts mismatched remote metadata. Workspace-wide
search finds only the test implementation of `ExperimentMetaRead`; all calls to
`read_experiment_metadata` are its module tests.

The production sibling `crab/src/cmd/exp.rs::read_remote_metadata` checks the
requested experiment ID and compares `ExperimentMetadata::content_hash()` with
the ref target. Its callers include push's existing-object path and the remote
experiment read paths. Publication writes `canonical_json()` bytes before the
hash ref. Therefore verification must compare the canonical metadata hash, not
invent a raw JSON byte-hash contract: harmless serialization whitespace must
not change logical identity.

Best-fix direction: establish one shared metadata decoding/identity-validation
boundary used by both the library reader and CLI reader, retaining caller-owned
storage lookup and missing-object policy. First inspect workflow-to-CLI error
conversion and schema compatibility: the shared reader probes schema before
full deserialization, whereas the CLI currently deserializes directly. Preserve
typed parsing causes and expose identity failures without making consumers parse
error text. Add public-entry regressions for incorrect ID, incorrect ref hash,
and noncanonical whitespace with a valid canonical hash. Do not silently broaden
this into a storage layout or publication concurrency change.

Evidence status: source/caller/writer contract inspected; no new regression or
runtime qualification performed in this investigation. The previously recorded
unchecked library-reader issue remains open. CI observed for c9bb054a23d: three
successes, eighteen in progress, one queued, four skipped; no reported failures
at this observation, not a completed CI qualification.

### Shared experiment identity validation

Move the CLI's ID/hash checks into `ExperimentMetadata::verify_identity` and
call it from the exported library reader as well. The library now rejects a
metadata object whose ID differs from the requested experiment or whose
canonical hash differs from the resolved ref. Add `WorkflowError::CorruptObject`
and map it to the existing CLI integrity error; the remote CLI reader still
adds its repository prefix to diagnostic paths.

Evidence map:

- Owner: workflow metadata; `canonical_json` and `content_hash` define identity.
- Entry points: `read_experiment_metadata` and CLI `read_remote_metadata`.
- Callees: shared ID comparison and canonical Blake3 hash; no transport changes.
- Writer: CLI experiment push writes canonical bytes before publishing the hash.
- Sibling: CLI already enforced identity on main; its checks are moved, not
  weakened. Library main ignored the ref hash and accepted mismatched IDs.
- Scope: schema parsing, not-found behavior, publication concurrency, and
  storage keys remain independent. Schema decoding and typed JSON causes still
  need follow-up; this change does not claim to solve them.
- Tests: 32 workflow experiment tests pass, including new wrong-ID/wrong-hash
  rejection and pretty-printed JSON acceptance. CLI prefix regression and
  focused lint results are recorded after verification below.

Is this the best fix? A shared method removes duplicate identity policy while
leaving storage-prefix ownership with the caller. Sharing complete decoding
before reconciling the different schema/error contracts would conflate changes.
The small production increase provides actual validation in the previously
unchecked reader plus a classified integrity error; no second identity path
remains in the touched readers.

Validation completed: the new library mismatch regression fails when the
verification call is disabled (unchecked reader returns `Ok(Some(..))` for a
wrong hash). Restored code passes all 32 experiment tests. The CLI regression
passes both mismatch cases and verifies prefixed corruption paths. Strict
all-target workflow Clippy and workflow rustdoc with warnings denied pass.
The initial CLI fixture reused an immutable ref across cases and hit the
expected create-only conflict; each case now owns its own in-memory store.
No storage behavior was changed to accommodate the fixture. Native/cloud E2E,
full CLI lint qualification, and schema-decoder consolidation remain unclaimed.

### Metadata decoding: complete consumer boundary survey

The next decoding change must cover more than the two remote readers. Current
source search identifies these distinct policies:

| Surface | Current decoding and failure policy |
| --- | --- |
| Workflow `read_experiment_metadata` | Probes schema v1, deserializes, verifies identity; JSON causes are stringified into Internal. |
| CLI `read_remote_metadata` | Direct deserialization, then shared identity verification; no explicit schema-version check, JSON cause stringified into CorruptObject. |
| CLI `read_local_metadata` | Direct deserialization; no explicit schema or requested-ID check; missing file is ExperimentNotFound, other I/O errors propagate. |
| CLI `collect_summaries` | Direct deserialization; read/parse errors log and skip entries; successful metadata supplies the displayed ID. |
| Workflow `collect_local_workflow_live_set` | Direct deserialization; directory-entry, stat, read, and JSON failures log and skip; checkpoint scanning instead propagates failures. |

The last surface can return an incomplete live set while its docs call that set
conservative. A grace period cannot prove that an unreadable metadata object's
references are dead. Exhaustive symbol search finds no production callers of
this exported collector, only module tests and its public re-export. Therefore
this is a dangerous library contract, not a demonstrated production deletion
path. The existing malformed-blob test explicitly protects skip behavior and
must be replaced when that behavior is removed, rather than retained as a
compatibility requirement. No release-tag contract has been established for it.

Implementation requirements for the next batch: one schema-aware decoding
boundary with a retained serde_json source, an explicit integrity classification
through the CLI error catalog, and caller-owned not-found/listing policy. A
live-set collector must propagate incomplete-enumeration and malformed-metadata
errors; local requested-ID checks need proof alongside remote identity checks.
Do not reuse a metrics error or stringify a parse cause simply to avoid updating
the error boundary. Add regressions for unsupported schemas, malformed metadata,
local identity mismatch, and fail-closed live-set collection. Preserve missing
parent semantics and valid checkpoint-only roots.

This survey changes the required implementation scope. No decoding/runtime
change is claimed yet. Latest observed CI for e4a90d01ab0 reports two successes,
two in progress, four skipped, with no reported failures at that observation.


### Canonical metadata decoding and complete live sets

`ExperimentMetadata::from_json` now probes the supported schema, decodes typed
metadata, and checks the requested ID. All five surveyed readers use it. Remote
readers additionally verify the canonical hash. The CLI keeps repository prefixes
on ID/hash diagnostics, listings retain warning/skip presentation policy, and
missing local files keep ExperimentNotFound behavior.

`ExperimentMetadataMalformed` retains serde_json::Error as its source. The CLI
wraps it without discarding the cause and classifies it as CRAB-E0020 in both
error-code surfaces, integrity exit/category handling, details, and storage
retry policy (one retry before failure, matching existing corruption errors).

The live-set collector propagates directory-entry and metadata read/stat failures
and rejects malformed, unsupported, or incorrectly identified metadata. Matching
metadata paths must be regular files. Missing parents and unrelated filenames
retain their existing meaning; checkpoint-only roots are still collected. The
old malformed-blob skip test was removed with that behavior and replaced by a
fail-closed regression. No production GC caller exists today, so this improves
the exported contract without claiming a live deletion-path repair.

Why this shape: decoding and identity belong to the metadata owner, while list
presentation and destructive-operation admission differ by caller. A second
schema parser or a metrics-error alias would perpetuate inconsistent contracts.
The collector shrinks substantially as silent failure branches disappear.
Schema-version errors retain the existing variant/code; its historical "newer"
wording also applies to unsupported older versions and remains diagnostic cleanup.

Validation: 33 experiment tests, seven live-set tests, three CLI decoder/error
and retry tests, and the remote prefix regression pass (44 total). The new
malformed-metadata live-set regression fails against the original collector;
restored code passes. Strict all-target workflow Clippy and strict rustdoc pass.
CLI library Clippy completes with 489 warnings versus 494 on the recorded main
baseline: 484 map exactly to main source; the remaining five are the previously
recorded large-future size differences, with unchanged messages and locations
modulo source movement. No new warning category/location is introduced.

Scope limits: the live-set helper is not wired into production GC; no native or
cloud E2E deletion proof is claimed. JSON serialization error handling remains
separate from the decoding changes. Metadata publication concurrency and async
scheduler lock waiting remain open work.


### Token-cache read outcomes under the cache lock

Remove `TokenCache::load`'s pre-lock `Path::exists` probe. Acquire the existing
lock, read once, return None only for read NotFound, and propagate other errors.
This prevents an intervening logout from turning a cache miss into a spurious
read failure and avoids interpreting failed metadata probes as missing tokens.
A lock-acquisition failure remains an error, including a removed cache directory
on Unix; TokenCache construction normally creates that directory.

Evidence map: load/load_any feed auth status, doctor, refresh, and logout
revocation. Store publishes under the same Unix lock; delete already classifies
remove_file's NotFound under that lock. delete_all holds the lock throughout its
walk. These sibling paths need no new existence check. File-key initialization
also locks first; macOS Keychain creation and non-Unix no-op locking require
separate lifecycle qualification. No encryption format, key storage, provider
naming, or fallback contract changes in this batch.

Dependency contract: [Rust Path::exists](https://doc.rust-lang.org/stable/std/path/struct.Path.html#method.exists)
can return false for metadata-access errors. Replacing it with try_exists would
retain a redundant check/read window; classifying the operation's result is the
better fix here. The current main loader has the same pre-lock probe.

Validation: 19 token-cache tests pass, including encrypted round trips, deletion,
provider selection, concurrent store/load, and a new Unix invalid-directory
regression. The new regression fails against the original loader. Strict default
all-target auth Clippy passes. Tests use temporary directories and fixture keys;
they never invoke TokenCache::new, Keychain, or the host key file. The logout
interleaving is established by the lock/read source ordering, not a forced
scheduler test; native Windows and live identity-provider proof remain open.


### Keychain initialization preserves the stored winner

`keychain_load_key` previously followed any failed lookup with
`add-generic-password -U`, allowing a second initializer to overwrite the first
initializer's encryption key. A lookup failure also does not establish that an
existing key is absent. The installed macOS `man security` documents `-U` as
updating an existing item, and says insertion without it requires absence.

Remove `-U`. A successful insertion returns its candidate; a failed insertion
rereads the stored key and returns only a valid decoded value. If both insertion
and lookup fail, the Keychain path returns an error. The outer, pre-existing
key-file selection policy remains unchanged and needs separate qualification.
This is winner resolution after create-only publication, not a new key-source
fallback. No existing key is migrated, rotated, or overwritten by this change.

Evidence map: TokenCache::new owns initialization; load_or_create_key selects
the existing Keychain/file paths; the Keychain command wrapper and hex decoder
are callees. Login/status/refresh/logout construct TokenCache. The file-based
sibling already uses create_new and reads an existing winner, but publishes
bytes directly to the final path and has no non-Unix lock; partial publication
and key-source selection are explicit follow-ups. Main uses the same unsafe
update option in the Keychain path.

A private command runner separates the algorithm from process execution and
removes repeated Command/Stdio construction. Two macOS unit regressions model a
competitor publishing between lookup and creation, and total insertion/lookup
failure. Re-enabling `-U` makes the winner-preservation regression fail. All 21
token-cache tests and strict default all-target auth Clippy pass. No test invokes
security, changes the host Keychain, or accesses a real key file. Native Keychain
service integration remains unqualified; these are deterministic algorithm
regressions backed by the installed command contract. Production code shrinks;
the net increase is regression coverage and documentation.


### Publish complete key files without replacing the winner

`write_key_file` now writes and syncs a NamedTempFile in the destination directory
before persist_noclobber publishes its final name. The old create_new writer
exposed an empty/partial final file before write_all completed; a write failure
left that invalid key for subsequent initializers. The new path preserves the
existing AlreadyExists contract consumed by file_based_key's winner read and
removes duplicated Unix/non-Unix write implementations.

Dependency evidence: Cargo.lock pins tempfile 3.27.0. Its create_named Unix
implementation defaults to mode 0600. persist_noclobber never overwrites the
destination; Unix uses no-replace rename where supported, otherwise hard-link
then unlink, and Windows uses MoveFileExW without replacement. The documented
hard-link path may leave an extra temporary link on interruption. This patch
claims complete-byte publication and non-overwrite behavior, not universally
atomic cleanup or full power-loss durability; directory-entry durability and
non-Unix access/locking qualification remain open.

Owner/caller proof: TokenCache::new → load_or_create_key → file_based_key →
write_key_file/read_key_file. The existing loader reads an AlreadyExists winner;
Keychain insertion was separately fixed to preserve its winner. Encrypted token
updates intentionally use replacing publication, so this create-only key rule
must not be copied indiscriminately into token refresh.

Regression: an isolated Unix subprocess sets RLIMIT_FSIZE to zero and ignores
SIGXFSZ, forcing the real filesystem write to fail. The parent proves the child
ran and the final key path remains absent. Against the original writer, the
same test fails because the final path exists. Resource limits affect only the
child; all files and keys are fixtures. Separate tests cover preserved winner
bytes and Unix permissions. All 24 token-cache tests and strict default
all-target auth Clippy pass. Production code shrinks; added code is regression
coverage. Native Windows and live identity-provider/Keychain proof are not
claimed. Latest observed CI for 9aff4e32456 had nine successes, fifteen running,
seven skipped, with no reported failures at that observation.

### HTTP response-stream cancellation investigation

Release-asset downloads and LFS downloads both wrap their source stream in
`take_until` for cancellation/deadline handling, retain a transfer permit and
cancellation guard in the response stream, and send an explicit Content-Length.
The ordinary release download integration test verifies the complete payload;
it does not inject mid-body cancellation.

Pinned dependency evidence changes the initial hypothesis: futures-util 0.3.32
returns stream EOF when take_until's stopping future resolves, but Hyper 1.9.0's
HTTP/1 length encoder rejects end-of-stream while declared bytes remain. Its
connection end_body path closes writing and returns a body-write-aborted error
instead of completing a reusable response. The server enables Axum's http1
feature, not http2. Thus stream EOF alone is insufficient evidence of a silent
successful truncated HTTP download. No speculative transport fix was made.

Owner map: app::admit bounds handler response creation; releases::download_asset
and lfs::download own transfer state; Body::from_stream hands frames to Hyper's
HTTP/1 writer. Archive output has a separate channel-backed body and cannot be
assumed to share the length-delimited contract. Future qualification should
inject cancellation after response headers, assert client body failure and
permit release, and inspect archive worker shutdown independently. Source
inspection is not substituted for that live client/transport test.

The published head remains 636f2a6b9f5 while CI runs. Two local HTTP documentation
commits describe request validation/reservation ownership and distinguish handler
admission from response-body ownership. Latest observed checks: eight successes,
twelve running, one queued, nine skipped; no reported failures at that observation.
