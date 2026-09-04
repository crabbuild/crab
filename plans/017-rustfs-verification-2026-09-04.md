# Local RustFS verification — candidate 31fdcb2

Date: 2026-09-04 UTC. Companion to `plans/017-post-launch-product-hardening.md`.

## Verdict and scope

Verification is **not entirely green**. Git 2.30.9 reproducibly fails a remote
read-only assertion. Three unit failures reproduce locally; broader CI has seven
failures. Keep these blockers visible; no product code, expectations, or baselines
were changed for this verification.

The 1,000-commit Kubernetes replay **passed**, including final clone/byte checks
and the full-profile report validator. The run completed in 35 minutes 38 seconds
(02:27:09–03:02:47 UTC); 1,145 commands and 22 checks, no failed check.

This is functional macOS/native-RustFS evidence, not Linux/Windows or production
S3/GCS/Azure qualification, a controlled performance comparison, or completion of
every phase in Plan 017. Independent jobs overlapped on the host. The replay
runner's default `valid_for_comparison: true` does not establish an idle host;
do not use this run for performance acceptance or baseline comparisons.

## Identity and isolation

- Candidate: `31fdcb29235d82929840ee2dc7e969c1720a5290`.
- Release executable SHA-256:
  `ae8f6de1190390f8f07789c54dedf8819b297ea713e256232f05491c367687b1`.
- Binary metadata: Crab 1.0.1, build `2026-09-04 01:58:25 UTC`, Git SHA `31fdcb2`.
- Kubernetes: `160bd16d98b7f688ce4f3b5ab0c5e4c045f36233`, 140,777 reachable
  commits. Read-only frozen checkout; source HEAD and status checked by runners.
- Replay base: `ec8eaa5789ffe26b3642911e445d12330843c5e4` (`HEAD~1000`).
  Replay advances the disposable remote through original first-parent commit
  OIDs; it does not rewrite Kubernetes commits. This imports reachable `main`
  history, not every upstream branch or tag. Separate fixtures exercise tags
  and multi-ref behavior.
- Store: native RustFS `1.0.0-beta.8`, macOS arm64, local port 9000. No Docker
  reset, existing-bucket migration, or bucket-wide GC performed.
- New run-specific buckets isolate publication/coordination domains. Main replay
  bucket: `crab-verify-31fdcb2-20260904-0228`. Other bucket identities are retained
  in each report. Evidence and relevant worktrees retained; do not reuse run IDs.
- Existing generated changes in `packages/web/.source/browser.ts` and
  `packages/web/.source/server.ts` were left untouched. Protocol reports correctly
  record a dirty checkout, although their binary/source revisions match.

## Completed local results

Each run name below has suffix `-31fdcb2-20260904`. JSON reports live in
`artifacts/report.json` under the run's qualification directory, except the two
standalone LFS reports, which use `report.json`.

