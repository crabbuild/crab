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
| crab-git | Discovery, process ownership, non-UTF-8 paths, and quoted line-mode fields remain | Delta and NUL worktree framing slices verified |
| crab-diff | Large term comparison: ordered matches and duplicate counts | Comparison slice verified |
| crab-xet | Broader parser/reconstruction and aggregate memory qualification remain | Coverage, decoded-length/offset checks, and bounded decompression output verified |
| crab-storage | Broader retry/error classification and cancellation cleanup remain | Diagnostics, multipart cleanup, and stream framing verified |
| crab-metadata | Catalog lifecycle, cancellation, and broader index qualification remain | Writer admission and diagnostic candidate ordering verified |
| crab-staging | Flush/publication, scale, and remaining clock-policy qualification remain | Recovery errors and invalid cleanup clocks verified through fsck |
| crab-coordination | Renewal control flow; provider and GC fencing contracts remain | Renewal slice verified |
| crab-lfs | First-verification cost and lock ownership remain | Upload cleanup, identity, and shared stream framing verified |
| crab-cache | Broader cache-key and invalidation qualification remain | Diagnostics, exact cached-file ranges, repair/accounting, and README navigation verified |
| crab-cache-store | Broader source-chain integrity and deployed-service qualification remain | Warm ranges, conditional/versioned bypass, metadata authority, and corruption provenance verified |
| crab-read | Term cancellation cleanup; hydration and source-chain qualification remain | Batch cleanup slice verified |
| crab-write | Shared cleanup error precedence; commit-graph coverage remains | Maintenance cleanup slice verified |
| crab-remote-git | Provider ranges, aggregate resource limits, and broader consumer qualification remain | Lifecycle documentation, README navigation, and coalescing admission boundaries verified |
| crab-vfs | Native backend/dependency child tasks and abandoned-future cleanup remain | Hydration and refresh ownership regressions, feature checks, lint/docs, and CLI build pass |
| crab-auth | Key-source policy, power-loss durability, non-Unix locking remain | Diagnostic, load-outcome, and key-publication slices verified |
| crab-auth-store | Shared bounded auth retry; provider concurrency and gateway qualification remain | Unary retry slice verified |
| crab-auth-server | Shared output classification; receive/view cleanup qualification remains | Output slice verified |
| crab-cache-server | Eviction concurrency, broader shutdown and request validation remain | Startup/TLS ownership, checked JSON/text output, and runtime resource-error slices verified |
| crab-http-server | Archive worker draining, production-route cancellation, embedded assets, service errors remain | HTTP/1 LFS and archive framing verified; request/admission ownership documented |
| crab-workflow | Remaining cancellation ownership and broader native qualification remain | Async lock waiting, retry parsing, metadata identity, serialized replay, root-relative materialization, and cleanup slices verified |

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

### Live HTTP/1 LFS cancellation qualification

A new loopback test exercises the production LFS route and response body through
Axum/Hyper and an HTTP/1-only reqwest client. Test middleware delays polling the
real body until the client receives headers. The cancellation case then cancels
the server token and releases that gate: client body reading fails without its
own timeout, and all four transfer permits return after the connection drains.
A normal-download control runs through the same gate and receives exact `hello`
bytes. Both cases verify a permit remains held after headers arrive.

The two-case test passes, as does strict all-target HTTP-server Clippy. The
required frontend build passes with its existing chunk-size warning. This closes
the inspected LFS HTTP/1 response-framing and permit-release slice with a real
client; it does not qualify release-download cancellation, archive worker
shutdown, native Git-LFS clients, or cloud storage. No production transport
behavior changed.

CI on published 636f2a6b9f5 reports a repository-browser failure in job
101751477879 (run 34124097720): the release tooltip has contrast 3.87 versus 4.5.
packages/repository and .github/workflows/rust.yml are identical to origin/main;
the job runs Node browser tests and releases.e2e.ts mocks API routes. No Rust
server is executed by that failing test. This is source-level isolation evidence,
not a fresh runtime reproduction on main. The check remains red; no UI test,
threshold, snapshot, or baseline was changed. Other qualification jobs continue,
so local commits remain unpublished to preserve their running head.

### Archive cancellation must fail the response body

Current main distinguishes cancelled traversal from other traversal errors in
`archive::write_zip`: it finalizes the ZIP and returns success for cancellation.
The download handler sends no Content-Length, so the LFS length-framing proof
cannot protect this response. The remote-git archive stream propagates operation
errors to `spawn_archive_reader`, which sends the terminal abort message.

The writer now treats every abort as unsuccessful traversal. ZIP finalization
still runs for cleanup, then the output channel carries an Interrupted error to
the Axum body. Cancellation remains quiet in logs. Removing the cancelled flag
from the private message keeps the decision at the traversal boundary without
maintaining two completion policies. Successful Finish and disconnected-client
cleanup retain their existing paths; release and LFS bodies have separate
stream implementations and are unaffected.

A regression drives the real ZIP worker and response body: the old cancelled
abort returns a successful collected body, while the corrected path errors and
releases its transfer permit. All five archive tests pass, including closed
receiver cleanup and response-size enforcement; strict all-target HTTP-server
Clippy passes. The frontend was built before these checks. This is worker/body
integration proof, not a live HTTP archive download or cloud qualification.
Detached archive-worker draining and the timing of permit release relative to
worker completion remain separate open lifecycle work.

The archive regression now replaces the body-only check with a loopback HTTP/1
check through Axum/Hyper and reqwest. It uses the production ZIP worker and body
adapter on a fixture route, delays body polling until HTTP 200 headers arrive,
and verifies Content-Length is absent. Finish yields a readable ZIP root entry;
Abort yields a client body error without relying on the client timeout. Both
cases hold the transfer permit after headers and release it after server drain.
Restoring the old abort-as-success behavior makes the new client assertion fail.
This qualifies transport framing, not production archive routing, remote Git
traversal, or detached-worker shutdown. The existing body-only test was replaced
rather than retained as duplicate coverage. All five archive tests and strict
all-target HTTP-server Clippy pass after restoring the fixed source.

The RustFS race, crash, and scale workflow completed successfully on published
head 636f2a6b9f5 (run 34124097701). Its evidence does not cover later local HTTP
commits. The PR description now links this successful gate while keeping the
browser contrast failure and outstanding qualification visible.

### Cache documentation entry point

The cache README placed nearly 250 lines of persistence and lifecycle detail
under one architecture heading before usage. It now leads with feature choices,
a read-through flow diagram, the verified-chunk example, and a source/test map.
The detailed contract body is retained verbatim in REFERENCE.md, with fourteen
section headings and navigation links. The scoped agent guide links both files.
This moves existing evidence without claiming new native/storage qualification.

Checked feature exports against Cargo.toml and lib.rs, key semantics against
key.rs, and constructor/read-through behavior against local_cache.rs. The Rust
example is unchanged; its dependency snippet now lists its direct bytes and
crab-xet dependencies as well. All 38 Markdown links resolve, example dependency
and feature names match workspace manifests, and the CLAUDE guide symlink is
intact. No Rust behavior or dependency manifest changed; no compilation was
needed for this presentation-only pass. Documentation grows overall because
existing contract detail is preserved while adding a short entry point.

### Worktree porcelain field identity

Current main applies trailing-CR removal to every porcelain field, including
NUL-delimited output. Git documents -z as NUL framing for paths with special
characters. A native Git 2.50.1 fixture confirms it emits a literal trailing CR
in a bare repository path; the old shared parser removes it. Both that native
regression and a pure field-preservation regression fail before the fix.

Owner: crab-git/worktree.rs::parse_worktree_list_porcelain and its field decoder.
The CLI adapter forwards unchanged bytes and the delimiter flag. Production
worktree JSON listing, state-location discovery, and hydration sibling discovery
all request --porcelain -z and pass true; they use parsed paths for identity or
filesystem lookup. Correcting this shared boundary covers those consumers.
Line-mode parsing retains CRLF handling, proved with a two-record control.
Unknown attributes and lock reasons receive the same field-preservation rule.

The parser now streams fields directly instead of allocating a temporary vector
and using two one-call helpers. Only newline framing strips CR. Public signature,
record shape, error mapping, and lossy UTF-8 behavior are unchanged. All eight
worktree tests pass with and without the facade feature; strict default
all-target crab-git Clippy passes. The native
fixture uses a temporary repository and does not modify a user checkout. This
is Git-output/parser integration proof, not full CLI command qualification.
Non-UTF-8 identity and non-NUL quoted-field decoding remain separate open work.

Dependency contract: https://git-scm.com/docs/git-worktree#_porcelain_format.

### One reconstruction-term report assembly path

compare_terms had separate added, deleted, empty, and modified report assembly.
Added/deleted constructors repeated the shared report fields and segment-detail
mapping. The entry point now selects file status once, skips matching when one
side is empty, then uses the same counters, detail builder, and final report.
Whole-file Added/Deleted reports still omit changed ranges, as documented by
ChunkDiffReport; their absent-side size remains zero. Empty/empty remains
Modified. Matching keys, exact/greedy thresholds, and dedup arithmetic are unchanged.

Evidence map: exported compare_terms -> classification -> byte counters,
compute_changed_byte_ranges, build_segment_details -> ChunkDiffReport. The
workspace has no production compare_terms caller; the CLI uses compare_sequences,
whose separate report path is unchanged. Existing term and sequence tests cover
shared ordered matching. Current main's report contracts and field documentation
were read before consolidation.

A temporary public-API probe compared complete Debug representations for all 49
pairs of seven sequences: empty, distinct single terms, both orders, duplicates,
and 4,097 repeated terms. Outputs match before/after, including the large-input
path. The probe was removed after comparison; all 27 retained library tests and
strict all-target Clippy pass. Production code shrinks by 104 lines. Added and
deleted inputs now allocate a linear status vector before the shared builder;
this is an explicit memory tradeoff for one report assembly path, not a claimed
performance improvement. Input validation and very-large-count arithmetic remain
separate qualification work.

### Diff report equality is reflexive through nested metrics

ChunkDiffReport and FileDiffEntry implement Eq on main and release tag v1.1.0.
The report already compares dedup_ratio through f64::to_bits, but its optional
ChunkDiffMetrics uses derived floating-point PartialEq. A caller can populate
public reuse_ratio with NaN and make a report unequal to its clone, violating
Eq's reflexivity requirement. The old docs incorrectly claimed derived report
PartialEq and relied on production calculations never producing NaN.

Preserve the tagged Eq API and use the report's existing bitwise ratio policy
for ChunkDiffMetrics too. Identical NaN payloads now compare equal; different
NaN payloads and signed zeroes differ. The signed-zero distinction is an
intentional change to nested-metric equality, aligning it with the enclosing
report. FileDiffEntry now derives its trivial wrapper equality. Serialized
fields and diff arithmetic are unchanged. Corrected adjacent field docs:
comparators compute details without a verbosity request, and term reports omit
Added/Deleted ranges while chunk reports mirror their new-side metrics.

Evidence map: compare_sequences -> build_metrics/report_from_metrics -> public
ChunkDiffReport -> nested ChunkDiffMetrics equality; FileDiffEntry delegates to
that report. compare_terms produces no chunk metrics and keeps its existing
ratio equality. CLI diff constructs these reports; formatter sorting uses paths
and renders/serializes fields without depending on ratio equality. Searches of
all workspace uses found no separate report equality owner.

Two regressions fail before the fix: report/entry clone equality with nested
NaN, and nested signed-zero comparison inconsistent with the report ratio.
Coverage includes infinities, two NaN payloads, signed zeroes, and a finite ratio.
All 29 diff library tests and strict all-target Clippy pass. No new dependency,
wrapper data type, storage format, or public field was introduced; added manual
field comparison is required by the existing tagged Eq contract.

Dependency contract: https://doc.rust-lang.org/std/cmp/trait.Eq.html.

### Workflow orphan cleanup requires ownership and a recognized name

Current main's inline run path sweeps sidecars before acquiring SchedulerLock,
passing an empty active-run set. YAML single-stage and DAG paths already acquire
before sweeping. A contention regression holds a real scheduler guard, creates
a materialization sidecar, and invokes inline execution with no-wait. Before the
fix it returns WorkflowLockTimeout but has already deleted the holder's file.
The fixed path leaves the exact bytes present. The private CLI sweep helper now
requires a borrowed scheduler guard at all three call sites.

