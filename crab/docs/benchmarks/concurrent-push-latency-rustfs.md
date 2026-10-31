# Incremental push latency on RustFS

Measured October 30–31, 2026 against `origin/crab-v2` at `07e92219964`; candidate implementation `328694a48e5`.

This change removes three Git subprocesses from common direct-parent pushes and reuses unchanged, version-bound ref-head bodies within a single discovery retry loop. Native pack fsck, conditional ref publication, stable before/after listings and fresh multi-ref activation checks remain active.

## Workload and method

- curl at `70c084a0123b468fe7078e3fc72c78e1b66394b6`: 40,069 commits, 4,565 files, 18.5 MB current tree, 288.7 MiB source pack.
- Mature RustFS snapshot with 87 refs and 4,630 prior successful pushes. Every trial starts from an offline copy of that same snapshot and pushes the same prepared commit chains.
- One new 4 KiB ordinary blob and commit per push, to distinct flat ref names. Ref creation, commit preparation and warmup are excluded.
- Full process startup through acknowledged durable publication and exit is timed. CLI uses `crab push --json origin <oid>:<ref>`; normal Git uses `git push origin <oid>:<ref>` through the helper.
- Baseline/candidate/candidate/baseline order, separate per-agent repositories and caches per trial. Each trial measures 16 synchronized rounds at 1 agent, 8 at 10, and 6 at 50, after one warmup round per case.
- Apple M2 Max, 12 cores, 32 GiB, shared host; native RustFS 1.0.0-rc.1 on loopback HTTP and APFS workspace storage. No request proxy is used for timing. Task compilation finished before measurement.
- Both binaries use release optimization and `--no-default-features --features simd-accel,gix-pathmatch,gix-transport` with Rust 1.97.0. Results do not qualify a default-feature release or production capacity.

## Paired results

Medians pool both trials per variant. Throughput is successful measured pushes divided by the sum of synchronized-wave wall times; setup and between-wave time are excluded. Background load and more than twofold variation between repeats prevent a reliable causal speedup claim. The final results are mixed, including regressions; they do not establish an overall end-to-end performance improvement.

| Client | Agents | Samples per variant | Baseline p50 | Candidate p50 | p50 reduction | Baseline push/s | Candidate push/s | Throughput change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cli | 1 | 32 | 247.6 ms | 268.0 ms | -8.2% | 2.48 | 3.60 | +45.2% |
| cli | 10 | 160 | 774.7 ms | 681.2 ms | +12.1% | 10.35 | 11.31 | +9.3% |
| cli | 50 | 600 | 2997.3 ms | 2755.1 ms | +8.1% | 14.29 | 14.01 | -1.9% |
| git | 1 | 32 | 391.5 ms | 503.2 ms | -28.5% | 1.60 | 1.76 | +9.7% |
| git | 10 | 160 | 915.8 ms | 1071.6 ms | -17.0% | 8.92 | 7.51 | -15.8% |
| git | 50 | 600 | 3679.8 ms | 3704.9 ms | -0.7% | 11.58 | 11.66 | +0.7% |

Per-trial medians, p95/p99, sample counts and binary/source hashes are retained in [the machine-readable results](concurrent-push-latency-rustfs.json). Interpret small changes alongside the spread between trials.

The 300 ms p50 target at 50 concurrent agents is not met. High-concurrency gains are small and variable; these runs do not establish a capacity increase or a higher concurrency limit.

### Trial variability

These repeated-trial medians show why pooled results need caution.

| Order | Variant | CLI, 1 agent | Git, 1 agent | CLI, 50 agents | Git, 50 agents |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | baseline | 219.1 ms | 227.9 ms | 2901.9 ms | 3575.3 ms |
| 2 | candidate | 165.9 ms | 276.7 ms | 2480.1 ms | 3412.2 ms |
| 3 | candidate | 323.9 ms | 782.8 ms | 3451.7 ms | 4266.6 ms |
| 4 | baseline | 498.6 ms | 1173.6 ms | 3082.5 ms | 4113.5 ms |

## Correctness evidence

All 3,656 pushes succeeded, including warmups; 3,168 pushes were timed. Exact live ref checks passed after every case in every trial.

After a clean RustFS restart, the second candidate trial was audited independently: all stored capsule runs were downloaded and decoded with the unchanged canonical capsule decoder, their Git packs installed into an empty native Git database without source alternates, and `git fsck --full --strict` passed. All 914 acknowledged commits, per-ref parent chains, final refs and 3,743,744 payload bytes matched. This proves stored Git bytes and ref discovery after restart; it does not qualify the standard empty cold-clone path.

Cache reuse explicitly excludes S3’s mutable `null` version ID unless a usable ETag identifies the body. An overwrite can retain that version ID ([AWS contract](https://docs.aws.amazon.com/AmazonS3/latest/userguide/AddingObjectstoVersionSuspendedBuckets.html)); a regression test reproduces the stale-head failure and passes with the guard.

Local checks passed:

- 540 metadata tests, including ref replacement/deletion during capture, ETag-only and version-only listings, missing/null/empty listed versions, fresh activation resolution, and the eight-attempt stability bound.
- 43 push tests; three existing multi-GiB tests remain ignored. Coverage includes physical ancestry despite local replacements/grafts/shallow boundaries, forged object identities, loose and packed pointer scanning, LFS and Xet round trips, force/delete/atomic policies, and native Git incremental readback.
- Repository Clippy correctness/suspicious severity gates using the same minimal features, formatting, and whitespace checks. An existing orphaned doc comment in push configuration was removed to pass the lint gate; it does not change the measured runtime.

## Remaining scope

Publication and storage work remain material at high concurrency. The CLI path already discovers selected refs, so ref-capture retry reuse primarily benefits normal Git discovery. This change does not remove the initial full ref census.

New-ref creation, continuous unsynchronized writers, the previously slow standard empty cold clone, WAN/provider behavior, and large-file throughput require separate measurement. LFS/Xet test coverage above should not be read as a large-file performance result.

Further work should measure ownership transfer during capture to avoid cloning large decoded ref heads, prove pointer-free introduced-object ranges before skipping redundant LFS discovery, and profile the durable publication path on a quiet host. These are opportunities, not measured improvements in this PR.

The task retains the identical offline RustFS input, immutable commit plan, both binaries, paired runner, raw per-push/per-wave timings, resource logs, and physical audit outputs outside the source tree. Credentials are excluded from this report and its JSON.
