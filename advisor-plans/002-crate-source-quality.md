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
| crab-types | Pointer error sources; timestamp range contracts remain | Pointer slice verified |
| crab-git | Shared delta decoder; discovery and process contracts remain | Delta slice verified |
| crab-diff | Large term comparison: ordered matches and duplicate counts | Comparison slice verified |
| crab-xet | Coverage count simplification; parser and reconstruction qualification remain | Coverage slice verified |
| crab-storage | Credential diagnostics; retry/error classification remains | Diagnostic slice verified |
| crab-metadata | Remote writer selection and close contract; catalog lifecycle remains | Writer selection slice verified |
| crab-staging | Recovery lookup errors; flush/publication and scale qualification remain | Recovery slice verified |
| crab-coordination | Renewal control flow; provider and GC fencing contracts remain | Renewal slice verified |
| crab-lfs | Upload I/O causes; integrity and lock ownership remain | Upload diagnostic slice verified |
| crab-cache | Credential diagnostics; cache keys and invalidation remain | Diagnostic slice verified |
| crab-cache-store | Startup outcomes; origin authority and range qualification remain | Startup slice verified |
| crab-read | Term cancellation cleanup; hydration and source-chain qualification remain | Batch cleanup slice verified |
| crab-write | Shared cleanup error precedence; commit-graph coverage remains | Maintenance cleanup slice verified |
| crab-remote-git | Finish/shutdown docs; range and consumer qualification remain | Lifecycle documentation verified |
| crab-vfs | Mount teardown and shared FUSE/NFS lifecycle invariants | Pending |
| crab-auth | Credential Debug output; token-cache lifecycle remains | Diagnostic slice verified |
| crab-auth-store | Credential refresh and storage adapter error boundaries | Pending |
| crab-auth-server | Receive cleanup and error-to-response mapping | Pending |
| crab-cache-server | Eviction concurrency, shutdown, request validation | Hex input guards verified; broader lifecycle proof pending |
| crab-http-server | Request validation, embedded assets, service errors | Pending |
| crab-workflow | Retry validation; cancellation, cache identity, resume remain | Retry parsing slice verified |

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
