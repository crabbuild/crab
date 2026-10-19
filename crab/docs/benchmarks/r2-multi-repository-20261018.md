# Cloudflare R2: multi-repository replay, October 18, 2026

This is the retained baseline. The [large-repository repair and recovery report](r2-large-repositories-20261018.md) records the subsequent implementation and verification.

**Large-repository qualification remains open.** Kubernetes and Git failed their seed uploads on the tested host. Redis failed local object validation. Vue, Gson, and yt-dlp completed the scoped replay and read-integrity checks below. Passing smaller histories does not close the failed large-history cases.

The dedicated bucket is `crab-v2-r2-20261018-d78b010`, location `WNAM`. Its managed public `r2.dev` domain is disabled. All repositories use separate prefixes in this one bucket; test objects are retained.

The frozen CLI reports Crab 2.0.0, source `d78b010cd207d154c403c63683fac3735f4811ce`. Binary SHA-256: `0227ad1c94d7bf7c6715feded359bcad2546c13d2ca944e076b9b4330fbd5bdf`. No production code, dependency override, or validation policy changed for this run.

## Coverage and result

| Input | Source packs, MiB | First-parent commits | Replayed / planned | Result |
| --- | ---: | ---: | ---: | --- |
| kubernetes/kubernetes | 1283.65 | 59,258 | 0 / 100 | failed |
| git/git | 577.17 | 24,380 | 0 / 100 | failed |
| redis/redis | 485.42 | 9,989 | 0 / 100 | failed |
| tokio-rs/axum (control) | 6.08 | 1,862 | 3 / 3 | ok |
| vuejs/vue | 31.48 | 3,401 | 100 / 100 | ok |
| google/gson | 50.33 | 2,055 | 100 / 100 | ok |
| yt-dlp/yt-dlp | 63.26 | 20,434 | 100 / 100 | ok |

Source-pack sizes cover all local mirror refs. Each successful seed publishes the history reachable from the captured `HEAD~100`, then the next 100 first-parent commits update `refs/heads/main` individually. The seed capsule sizes below measure the actual stored payload. The axum control uses three replay commits. Source revisions and exact commit identities are retained in the summary and CSV; these are captured local histories, without fetching or updating the inputs.

The first Git/Redis preflight attempts rejected bare inputs before any upload. Disposable `--shared --no-checkout` working views supplied the expected input shape for their retained seed attempts. The original mirrors were never reset, cleaned, fetched, or updated.

## Incremental push measurements

| Repository | Mean, s | p50, s | p95, s | p99, s | Max, s | Commits / active push second |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| vuejs/vue | 2.080 | 1.710 | 3.594 | 11.680 | 13.287 | 0.481 |
| google/gson | 1.615 | 1.469 | 2.063 | 4.409 | 4.568 | 0.619 |
| yt-dlp/yt-dlp | 1.862 | 1.552 | 1.960 | 12.831 | 14.658 | 0.537 |

Each row has 100 push samples. Percentiles use nearest rank. Active push rate divides the replay count by summed push duration; inventory, fetch, and verification pauses are excluded. Complete per-repository validation durations are recorded separately. This is one run per input on a shared macOS host, without a matched baseline or repeatability claim.

## Seed and clone throughput

| Repository | Seed capsule MiB | Seed push, s | Seed effective MiB/s | Cold clone, s | Cold effective MiB/s | Warm clone, s | Warm effective MiB/s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| vuejs/vue | 22.44 | 8.895 | 2.522 | 10.387 | 2.065 | 6.460 | 3.320 |
| google/gson | 10.60 | 5.158 | 2.054 | 10.487 | 1.772 | 16.096 | 1.154 |
| yt-dlp/yt-dlp | 73.98 | 45.388 | 1.630 | 20.359 | 3.387 | 13.524 | 5.099 |

Upload throughput uses sizes of new immutable `/v2/capsules/` objects from retained, paginated S3 inventories, divided by complete push wall time. It includes rollup payloads and local preparation, and excludes retry/readback traffic. Clone throughput uses installed Git pack bytes divided by complete clone wall time. Neither measurement is raw R2 network bandwidth. Clones use `--no-checkout`; the warm clone reuses the cold clone cache. The CSV records every push, fetch checkpoint, clone, and failed seed attempt.

The raw harness also records changes in all visible objects. Those snapshots can include initialization and intervening coordination metadata; the published upload rates above filter to immutable capsules.

## Correctness proof

Each passing 100-commit run verified seed and final tips, four incremental fetch checkpoints (25/50/75/100), strict full Git fsck, remote Crab fsck, and independent cold/warm clones without Git alternates. Each clone matched 32 deterministic blob SHA-256 samples against the source. The final incremental consumer also matched source samples. Stored v2 capsule and ref-head objects prove that the capsule protocol was exercised.

After all repositories finished, all successful tips were checked again from the shared bucket. Original mirror ref/HEAD digests and every captured working-view HEAD/status were checked again. Failed imports had no published v2 ref-head objects. Binary digests remained unchanged. The summary includes these final proofs and the bucket inventory digest.

## Retained failures

| Input | Failed seed command, s | Exit | Observed failure |
| --- | ---: | ---: | --- |
| kubernetes/kubernetes | 220.992 | 3 | TLS/HTTP transport failure while uploading an immutable capsule |
| git/git | 95.164 | 3 | TLS/HTTP transport failure while uploading an immutable capsule |
| redis/redis | 28.301 | 4 | Historical tree rejected by Git object fsck: zeroPaddedFilemode |

