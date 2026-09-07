# Rust crate source quality

Status: in progress. Scope: all 21 crates under `crates/`.

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
| crab-xet | Range arithmetic, malformed payloads, reconstruction checks | Pending |
| crab-storage | Credential diagnostics; retry/error classification remains | Diagnostic slice verified |
| crab-metadata | Reader/writer closure and feature boundaries | Pending |
| crab-staging | Flush-before-publication and recovery ownership | Pending |
| crab-coordination | Renewal cancellation and lock release ownership | Pending |
| crab-lfs | Upload I/O causes; integrity and lock ownership remain | Upload diagnostic slice verified |
| crab-cache | Cache keys, token/path diagnostics, cache invalidation | Pending |
| crab-cache-store | Origin authority, corrupt-cache repair, range validation | Pending |
| crab-read | Hydration integrity and error propagation to consumers | Pending |
| crab-write | Commit uncertainty and generation cleanup | Pending |
| crab-remote-git | Operation finish/shutdown and range error propagation | Pending |
| crab-vfs | Mount teardown and shared FUSE/NFS lifecycle invariants | Pending |
| crab-auth | Credential Debug output; token-cache lifecycle remains | Diagnostic slice verified |
| crab-auth-store | Credential refresh and storage adapter error boundaries | Pending |
| crab-auth-server | Receive cleanup and error-to-response mapping | Pending |
| crab-cache-server | Eviction concurrency, shutdown, request validation | Pending |
| crab-http-server | Request validation, embedded assets, service errors | Pending |
| crab-workflow | Execution cancellation, cache identity, resume state | Pending |

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
