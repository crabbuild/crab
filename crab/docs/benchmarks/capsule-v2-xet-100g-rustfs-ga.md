# Capsule v2: 100 GiB Xet RustFS GA qualification

## September 28 current-head run: bytes passed, transport gate failed

Candidate `9b91d0b3d06620cdbadf8ae85f93955877e266c6` used the frozen Crab
binary SHA-256 `019cbb5e6056def05905b0421e5303dc4180cb39b9a73ca441d0a2738f9eb4b1`
against isolated local RustFS 1.0.0 GA. The default workload had 50 model files
of 2 GiB each, 500 code files, and three successive 100 GiB file versions.
Its 300 GiB logical model-file history retained 21,604,718,908 bytes of
external xorbs (6.71% of logical bytes), with shards still independent of
capsules. A cold cross-repository push reused the shared chunks of a
537,919,488-byte file while adding only 1,433,575 xorb bytes.

| File version | Add | Push | Push requests | Repack | Repack requests |
|---|---:|---:|---:|---:|---:|
| 0 | 1,012.8 s | 904.6 s | 760 | 3.0 s | 11 |
| 1 | 631.9 s | 10.0 s | 132 | 2.2 s | 18 |
| 2 | 487.7 s | 10.1 s | 132 | 1.7 s | 18 |

The initial current-tip cold hydrate took 37.9 minutes; its warm-cache repeat
took 16.3 minutes. Historical versions 0, 1, and 2 each hydrated from a fresh
cache and passed all 50 model-file and 500 code-file SHA-256 comparisons.
Retained-history dependency verification passed for all three versions;
version 2 took 21.9 minutes. Restoring the oldest checkpoint preserved every
xorb and shard object and the exact old ref. A fresh clone hydrated that old
100 GiB version byte-identically, then fetched and hydrated the republished
latest 100 GiB version byte-identically. Across seven full hydrate/hash sweeps,
all 3,850 file comparisons passed. Strict native Git fsck and final remote
Crab fsck passed; remote fsck reported zero errors and repair failures.

**The harness status is failed.** Its request meter recorded three upstream
`TimeoutError` events during the initial version-0 seed push, yielding three
meter-generated 5xx responses. The push retried and completed; version-1 and
version-2 pushes and every measured read phase recorded no proxy errors. The
meter uses a 60-second upstream connection timeout. A separate path-traced
seed-push diagnostic reproduced the three timeouts: each was a duplicate PUT
to an xorb key that had already received successful PUT and GET responses in
that push. All 330 distinct xorb keys had a successful PUT. A subsequent
create-only PUT to one of those occupied keys returned HTTP 412 in 41 ms
directly against RustFS and in 382 ms through the meter. These probes narrow
the failure but do not establish why the three original duplicate requests
stalled under load. The diagnostic was stopped after its successful seed push;
it is not a substitute for a complete clean rerun. Passing byte and fsck
checks do not waive the zero-proxy-error gate. This run also does not compare
protocol v2 against v1 or qualify hosted providers.

Retained artifacts under the mounted CrabBuild workspace:
`pr208-live-20260928/xet-100g-head9b91-r1/artifacts/report.json` (SHA-256
`e661df4732a6b139159bf660ede8f91ce0ea5415714c8a7e8e07902bb90e0144`)
and `capsule-xet-transport.json` (SHA-256
`5a133461e37d9566b9ecc24d867abdb8364d99b4a8ba1ebbc5a4947dad5e48c8`).
The isolated remote and failed-run evidence were retained.
The path-traced diagnostic is retained separately as
`pr208-live-20260928/xet-seed-trace-head9b91-r1/artifacts/capsule-xet-transport.json`
(SHA-256 `26f4ffa668aff149e1d751f674643f00ed48a7fd92928aae6221296cbd576d4f`).

## September 29 GA rerun: byte and transport gates passed

The isolated `xet-100g-headc7c88-ga-20260929-r2` rerun completed the same
three-version 100 GiB workload on RustFS 1.0.0 GA with the frozen binary
SHA-256 `019cbb5e6056def05905b0421e5303dc4180cb39b9a73ca441d0a2738f9eb4b1`.
Its report records `passed`, 4,207/4,207 checks, 322 commands, no failed
checks, and no request-meter proxy errors across 16,219 object-store requests.
All historical and restored-file byte checks passed; final remote fsck found
zero errors and performed zero repairs. The initial push used 749 requests,
successive pushes 132 each; the three repacks used 11, 18 and 18 requests.
The earlier failed r1 and diagnostic remain retained; this clean run does not
erase their evidence.

The report identifies source commit `c7c88bfd57dea16368e4180138ed40295aa73a5e`
with a dirty source worktree, but verifies that the selected binary stayed
unchanged. The run therefore qualifies that frozen binary and fixture, **not**
the later PR head or a reproducible clean source commit. It also does not
establish v1 parity or hosted-provider qualification. Retained report and
transport SHA-256 values are respectively
`b0d2786be6fa9b16777885290a37b927f7befef6a2a187c58e6a1ef952805b29`
and `d7afceffb4689ebb65bc06d00fbe1c679a3b56874098fcecf75a259e0225d37a`.
