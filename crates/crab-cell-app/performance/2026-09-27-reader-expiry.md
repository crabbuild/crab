# Reader replacement after signed-session expiry

The constrained 3/5/10/20-node reference application passed against GA RustFS
on 2026-09-27 after pending activation hints began rechecking their selected
boot at its observed lease expiry. Reader replacement took **10.983 s** in this
run, compared with **34.830 s** in the preceding
[same-profile local run](2026-09-27-reader-scaling.md). This is one comparison,
not a recovery SLO or sustained-capacity qualification.

## Cause and change

Recruitment joins its selected activation attempts before selecting again.
Previously an unresponsive peer could hold that pass for the thirty-second
transport budget after its signed session expired. The earlier spare's own
readiness marker appeared 34.191 seconds after the kill acknowledgement;
driver polling did not account for the delay. Independent baseline Compose CI
also observed 34.878 seconds.

`ReplicaPeerClient::activate` now races the existing request against a fresh
exact-session lookup at the observed expiry. Expired, withdrawn or invalid
sessions release the hint; a renewed session preserves the original request
and transport deadline. A ready response can interrupt a stalled directory
lookup. Existing receiver admission, ownership and receipt checks still apply.
The change adds no task, timeout setting, wire shape or dependency.

The reduced expiry test failed before the change and passes afterward. Its
renewal companion proves a slow valid request is neither canceled nor resent.
Both the reference TCP transport and product HTTP transport honor the caller's
request budget. Product reqwest 0.12.28 has no separate connect timeout by
default, and the product client does not set one. No packet-level cause for
the original connection delay is claimed.

## Source and environment

- Source: `72cb8b83adf6300e71b42486cc1c801af3982b77`.
- Release binary SHA-256:
  `a6db7c6fdbd31ec338fcda22aad29fdcb98128d2e6d6826ce51c400f029b84e5`.
- Fresh immutable source archive and separate Linux target; only downloaded
  Cargo dependencies were shared. Release build: 3 minutes 11 seconds.
- Same pinned Rust 1.97-bookworm and RustFS 1.0.0-glibc image digests and
  dedicated four-CPU / 8,307,101,696-byte ARM64 Colima VM as the baseline.
- Each node and driver: one CPU cap, 1 GiB memory, zero swap, 256 processes,
  private disk volume, read-only root/source/binary and dropped capabilities.
  The VM supplies four physical CPUs to the whole fleet; per-node caps do not
  reserve twenty physical cores.
- Unchanged `qualification/scale.py` profile, five initial iterations per
  primitive lane. Only activation lifetime behavior changed in runtime code.

## Results

| Live nodes | Readers | Exact reads | p50 ms | p95 ms | p99 ms | Serial window | Second-write readiness |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 3 | 2 | 60 | 1.030 | 1.473 | 2.914 | 0.068 s | 5.050 s |
| 5 | 3 | 90 | 0.969 | 1.315 | 2.572 | 0.094 s | 5.016 s |
| 10 | 9 | 270 | 1.046 | 1.404 | 1.872 | 0.290 s | 4.774 s |
| 20 | 19 | 570 | 1.040 | 1.512 | 1.994 | 0.635 s | 4.865 s |

Every selected reader served exactly thirty measured queries; the owner served
zero replica queries. All values and receipts matched. Duplicate commands
retained one effect and receipt. Separate owner queries traversed all live
ingresses with counts differing by at most one; replica queries used selected
peer routing. Seven writer Cells stayed on the original three owners.

At five nodes the controller killed selected reader-only node 3, verified
exit 137 without OOM, and retained its pre-kill counters. Normal signed
membership expiry and recruitment admitted the spare without a fixture hint.
Twelve queries then verified the acknowledged value and receipt on the three
current readers. Driver fault-request-to-ready time was 10.983 seconds,
including 0.723 seconds for command acknowledgement. Owner/session/epoch and
incarnation stayed unchanged. The remaining delay includes lease expiry and
reconciliation; it has not been characterized across fault timing phases.

Target zero evicted all nineteen final reader views. Twenty surviving nodes
and the driver each passed exactly one selected test and exited zero. Nodes
withdrew renewed sessions and proved their retained managers could not reopen
after drain. Driver duration: 76.34 seconds.

Independent inspection verified all twenty-two node/driver records, kernel
limits, private volumes and identical binary hashes. No OOM or CPU throttling
was recorded. Surviving-node whole-cgroup peaks were 7.609–17.195 MiB; driver
peak was 23.656 MiB. The killed node's last sample was 20.023 MiB. These short
measurements do not establish many-Cell resource slopes.

Seven server recruitment tests, seventeen protocol tests, four public-host
correctness tests and four snapshot-lifecycle tests passed. Three manual host
performance cases and one manual single-process RustFS case were ignored in
those focused suites; the dedicated Compose run supplies this change's live
RustFS proof. Minimal runtime/host builds, strict Clippy for runtime/host/app
and HTTP library/tests, formatting, layout, policy entry points and runtime
documentation validation passed.

## Evidence and remaining gates

Raw evidence is retained under
`reader-expiry-72cb8b8-20260927/evidence/scaling`; the project was stopped after
capture, with containers and volumes retained. The existing Compose CI runs
this same profile. This local result is not a protected release receipt.

| Artifact | SHA-256 |
| --- | --- |
| `driver.log` | `e191654c556331df0bc850dde766cecd81655524ece1812b66158c2ca6b5ab5e` |
| `events.json` | `ec217d78cb3e20a8b4b5d8b61809f87213da00c404ad40435b39ae8cc2d814c8` |
| `containers.json` | `e6bb17c0f5549c955dd757e569aeeb281e2fbb6a846c10176a3299b181c2b642` |
| `verification.json` | `092d25ebd960881ecc8b8c53bf973280e4c13f29fad7ed6cef7d8edcf532f14d` |

The five-second refresh interval remains visible. Query samples exclude
freshness waiting and are serial, brief and limited to one replicated SQL
Cell. Sustained concurrent reads/writes, many-Cell admission, traffic during
owner loss and rollout, writer redistribution, independent hosts/providers
and protected release qualification remain open.
