# RustFS push latency budget

Qualification date: November 7, 2026. The 500 ms median budget at 50 clients
has **not** been demonstrated. The retained patch removes redundant scratch
sync work; these local measurements do not establish an end-to-end speedup.

## Retained change and safety boundary

`crab/src/git/pack.rs::generate_pack_files_via_git` passes command-scoped
`core.fsync=none` to `git pack-objects` for its private temporary pack set.
Those files are upload scratch, outside Git's persistent object database.
Original local objects, strict indexing, object authentication, verified
immutable upload, ref CAS and epoch confirmation retain their existing paths.
Persistent fetch/install operations do not receive this setting. The sibling
stdout pack generator does not ask Git to harden an output file.

[Git's fsync contract](https://git-scm.com/docs/git-config#Documentation/git-config.txt-corefsync)
allows disabling hardening for selected command output. On Apple Git 2.50.1,
a direct probe produced byte-identical pack and index files while fsync
writeout counters changed from three operations to none. That proves removal
of scratch sync work, not a latency improvement under contention.

The proposed two-worker native push runtime budget was removed. Earlier
[runtime qualification](concurrent-helper-runtime-rustfs.md) and the new
shared-host trials did not support retaining it. Native and helper runtime
scheduling remain at the selected v2 base behavior.

## Final local observations

Source: `907d6a97fcf858a5f532be98c37a57a7d8dcd1da`, based on selected
`crab-v2` commit `ac66487eae6108998de8509f82771157300f97a8`. Rust 1.97.0,
optimized release build with the same full non-FUSE features as the control.
Host: Apple M2 Max, 12 cores, 32 GiB; loopback native RustFS 1.0.0-rc.1,
APFS on the external workspace USB SSD.

Each measured command starts a new native CLI process and pushes an actual
new 4 KiB ordinary Git blob to an existing independent ref. Commit preparation
and ref bootstrap are outside command timing. The read-only curl source is
`70c084a0123b468fe7078e3fc72c78e1b66394b6` with 40,069 commits. Diagnostic
publication traces are enabled locally. The 50-client cohort runs first,
followed by 10 and 1 on the same private backend. Percentiles use nearest rank.

| Clients | Samples | p50 ms | p95 ms | Maximum ms |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 100 | 166.74 | 484.03 | 732.17 |
| 10 | 100 | 982.04 | 7,425.49 | 7,635.78 |
| 50 | 500 | 6,086.60 | 11,626.24 | 14,246.93 |

Other Rust builds/tests, a VM and filesystem activity shared this host and
device. These observations fail the requested budget at 10 and 50 clients;
they are not accepted as a causal comparison of the patch. The same-session
stock trial measured p50 2,904.56 ms at 50 clients, also above budget. Changing
load prevents attributing the difference to the patch.

All 700 final-build pushes succeeded. Independent cold consumers verified
every acknowledged commit, exact final refs and 4 KiB file bytes without
alternates. Full strict Git fsck and Crab fsck passed with zero errors or
repairs. All private RustFS processes were stopped after verification.

## Storage isolation

A separate signed-S3 control removes Crab and Git: create an 8 KiB object,
read and verify it, then conditionally update an independent head. Against a
private RustFS 1.0.4 fixture with defaults, 500 chains at 50 clients measured
p50 2,221.35 ms, p95 3,120.23 ms and maximum 3,451.44 ms. All succeeded.

This reproduces substantial backend publication cost on the shared deployment.
It is not a universal lower bound or a decomposition of the CLI median.
Remaining client/preparation overhead is not fully attributed. The separate
1.0.4 push experiment also missed the budget; no installed provider, repository
dependency pin or durability setting was changed. Its default fsync isolation
still awaits synchronization; experimental group-commit flags stayed disabled.
See the earlier [backend diagnosis](concurrent-push-bottleneck-rustfs.md).

## Isolated comparison and reproduction

The final candidate and stock control differ only in `crab/src/git/pack.rs`;
their workflow and benchmark code are identical. These Linux jobs use the
existing pinned RustFS 1.0.0-beta.8-glibc image, so their environment must be
reported separately from the local macOS observations. Qualification is pending:

- [Stock control c40100d067c](https://github.com/crabbuild/crab/actions/runs/42425056610).
- [Scratch-only candidate 907d6a97fcf](https://github.com/crabbuild/crab/actions/runs/42430634932).

The manual performance job measures 350 updates at 50 clients, then 100 at
10 and 100 at 1, with default product logging. Native p50 must be strictly
below 500 ms in every cohort; 200 ms remains preferred. Normal Git is measured
separately. Reports require real 4 KiB payloads and complete sample counts;
integrity failures cannot qualify. Cold readers bind final bytes to Git blob
identities authenticated by strict Git fsck.

```bash
python3 crab/scripts/e2e/run_existing_ref_push_rustfs.py \
  --crab-bin /path/to/release/crab --source /path/to/read-only/curl \
  --root "$HOME/Workspace/CrabBuild/crabbuild-qualification/push-latency" \
  --run-id fifty-existing-refs --bucket crab \
  --endpoint-url http://127.0.0.1:9000 --push-client native \
  --agents 50 --branch-update-rounds 7 --max-parallel-pushes 50 \
  --consumer-mode cold-final --skip-same-branch --no-request-capture
```

Completed scoped proof: release builds, four bounded pack tests, three indexer
lifecycle/byte-preservation tests, incremental push round-trip/request bounds,
38 Python harness tests, native/Git reader-mode live smokes, CLI help/error
envelopes, formatting and Actionlint. Full isolated performance proof remains
required before claiming the 500 ms target or an improvement over stock.