The shared resume sweep also treated any filename containing .crab.tmp. as a
sidecar, including malformed run IDs; its docs promised UUID recognition.
materialize::sidecar_path is the producer and appends the run UUID. Cleanup now
parses that suffix once before classifying a file or directory for deletion,
then checks the supplied active set. Malformed names remain ordinary entries;
ordinary directories still permit traversal to recognized nested sidecars.
A second regression proves malformed file and directory names survive; the old
implementation removes both. The five existing/new sweep tests cover active
preservation, orphan removal, recursion, missing roots, and unknown names.

Owner map: CLI inline/YAML/DAG orchestration -> private sweep_orphans -> shared
resume::sweep_orphan_sidecars -> UUID recognition and filesystem removal.
SchedulerLock owns exclusion; materialize.rs owns sidecar creation. No other
production sweep callers were found. The zero-active-set precondition is now
explicit in public rustdoc and the crate README/agent guide. The change preserves
file/directory cleanup and active UUID behavior without adding a second policy.

Both regressions fail on the original paths and pass after the fix. Five shared
sweep tests, the real-lock CLI contention regression, and strict all-target
workflow Clippy pass. This does not qualify every concurrent publication path:
async lock waiting, pre-lock dependency/lockfile reads, cache-only execution,
and adversarial directory replacement remain separate open work.

Published head 636f2a6b9f5 completed with 29 successful checks, 12 skipped, and
one failed browser contrast check. Both Windows jobs finished successfully.
These completed results do not qualify the later local commits; publication
will start fresh validation for the accumulated changes.

CLI library Clippy still reports 489 warnings versus 494 on the recorded main
baseline: 484 match unchanged main source, and the remaining five large-future
messages match the prior comparison. The cleanup change introduces no new
observed diagnostic; the CLI is not claimed warning-free.

The debug CLI build passes with a macOS linker warning that __eh_frame exceeds
16 MiB; no warning-free binary-build claim is made. A separate Python process
holds the real advisory lock while the built crab run command attempts no-wait
execution in a temporary fixture on the workspace volume. It returns CRAB-E0230,
preserves the holder sidecar bytes, and creates no final stage output. This
adds native command-to-filesystem proof for the contention path.

### Inline cache-only replay shares scheduler admission

The inline cache-only branch returned before SchedulerLock acquisition, even
though local and remote cache hits call materialize_hit through the same
sidecar publication helpers as execution. A native reproduction on published
787e7089b9a seeds a real stage cache, removes its output, and holds the scheduler
lock in a separate process. Cache-only replay incorrectly returns success and
restores the output while that holder remains active.

Acquisition now occurs once before choosing replay versus execution. It covers
remote cache pulls and final cache-hit materialization, while the normal path
retains its guard through sweeping and execution. Timeout/no-wait policy remains
caller-owned. The retained contention regression now checks both inline modes;
no duplicate test/helper or new lock implementation was added.

The debug CLI build and two-case regression pass. A fresh native smoke verifies
CRAB-E0230 and no output under contention, then exact restored bytes after lock
release. A command-execution marker remains unchanged during replay, proving
that the positive control uses cached output rather than rerunning the stage.
The native fixture lives on the workspace volume and is removed after use.
The build retains the previously observed macOS debug unwind-size linker warning.

Source map: run_inline_single_stage -> cache_only_path -> cache_only_emit_hit ->
materialize_hit -> shared materialize helpers. The YAML execution paths already
hold scheduler guards during materialization; YAML interpretation of cache-only
flags remains a separate inspection target. Async lock waiting and pre-lock
input/lockfile reads remain open. This change qualifies inline replay ownership,
not every workflow invocation mode or remote-provider race.

CLI library Clippy remains at 489 warnings versus the recorded main baseline's
494. Of these, 484 match unchanged source and five large-future messages match
the preceding comparison exactly. No new diagnostic was observed for this fix.

### YAML cache-only replay follows recorded stage identities

The YAML/DAG entry point never consumed `RunArgs::cache_only`. A native CLI
fixture with no lockfile or cache returned success and executed its stage
command. The new missing-lockfile regression also fails on the prior source
with `Ok(())`. This contradicts the workflow guide's recorded-state replay and
exit-3-on-miss contract; rejecting YAML replay would not satisfy that contract.

`run_with_yaml` now routes replay to a bounded product orchestration path:
acquire the scheduler guard, load the existing single/split lockfile context,
apply canonical stage filters and graph order, and look up recorded stage
hashes. It never resolves live inputs, starts the executor, invokes cache-hit
hooks, writes a journal, or saves the lockfile. Watch and cache-only flags now
conflict because watch explicitly schedules fresh execution.

Inline and YAML replay share local/remote lookup and materialization. The
helper returns a stage result instead of emitting output, so inline reporting
stays single-stage while YAML emits one workflow summary in JSON or JSONL.
Remote candidate construction is shared; selected-remote, primary fallback,
and artifact-store routing retain their existing order and owners. No provider
or serialized cache format changes. The production LOC increase adds the
missing YAML orchestration; it does not duplicate an executor or materializer.

Evidence map: CLI `RunArgs` -> `run_with_yaml` -> `replay_yaml_cache` ->
`LockfileContext::load`, `filter_stages`, `Graph::toposort`, and `cache_only_path`
-> `cache_only_materialize_hit` -> shared publication helpers. The sibling
inline path consumes the same returned stage result. Existing normal DAG and
watch execution remain separate callers of the executor. Current main also
routes YAML directly to DAG execution without a cache-only branch.

Two focused regressions cover missing records, recorded hashes despite absent
live inputs and changed commands, skipped hooks, selected-stage filtering,
real lock contention, and unchanged lockfile/journal state. A built-CLI smoke
uses real files and an external advisory-lock holder: missing records and
missing cache entries exit 3 without execution; contention prevents writes;
JSON and JSONL each produce one summary while restoring exact cached bytes
after deleting the input. The execution marker stays at one seed invocation.
The existing inline native replay/lock smoke also passes after the shared
helper refactor. The debug build passes with the existing macOS unwind warning.

Remaining limits: live remote replay and split-lockfile replay are not newly
runtime-qualified by this batch; their existing helpers are reused. Working
root versus process-CWD output anchoring and asynchronous lock waiting remain
separate issues. This is not an all-workflow or all-21-crate completion verdict.

Final focused proof: both YAML replay tests, target-flag parsing (including
watch/replay conflict), and the two-mode inline contention regression pass.
CLI library Clippy remains 489 warnings versus 494 on the recorded main
baseline: 484 unchanged-source matches and the same five large-future messages.
No new diagnostic is observed. Formatting and diff checks pass. All 21 crate
AGENTS guides and matching CLAUDE symlinks are present.

### Cache materialization belongs to the invocation repository

The executor records relative artifact paths with any stage `wdir` prefix
already included. `store_local_xorbs` resolves these against its working root.
The product cache-hit materializer instead passed raw paths to inspection,
file reads, atomic writes, and directory reconstruction. Therefore an explicit
`run_in` repository different from process cwd could publish into the wrong
worktree. This is a production boundary: experiment execution calls
`run_in_with_options` with its temporary worktree.

A regression with distinct process and invocation roots reproduced writes into
process cwd before the fix. Its disposable fixture contains both possible
destinations; it does not mutate global cwd. The directory fixture uses the
canonical tree hasher rather than an invented directory digest.

The materializer now resolves each cached path once against a required
repository root. File/stdout inspection, overwrite policy, verified on-disk
fallback reads, atomic writes, and directory materialization use that target.
Stage wdir is not applied again. Inline execution, parallel DAG execution,
single-stage/watch execution, and local/remote cache-only replay all pass the
same invocation root. Cache-only context no longer represents this required
root as an optional working directory. Serialized paths and cache identities
are unchanged.

Evidence map: `exp::run_exp_run_with_id`'s call to `run_in_with_options` and direct
`run_in` callers -> run dispatch -> shared product `materialize_hit` /
`materialize_hit_with_flags` -> shared `materialize_directory`, `write_atomic`,
`overwrite_policy`, and verified cached-file reads. Executor artifact recording
in `crates/crab-workflow/src/executor.rs` and `store_local_xorbs` are sibling
contracts for interpreting these repository-relative paths. Main has the same
raw-path materializer, so this is not limited to the new YAML replay route.

Is this the best fix? Requiring and threading the owning repository through
the canonical materializer covers all existing callers. Changing process cwd
would introduce cross-task global state; changing stored paths would alter a
persistent contract. Neither is needed. Production growth consists of root
arguments and one target resolution, with no new adapter or fallback policy.

Four focused YAML cache tests pass. The new regression covers file, stdout,
and directory artifacts, nested recorded paths, verified reuse after content
cache eviction, and local-edit preservation with no-overwrite. Existing inline
execution/cache-hit and two-mode lock-contention tests also pass. Guide and
public entry-point docs now state artifact-root ownership.

Limits: no claim that every workflow path is cwd-independent; dependency
resolution, cleanup, remote outputs, and other product operations need their
own qualification. No new native experiment/cloud E2E claim. This change fixes
cache-hit materialization's root contract, not unrelated directory overwrite
policy or concurrent symlink replacement.

The debug CLI builds; its native YAML and inline replay/lock smoke tests pass.
The existing macOS debug unwind-size warning remains. CLI library Clippy has
489 diagnostics, identical by file/lint/message to the preceding YAML batch;
no new diagnostic is observed. The previously recorded main count is 494.
Formatting and diff checks pass. Keep this batch local while the current
published head's CI completes, then include it in the next grouped PR update.

### Cache adapter metadata and origin preconditions

Inspection found a documentation ambiguity rather than a new routing defect:
`CachingStore::head` always delegates to origin, while the ObjectStore adapter
intentionally uses cache-service HEAD for unconditional immutable requests and
synthesizes metadata. Its ETag/version are absent and its modification time is
response-construction time. The prior scoped guide said all HEAD operations
retain origin authority; the existing remote HEAD test contradicts that claim.

The README now compares the direct store, mutable adapter reads, conditional
and versioned adapter reads, unconditional immutable reads, and adapter HEAD.
The public adapter docs state the same boundary. `get_with_etag` rustdoc is
shorter, provider-neutral, and explains synthetic tokens without a stale task
identifier or an unsupported blanket claim about every consumer's CAS usage.
The early-bypass comment explains why cached metadata cannot evaluate origin
preconditions or select an object version. Runtime routing is unchanged.

Dependency proof: pinned object_store 0.14.1 `ObjectStoreExt::head` delegates to
`get_opts` with `head = true`; GetOptions defines ETag/time preconditions and
version selection. The adapter's `cacheable_get_options` checks all five
selectors before any cache lookup. `head_immutable_object` and
`bytes_get_result` own synthetic immutable HEAD metadata. The direct `head`
method and explicit cache-service HEAD method are distinct sibling contracts.

Consumer map: `crab-read::StoreClient` file-index lookup and batch lookup ->
`cache_aware_storage` -> `object_store` adapter -> SlateDB-backed metadata
sessions. Hydrate and remote-helper metadata setup also use this facade.
Read-through range helpers remain a separate inspection target.

A retained contract test warms a real local immutable cache and first checks
that a successful conditional GET returns the full origin ObjectMeta and body.
After deleting origin, unconditional GET still succeeds from cache. Each of
if-match, if-none-match, modified-since, unmodified-since, and explicit version
then returns origin NotFound for GET and HEAD rather than cached success.
The test passes both without default features and with remote-client enabled;
the latter enables the feature but does not configure a remote service in this
new fixture.

Five existing remote-enabled tests also pass: direct HEAD, warm cache-service
HEAD, adapter HEAD without origin GET, mutable-path bypass, and explicit
cache-service mutable-path rejection. These use loopback service fixtures,
not live deployed providers. Strict remote-client all-target Clippy passes.
This establishes the documented API distinction and strengthens regression
coverage; it does not establish all range, freshness, or cache-corruption paths.

Minimal-feature strict Clippy exposed an unfulfilled expect_used expectation:
all four test expect() calls are behind remote-client, but the module-level
expectation was unconditional. Gate that expectation with the same feature;
unwrap_used remains expected for both test builds. Strict all-target Clippy
now passes with and without remote-client, without disabling the unfulfilled
expectations lint. Rustdoc builds with warnings denied and no dependencies
rendered. Formatting and diff checks pass. No routing or dependency changes.

