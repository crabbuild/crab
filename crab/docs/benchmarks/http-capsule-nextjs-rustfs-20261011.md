# HTTP server capsule v2: Next.js and RustFS qualification

Core reads and ordinary native Git writes passed live correctness checks. Full
performance qualification did not pass: higher offered rates missed the capacity
gate, historical commit queries had long tails, background index construction
issued substantial origin traffic, and bulk native receive exceeded the harness
deadline. This receipt does not establish production readiness.

## Runtime and workload

October 11, 2026; server source `26d7b6d68d84d3eb6b870d568120b2c56a5d7242`,
based on `02efa0153e88be998ebdc3ade3732535aa4cdeb1`. Server binary SHA-256:
`6f70de77a6e62745bdd834a430bdab6a5719bb1d507f779c5aefb88604d6c774`.
RustFS 1.0.1 ran as a separate native process behind the request-counting proxy.
The single object-durable server used loopback HTTP, mTLS management, and local
administrator access on macOS 26.5.2 with 12 CPUs and 32 GiB memory. Other host
workloads remained active. No qualification source checkout was modified.

The source was `vercel/next.js` at
`d8c1c1b0002db8ccb63b58095f338e8bf423fa99`: 32,431 first-parent commits and
33,998 tracked paths. The current CLI published two packs totaling 2,661,416,518
bytes in 330.7 seconds, then checkpointed in 2.75 seconds. The HTTP server
adopted this fresh repository in 222.2 seconds. All results below use the reset
`CRBRUN01` format; the older `CRBRUN07` fixture was not reused as current-format
proof. The background integrity proof completed in 305 seconds and
`/integrityz` returned 200 for the populated repository's exact state digest.

## Correctness and lifecycle

- `verify_live.py` matched native Git for 234 paginated tree entries, ten commits,
  three blobs, and 20 diffs, and checked six invalid-input/path responses plus
  the runner's repository, host, asset, and shell checks.
- A separate seeded sample matched 100 historical commits across the preceding
  5,000 first-parent commits, 50 blobs, and 20 directories at concurrency eight.
- `verify_git_transport.py` fetched 1,600 exact blobs independently through
  buffered and chunked native Git HTTP requests: 4.26 and 3.23 seconds.
- A writable 21-commit fixture exercised branch publication, an annotated tag,
  binary content, a UTF-8 path, and a symlink. Native HTTP clone reproduced all
  68 objects byte-for-byte and passed `git fsck --strict`. A subsequent push
  became visible through refs and blob reads. Three byte ranges, `HEAD`, and an
  unsatisfiable range passed. A durable issue survived server restart.
- `verify_process.py` ran the Next.js oracle before and after a real process
  restart. Startup took 3.01/2.33 seconds; graceful shutdown took 0.29/0.45
  seconds; both processes exited with code zero.
- The dedicated current-format build passed 292 HTTP server tests (eight
  ignored), 37 capsule reader tests, formatting, and Clippy with `--no-deps`
  for both changed crates. Broader Clippy stopped at existing unfulfilled lint
  expectations in `crab-coordination`; that crate was unchanged.

## Load and latency

`qualify_http_load` used three workers for each of refs, commit at `main`,
20-commit history, the `packages/next/src` tree, and the 16,109-byte
`package.json` blob. Each run had a five-second warmup; measurements used
persistent HTTP connections. Index construction remained active. Histograms
use integer milliseconds; zero means less than one millisecond. The fixed-rate
gate requires at least 95% of the configured successful request count.

| Offered requests/s | Duration | Successful requests/s | p95 | p99 | Capacity gate |
| ---: | ---: | ---: | ---: | ---: | --- |
| 100 | 30 s | 98.7 | 31 ms | 165 ms | Pass |
| 500 | 60 s | 447.0 | 6 ms | 108 ms | Fail |
| 1,000 | 60 s | 855.4 | 15 ms | 59 ms | Fail |

All 81,110 measured responses succeeded. There were no HTTP errors, admission
rejections, transport errors, or response-body-limit failures. Peak sampled
server RSS was 931 MiB. These runs establish neither a maximum throughput nor
Internet/provider latency.

| Endpoint at the 1,000/s offered rate | p95 | p99 |
| --- | ---: | ---: |
| Refs | 14 ms | 56 ms |
| Commit | 13 ms | 59 ms |
| Commit history | 18 ms | 67 ms |
| Tree | 15 ms | 57 ms |
| Blob | 14 ms | 59 ms |

The first process-cold reader open took 1.61 seconds. In the mixed historical
sample, commit p95 was 3.75 seconds and maximum 5.14 seconds; blob p95 was
50.6 ms and directory p95 was 87.4 ms. Historical OID authorization used bounded
raw reachability traversal while complete browse indexes were unavailable. One
slow commit operation read 4,601 storage objects. Hot `main` reads do not prove
equivalent latency for arbitrary historical commits.

## Origin traffic and remaining work

The unchanged-reader regression proves three GETs and no immutable payload
reload for a two-second refresh of a single-ref fixture; the dependency also
performs two LISTs to capture stable ref positions. Changed refs, changed
derived-index records, and corrupt records retain their existing behavior.
Native fetch and mutation validation continue to open fresh snapshots.

Whole-deployment counters include warmup and concurrent background work:

| Interval | Store requests | Response bytes |
| --- | ---: | ---: |
| 30.09 s without foreground requests, indexing active | 33,139 | 128,063,115 |
| 100/s load plus warmup | 32,785 | 127,920,633 |
| 500/s load plus warmup | 46,424 | 242,340,163 |
| 1,000/s load plus warmup | 25,521 | 227,834,718 |

The idle interval included 32,342 Next.js Git requests and 691 Cell requests.
These totals cannot be assigned to individual HTTP reads or treated as a
steady-state request budget. `generation::browse::build` constructs a complete
graph, but path-state construction still uses the reader opened before that
graph was attached. Historical snapshot resolution consequently repeats raw
reachability walks. Reusing the validated graph during index construction and
measuring a completed-index deployment remain required performance work.

A separate full native HTTP push sent a 2,449,495,576-byte pack containing
714,606 objects. It missed the harness's 1,200-second deadline before a success
acknowledgement. Approximately 33.0 GiB of staging was observed with a configured
32 GiB Cell disk limit. Cancellation removed the staging files and left the
target ref empty. This does not prove that the server's separate 30-minute
receive deadline expired. Quarantine inflation, repeated graph reads, pack
normalization, and actual staging admission require further qualification.

The live fixtures contained no reachable Crab or LFS pointers. Fleet durability,
hosted storage, OIDC browser flows, abrupt crashes, and sustained production load
were not qualified by this run. Raw reports, request traces, private configuration,
source clones, and binaries remain outside the PR on the workspace volume.
The private manifest hashing the 18 current-format reports has SHA-256
`5dc6154deec0ee3f338a3b4a4fbc9b4bbe26677d7631b2c485144482c25879b3`.