| Run | Result | Evidence |
| --- | --- | --- |
| `k8s-replay` | Pass | Initial import plus 1,000 original commits; full cold/warm, blob:none, shallow depths 1/10/100/1000; four incremental fetch checkpoints; final refs and 1,000 sampled object bytes match |
| `k8s-managed` | Pass | 63 commands, 48 checks; both Crab add/push and Git add/push; duplicate 65 MiB payloads, clone, hydrate, dehydrate, rehydrate, deduplication |
| `k8s-managed-bytes` | Pass | 1,000 sampled Git objects per clone; type, size, and raw content match; 28,169,810-byte batch stream per clone |
| `k8s-managed-integrity` | Pass | 13 checks; both clones pass offline full Git fsck and authoritative Crab fsck; exact managed-file SHA-256 matches |
| `git-current` | Pass | Git 2.50.1 (Apple Git-155); 482 commands, 139 checks |
| `git-2.40.4` | Pass | 482 commands, 139 checks |
| `git-2.45.4` | Pass | 482 commands, 139 checks |
| `git-2.30.9`, `git-2.30.9-repeat` | Fail | Same filter read-only assertion in two fresh buckets; each stops at check 59 |
| `git-2.30.9-collect` | Fail | 447 commands, 119 checks pass, one fails; remaining original cases executed without clearing the failure |
| `git-2.30.9-pinned` | Fail | Selected Git also pinned on PATH for Crab's child processes; same sole assertion failure, 119 checks pass |
| `git-2.40.4-pinned`, `git-2.45.4-pinned` | Pass | Selected Git also pinned on PATH; each passes 139 checks |
| `canary` | Pass | 326 commands, 132 checks; corruption/missing-object failures, concurrency, atomic rejection, ref lifecycle, shallow fetch, managed-file workflows |
| `gc-upgrade` | Pass | 16 commands, 20 checks; explicit fence migration, idempotence, tagged old-writer refusal, refs/fence preserved after refusal |
| `cold-lfs` | Pass | 25 checks; eager/lazy/selective clone, exact bytes, denied content, terminal error output, broken filter-output pipe |
| `mirror_lfs_admission` | Pass | 14 checks; installed hooks reject admission before payload upload, preserve both remotes, retry and hydrate |
| `mirror_readonly` | Pass | 12 checks; equal/ahead/diverged state, cold/incomplete cache, missing/corrupt objects, cancellation; zero attempted remote writes |
| `mirror_receipt` | Pass | 11 checks; kill publisher after accepted marker, tagged-client compaction, exact receipt recovery without republishing |
| `hydration-callers` | Pass | 24 checks; cold clone profile and deferred post-pull hydration |
| `lfs-cache-mutation` | Pass | Seven checks; native filter, LFS filter, standalone smudge reject cache truncation during output |
| `lfs-standalone` | Pass | 100 generated objects over 10 commits/10 paths; Crab and Git LFS push/fetch/fsck/checkout; exact final bytes |

Both managed Kubernetes clones additionally passed `git fsck --full --strict`
with replacement objects and network protocols disabled. The replay's retained
full and incremental clones also passed offline strict fsck. Full cold/warm and
incremental clones have full-fsck evidence; the large-repository harness does not
run separate post-clone fsck on its temporary shallow/filtered clones. The real-Git
protocol matrix independently exercises shallow/partial-clone integrity.

Replay final state: advertised HEAD/main, full-clone HEAD, and incremental
origin/main equal the pinned source revision. The 1,000-object batch stream is
28,169,810 bytes with SHA-256
`5168883b61da07077b6a4214f7e354241e0f61b2c19342a9e722bcfb3fd29572`
on source and clone. Source checkout remains clean and unchanged.
Final metadata has two active packs (1,255,072,162 bytes), two commit-graph layers
covering 140,777 commits, current locator/visibility generation 15, valid generation
receipt, and no repair required. Bucket-wide discovery remains incomplete and
destructive bucket GC remains disabled; repository registry proof is complete.

Observed timings only: initial import 124.696 s; full cold clone 66.656 s; warm
clone 28.891 s. The recorded push distribution, including seed import, has median
1.117 s and p95 1.563 s. Final maintenance took 31 passes / 202.051 s. No SLO or
regression conclusion follows from these overlapping-host measurements.

`crab/scripts/verify-large-repo-rustfs-report.py verify` accepted the full-profile
report without `--allow-smoke`. Its saved `artifacts/verification.json` proves the
report contract, not the unrequested team-load/cache-service or paired-performance
gates. Correctness fingerprint:
`ebc684c00fe5d8d1f6d56fb3f00954d459b6806f91f65e692fdff10530b1486e`.

The declared 19 operation families in
`crab/docs/architecture/git-capability-matrix.json` have all required check names
present and passing in the three green Git versions' completed reports and the
2.30.9 collecting reports, including the fully PATH-pinned repeats.
The additional failed read-only check still makes 2.30.9's overall result **fail**.
Git 2.30.9 lacks the client object-type filter syntax; its check count is lower.
“19 operations covered” does not mean every possible Git command was tested.