### Warm local cache resolves complete ObjectStore range semantics

A warm immutable xorb did not satisfy an EOF-clamped bounded request when
origin was absent: the local exact-range helper returned a miss, then range
resolution attempted origin HEAD and failed NotFound. Offset and suffix
requests skipped the local bounded fast path entirely and also required a
remote/origin size probe. The new regression fails on the preceding source
with a real local cache and a deleted in-memory origin object.

Pinned object_store 0.14.1 defines these policies in GetRange::as_range:
bounded ends clamp to size, offsets read through EOF, and suffixes saturate
at zero (including an empty suffix). ObjectStoreExt::get_range constructs the
same bounded GetOptions request. The adapter now uses that resolver for all
local range forms, including its direct range_get API.

The size-aware local xorb method now accepts a range resolver. It opens once,
reads that handle's size, validates the resolved bounds, verifies identity,
and reads the requested bytes from the same handle. It does not separately
probe a path and reopen it for payload, which could observe a replacement.
The adapter resolves shard ranges against the verified cached body. One
cached_object_range helper consolidates local, cache-service, and resolved
fallback dispatch, deleting duplicate get_opts branches.

API/caller audit: get_xorb_range_with_size_if_present had two workspace
callers: the exact get_xorb_range_if_present wrapper and the adapter. Both now
supply explicit resolver policy. The crate is publish=false. No new dependency,
serialized shape, cache key, or provider option was introduced. The exact
wrapper still supplies the unchanged requested interval and rejects an end
past EOF. Its xorb-read and VFS hydration consumers keep the same signature
and behavior; they are not switched to HTTP-style clamping.

Proof: the retained adapter matrix covers admitted local xorb and shard paths,
EOF-clamped bounds, offsets, short/oversized/empty suffixes, direct range_get,
invalid ranges, object size, returned interval, and exact returned bytes.
Invalid requests leave the valid cache object available. Both minimal and
remote-client feature builds pass this test. The remote-enabled instance of
this new test uses local cache, not a deployed remote service.

Sibling proof: existing exact xorb range and invalid-range recency tests pass.
Existing cache-service EOF-clamping, origin range fallback, and conditional
cache-bypass tests also pass; the service test uses loopback HTTP. Strict
all-target Clippy passes for both changed crates with local-only and remote
features. Their rustdoc builds with warnings denied. Production dispatch is
smaller after consolidation; added test code covers distinct public range
contracts rather than private branches.

Is this the best fix? Resolving against the open handle preserves file identity
without importing ObjectStore policy into crab-cache. Making all low-level
reads clamp would violate hydration's exact-range contract, while a separate
size probe would add I/O and a replacement window. Current source keeps those
owners explicit and uses the dependency's canonical range semantics.

Remaining limits: no new deployed-provider or adversarial in-place mutation
qualification. Cache-service/origin fallback keeps its existing transport and
metadata behavior; this batch specifically removes unnecessary origin access
when a valid local range can answer the request.

The recorded origin/main has the same exact local-range helper and bounded-only
adapter dispatch as the reproduced source. The debug CLI build passes through
crab-read and crab-vfs consumers, retaining the known macOS debug unwind-size
warning. Formatting and diff checks pass.

Published-head CI note: Real Git compatibility (git-2-45), job 101779421800,
failed before qualification while downloading the v1.0.1 rollback archive from
GitHub Releases. Four curl attempts returned HTTP 504; the step exited 22.
This is missing compatibility evidence, not a demonstrated Git behavior
failure. Published head remains dc28b041ca0; local batches are not covered by
that running CI. The browser tooltip-contrast failure remains separate.

### Xorb readers share decoded-length and hash verification

The parser had two decompression paths. `decompress_chunk_data`, used by the
compressed-payload verifier, checked the decoded length and chunk hash. The
Bytes path used by get_chunk, verify_all_chunks, and range reads checked only
the hash. A malformed recorded decoded length was therefore rejected by one
public verifier but accepted by the other readers. A retained regression fails
on the prior source at raw single-chunk retrieval; the fixed matrix covers
None, LZ4, and byte-grouped LZ4 across all reader forms.

Both paths now use the same decoder returning Cow bytes after size/length/hash
checks. Pinned xet-core-structures 1.6.0 CompressionScheme returns borrowed
bytes for None and owned bytes for compressed schemes. Its Chunk constructor
only computes compute_data_hash and stores the bytes. The Bytes wrapper can
therefore reuse the verified hash and preserve its original allocation without
copying or hashing twice. Decompression errors still retain CoreError sources.
The verification-only raw path also avoids the old into_owned copy.

A second regression shows the parser accepted metadata whose decoded total
exceeds u32 offsets, while the builder rejects that layout. Metadata parsing
and detached range decoding now share checked decoded-size accumulation.
The exact u32::MAX structural boundary remains accepted; the test constructs
only small metadata and does not allocate the claimed payload. Range assembly
no longer reserves a buffer from unverified advertised sizes; it grows after
individual chunks pass length and hash checks.

Evidence map: XorbParser get_chunk/verify_all_chunks/get_chunk_range_bytes and
public decode_chunk_range_bytes -> shared decompress_chunk_data -> pinned
CompressionScheme and compute_data_hash. Siblings: verify_compressed_chunk is
used by local-cache file verification and cache-server verification; cached
selective reads call decode_chunk_range_bytes. Builder push/finalize already
check cumulative decoded size in u32. Payload-digest verification remains a
separate explicit operation, documented alongside layout and chunk checks.

Is this the best fix? Sharing the real verifier removes the divergence while
preserving zero-copy raw reads. Adding another length check only to the Bytes
path would retain duplicate hashing/error policy. Narrowing every decoded
xorb to the compressed-size limit would reject a layout the builder supports;
the shared u32 accumulation instead matches its existing contract.

Proof: 20 parser tests pass, including malformed-length matrices, decoded-total
rejection/boundary acceptance, compressed offsets, corruption, and raw-buffer
sharing. Local-cache failed-file/accounting repair passes. Both cache-store
origin-provenance and cache-service corruption tests pass with remote-client;
the service fixture is loopback, not a deployed-provider claim. Strict default
all-target Clippy and warnings-denied rustdoc pass. Production parser source
changes from 486 to 484 lines; regression tests account for the net growth.

Remaining limits: upstream decompression output admission and whole-process
allocation budgets remain separate qualification work. Incremental assembly
avoids trusting advertised totals, but is not a promise that arbitrary valid
multi-gigabyte decoded results fit memory. This does not replace content-address
comparison, serialized-payload verification, or final reconstructed-file checks.

Compatibility proof: v1.1.0 and recorded origin/main both enforce the u32
cumulative decoded bound in builder push/finalize. The parser's new rejection
aligns with that shipped producer contract rather than changing valid producer
output. Origin/main retains the divergent Bytes verifier reproduced above.
The debug CLI build passes through data-plane consumers, with the existing
macOS unwind-size warning. Formatting and diff checks pass. Publication waits
for the currently running Windows workflow check on dc28b041ca0; this local
commit is not yet covered by that published-head CI.

### Bound compressed chunk output before accumulation

The shared decoder previously expanded the entire compressed chunk before
checking its declared length. A retained regression uses valid 1 MiB LZ4 and
BG4 frames with metadata declaring 128 bytes: the old implementation reports
a length mismatch only after producing all 1 MiB. Both schemes now fail in the
streaming output sink when a write would exceed the declared length. The same
frames with correct metadata remain valid.

Evidence map: all parser chunk readers and verify_compressed_chunk share
this decoder; local-cache verification, cache-store selective reads, and
cache-server verification consume those entry points. Pinned
xet-core-structures 1.6.0 LZ4 decoding streams into Write, but its BG4 reader
buffers the complete frame internally before writing. BG4 therefore uses the
same bounded LZ4 decoding followed by the upstream public, length-preserving
bg4_regroup function. The upstream BG4 benchmark counters have no workspace
consumers and are not updated by this path. No dependency or wire-format
changes are required. Raw bytes retain the borrowed/zero-copy path.

The decompression error retains its CoreError source and original compression
scheme. Cache verification already classifies decompression failures as corrupt
cache content; cache-store preserves origin integrity provenance and cache
service repair behavior. Those consumers do not require a CorruptObject error
for excess output. Current main's slice decompressor has the same unbounded
output accumulation reproduced by the regression.

Is this the best fix? A bounded writer uses the existing decoder and rejects
excess bytes before extending the result. Wrapping the BG4 reader directly
would not bound its internal full-frame buffer. Reimplementing the codec or
adding a dependency would add unnecessary ownership and compatibility risk.
The private writer adds a real admission boundary shared by every reader.

Proof: 21 parser tests pass, including valid compressed positive controls,
malformed lengths, hashes, offsets, and zero-copy reads. Both remote-client
cache-store corruption/provenance tests pass. Strict all-target Xet Clippy and
warnings-denied rustdoc pass. The regression fails on the previous source.

Limits: this bounds accepted decoded bytes, not decoder block buffers, Vec
capacity, BG4 regrouping allocations, CPU time, or total process memory. No
allocator instrumentation or deployed-service qualification is claimed.

The debug CLI build passes through the affected consumers (existing macOS
linker unwind warning). Formatting and diff checks pass. All 21 crate guides
and their CLAUDE.md symlink targets are verified. Prior published dc28b041ca0
CI is terminal: 29 success, 11 skipped, two failures described above (browser
contrast and rollback-artifact HTTP 504). New grouped changes require fresh CI;
this evidence is not a full-workspace green verdict.


### Remote Git README navigation

The 280-line README mixed first-use guidance with detailed performance and
qualification contracts. The entry page now presents the ownership/read path,
an API selection table, the explicit operation-completion example, content
representation ownership, and direct links to qualification instructions.
The complete previous README body (14,290 characters) is preserved verbatim in
REFERENCE.md, with a link back to the entry page. The scoped guide links both.

Evidence: checked public exports and the open, operation, snapshot, finish,
archive_stream, and runtime shutdown implementations against the entry page.
The example remains byte-identical to its previous form and follows the
finish rustdoc example. Explicit finish retains semantic and close errors;
archive_stream owns that context, and shutdown waits for tracked contexts.
Empty repository opening versus EmptyRepository snapshot failure is explicit.

This is documentation navigation, not range/cancellation qualification or a
runtime behavior change. No source, dependency, feature, fixture, or baseline
changes. Local file links and all linked Markdown heading anchors resolve.
The all-crate objective remains incomplete; the added reference preserves
existing details without claiming fresh proof for every historical assertion.

Validation: the matching OperationContext::finish doctest compiles and passes;
formatting and diff checks pass. No runtime test expansion is needed for this
prose-only batch. Published c808a1553d2 CI currently has two running and 19
queued checks; keep that head stable while qualification runs and include this
local documentation commit in the next grouped publication.


### Remote Git range admission boundaries

Inspected the packed batch reader, coalescing, range fetch/slicing, operation
caller, and single-entry reader siblings. Individual entry limits precede
coalescing. Range fetches charge aggregate storage and byte budgets and require
an exact returned byte count; entry slicing preserves CRC verification before
decode. Operation callers charge logical-object work separately.

The source comment conflated the 8 MiB merge threshold with a response-memory
bound. An individually admitted entry can exceed that threshold (the default
packed-entry limit is 64 MiB). The comment and scoped guide now distinguish
merge policy from individual and aggregate admission. No runtime policy changes.

Retained boundary coverage checks exact and exceeded merge/gap thresholds,
unsorted input, preservation of complete entries larger than the merge threshold,
the last addressable range, and overflow rejection. It constructs metadata only,
not large payload allocations. Existing nearby-entry/multiple-pack coverage
remains. Current origin/main implements the same policy; these are contract
qualification tests, not a claim of a reproduced runtime defect.

Evidence map: OperationContext::read_packed_entries_with_locators and the reader's
batch object/materialization path -> read_packed_many_with_session_and_locators
-> coalesce_ranges -> read_coalesced_range -> Store::range_get. The single-entry
path retains its own packed-entry limit and checked end offset. The existing
repository fixture test rejects_entry_before_fetch_when_packed_budget_is_too_small
covers the configured limit, but was inspected rather than rerun in this batch.
No dependency contract or wire format changes. Broader real-range provider and
consumer qualification remains open.

