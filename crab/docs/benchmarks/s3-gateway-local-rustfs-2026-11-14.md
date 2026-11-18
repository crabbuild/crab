# Local S3 gateway qualification

Status: **passed for the frozen S3 profile**, on native macOS ARM64 with
RustFS 1.0.0-rc.1 and a release build of this v2 checkout. Run completed on
November 14, 2026 Pacific time. Source, binary digests, client versions,
individual operation counts, fixture digests, and results are retained in the
[JSON evidence](s3-gateway-local-rustfs-2026-11-14.json).

The Boto3 run observed 114 successful calls across all 23 advertised operation
names. It exercised real signed HTTP requests, object-store persistence,
publication, and readback; no mock backend was used. Three isolated repository
placements shared one local RustFS server. Two gateway processes used separate
private caches. All task-owned listeners were stopped after verification.

## Failure found and corrected

`GetObjectAttributes` accepted `StorageClass` in the requested attribute list
but returned HTTP 200 without the field. The gateway now returns `STANDARD`
when requested. A size-only request still omits unrequested attributes.
The AWS CLI's [official example](https://docs.aws.amazon.com/cli/latest/reference/s3api/get-object-attributes.html#examples)
returns `StorageClass: STANDARD` for this XML attribute; this differs from the
optional storage-class header on ordinary GET/HEAD responses.

Implementation: `crates/crab-s3-gateway/src/gateway.rs`. The regression and
previously missing operation coverage now run through the existing signed
Boto3 client, `crab/scripts/e2e/s3_gateway_boto3.py`, used by packaged-image CI.
The protocol dependency remains the unmodified pinned `s3s` 0.14.1.

## Observed results

| Surface | Result |
| --- | --- |
| Bucket APIs | List, HEAD, location, and honest unversioned response passed |
| Object APIs | PUT/GET/HEAD, selective attributes, response override, tags, copy, conditional writes, and single/multi-delete passed |
| Listings | V1/V2 pagination, delimiter groups, encoded refs, Unicode/literal-percent keys, and URL-encoded results passed |
| Multipart APIs | Create, upload, range copy, replacement, upload listing, part listing, abort visibility, and completion passed |
| Large LFS object | Eight 64 MiB parts; 512 MiB full read and a 67,117,056-byte range spanning part boundaries matched independent source SHA-256 values |
| Native Xet object | SDK-published 64 MiB source; full, open/suffix/ranged reads, full copy, and multipart range copy matched original bytes |
| Checksums | CRC32, CRC32C, CRC64NVME, SHA1, and SHA256 survived PUT/HEAD/GET; a bad digest left prior content intact |
| Authentication | SigV4/SigV2 headers and presigning, temporary credentials, virtual-host routing, and rejected unsigned/bad-token/bad-signature requests passed |
| Authorization | Read-only and unauthorized principals, protected branches, malformed XML, and documented unsupported-option rejection passed |
| Concurrent publication | Eight writers through two instances preserved all 16 objects; four conditional creators produced exactly one winner; cross-repository copy matched bytes |
| Recovery | Graceful replacement and abrupt SIGKILL both preserved acknowledged parts; the standby completed uploads and a cold-cache replacement read exact bytes |
| Completion retry | Independent native SDK reads proved an identical retry preserved the committed Git identity |
| Independent visibility | Native SDK verified a 42-byte Git blob and a 2,097,184-byte hydrated LFS object written through S3; a historical commit retained original bytes after branch replacement |
| AWS CLI | Nine commands verified bucket discovery, PUT/HEAD/attributes/range and bidirectional high-level `s3 sync` |

The large-object run was repeated with Boto3's default 60-second read timeout,
without SDK retries. It passed in 34.161 seconds: upload/completion 17.934
seconds, full GET 1.628 seconds, and range GET 47 ms. These are local smoke
observations, not throughput or production latency guarantees.

Before shutdown, both instances were ready. All three repository listings had
zero open multipart uploads. Every captured live scratch-byte, scratch-file,
and pending-byte gauge was present and zero. An earlier unoptimized attempt
surfaced `NoSuchUpload` during cleanup and is retained as failed diagnostic
history, not passing evidence.

## Repeatable API client and checks

With a running configured gateway, protected credentials available in the
environment, and a capacity-managed scratch directory:

```sh
TMPDIR="$QUALIFICATION_SCRATCH" \
  python3 -B crab/scripts/e2e/s3_gateway_boto3.py \
  --endpoint "$S3_GATEWAY_ENDPOINT" \
  --bucket "$S3_GATEWAY_REPOSITORY" \
  --large-object-bytes 536870912 \
  --report "$QUALIFICATION_REPORT"
```

The client reads `S3_GATEWAY_BOTO3_ACCESS_KEY` and
`S3_GATEWAY_BOTO3_SECRET_KEY`; temporary credentials additionally use
`S3_GATEWAY_BOTO3_SESSION_TOKEN`. Add `--xet-fixture` and `--xet-key` to test
an independently SDK-published object. The existing gateway CI workflow owns
packaged-image, provider setup, and multi-instance orchestration. Supplemental
local fault/security probes are recorded separately in the JSON evidence.

Validation passed: 225 gateway library tests, 32 Python qualification tests,
Rust formatting, Clippy for all gateway targets with warnings denied, Python
syntax, and patch whitespace. The explicit 122,880-write CPU scaling diagnostic
remained ignored and is not counted as passed.

This run establishes end-to-end smoke coverage of the supported profile. Live
AWS S3/GCS/Azure backends, packaged images and deployment ingress, other operating
systems, exhaustive request-option conformance, and production-scale limits
remain separate qualification work. Git-path and other deliberate profile
restrictions remain defined by `crab/docs/architecture/s3-gateway-contract.md`.