Git 2.30.9, 2.40.4, and 2.45.4 were built from the official kernel.org release
archives into isolated local prefixes, with Tcl/Tk and gettext disabled. Final
older-version repeats pin both `--git-bin` and the selected installation's `bin`
directory at the front of PATH; direct Crab invocations therefore launch the same
Git version internally. System Git was not replaced. Earlier reports remain
unchanged. The
collecting driver catches only the final reproduced filter-inventory assertion,
continues remaining cases, and restores terminal failure whenever a check failed.

## Failures and release-evidence gaps

### 1. Native fetch admission changes canonical metadata

In all four Git 2.30.9 runs, `filter-matrix-read-only-remote` fails. The first
run's canonical object count grows from 74 to 89, and bytes from 8,798,331 to
8,812,952. This is not just the generated response-pack cache: retained inventories
show journal frontier, manifest history, pack/shard index, visibility, and object
catalog writes. Clone logs explicitly record journal compaction and locator/
visibility repair during upload-pack admission.

Evidence path: `crab/src/git/upload_pack_wire.rs:934` calls
`crab/src/git/push.rs:5802` when active journal transactions need compaction.
The same reader-compaction path exists in inspected `origin/main` revision
`e26d139038414dcb8ddc591712d726f052547131`; this observation alone does not prove
a new PR regression or a Git-specific corruption bug. The matrix's read-only
assertion and repair-on-read behavior need an explicit contract resolution.
Do not merely exempt more canonical objects to turn the test green.

### 2. Unit/full-suite failures remain