The independent `object_store` 0.14.1 control uploaded 8 MiB and 64 MiB successfully. A 256 MiB body failed with a TLS `BadRecordMac` alert; reducing upload frames to 64 KiB and allowing HTTP/2 negotiation did not resolve it. AWS CLI failed the same 256 MiB body with an SSL protocol EOF. Curl also reported a bad-record-MAC alert, including with TLS limited to 1.2. Certificate verification remained enabled. The transport root cause is unresolved; these controls do not attribute the fault to Crab, R2, or the intervening network.

The 256 MiB control is below R2’s documented 5 GiB single-part upload limit. [Cloudflare R2 limits](https://developers.cloudflare.com/r2/platform/limits/) also document the service’s multipart limits. No successful large-import throughput can be derived from the failed seed commands. Diagnostic uploads overlapped portions of the failed medium seed attempts, so their durations are failure observations rather than performance baselines.

Redis tree `1f9ef1b6556b375d56767fd78bf06c7d90e9abea` contains literal `040000` directory modes. The captured raw tree confirms this independently of the error message. Push preparation enables object fsck in `crab/src/git/capsule_push.rs:1096`; `crates/crab-git/src/pack.rs:982` invokes native `git index-pack --fsck-objects`. The existing `install_pack_file_fsck_rejects_malformed_commit` test covers rejection of malformed objects. The live Redis attempt is the retained proof for this historical tree. [Git index-pack](https://git-scm.com/docs/git-index-pack) documents object-fsck rejection. No severity override or history rewrite was applied.

## Reproduction and evidence

### Follow-up multipart and conditional-publication controls

The same 256 MiB control body subsequently uploaded successfully using AWS CLI's classic transfer client, 16 MiB multipart parts, and four concurrent requests. Upload took 14.625 s (17.50 MiB/s); download took 10.332 s (24.78 MiB/s) and matched the source SHA-256. These are transfer controls, not Crab repository throughput measurements.

A separate server-side multipart-copy control completed with `If-None-Match: *`: the first publication returned HTTP 200 and produced the expected 268,435,456-byte object. A second attempt to publish a different, 8 MiB body to that destination returned HTTP 412; the canonical object's size and ETag remained unchanged. The failed attempt's multipart session was aborted. This establishes the tested R2 endpoint's conditional-completion behavior; it does not yet test Crab's integrated upload path or simultaneous writers.

Raw results are retained as `transport-probe/multipart-proposal-control.json` and `transport-probe/conditional-promotion-control.json` beneath the evidence directory below. These controls ran after the original final bucket inventory; that inventory is a snapshot of the earlier qualification, not a current count.

The proposed repair belongs in `crates/crab-storage/src/store.rs`: select bounded multipart before transferring a large immutable body, preserve create-only publication, verify canonical size and digest, then permit ref publication. Existing multipart-source upload and staged content-addressed promotion supply most of these mechanics. Ordinary multipart upload is not a direct replacement for immutable create: the pinned [object_store 0.14.1 S3 implementation](https://github.com/apache/arrow-rs-object-store/blob/v0.14.1/src/aws/mod.rs) completes ordinary uploads in overwrite mode. Its conditional-copy implementation uses one copy part, so reuse also needs an explicit 5 GiB boundary and a larger-object solution, such as ranged multipart-copy with conditional completion. Provider limits must be planned before upload. File-backed capsule construction and compaction are a separate necessary step to bound large-history memory usage without changing the serialized format.

The [R2 harness](../../scripts/e2e/run_r2_multi_repo.py) reuses the existing process supervision, binary snapshots, source checks, redaction, and S3 pagination. Supply AWS access/secret credentials and the R2 S3 API endpoint through environment variables, with region `auto` and path-style addressing. See [Crab S3-compatible configuration](https://crab.build/docs/cli/authentication/static-credentials). Cloudflare documents [conditional S3 writes](https://developers.cloudflare.com/r2/api/s3/api/) and [strong consistency](https://developers.cloudflare.com/r2/reference/consistency/).

Use a fresh dedicated bucket or unused source-derived prefixes. An existing prefix is rejected; changing only the local output directory does not change the remote prefix. Sources must be full working trees, or disposable shared working views of full bare mirrors.

```bash
python3 crab/scripts/e2e/run_r2_multi_repo.py \
  --bucket "$R2_BUCKET" --crab-bin "$CRAB_BIN" \
  --root "$HOME/Workspace/CrabBuild/crabbuild-qualification/$RUN_ID" \
  --source "$FIRST_SOURCE_TREE" --source "$SECOND_SOURCE_TREE" \
  --commits 100 --interval 25
```

Retained local evidence is under `CrabBuild/crabbuild-qualification/crab-v2-r2-20261018-d78b010/`, relative to the mounted workspace volume. It includes all raw reports, logs, frozen binaries, source/probe snapshots, inventory snapshots, and the summary generator. Original failed/preflight runs remain intact. The raw log manifest digest and per-run report digests are in the [summary JSON](r2-multi-repository-20261018-summary.json); operation measurements are in the [CSV](r2-multi-repository-20261018.csv).

This scoped benchmark does not replace the 1,000+ commit release qualification, repeat idle-host runs, provider contract matrix, managed Xet file coverage, concurrent-writer tests, or cross-platform proof. The follow-up controls support the multipart repair direction; closing the large-history gap still requires integrating that path and rerunning the exact retained Git/Kubernetes inputs. Redis requires an explicit historical-object policy decision or a separately identified qualification fixture.