Validation: both final reader coalescing tests pass, including all added cases;
strict all-target crab-remote-git Clippy passes. Formatting and diff checks pass.
Production changes are comments only; test growth protects distinct admission
boundaries and entry preservation. No binary rebuild is required for these
comment/test changes. Keep this local commit for the next grouped PR update
rather than cancelling the current published-head qualification.


### Reject invalid multipart cleanup clocks

MultipartRegistry::find_abandoned converted pre-epoch and unrepresentable
SystemTime values to i64::MAX. That sentinel made active uploads satisfy both
lease-expiration and grace predicates. A retained regression creates an active
lease and scans at one second before the epoch: the old source returns the
active row. The fixed code returns InvalidInput before issuing the query and
preserves SystemTimeError inside StagingError::Io. Integer conversion overflow
also returns InvalidInput with its typed cause instead of saturating.

Evidence map: fsck scan -> StoreChecker::check_multipart_uploads ->
MultipartJournal::find_abandoned (spawn_blocking adapter) -> registry scan ->
checked SystemTime and SQLite integer conversion. CrabError preserves the Io
value. fsck logs a failed scan and creates no multipart issues from it. The
repair path separately uses a row-revision and expired-lease claim before
provider abort; this regression proves false candidate selection, not an
observed provider abort. The repair unix_now helper falls back to zero and
remains separate clock-policy work; it does not use the maximum-time sentinel.

Sibling inspection: staging batch/publication/temp identifiers use timestamps
as nonce inputs with process/sequence components, not expiration cutoffs.
Lease duration saturation is distinct from converting an absolute scan time.
The shared RFC3339 helper has a different range contract and is not a staging
dependency; no new dependency or format change is needed here. Installed Rust
SystemTime documentation confirms duration_since returns SystemTimeError when
the comparison time is later. Current origin/main has the reproduced sentinel.

Is this the best fix? A fallible conversion uses the scan's existing Result
boundary and prevents fabricated cleanup candidates while retaining error
causes. Clamping invalid time to zero or returning an empty success would hide
the failed scan. No public error variant, schema, or lease policy changes.

Proof: regression fails before the fix; all 12 multipart journal tests pass
afterwards, including ownership contention, renewal, takeover, and fsck/resume
races. Production provider cleanup and cross-platform clock limits are not
claimed by these SQLite-local tests.

Strict all-target staging Clippy passes. Strict rustdoc initially exposed six
existing public documentation link failures in lib.rs: two links to a private
blocking-budget constant and four unqualified method links. Qualified the
methods with Self and documented the verified 120-second value without linking
the private item. Warnings-denied rustdoc now passes with no suppression.
Formatting and diff checks pass. A fresh CLI build is still required before
publishing this runtime batch; no latest-head consumer-build claim is made.

### Multipart clock consumer qualification

A retained CLI regression now calls StoreChecker through MultipartJournal's
spawn_blocking adapter with an on-disk SQLite registry and a pre-epoch scan.
It verifies CrabError::Io retains InvalidInput and the SystemTimeError cause.
The staging regression covers active-lease misclassification; this consumer
test covers error preservation across the asynchronous composition boundary.

All five selected CLI multipart tests pass: the new checker regression,
endpoint-change repair protection, row-replacement repair protection, exact-byte
multipart export, and push-packer spill behavior. The separate existing
checker_and_repairer_abort_exact_journal_destination test also passes using its
in-memory provider and real SQLite journal. These are local integration checks,
not live-provider qualification.

The debug CLI build passes for the staging runtime fix, with the existing
macOS linker warning. This closes the consumer-build gap recorded above.
Formatting and diff checks pass; the only new source is the 20-line test.
Published c808a1553d2 CI remains running; local commits are still awaiting the
next grouped update rather than cancelling that qualification.

### Build metadata tracking in linked worktrees

Consecutive focused CLI builds exposed avoidable recompilation. build.rs watched
../.git/HEAD and ../.git/index, but .git is a file in this linked worktree.
A small Cargo fixture using those exact directives reproduces an unchanged
rebuild; Cargo verbose output states that ../.git/HEAD is missing.

The build script now asks Git for absolute HEAD, current-branch, and packed-refs
paths and emits watches only for existing paths. HEAD tracks detached commits
and branch switches. The current loose ref and packed-refs track branch movement;
when the loose ref is absent, its nearest existing parent detects recreation. The index is not an input
to the embedded short HEAD SHA. No build version, timestamp, pricing generation,
or environment-override semantics change. Existing missing-Git/archive metadata
behavior remains best effort.

Dependency evidence: installed Cargo documentation says directory watches scan
for modifications. Installed Git rev-parse documentation specifies absolute
path output and git-path relocation handling. Native Git resolves HEAD into
this worktree's private Git directory and branch refs into its common Git directory.
No sibling build script in the workspace emits Git watches. Current origin/main
has the two literal paths reproduced by the fixture.

A disposable Cargo/native-Git probe executes the new helper verbatim with
separate target directories per checkout. Twenty observations cover the old
unchanged rebuild, new normal/linked-worktree build reuse, unrelated checkpoint
updates, branch commits,
packing refs, commits after packing, detached checkout, and detached commits.
Every invalidating operation rebuilds and embeds the expected native short SHA;
unchanged runs after each operation remain fresh. This is local files-backend
Git 2.50.1 evidence, not cross-platform or reftable qualification. Probe output
is in /tmp/crab-089c-build-watch-results.json; no production fixture or dependency
was added. Git metadata paths that cannot be represented in Cargo's textual
watch directives remain outside this qualification.

Is this the best fix? Resolving Git's own paths addresses both worktree layout
and branch metadata without watching the entire Git object directory or shared
refs tree. Watching
only the gitfile would miss commits. Watching nonexistent optional paths would
retain the repeated-build defect. The helper owns real build invalidation;
its added lines replace two incorrect watches rather than adding another mode.


Published-head qualification update: c808a1553d2 passes RustFS race/crash/scale
(job 101789745232, run 34136856427) and binary/integration contracts
(job 101789745447, run 34136856340). These replace older-head evidence for
those surfaces. Workflow, cache-service, protocol platform, and split-crate
jobs remain live; unpublished follow-ups are outside this CI scope.


The first full CLI build passed with the initial shared-refs watch, but the
unchanged rerun compiled again. Inspection found Codex checkpoint refs updated
inside that shared tree during verification. Narrowed the watch to the current
branch and added unrelated-ref probe coverage. Stopped the superseded rebuild
with SIGINT (exit 130); that was an intentional stop after a source correction,
not a timeout or a failed compiler result. The corrected full build followed by
an immediate unchanged build completed successfully as one sequential process.
The full build took 207.21 seconds; the unchanged run took 4.88 seconds and
did not compile Crab. These are observed local timings, not a controlled
benchmark. The existing macOS linker warning remains.


Current-head browser qualification: job 101796901979 (run 34136856236)
failed on the release-delete tooltip, contrast 1.43 versus 4.5. Filtered job
logs identify “Delete Crab 1.0 patched”; browser source and rust.yml still
match recorded origin/main. This is source isolation, not a new main runtime
reproduction or a reason to suppress the accessibility check. The draft PR
records this alongside the successful RustFS and binary/integration gates.


Final build-tracking verification: all 20 fixture observations pass and the
actual linked-worktree CLI remains fresh on its unchanged rerun. Build metadata
is still invalidated by the commit changes exercised in the fixtures. No source
or dependency changes followed the successful sequential build. Formatting and
diff checks pass. This batch is ready for grouped publication; current PR CI
continues to qualify c808a1553d2, not these local follow-ups.


### Checked push-lock deadlines

Push-lock acquisition and both renewal write paths added TTL seconds to the
Unix clock without checking overflow. Renewal also added a duration directly
to Instant. A retained regression invokes the public acquisition paths with
Duration::MAX and panics on the old source before reaching its assertion.
Current origin/main contains the same four Unix additions and Instant addition.

A shared expiry helper now returns Configuration when the Unix sum cannot be
represented. Renewal validates before a holder lookup/CAS and recomputes the
expiry for each write attempt, since retries can cross clock seconds. Its
monotonic retry deadline uses Instant::checked_add. No arbitrary maximum TTL,
new option, dependency, public variant, or wire-format change is introduced.
The native Instant documentation specifies that checked_add returns None when
the platform cannot represent the result.

Evidence map: product metadb/push callers construct Duration from resolved
push_lock_ttl_secs; product config validates a minimum (>20) but not an upper
limit. The public crate context also accepts Duration directly. Context ref,
internal, and nonblocking acquisition converge on the two checked creation
paths. PushLock::renew and renew_if_holder converge on renew_one; token fast
CAS and read-after-conflict renewal use the same expiry helper. Background
heartbeat and while_renewing preserve the existing error/drain/release policy.
The Duration::MAX reproducer is a crate API test, not a claim that TOML can
encode every u64 value.

Siblings: push admission and GC fences use saturating timestamp arithmetic and
minimum-TTL validation, rather than these unchecked additions. Their large-TTL
and clock policies remain separate qualification work. Short-TTL renewal cadence
also remains outside this overflow fix; the product minimum is unchanged.

Is this the best fix? Checked arithmetic rejects invalid input through the
existing Result boundary instead of panicking or changing the requested lease
by saturation. The shared helper removes repeated expiry policy at all four
writes. The renewal preflight prevents an invalid duration from causing storage
work, while recalculation in each attempt preserves the existing clock behavior.

Proof: all 30 push-lock tests pass, including the new public-path regression,
zero storage requests for oversized TTLs, renewal/release retries, expiration,
reclamation, and tombstone protection. The old regression fails with arithmetic
overflow. This does not qualify deployed backend behavior or every platform's
Instant bound. Consumer build is required before grouped publication.

The existing while_renewing regression passes both work-result outcomes after
lost-lease cancellation and draining. Strict all-target Clippy and warnings-denied
rustdoc pass with object-store-lock. Heartbeat source confirms any renewal error
cancels the push and stops the heartbeat. Formatting and diff checks pass.
Production growth provides the common checked expiry and platform-checked retry
deadline; regression coverage accounts for the remaining source growth.

### Checked deadline consumer qualification

The CLI heartbeat now has a retained regression for invalid renewal deadlines:
a live stored lease is observed, the heartbeat is given an oversized duration,
and push cancellation must occur without changing the stored bytes or ETag.
All 11 local heartbeat tests pass; the dedicated S3-compatible provider case
remains ignored. Existing cases cover stolen/deleted/released leases, renewal,
clean stop, and independent shutdown ownership. The debug CLI build passes
through coordination's consumers with the existing macOS linker warning.
Formatting and diff checks pass. This closes the pre-rebase consumer-build gap.