[Main CI run](https://github.com/crabbuild/crab/actions/runs/33827702377): the
reported Crab unit batch has 4,091 passes, seven failures, and three ignored tests.
[Protocol CI run](https://github.com/crabbuild/crab/actions/runs/33827702266)
also fails; its real-Git version matrix and released-shape lifecycle were skipped.
Its macOS and Windows protocol-contract jobs passed; those are narrower evidence.

Focused local rerun: 18 tests, 15 pass, three fail:

- `crab/src/cmd/clone.rs:1745`: replica shard-sync fixture has no canonical layout.
- `crab/src/cmd/version.rs:271`: schema count is 63; assertion expects 59.
- `crab/src/git/push.rs:24808`: materialized ref-journal HEAD does not resolve.

Four `lfs::recent` tests encounter poisoned locks in full CI but pass in the
focused local run. This does not dismiss CI: investigate shared test-state
isolation and rerun the complete affected batch.

Additional fresh local proof: all 36 upload-pack wire unit tests and all 52
integration/transcript tests pass (42 remote-helper, six v2 transport, four
pre-push input). Linker emitted a large unwind-table warning.

### 3. Local success is not a release gate

The strict Git report verifier rejects the otherwise passing current-Git report
because source-cleanliness is false. No provenance flag was rewritten to bypass
that requirement. Clean-source, exact-artifact CI reruns remain required.
This session does not establish team-load/cache-service gates, paired performance
SLOs, production cloud-provider contracts, or full Linux/Windows live compatibility.

## Follow-up phases and acceptance

1. **Resolve the three reproducible unit failures and suite isolation.** Audit
   fixture contracts and registered schemas before changing anything. Preserve
   production rejection of missing layout and unresolved journal HEAD. Acceptance:
   focused tests and the complete selected CI batch pass; no poisoned-lock cascade;
   no suppressed or ignored failures.
2. **Resolve native read-only versus repair ownership.** Establish which phase
   owns journal/catalog readiness and whether fetch credentials must permit those
   writes. Keep read-only checks meaningful and qualification setup explicit.
   Acceptance: all four Git versions pass fresh full reports, including the
   canonical-inventory assertion; exact refs/bytes remain unchanged by reads;
   any permitted maintenance is qualified separately at its intended boundary.
3. **Requalify the final artifact.** Build from a clean checkout, use unique
   buckets, rerun this matrix and Kubernetes replay, then required Linux/provider,
   team-load/cache-service, and paired-idle-host performance gates. Acceptance:
   required report validators pass without exceptions and required CI jobs run
   rather than skip. Do not label an unqualified platform/provider supported.

## Evidence location and reproduction

Qualification root: the workspace-volume `Github/crab-qualification` directory.
`verification-index-31fdcb2-20260904.json` indexes 22 retained reports, SHA-256
digests, operation coverage, and exact-head CI state. It marks the session as
unsuitable for controlled performance comparison.

Canonical runners:

- `crab/scripts/e2e/run_large_repo_rustfs.py`: use pinned Kubernetes source,
  `--replay-count 1000 --sample-size 1000 --retain-worktrees`, new run/bucket names,
  the exact release binary, native RustFS version, and local endpoint.
- `crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py`: run default canary, then
  `--source` Kubernetes with `--size-mib 65`; migration uses
  `--only-gc-fence-upgrade --rollback-crab-bin` with the tagged v1.0.1 executable.
- `crab/scripts/e2e/run_protocol_v2_partial_clone_rustfs_smoke.py`: use
  `--mirror-reconciliation`, each pinned `--git-bin` and matching PATH prefix, tagged rollback binary,
  `--rollback-crab-tag v1.0.1`, and a unique bucket/run for every attempt.
- Use bundled Python with `PYTHONDONTWRITEBYTECODE=1`; the system SQLite build
  previously failed read-only WAL access. Never reuse an existing report directory.

All Rust compilation used the dedicated `crab-7129` target directory under the
workspace-volume `crabbuild-target` root. No changes were committed or pushed
during this verification; this note is the only intended repository edit.

## Approved CI fixture corrections — 2026-09-04

The preceding results remain the historical `31fdcb2` evidence, not a green
release verdict. The user subsequently approved four fixture/inventory corrections
and rerunning PR #148. No production validation, baseline exemption, test skip,
or poisoned-lock recovery was added.

| Surface | Cause and correction | Contract / acceptance |
| --- | --- | --- |
| Clone replica shard sync | Initialize the canonical layout before publishing either fixture manifest | `run_post_clone_shard_sync_with_selector` calls shared post-fetch snapshot admission; the existing primary-versus-replica cache assertions remain unchanged |
| Version registry | Add the four implemented mirror JSON/JSONL schemas to the expected inventory; 59 becomes 63 | `MirrorCommandOutcome` emits these names at version 1.0; removing production registry entries would misreport the command contract |
| Under-lock journal refresh | The fixture's first published ref becomes HEAD rather than leaving HEAD aimed at absent `main` | Journal materialization rejects a nonempty ref map with unresolved HEAD; the test still requires unchanged manifest ETag, refreshed sibling state, and invalidation of the prior receipt |
| Post-push protocol admission | Clone the pushed branch into a fresh ODB and verify its exact tip before the filter matrix | Git 2.30.9 `builtin/fetch.c::check_exist_and_connected` / `fetch_refs` can skip transfer into the writer; the canonical count/byte equality assertion is unchanged |

The fixture changes are preferable to relaxing production validation: all three
Rust failures violate their existing owner contracts. The journal test panic
also poisons `GIT_DIR_MUTEX`, which explains the four later LFS lock failures;
the clone fixture holds the separate `CACHE_DIR_MUTEX`. Full-suite CI remains
necessary to prove the cascade is gone.

Read-admission compaction also exists on inspected `origin/main` (`e26d139`).
The corrected protocol setup exercises that existing path deliberately, instead
of allowing the first matrix clone to perform it after the read-only baseline.
`crab/docs/architecture/git-protocol-v2.md` now states that qualification boundary.
No canonical metadata keys were excluded from the inventory.

Acceptance: the focused Rust batch passes together; the fresh Git 2.30.9 lifecycle
passes including `post-push-read-admission` and `filter-matrix-read-only-remote`;
then the exact pushed head passes the full required CI and real-Git matrix.
Skipped infrastructure/provider gates must still be reported as unqualified.

Fresh local protocol result: `git-230-approved-fixtures-20260904-0420` passed
448 commands and all 120 checks with Git 2.30.9 pinned on PATH and `--git-bin`.
The frozen `31fdcb2` executable has the same SHA-256 recorded above, isolating
the harness correction from production changes. The new bucket is
`crab-ci-fixtures-20260904-0420`; the original failed reports remain untouched.
Post-push admission verifies the exact new tip. Both filter-matrix inventories
contain 89 canonical objects / 8,812,942 bytes; only the separately accounted
generated response-pack cache grows. This dirty-source local run does not replace
the clean-source exact-artifact CI gate.

Focused Rust proof: all 19 selected tests pass together (both under-lock refresh
tests, replica shard sync, schema inventory, and all 15 `lfs::recent` tests).
`cargo fmt --all` / format check, `git diff --check`, the executable capability
matrix verifier, its ten unit tests, and both protocol log-path tests pass.
Full CI results must be attached to the pushed revision rather than inferred
from these focused checks.

## Hosted evidence audit and workflow correction

At `c955248`, the full Crab library batch passes 4,098 tests with three existing
ignored tests; the binary batch passes 52 tests and fails the public-help
inventory test. Its duplicated internal-command list omits the already-hidden
`mirror-pre-push` hook command. Correcting this additional inventory still needs
explicit approval; neither the command's visibility nor the test was changed.

The protocol workflow `33836634284` passes all four hosted Git lifecycles, the
released-shape lifecycle, and both platform protocol-contract jobs. The GC
race/crash/scale gate also passes. Hosted evidence uses GitHub's test merge
revision `bcb952b5280f22ca8e601261b1fa24056be4ada1`, which contains `c955248`,
not a binary reporting the PR head directly. The Linux executable SHA-256 is
`c37e96798e619ce48548dcd7cd453c9384c91fe11a5c14b3757770f0bc24dba6`.

These passes were not sufficient release evidence. Retained reports show
`crab_source_checkout_clean: false`: fixture files under `.ci/` dirty the source
checkout. PR lifecycle jobs also omitted the pinned rollback binary and strict
report verifier, and selected Git was not on PATH for Crab's child processes.
Do not reinterpret these reports as clean-source/full-version qualification.

The workflow correction keeps RustFS fixture roots outside the checkout, exports
the selected Git's directory on PATH, reuses the existing checksum-pinned v1.0.1
binary for both rollback and fence migration, and invokes the existing strict
capability verifier in PR CI. The RustFS release jobs receive the same root/PATH
correction; their verifier and rejection rules remain unchanged. Packaged smoke
binaries also move outside the checkout. No ignore rule or provenance flag is
relaxed. Cloud/platform preview jobs remain separate unqualified surfaces.

Roots are set in runtime steps through `GITHUB_ENV`, where `RUNNER_TEMP` is
available; the runner context is not admitted in job-level `env` expressions.
See [GitHub context availability](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability).
Acceptance: all four fresh PR reports pass the strict verifier, show a clean
source checkout, match the packaged binary and selected Git, retain pinned
rollback evidence, and preserve every required operation check.

Additional local exact-head proof: `git-{230,240,245,current}-c955248-20260904-0433`
all pass (537 checks / 1,897 commands total), with release executable SHA-256
`3e5d2361b1be9e4912e66f0de5120dde51e69bbb548aec80fa98a61f36a75e8f`.
The three earlier `git-{240,245,current}-approved-fixtures-20260904-0428` attempts
correctly failed provenance because they used the prior binary after the source
commit changed; they are retained and excluded from passing counts.

Host-enforcement audit remains blocked: GitHub reports no classic protection or
effective branch rules on `main`; the repository's `Main` ruleset is disabled.
No protection settings were changed. The exact-candidate merge-rejection
criterion requires explicit host/branch/check authorization and a controlled
missing-data candidate; passing local mirror CI does not prove host enforcement.

The workspace CI test command now uses Cargo's `--no-fail-fast` so a failing
binary test cannot hide failures in later test executables. This does not
permit a green result with failures. An isolated two-executable Cargo probe
ran the second executable after a deliberate failure and still exited 101.
Workflow lint passes; no test expectations or ignored-test lists were changed.

## Bucket-only Git workflow follow-up

The helper now accepts Git's `dry-run` option and returns preview statuses
before staging, protected-write preparation, locking, uploads, audit receipts,
or ref publication. Git retains its client-side ancestry and lease checks;
previews do not promise future write authorization or race outcomes. The
existing `crab push --dry-run` policy remains unchanged. No storage format or
server dependency was added.

The executable matrix now requires 25 operation groups, adding push previews,
pull/rebase, notes, linked worktrees, recursive submodules, and bare mirror
clones. Submodule tests permit only the Crab transport, invocation-locally,
following Git's explicit-trust policy. They run after existing cold-transfer
measurements so their reads cannot warm the measurement baseline.

Local RustFS proof, using release binary SHA-256
`f57639f3a75b262709cffef1b07907da2b5b81de1ae2c88b52f9ac1e8358284c`:

| Retained run | Commands | Checks | Result |
| --- | ---: | ---: | --- |
| `bucket-workflows-230-20260904-0550` | 495 | 128 | passed |
| `bucket-workflows-240-20260904-0550` | 530 | 147 | passed |
| `bucket-workflows-245-20260904-0550` | 530 | 147 | passed |
| `bucket-workflows-current-20260904-0550` | 530 | 147 | passed |

Reports live beneath the existing workspace qualification root, each at
`<run>/artifacts/report.json`. All four include the pinned v1.0.1 rollback and
mirror reconciliation checks: 2,085 commands and 569 checks total. They are
macOS development-worktree evidence, not clean-source Linux release proof.
The binary reports parent revision `536134f`; the helper changes were uncommitted
when it was built, and each report explicitly retains the dirty-source flag.

`k8s-dry-run-20260904-0551` separately previews a new branch from Kubernetes
tip `160bd16d98b7f688ce4f3b5ab0c5e4c045f36233` (140,777 reachable commits).
The preview took 254 ms in this observation; all 8,623 bucket objects retained
their keys, sizes, and ETags. An isolated shared clone kept the input checkout
unchanged. This is dry-run correctness evidence, not a repeated large-repository
upload benchmark or a latency SLA.

Additional proof: 134 remote-helper tests pass with CI's existing 8 MiB test
stack; 24 dry-run-related Rust tests, 12 Python verifier/log tests, release build,
format checks, and scoped Clippy correctness/suspicious gates pass. Clippy still
reports existing warning-level findings. A direct test-binary invocation without
CI's stack setting aborted in an existing large async fixture; its correctly
configured rerun passes without modifying that test.

Failed exploratory reports remain intact: `dry-run-before-20260904-0535`
reproduces Git's unsupported-option failure; early `bucket-workflows-*` attempts
exposed a read-lease observation-order mistake, a missing harness log label, and
cache warming before the cold-transfer assertion. Only new harness wiring was
corrected; the bucket identity and existing performance assertions were retained.

At parent `536134f`, all four hosted Git compatibility jobs passed the strict
clean-source evidence verifier, and macOS/Windows protocol contracts passed.
Workspace CI completed all test executables and still fails the previously
identified hidden-command help inventory test. That separate test correction
remains unapproved. The new operation groups still require fresh hosted evidence;
neither this follow-up nor the older Kubernetes reports establishes support for
every Git flag, production provider, platform, or server-side policy feature.