Fetched origin/main has advanced to a371fb7d002 (add/push hardening, #156),
affecting staging, cache-store, and product callers. Its Cargo.lock diff only
reorders an existing windows-sys dependency. Rebase and overlap qualification
are required before publication; the above consumer results precede that rebase.

### Rebase overlap and guide checks

Rebased onto a371fb7d002 without conflicts. Range-diff matches all 83 patches
unchanged; this preserves the authored changes but does not replace validation
against the new base. The 12 multipart journal tests, warm-cache range regression,
and conditional/versioned-read regression pass on the combined source. The
post-rebase CLI build passes with the existing macOS linker warning.

All 21 per-crate AGENTS.md files and sibling CLAUDE.md symlink targets are
present. All 457 repository-relative source paths found in those guides resolve,
as do 129 local file links in crate READMEs/references. These checks establish
path/link integrity, not semantic correctness of every linked implementation
or external website availability. No baseline or ignore file was changed.


Post-rebase validation is complete for the selected overlap checks and CLI
build. Broad qualification of the new combined head still requires fresh CI.
The published review branch remains c808a1553d2 while its Windows workflow job
101789745148 finishes. Updating that branch after the rebase requires an exact
force-with-lease against the verified published head, rather than a fast-forward.


## Metadata diagnostic candidate ordering

`read_chunk_index_entry` prefers the stored head, then scans immutable placement
rows if the head is absent. On main a371fb7d002, the scan kept the first receipt
at a generation while both writers selected the greatest placement ID within
that generation. A real SlateDB regression reproduces the inconsistency: after
publishing two competing placements in one batch, deleting only the head changes
the diagnostic result from chunk index 0 to chunk index 1.

The scan now compares (committed generation, placement ID). It retains the
already validated key ID alongside the selected receipt, avoiding repeated
placement hashing. Key/value identity, proof and anchor validation, error
propagation, and explicit reader close remain unchanged. Rustdoc distinguishes
candidate lookup from proof of current source visibility or object availability.

Evidence map: the changed public diagnostic reader calls key_codec decoding,
decode_placement, resolve_receipt, and SlateDB DbReader. Its workspace consumer
is the auth-server receive publication test; production reconstruction uses
crab-read. RemoteIndexWriter::write_entries and product ChunkIndexStore's
save_committed_receipts already agree on the generation/ID tie-break. Product
get_committed_candidates_page reads heads directly; receipt-pinned reads use
exact immutable IDs and preserve a prior candidate after head replacement.
Neither production read path uses this diagnostic history scan.

The locked SlateDB 0.15.0 prefix scan uses suffix-relative bounds; `..` covers
all placement IDs for the chunk. The regression writes to real SlateDB over
InMemory storage, closes each writer, removes the head, closes again, and reads
through the public helper. Deletion buffers with await_durable=false because
this writer disables timer flushes and owns the final close barrier. An initial
test-build attempt was intentionally interrupted after correcting the fixture's
default durable-write wait; it was not an observation timeout or source failure.
The corrected before-fix test fails on the selection assertion.

Is this the best fix? The scan adopts the existing writer ordering with a local
comparison, without a new public abstraction or format change. Its agreement is
within a batch: a stored head can reflect a later batch rather than the maximum
generation across all history. No claim of global freshness, dedup corruption,
or byte-reconstruction failure follows from this diagnostic discrepancy.

All six remote-index tests pass, including equal/different generations in both
input orders. Strict all-target Clippy and warnings-denied rustdoc pass with
remote-index. Most source growth is the retained I/O regression and shared test
fixture; production growth explains the selection and candidate contract.
The auth-server receive publication test also passes, exercising committed file
and chunk-index publication through the existing public diagnostic consumer.
Formatting and diff checks pass. This batch is locally qualified; its broader
CI evidence must follow publication with the next grouped PR update.


## One owner for VFS background hydration (in progress)

The previous queue-worker handle collections did not include read-window
prefetch. A retained regression additionally proves that a cancelled service
still accepted a new prefetch request. The before-fix test fails on that public
admission result; it is not a claim that the cancelled request published corrupt
bytes or that every backend task has leaked.

HydrationService now retains both task types in the existing tokio-util 0.7.18
TaskTracker. A short mutex serializes Ready/Running/Stopped admission with task
registration and tracker closure. No lock crosses an await. Queue startup is
idempotent; enqueue and prefetch reject stopped/cancelled services. A private
child token stops queue workers without cancelling foreground reads.

request_shutdown closes admission and requests worker exit. shutdown awaits the
same tracker, including admitted prefetch reconstruction and blocking cache
writes. Multiple waiters and resumed waits share completion ownership. Prefetch
is not aborted: write_cached_window awaits spawn_blocking, and dropping that
outer future would not prove its file work had stopped. Locked TaskTracker docs
state that close alone permits new spawns and that wait covers task-future
destruction. The admission mutex supplies the missing no-new-tasks boundary.

The worker-handle vectors are removed from PipelineOutput and RepoRuntime.
Pipeline, daemon, coordinator, IPC setup/unmount, NFS, and both foreground CLI
mount paths now retain the service or engine until its background shutdown
finishes. Daemon and CLI fallible setup blocks drain on errors too. Coordinator
unmount precedes background drain; its ten-second warning does not cancel the
work or release ownership. Synchronous coordinator cleanup can only request
shutdown, as before; callers requiring completion must use the async path.

Evidence map: worker and prefetch production spawning both live in hydration.rs.
Pipeline and daemon start queue workers after preparation; engine reads and
prefetch_dir dispatch prefetch. Coordinator/IPC and CLI own FUSE session endings;
NFS run_until_cancelled owns its listener result and still attempts journal sync
and native unmount before stopping background hydration. Engine's small shutdown
method exposes that ownership operation to the NFS and legacy foreground owners
without exposing its service field. No dependency, feature, or storage-format
change is introduced. All workspace callers of the changed worker-start and
unmount APIs were searched and migrated.

Is this the best fix? It gives queue and speculative work one completion owner,
removes the obsolete handle vectors and queue-only grace helper, and preserves
foreground/backend ownership as a separate explicit boundary. It avoids aborting
an async wrapper around still-running blocking I/O. This intentionally provides
completion rather than a hard shutdown latency bound.

Initial proof: 34 hydration tests pass with NFS and FUSE, including admitted
prefetch held behind its real window lock, concurrent shutdown calls, completed
cache reads with origin access disabled, startup/shutdown races, and rejection
of late queue/prefetch work. The 14 pipeline and 38 daemon tests also pass;
daemon teardown now observes actual hydration-service release as well as its
refresh/watcher task destruction. Other owner checks, Clippy, feature builds,
and the CLI consumer build remain in progress before publication.

Native NFS connection handlers and the dependency cleaner remain outside this
tracker. Joining the listener still does not prove their completion, and this
change must not be described as whole-backend shutdown proof. Failed native
unmount and foreground request draining require separate qualification. Native
NFS smoke jobs are skipped on pull_request events; a workflow_dispatch on the
published review ref is needed for those existing dedicated jobs.

The 13 coordinator and 16 NFS parent-lifecycle tests pass as well (115 focused
tests across the five selected modules before the final test additions). A final
regression strengthens timed-out-wait resumption and positive single-pool startup;
pipeline success now also verifies cache-reference release after shutdown.
Those test additions require their final rerun. Lint/feature/doc and CLI build
proof is still pending; this VFS migration is uncommitted and unpublished.
Abandoning a whole mount/setup/shutdown future is not covered by the normal
awaited owner paths, and dependency child requests remain separate gaps.


### Published-head CI completion

Head 0f268b2c21c now has 31 successful checks, 11 skipped checks, and no failures.
RustFS race/crash/scale run 34141980690, binary/integration contracts in run
34141980569, and repository browser interactions in run 34141980655 pass. All
checks are terminal. This supersedes the pending-CI note for that published head;
it does not qualify local metadata commit e27c565392a or the uncommitted VFS
ownership migration. The PR description now separates current results from
remaining native/cloud/lifecycle qualification.

The two legacy foreground preparation tests also now await the engine's new
background shutdown boundary. They will run with the CLI consumer checks; no
native mount is needed for those real-Git preparation fixtures.

The legacy foreground builder creates a fresh hydration cancellation token and
returns only resolver/engine. Its session token is separate. The explicit engine
shutdown therefore supplies completion ownership that session cancellation alone
cannot provide; the new call-site comment records that caller difference.


### Final VFS regression and lint results

The final 35 hydration tests and 14 pipeline tests pass, including timed-out wait
resumption, positive single-worker-pool startup, worker-owned service release,
and the pipeline cache-reference check. Together with the unchanged 38 daemon,
13 coordinator, and 16 NFS checks, this is 116 focused passing tests. Strict
all-target Clippy passes with NFS and FUSE. The existing lint process completed
normally; it was not restarted after its observation delays.

Separate NFS/FUSE checks, rustdoc, the CLI build, and the three foreground
preparation tests are running in the final verification driver. This is still
an uncommitted VFS batch pending those results and native workflow qualification.

## Cache-server startup ownership inspection (next candidate)

Read server prepare/run/shutdown, evictor startup/loop/shutdown, origin client
construction, config URL loading, and preflight's startup options. prepare_server
starts an evictor holding Arc<CacheStore> before the fallible OriginClient::from_url
call. The config loader requires an origin string but does not construct its
object store; build_url_object_store can reject malformed URLs before reading
provider environment options. Preflight disables both eviction switches, while
run_server enables them.

The evictor loop retains a strong cache-store Arc, and its handle has no Drop
implementation. Its rustdoc's suggestion that dropping an external cache-store
Arc stops the task does not match this ownership. A bounded next change should
reproduce failed preparation with a malformed origin, prove no maintenance task
or cache mutation survives rejection, and move fallible origin construction
before cache work/background spawning. Verify the enabled and preflight-disabled
paths. No cache-server source has been changed, and runtime reproduction remains
required before treating this inspection as completed defect qualification.

The cache-server regression is now written, with separate current-thread Tokio
runtimes for service-enabled and preflight-disabled eviction. It checks that an
invalid origin returns an error without a live task or newly created cache root.
Tokio 1.52.1 runtime metrics document spawn/exit task counts and only qualify
multi-threaded counts as weakly consistent; the fixture uses the current-thread
runtime deliberately. The before-fix run is queued after the active VFS driver
(session 34608), in session 80278, logging to
/tmp/crab-089c-cache-startup-before.log. Production code remains unchanged until
the regression result is inspected. This is not yet a reproduced defect.

The remaining FUSE refresh ownership gap is specifically the sole production
caller of pipeline::spawn_refresh_loop in IPC handle_mount: its returned
JoinHandle is discarded. RefreshService::run serializes two timers in one task,
and poll_remote awaits a spawn_blocking Git fetch. A follow-up owner must retain
and await the refresh task after cancellation; merely aborting that outer task
would not establish completion of the blocking fetch. Daemon and NFS refresh
owners use different loops and must be reviewed separately. This is distinct
from the hydration tracker, whose comments now consistently name hydration.

Separate NFS-only and FUSE-only cargo checks passed (14m32s and45.90s).
The final verification driver exited101 at strict rustdoc: existing bare links
for DaemonService::start and MountPipelineBuilder::execute did not resolve.
Both links now use Self qualification. Rustdoc must rerun; CLI build/tests did
not execute in that driver. The queued cache startup before-test has begun.

### Cache-server invalid-origin preparation regression

The before-fix test failed at the intended assertion: preparing an invalid
origin with eviction enabled left (alive tasks, cache-root existence)=(1,true),
against the expected(0,false). Log: /tmp/crab-089c-cache-startup-before.log;
session80278 exited101. This confirms a detached task and disk initialization,
not merely an inferred ownership risk.

prepare_server now constructs the origin immediately after policy validation,
before cache recovery/index rebuild/startup eviction or task spawning. No new
cleanup abstraction or error mapping is needed; successful construction still
hands the evictor directly to PreparedServer. run_server and preflight retain
their existing awaited shutdown paths. The evictor rustdoc now correctly states
that the task holds its own strong cache-store reference. README/AGENTS explain
the preparation boundary and distinguish client construction from connectivity.

The after-test, existing preflight tests, strict all-target lint, and strict
docs are running in session34303. No dependency, format, or configuration
surface changed. VFS strict rustdoc passed after fixing the two method links;
its remaining CLI build/tests continue in session53042.

The cache-server after-run passes all three server tests, including invalid
origin rejection with eviction enabled and disabled. The CLI binary build also
passes (4m39s), with the previously observed macOS large-debug-unwind linker
warning. CLI foreground preparation tests and remaining cache preflight/lint/docs
are still active. Guide validation resolves all31 referenced paths across the
two touched guides, and both CLAUDE symlinks still target their AGENTS files.

### Publication-ready local results

VFS consumer verification completed: the CLI build and all three foreground
preparation tests pass. Separate NFS/FUSE checks, strict VFS rustdoc, and strict
all-target VFS Clippy pass alongside116 focused lifecycle tests. Runtime source
has not changed since those tests/lint; subsequent VFS edits correct rustdoc
links and ownership comments. Cache startup passes three server tests and16
preflight tests, strict all-target Clippy, and strict rustdoc. The exact-type
configuration-error assertion was included in the passing after-run.

Latest fetched main remains a371fb7d002; HEAD contains it. The unpublished
metadata commit and these two source batches are ready for grouped publication.
Native NFS workflow dispatch and refreshed PR CI remain required after push.
These results do not establish whole-backend/refresh completion or finish the
all21-crate objective.

## Refresh completion ownership (in progress)

Published d58bcf91151 with metadata/VFS/cache startup batches; native NFS
workflow34148861825 is running on that exact head. Current local follow-up
retains the coordinator refresh handle and joins refresh in all three owners:
coordinator removal/graceful shutdown, daemon teardown, and interactive NFS.
Daemon and interactive NFS previously aborted the outer refresh task, even
though RefreshService::poll_remote and nfs_control::refresh_runtime await
spawn_blocking work. Cancellation now stops later polls; joining includes the
admitted blocking fetch/snapshot operation. Watcher/control ownership is separate.

Before-fix proof adds only handle retention plumbing and a blocked refresh task
to the existing shared-resource test; removal behavior is unchanged for that
run. Session17267 exits101 at the intended assertion that removal detached
in-progress refresh work. A channel releases the blocking worker before the
assertion so failure cannot hang the runtime. The fix adds awaited completion
before backend/cache release and cancels the daemon repo token before joining.
Its daemon fixture now models cooperative cancellation instead of requiring an
abort to finish. Focused coordinator/daemon/NFS/refresh tests, strict lint/docs
are running in session81396. No native mount has been exercised locally.

The refresh batch passes13 coordinator,38 daemon,23 refresh-loop, and initially
16 NFS parent-lifecycle tests, strict Clippy/rustdoc, separate NFS/FUSE checks,
and the CLI build. Those NFS fixtures disabled auto-refresh, so a new regression
now admits real nfs_control auto-refresh behind a held runtime mutex, cancels,
and verifies teardown remains pending until the blocking operation can finish.
The fixture first polls synchronous startup to completion so temporary control
state references cannot masquerade as an admitted refresh. Its first attempt
missed that synchronization; the corrected fixture passes all17 NFS tests.

Restoring the old NFS handle.abort line makes this new regression fail at the
intended detached-refresh assertion (session95988, exit101). The script restored
the fixed file afterward, and cmp verified exact restoration. Logs:
/tmp/crab-089c-vfs-refresh-nfs-admission-before.log and
/tmp/crab-089c-vfs-refresh-nfs-admission-after.log. This provides NFS-owner proof
in addition to the coordinator regression. Strict all-target Clippy is rerunning
for the new fixture; runtime source remains identical to the passing build.

Final all-target VFS Clippy passes with the new NFS fixture. The refresh batch
now has91 focused passing tests, backend checks, strict docs/lint, and CLI build
proof. Native qualification remains tied to published head d58bcf91151 until
this follow-up is published and separately qualified.

## Scheduler contention yields to async callers (in progress)

All four production acquire callers in crab/src/cmd/run.rs are async: inline
execution/replay, YAML cache replay, YAML single-stage, and DAG execution.
SchedulerLock::acquire previously used thread::sleep during contention. A
biased tokio::join fixture polls the waiter before a holder-release future;
the old implementation times out because the latter cannot run. Session36156
exits101 at the intended starvation assertion. The same fixture passes after
making acquire async and using Tokio sleep for backoff.

There is one acquisition wait API, with all workspace callers migrated; no
blocking compatibility alias was added. The crate has publish=false. Existing
try_acquire keeps its immediate-contention semantics. fs4 0.13.1 documents
Ok(false) for contention, and Tokio 1.52.1 documents sleep cancellation by
future drop without extra cleanup. Filesystem attempts and PID writes remain
synchronous; this change specifically fixes contention backoff, not all I/O.
Timeout errors, no-wait policy, guard ownership, retained inode, and PID cleanup
remain unchanged. Tests formerly wrapping acquisition in spawn_blocking now
exercise it directly or as an async task.

The first13 scheduler-lock tests pass. A14th cancellation/reacquisition test
was added before final verification. Session13815 runs those tests, strict
all-target workflow Clippy, strict rustdoc, cache-only CLI consumers, and CLI
build. The inline sidecar contention consumer also needs its focused run.
This batch is uncommitted; refresh commit0cd4fea1b07 remains unpublished.

Scheduler final checks pass:14 lock tests, strict all-target Clippy, and strict
rustdoc. CLI cache_only filtering passes8 tests, of which two are the directly
relevant YAML replay consumers; the other six are incidental push/migration
matches. The separately selected inline contention test also passes for normal
and cache-only execution, preserving holder sidecars. The CLI build passes with
the same macOS debug-unwind linker warning. Runtime behavior is unchanged after
those checks; the final edit only improves module-doc wording.

## Cache-server signal and runtime inspection (next batch)

Read server bootstrap/plain/TLS paths, signal helpers, error mapping, and
binary serve/check/onboarding-probe dispatch. TLS starts a signal task without
retaining its handle, then can return on listener bind/serve failure. Plain
HTTP instead owns its shutdown future through axum. Signal registration uses
expect on Unix and non-Unix paths. Three binary Runtime::new calls also expect:
serve, check, and onboarding probe. These are distinct startup failure surfaces.

For TLS completion ownership, prefer an owned signal future alongside the serve
future rather than an independently spawned task. Locked axum-server0.8.0's
Handle::graceful_shutdown uses notify_waiters; its waiter does not check a
sticky shutdown flag. A refactor must register the signal before serving and
ensure the listener has entered its wait before notifying, including a signal
arriving during startup. Preserve the current bounded TLS drain and indefinite
plain-HTTP drain; timeout-policy changes require their own product decision.
The existing cache-service mTLS smoke already generates temporary certificates
with openssl. No server/binary source has changed in this inspection.

Current native NFS dispatch34148861825 passed its feature gate; Linux native,
Linux RustFS/Xet, macOS, and Windows native jobs are all running on d58bcf91151.
Refresh0cd4fea1b07 and scheduler9503226fede remain local pending grouped push.

### TLS signal lifetime regression and fix

A real occupied loopback listener and generated temporary TLS certificate expose
the old failure path: serve_tls returns a bind error but leaves one Tokio signal
task alive. Session2578 exits101 at that task-count assertion. The current-thread
fixture isolates task counts and covers both TLS and mTLS acceptor branches.

Signal registration now happens at run_server entry, before prepared cache state
or maintenance tasks exist. Registration errors preserve their io::Error through
InternalError instead of panicking. TLS owns its signal future alongside the
serve future, polls serving first, and waits for listener readiness before the
dependency's non-sticky graceful notification. The router make-service's
poll_ready/call are immediately ready in locked axum0.8.9; this supports that
ordering. Plain HTTP retains axum's graceful drain behavior. Bind/serve errors
now retain typed I/O causes instead of converting them to strings.

Four server tests pass, including typed AddrInUse and an already-ready shutdown
signal in both TLS modes. The initial generic HTTP signal parameter needed
axum's Send+'static bound; it is now declared explicitly. Preflight, strict lint,
strict docs, and binary build are running in session46431. Tokio Windows ctrl_c
registration/recv contracts were read alongside the Unix path; Windows execution
still requires CI. Unix test setup requires openssl, already used by the
cache-service mTLS smoke, and writes only temporary synthetic certificates.

Dependency caveat: axum's graceful serve implementation itself spawns a signal
future. Normal awaited HTTP completion includes that signal; abandoning the
whole serve future can leave dependency work. This batch does not qualify
abandoned-server cleanup or fix the three binary Runtime::new expect sites.

TLS signal validation completed: four server tests,16 preflight tests, strict
all-target Clippy, strict rustdoc, and the cache-server binary build pass.
Published PR head d58bcf91151 has no reported failures; its Windows workflow
test job is the only remaining PR check at this observation.


### Native NFS evidence and read-pool ownership follow-up

Run34148861825 on published d58bcf91151 passed Linux native smoke and Linux
RustFS/Xet E2E. macOS failed the retained positive read-lease-hit assertion;
Windows remains running at this observation. The macOS benchmark returned8MiB
to userspace over two4MiB passes, while NFS recorded four READ RPCs totaling4MiB,
four lease misses, zero hits, and zero benchmark-time evictions. These counters
do not prove a broken cache: concurrent first misses and kernel read caching
must be distinguished from sequential adapter reuse. The smoke assertion and
workload remain unchanged pending that qualification.

Separately, source inspection found an entry-lifetime defect already present on
origin/main: ReadLeasePin drops unpin by file ID alone. NFS refresh/switch clears
entries, and mutation/stale-read paths evict individual IDs. If an active old
pin survives removal and the same ID is reinserted, its eventual drop decrements
the replacement's count. Under budget pressure the pool can then evict a still
pinned replacement. The regression exercises both evict and invalidate_all,
reinsertion, and actual LRU pressure; it does not assume concurrent scheduling.
FUSE uses handle-owned VfsReadLease values, not this pool. The engine source
cache has no pin/drop accounting and is unaffected by this particular defect.
This finding is separate from the native macOS positive-hit failure.


Read-pool regression failed before the fix (session10780, exit101). Entry
identity now uses an Arc token shared by the cached entry and its pins. The
pool retains the same identity when concurrent opens replace a still-present
entry; explicit removal creates a new identity on reinsertion. Arc::ptr_eq
checks that lifetime under the existing mutex before decrementing pin counts.
The extra private token allocation per entry avoids numeric generation rollover
and adds no public API, dependency, or storage-format surface.

Nine pool tests, nine NFS read/readdir/readlink tests, and the control-generation
invalidation test pass. Strict all-target NFS-feature Clippy and strict rustdoc
pass. CLI build remains running in session57813 at this observation.

Retained main-run34136668045 macOS evidence used the identical benchmark script:
four READ RPCs and4MiB at the protocol boundary, with three benchmark lease
misses and one hit. The PR run instead has four misses and zero hits. This
supports a first-open scheduling difference but is not a complete causal proof.
The main workflow was cancelled overall and its retained-evidence verification
failed; only its completed macOS native job is cited as successful here.


Read-pool consumer build completed successfully (session57813, exit0). The
macOS debug linker emits the previously observed large-unwind-section warning.
Formatting and git diff --check pass. Production code grows17 net lines to
carry entry identity through pin admission and release; the regression and
short ownership documentation explain the payoff. PR checks on d58bcf91151 are
now terminal with no failures; its separate Windows native job remains live.


### macOS native read workload correction

The retained benchmark verifier requires both lease misses and lease hits in
its own before/after window, not just the later mount-status snapshot. A
separate probe after that window would therefore leave the benchmark unproved.
The macOS script now sets fcntl.F_NOCACHE before the first read in each pass and
records client_cache=disabled. The fixture is previously unread at this point.
No positive-hit assertion, threshold, baseline, or verifier expectation changed.

Dependency evidence: Apple's [fcntl manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fcntl.2.html)
defines F_NOCACHE. Its [NFS client source at93733ffc](https://github.com/apple-oss-distributions/NFS/blob/93733ffccece4daee73bf6306b291d1ae69130c0/kext/nfs_bio.c)
limits uncached read-ahead to the request and marks freshly read buffers
NB_NOCACHE; release dumps those pages. Existing cached pages may still be read,
which is why setting this before the fixture's first read matters. Historical
kernel-cached throughput is not an equivalent workload baseline. This source
is published upstream evidence, not proof of the exact runner kernel build.

Linux and Windows retain their existing native workloads. The d58 Linux run
passed its positive-hit assertion; Windows is still executing its native step.
Platform-general cache-control/benchmark comparison policy remains follow-up;
this correction uses a macOS-specific API and does not claim those other kernels
have identical caching behavior.

Local shell syntax, all seven embedded Python ASTs, all three platform script
contracts, script-contract self-test, and retained-report verifier self-test
pass. A temporary local file accepts F_NOCACHE and preserves bytes over repeated
reads; that checks the host API only. Native macOS CI remains required. The
initial verifier invocation used --self-test and was rejected by argparse; the
actual documented self-test subcommand was then run and passed.


### Cache-service JSON output completion

The cache-server binary had seven report-specific JSON emitters. They logged
serde_json write errors but returned unit, so all successful preflight,
evidence, and onboarding commands could still exit0 after output failure.
The same code is present on origin/main. Ten call sites were inspected: eight
normal report paths now return exit1 on output failure; the two configuration
error paths already return None and exit1 independently of report emission.

An initial read-only-stdout fixture was invalid for this purpose: Rust standard
stdout intentionally treats EBADF as a successful write/flush (local toolchain
std/io/stdio.rs, StdoutRaw through handle_ebadf). It failed both before and after
the proposed fix and has been replaced. The actual regression closes a Unix
socket peer before starting the CLI. Old evidence verify logs Broken pipe but
exits0; fixed verify and summarize exit1 without panic. Successful invocations
with identical input are checked first. The old-source recheck in session96010
failed as expected and restored the fixed source afterward.

One private write_json writer now checks serialization, the trailing newline,
and flush. Stdout is locked across that sequence, and emit_json reports failure
to all normal command owners. Evidence-file output uses the same writer and
keeps its original file handle through the newline, removing the reopen-for-
append path. No wire shape, public API, manifest, dependency, or lockfile changes.

Locked serde_json source shows to_writer_pretty serializes without flushing;
its From<serde_json::Error> for io::Error returns the original I/O error for
writer failures. The helper uses that conversion, and injected body, newline,
and flush failures retain BrokenPipe. Serialization-specific errors remain
wrapped through that dependency conversion rather than stringified.

Four binary tests and40 cache-server CLI tests pass, including normal preflight,
evidence release/gate/doctor, and onboarding contracts. Strict all-target Clippy
and the cache-server binary build are running in session25957. The subprocess
failure fixture is Unix-specific; the writer failure matrix is platform-neutral.
This is JSON output completion, not whole-process output or durable publication
qualification. Text emitters still use print macros and need their own error
propagation cleanup; runtime creation still has three expect sites. No claim is
made that failed artifact writes preserve the previous destination atomically.


JSON validation completed: strict all-target Clippy and cache-server binary
build both pass. Formatting and diff checks pass. Production source shrinks by
39 lines after excluding the added failure-matrix test; duplicate emitters and
the file reopen path were removed. This batch is ready for grouped publication.


### Cache-service text output sibling

Extending the same closed-peer fixture to text mode exposed a separate normal-
error problem: evidence verify panics in std::io printing and exits101 instead
of returning an output error. The regression failed before the text refactor.
All binary stdout print/println calls are now checked write/writeln operations
on locked stdout, with explicit flushes. Evidence, optional gate doctor,
onboarding check/probe/render, and preflight owners propagate output errors.
JSON and text share the final output-error reporting boundary. Text formatting
strings and field order remain unchanged; no second renderer or output mode
was introduced. Diagnostic stderr/tracing failures are not qualified here.

The onboarding-render regression also verifies the written bundle survives an
output failure. Its first attempt omitted required origin/service/prefix CLI
arguments and exited2 before rendering; the fixture was corrected to include
those required arguments. It now exercises the intended post-write output
failure.41 CLI tests and four binary tests pass. Strict Clippy identified one
collapsible optional-doctor guard; it was collapsed without a lint suppression.
Final Clippy/build proof follows. The additional text error propagation adds
explicit ownership of output completion;46 unchecked stdout macros are removed.


Text validation completed: strict all-target Clippy and the cache-server binary
build pass after the guard cleanup. The41 CLI/four binary tests passed before
that equivalent guard simplification. Formatting and diff checks pass. Both
output commits are retained locally for grouped publication while PR head
b81b4fdda9c has active CI, including cache-service smoke and native workflow
checks. Native dispatch34148861825 still has its Windows smoke step running;
no competing dispatch or restart has been issued.


### Cache-service runtime resource failure

All three binary runtime creation sites (serve, check, onboarding probe) called
Runtime::new().expect. Locked Tokio1.52.1 Runtime::new delegates to the unchanged
multi-thread builder with all drivers enabled; the builder propagates
Driver::new errors, and the I/O driver propagates mio::Poll::new errors. These
recoverable errors now share one binary handler, preserve the I/O diagnostic,
and exit1 before block_on. Runtime flavor, workers, and driver settings remain
unchanged. This does not catch arbitrary dependency panics or prove every OS
thread-creation failure is recoverable.

An isolated Unix test child lowers its descriptor limit via sh, loads the test
binary, and then fills available descriptors before invoking onboarding probe.
Old code panics on the real "Too many open files" runtime error. New code
returns normally with exit1 and the startup diagnostic; no probe work starts.
An earlier standalone experiment filled descriptors before exec and aborted in
loading, so it was rejected as evidence; the retained test fills only after the
child binary has loaded. Host limits are unchanged. The test-only child marker
is not a production configuration or runtime override.

Five binary tests,41 cache-server CLI tests, strict all-target Clippy, and the
cache-server binary build pass. The two sibling handlers use the same fallible
runtime owner and immediate failure return; their normal CLI contracts pass.
No expect call remains in the binary production source. The Unix failure test
still needs Linux CI execution; this local proof ran on macOS.

### Published-head browser gate observation

PR head b81b4fdda9c has a failed Repository browser interactions job101837452434
in run34152047506.29 scenarios pass; the release-page scenario fails axe color
contrast on the delete tooltip (#171b21 foreground over #0f1319, ratio1.07).
The packages/repository tree object is exactly identical to origin/main
(4889f8fd8012415770e0d8fa50ef54f8dbb85ace). Its release test mocks /api/** and
Playwright serves Vite, so this observation does not exercise Rust cache-server
output/startup behavior. No repository-browser file, dependency, or CI workflow
has been changed by these source batches. The UI root cause has not been
reproduced on main; the failed gate remains recorded rather than relabeled green.
Native dispatch34148861825 still has a live Windows smoke step at this point.


### Git directory environment-path inspection

Shared discover_git_dir_from and the CLI config resolver both read GIT_DIR
through std::env::var. Rust's documented/source contract rejects non-Unicode
values there; var_os retains an OsString. Both public entry points already
accept nonempty overrides without validating existence, so silently ignoring
an opaque OS path can instead select a different repository or the .git
fallback. The shared code on origin/main has the same behavior.

The shared regression runs the actual helper in a child with an invalid-UTF8
GIT_DIR and fails before the fix: .git is returned instead of the exact override.
The first attempted native Git setup could not create that filename on this
Mac filesystem (Illegal byte sequence); it is not valid native-path evidence.
The retained test limits real Git init/rev-parse comparison to Linux, while the
OS-string override test runs on Unix without requiring that filename to exist.
A separate CLI config-resolver regression is compiling in session37928.

Callers inspected: shared ref_resolve's unqualified ref/HEAD helpers, the CLI
Git discover adapter, FetchConfig's default git_dir, and LFS publication's
common-directory comparison. The CLI config resolver is a sibling with a
different no-repository policy (error instead of .git fallback), so that policy
must stay distinct. current_worktree_root bypasses the override deliberately;
commondir text decoding is a separate remaining path-format concern. Other
GIT_DIR string reads found by workspace search are test environment restorers,
not additional production discovery implementations.


The config-resolver default-feature test invocation selected zero tests because
that module is gated by gix-config. The corrected feature-enabled regression
failed before the fix with a discovery error instead of the supplied override.
Both production readers now use var_os and keep their existing absent/empty
and no-repository policies. Six shared discovery tests pass with and without
facade, six ref-resolution tests pass, and strict facade all-target Clippy
passes. Strict rustdoc found a bare from_entries link and a redundant FindExt
link; both are corrected and strict docs pass. CLI feature consumer/adapter
checks and build remain running in session99689.

### Windows native smoke evidence retention

Dispatch34148861825 is terminal failed. Windows job101828929585 finished both
builds and successful mount-doctor checks by18:11:58UTC, then reached the60-minute
job limit at18:57:44UTC. This was not a compiler timeout. Retained artifacts
contain no mount.log because Invoke-Native collected all output in a variable
before teeing it after command completion. Cleanup reports a stale mount was
removed, which alone does not identify the mount-startup failure.

The wrapper now pipes native output directly through Tee-Object while retaining
LASTEXITCODE checking and working-directory restoration. All call sites invoke
the wrapper for side effects, not as a value-returning API. Microsoft documents
[Tee-Object](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/tee-object?view=powershell-7.6)
as forwarding and retaining pipeline output; the native exit-code check remains
separate from cmdlet pipeline success. The change removes script-level buffering
so future CI logs can expose progress before timeout. It does not fix or waive
the Windows native mount hang, extend its timeout, or establish crash-durable
logging. Local PowerShell is unavailable; all three script contracts and their
self-test pass, with actual PowerShell execution pending the next native run.


Git-dir validation completed: seven gix-config resolver tests, three CLI Git
adapter tests, and the feature-enabled CLI build pass. The CLI build retains
its known macOS debug-unwind linker warning. Attribute-cache constructor docs
also now describe the actual index-based collection instead of a recursive
filesystem walk; strict rustdoc passes after that prose correction. No public
signature, stored format, default feature, or dependency changed. Native Linux
non-UTF8 directory proof remains for CI; local tests prove the OS-string
boundary and all selected consumers, not a native Mac filesystem path that the
host filesystem cannot create.


### NFS control exchange deadlines

Owner: `crates/crab-vfs/src/nfs_control.rs`, reached by mount-control status,
refresh, switch, commit and shutdown, daemon readiness, and CLI background
readiness. Both TCP and Unix transports previously timed only `next_line`;
connect, write_all and flush ran outside that timeout. The same implementation
is present on inspected origin/main a371fb7d002. Two local socket regressions
with a listener that never reads and an 8 MiB synthetic commit request both
exceeded a three-second watchdog despite a 100 ms operation timeout.

The timeout now owns connection setup and one shared JSON exchange. TCP retains
its token envelope; Unix retains the plain request. Normal requests keep ten
seconds and commits thirty minutes. Each call owns its socket, so timeout drops
all transport state without leaving a reusable, partially written stream or
retrying an uncertain mutation. This reduces production code by 38 lines before
imports/constant renaming; added lines are regression coverage and documentation.
Tokio 1.52.1 timeout source documents cancellation by dropping the future and
its cooperative polling limit: this is not CPU preemption. Socket closure does
not cancel a helper operation already dispatched.

Existing real local control-server tests cover authenticated TCP and Unix
status/shutdown. New coverage proves both stalled writes time out and an
unanswered ping closes the TCP connection after sending the request. All 19
control tests and ten mount-control consumer tests pass. Strict all-target
Clippy initially rejected an expect in the test helper; changed it to the
module's existing unwrap convention without a new suppression. Final all-target Clippy,
strict docs and the no-default-features NFS CLI build pass. The CLI emits 13
feature-specific warnings and its macOS debug-unwind linker warning; this is
not a claim of warning-free CLI qualification. No default feature, dependency,
public signature, wire shape, or successful response changed.

Sibling follow-up: FUSE's reusable IpcClient::send_with_timeout also bounds only
the response read. It must invalidate or close a partially exchanged connection
before reuse, so transplanting a timeout alone would be incomplete. Qualify that
client's callers and cancellation contract next. Server-side connection tasks
and mutation completion after disconnect also remain separate ownership work.

Windows startup inspection found a minimum-four-worker CLI runtime, ruling out
a single-thread runtime explanation. Native mount commands and Windows
is_mounted still invoke synchronous subprocess output with no subprocess
deadline. This NFS control fix does not prove or claim to fix that native hang.
Native run34154546592 on6ae0432975e has started its feature gate; preserve that
run for the earlier macOS uncached-read and Windows streaming-log evidence.


### FUSE IPC exchange ownership

The NFS sibling audit led to `ipc_client::IpcClient::send_with_timeout`. On
origin/main a371fb7d002 it writes without a deadline and borrows persistent read
and write halves through a response-only timeout. Dropping the send future or
returning a timeout retains that socket in the client. The server uses ordered
newline JSON without response IDs, so preserving an incomplete exchange leaves
a later call exposed to its late response.

Two before-fix regressions fail: an 8 MiB synthetic commit cannot reach its
response-only deadline while the listener does not read, and cancellation after
the peer receives a ping leaves the connection open past the EOF watchdog.
The exchange now takes ownership of one buffered Unix stream and restores it
only after a complete, valid response. Its operation timeout covers write,
flush and response read. Cancellation, timeout and I/O/parse errors close the
socket; later calls return SendFailed with NotConnected, without auto-reconnect
or mutation retry. Valid application errors preserve connection reuse.

This is the same per-exchange ownership rule as NFS, adapted to the existing
reusable FUSE client. NFS still opens a fresh transport for every call. No new
transport wrapper or parallel request path is introduced. The small non-test growth
expresses temporary connection ownership and the unavailable-connection error;
most growth is regression coverage and documentation. The public error variant
is now RequestTimeout to describe its wider scope; workspace search found no
external match on the old ResponseTimeout variant, and this crate has
publish=false. Method signatures and wire shapes are unchanged.

Caller evidence: all eight IPC command helpers, mount_control's FUSE transport,
CLI mount status, and coordinator stop/status use send/send_with_timeout.
Coordinator lifecycle integration tests cover the same public client. The
existing client/server test now also sends after a complete application error,
preserving successful reuse. New local socket tests cover stalled writes,
caller cancellation, timeout, rejected reuse, malformed responses and EOF.
Tokio 1.52.1 Lines::next_line documents cancellation safety and preserves its
buffer; cancellation safety of one read alone does not identify which request
a late response belongs to. Owning the full buffered stream avoids losing
buffer state on successful reuse and drops it on an uncertain exchange.

Five focused client tests, strict fuse+nfs all-target Clippy/rustdoc, and the
CLI foreground-start/ping consumer integration pass. The default-feature CLI
build passes with the known macOS debug-unwind linker warning.
No native mount is used by these fixtures. Remaining boundaries include
connect/spawn deadline and stale-socket classification, IPC server child-task
ownership, mutation completion after disconnect, and error-source retention in
the CLI conversion. Native run34154546592 passed its feature gate; all four
platform/service jobs are running on the earlier6ae0432975e commit.


### Coordinator startup error and socket ownership

Owner: IPC connect-or-spawn policy in `crab-vfs/src/ipc_client.rs`. Both initial
connection and retry matches accepted every ConnectionFailed variant on
origin/main a371fb7d002, including invalid paths and permissions. The initial
branch also unlinked stale-looking sockets/empty files without holding the
daemon lock. Coordinator::start already acquires that lock before stale socket
cleanup; IpcServer runs while retaining the coordinator through Arc.

Before-fix regressions fail for an embedded-NUL path (its connection error is lost)
and preservation of a socket artifact (unlinked by the client). The path fixture
also covers a regular-file ancestor; the artifact fixture covers both an empty
file and an actual closed Unix listener. Tokio 1.52.1 UnixStream::connect passes
through pathname conversion, mio connect and socket errors; the fix classifies
the typed I/O kind, never strings. A shared private predicate permits only
NotFound and ConnectionRefused for both spawn and retry. Other failures return
the original ConnectionFailed with its I/O source.

Client-side file deletion is removed entirely. The coordinator holding the
lock remains the cleanup owner, preventing a client from unlinking a socket
that bound after its failed connection. A new owner regression holds the lock,
checks a second coordinator cannot remove a stale Unix socket, releases the
lock, and verifies successful startup performs cleanup. It passes. Two old
crate/CLI tests that required unlocked client deletion were retired, replaced
by client-preservation and lock-owner coverage. No inventory/baseline/assertion
was weakened to hide a failure; the removed expectation was the behavior fixed.

Callers remain try_ipc_mount and the direct coordinator-lifecycle consumer.
Production coordinator startup already composes Coordinator::start before
IpcServer::run_with_bound_hook; wire requests and CLI options are unchanged.
NFS control has its own endpoint probes and never uses this spawn helper.
The retry budget still does not bound an individual connect; that limit is now
stated accurately rather than claiming a five-second total startup deadline.
Further connect timing, duplicate server startup, cleanup error handling, and
child-process/task ownership remain open.

All 26 focused IPC client tests and the new coordinator lock-owner test pass.
Strict fuse+nfs lint/docs, the running-coordinator CLI consumer, and default CLI
build pass. The CLI retains its macOS debug-unwind linker warning. Native
run34154546592 on6ae0432975e has passed both Linux jobs and the macOS native
smoke; Windows remains running. These native results apply to that earlier
commit, not the later control/startup changes.


### macOS native evidence verified

Downloaded nfs-smoke-macos-34154546592-1 into the workspace evidence directory.
The retained report identifies 6ae0432975e872c8e8261fdc80e49dfa344ae3b7; the
current report verifier passes with required artifacts and that exact commit.
The uncached benchmark records 31 lease hits, one miss, 32 READ RPCs, and 8 MiB
returned to its caller. NFS returned 32 MiB, so the recorded amplification is 4.0.
This proves native lease reuse for the corrected workload; it does not claim
an efficient client read pattern or comparability with historical cached runs.
Both Linux jobs also pass; Windows remains live in run 34154546592. Later control
and coordinator-startup commits are not included in that native head.

### Cache eviction removal failures

Inspection moved to cache-server eviction and found a concrete accounting gap.
On origin/main a371fb7d002, CacheStore::remove_object logs non-NotFound unlink
errors but still deletes SQLite metadata, subtracts tracked bytes and records
an eviction. A regression replacing a cached file with a nonempty directory
fails: the exact-key path reports one eviction and 18 freed bytes despite the
filesystem refusing removal.

The canonical indexed-file removal helper now returns a typed error with its
path and I/O cause before metadata or counters change. It is shared by normal
eviction and invalid-object cleanup, replacing their separate unlink matches.
Confirmed absence remains idempotent. The new error type is private; there is
no public signature, dependency, persistent format, threshold or wire-shape
change. Existing unindexed startup cleanup already returns deletion errors;
it has a separate directory-removal policy and is not changed here.

Caller map: exact-key and filtered admin eviction return the error as HTTP 500;
budget eviction is used by startup and the periodic evictor; emergency eviction
is used by handlers' cache-budget admission paths. All four store entry points call
remove_object. Failed cleanup cannot claim available cache capacity. Corruption
repair also uses the shared unlink helper and keeps its metadata on failure.
The mutation lock continues to serialize deletion and accounting with writes.

All 42 cache-store tests, six evictor tests, and three admin-eviction loopback
integration tests pass. The regression checks exact, budget, emergency and
filtered eviction; it retains metadata, accounted bytes, counters and the
blocking directory, and verifies the underlying I/O source. The HTTP regression
uses both filtered and exact admin routes and observes 500 plus unchanged stats.
Successful canonical pack eviction remains covered. Strict all-target Clippy,
rustdoc and the cache-server binary build pass.

Remaining eviction qualification includes synchronous disk/SQLite work on async
workers and its shutdown ownership, zero-byte/missing-candidate reporting, and
aggregate candidate memory. Moving eviction to spawn_blocking must also change
shutdown: aborting its outer task would otherwise detach admitted blocking work.
These findings are follow-up work, not claims of completed eviction concurrency
or full cache-service qualification.


### Eviction count and byte contracts

The next accounting check reproduced an empty-object defect in evict_key:
deleting a zero-byte entry returned evicted_count=0 because the method treated
zero freed bytes as absence. This is also present on inspected origin/main
(a371fb7d002). The new regression fails with count 0 instead of 1 before the fix.

CacheStore::remove_object now returns the existing EvictStats rather than u64.
It determines both count and bytes under the mutation lock: an existing empty
entry returns (1,0), an absent entry (0,0). Exact eviction returns that result;
budget, emergency and filtered paths sum actual counts instead of incrementing
for every candidate. A candidate already removed by another caller therefore
contributes no count. This is a Rust return-type change, not a compatibility
wrapper. Workspace search found raw-removal consumers only in this crate's
methods and tests; crab-remote-git's similarly named method has a different
owner. The HTTP JSON fields remain unchanged.

The empty-object store test verifies deletion once, repeated absence, and
agreement with lifetime eviction counters. A loopback test PUTs an empty pack
and invokes exact admin eviction twice, observing counts 1 then 0 and zero bytes
in both HTTP 200 responses. Existing missing-object and nonempty-removal tests
now inspect both result fields. Emergency eviction docs now describe its actual
pre-admission caller and snapshot selection; concurrent removals can reduce
the achieved count. This does not promise a fixed count under concurrency.

All 43 cache-store tests, six evictor tests and four HTTP admin tests pass, as do
strict all-target Clippy, rustdoc and the cache-server binary build. The
crab-cache-store test-target compilation also passes with its cache-server
fixture dependency. Scheduling/shutdown and aggregate candidate-memory work
remain open; the all-crate objective is not complete.


### Background eviction scheduling and drain

The periodic evictor called synchronous CacheStore::evict_to_budget directly
inside a Tokio task, also present on origin/main a371fb7d002. A current-thread
runtime regression holds the real cache mutation lock from a bounded helper
thread. Before the fix, a heartbeat cannot progress and the three-second
watchdog releases the lock; the test fails for blocking the async executor.

Each admitted periodic batch now runs in spawn_blocking, with one outstanding
batch per evictor. Shutdown notifies the loop and awaits its join instead of
aborting it. The loop awaits the blocking batch before observing the shutdown
signal, and shutdown has priority over a simultaneously queued nudge. Ordinary
cache errors retain warning-and-retry-on-next-wake behavior; a worker JoinError
stops the maintenance loop. Dropping an unused handle still does not stop it.
No new dependency, feature, configuration option or public signature is added.

Tokio 1.52.1 documents that a started blocking task cannot be aborted. Its Notify
implementation retains notify_one permits when there is no waiter and transfers
an unconsumed notification when a waiter is dropped. Thus shutdown notification
survives an in-flight batch or a cancelled select branch. Awaiting the parent
loop retains cache ownership until its blocking mutation finishes.

The regression now proves the heartbeat progresses while the mutation lock is
held, shutdown remains pending until release, and eviction is complete when
shutdown returns. A second current-thread test queues a nudge before shutdown
and verifies no batch is run. All 44 cache-store, seven evictor, four service-owner
and two HTTP eviction tests pass, along with strict all-target Clippy,
rustdoc and the cache-server binary build.

Consumer: PreparedServer::shutdown awaits EvictorHandle::shutdown after serving;
the loopback service fixture does likewise. Sibling paths remain explicit work:
startup, admin and emergency-admission eviction still call synchronous storage
operations and need their own cancellation/ownership review before offloading.
This batch does not qualify abandoned server futures, whole-process shutdown,
or aggregate candidate memory. The added production state is one shutdown
notification and the blocking-task join boundary; no parallel eviction API or
worker abstraction was introduced.


### Request-triggered cache mutation: cancellation evidence

Follow-up audit after `374410f6dc1`; implementation remains open.

- Entry/owner: `state.rs::build_router` applies Tower's 300-second timeout
  inside a 200-request concurrency limit. `PreparedServer::shutdown` joins
  only the background evictor.
- Callers: `handlers.rs::admin_evict` invokes exact/filter eviction directly;
  `write_file_backed_object` and `commit_origin_fill_or_read_temp` invoke
  emergency eviction and synchronous cache publication. Startup recovery and
  eviction are a separate pre-listener path in `prepare_server`.
- Dependency evidence: Tower 0.5 timeout `ResponseFuture::poll` polls the
  response before its timer. A synchronous blocked poll cannot be preempted
  by that timer. Tokio 1.52 documents that an admitted `spawn_blocking` task
  cannot be aborted; dropping its join handle does not drain it.
- Best-fix constraint: do not replace these calls with untracked blocking
  jobs. Introduce service-owned admission/drain only after tracing staged
  temp-file ownership, request cancellation, and TLS forced drain together.
  Background eviction's joined worker is evidence for that one owner only.
- Required proof: hold the real mutation lock while a request executes;
  demonstrate a concurrent health request stays responsive, cancel the
  mutation request, and prove shutdown waits for admitted work. Cover both
  admin removal and upload/origin publication; retain current error responses
  and temp-file cleanup behavior. No claim of completion from source review.
- Qualification handle: cache-service run `34159002206` is active on
  `374410f6dc17a684483fc114a2c9a32e6ce90874` (manifest validation at inspection).
  Preserve this run rather than cancelling it with another publication.


### Temp-file commit ownership simplification

- `CacheStore::put_unverified_temp_path_recoverable` now moves its `TempPath`
  directly into every early-return error or into `persist`. Removed the
  optional slot, extraction closure, and unreachable missing-path error.
  This represents pre/post-persistence ownership in Rust rather than a
  runtime sentinel; public signatures and recoverable errors are unchanged.
- Evidence map: streamed PUT and origin fill are the HTTP callers;
  `put_unverified` and its non-recoverable wrapper use the same commit path.
  `put_budget_plan`, the mutation lock, tempfile persistence, and SQLite
  publication remain ordered as before. Inspected `origin/main` has the
  same optional-slot implementation. Tempfile's `TempPath::persist` consumes
  ownership and returns it as `PathPersistError.path` on failure.
- Existing tests exercise successful file/metadata publication, rejected
  replacement preserving the old object, recoverable budget failure returning
  the staged bytes, cleanup on drop, and the origin response recovery caller.
  All 44 cache-store tests and the focused handler recovery test pass;
  strict all-target Clippy, strict rustdoc, the cache-server binary build,
  and formatting/diff checks pass.
- This is the simpler fix for the redundant ownership state, not completion
  of request cancellation work. Service-owned blocking admission/drain and
  upload/origin sibling qualification remain open as described above.
