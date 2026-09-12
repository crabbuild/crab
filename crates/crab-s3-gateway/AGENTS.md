# AGENTS.md

Scoped rules for `crates/crab-s3-gateway/`. Root and `crates/AGENTS.md` rules
also apply.

## Purpose

- This crate is the top-level S3 protocol and service-composition boundary for
  configured Crab repositories.
- It translates authenticated S3 requests into pinned Crab reads and atomic Git
  publications. Reusable Git, storage, Xet, cache, metadata, coordination, and
  publication mechanics remain in their owner crates.
- The canonical wire contract is
  `crab/docs/architecture/s3-gateway-contract.md`. The architecture and
  qualification record is `crab/docs/architecture/crab-s3-gateway.md`.

## Code Map

- `src/main.rs` — CLI selection, logging boundary, and process exit behavior.
- `src/server.rs` — S3 and management listeners, HTTP limits, probes, graceful
  shutdown, and request/response observation.
- `src/config.rs` — strict TOML schema, secret-file checks, logical repository
  catalog, and startup validation.
- `src/auth.rs` — access-key lookup, SigV4/SigV2 authentication material, and
  principal mapping.
- `src/namespace.rs` — logical bucket, encoded ref, and Git path parsing.
- `src/repository.rs` — configured repository state and immutable read views.
- `src/gateway.rs` — S3 operation implementation, authorization, response
  mapping, and admission ownership.
- `src/content.rs` — upload spools, checksum validation, LFS/Xet content reads,
  and bounded streaming.
- `src/mutation.rs` — path-local tree rewrites, ref leases, visibility evidence,
  and atomic publication.
- `src/attributes.rs` — commit-bound S3 metadata, tags, checksums, and ETags.
- `src/multipart.rs` — durable multipart state, distributed capacity,
  completion recovery, expiry, and cleanup.
- `src/admission.rs` — bounded control, read, transfer, scratch, and same-ref
  queues.
- `src/metrics.rs` and `src/metrics/` — bounded-label service, backend, cache,
  and filesystem metrics.
- `deploy/` — container, Compose, Helm/EKS, ECS, alerts, and operations assets.

## Protocol Invariants

- Verify signatures against the original HTTP path and query before namespace
  decoding. Never normalize a signed request before authentication.
- One configured logical bucket maps to one backing Crab repository placement.
  Never expose provider bucket names, prefixes, credentials, or internal object
  keys through S3 responses.
- Keys use `REF/path`. Branches may be writable; tags and commit IDs are
  immutable. Recheck permission, protection, and branch identity under the
  canonical publication lease.
- Pin one immutable commit for each read. Tree bytes, object attributes, ETag,
  size, modification time, and checksums must come from that same view.
- A successful object mutation publishes exactly the documented Git result.
  Never acknowledge bytes before durable content, attributes, visibility
  evidence, and ref-journal publication reach their required boundary.
- Map failures to stable S3 status, code, headers, and XML. Unsupported modeled
  fields fail explicitly; they are not silently ignored.
- Preserve sibling semantics. Changes to GET usually require HEAD, attributes,
  range, conditions, and listing review; PUT changes require COPY, multipart
  completion, checksums, attributes, and conditional-write review; V1 listing
  changes require V2 and delimiter/continuation review.

## Large Content and Multipart Invariants

- Reconstruction is byte-identical or fails. Never bypass Xet/LFS digest,
  length, part, or provider-version validation.
- Range GET reconstructs only overlapping Xet chunks and remains bounded by
  stream backpressure. Complete GET retains terminal whole-file verification.
- Request memory and scratch use must not scale without an admission or durable
  repository bound. Disconnect, cancellation, timeout, and backend error paths
  release permits, reservations, temporary files, and tasks.
- Multipart state and payloads are shared durable repository state; process
  scratch and cache are not recovery boundaries.
- Part replacement, abort, completion, expiry, and recovery are idempotent.
  A losing or late transfer may reclaim only the payload it owns.
- Distributed session slots remain charged until all registered and reserved
  payloads are retired. A process crash must not produce unaccounted staging.
- Completion publishes the exact ordered selected parts once. A frozen session
  closes only from durable publication evidence and otherwise remains safe for
  an identical retry.

## Service and Security Invariants

- Keep the S3 and management listeners separate. `/livez`, `/readyz`, and
  `/metrics` are unauthenticated and must stay on a private management network.
- Normal serving never initializes, migrates, repairs, or converts repository
  storage. `--initialize` is the explicit idempotent empty-prefix operation.
- Control, read, transfer, connection, XML-body, response-idle, and scratch
  limits are product behavior. Preserve bounded failure and S3 `SlowDown`
  semantics under overload.
- Metric labels are fixed enums. Never label or log repositories, refs, keys,
  upload IDs, principals, access keys, secrets, signatures, tokens, signed
  requests, or malformed raw bodies.
- Gateway credentials and backend cloud credentials are separate trust
  boundaries. Secrets come only from permission-checked files.
- Every listener, background reconciler, response stream, repository handle,
  lease, and task must drain or close on success, failure, cancellation,
  timeout, and shutdown.

## Dependency Contracts

- Read `s3s` source/types and the relevant official S3 SDK behavior before
  changing wire defaults, request parsing, authentication, streaming, errors,
  pagination, conditions, or checksums. Do not infer dependency behavior.
- Keep gateway policy in this crate. Move code downward only when it is a
  provider-neutral reusable mechanic with real consumers and a clear owner.
- Use `crab-storage` for backing placement, provider construction, object paths,
  ranges, retries, and error classification. Do not reproduce provider rules.
- Use `crab-read`/`crab-remote-git` for verified reads, `crab-write` and
  `crab-coordination` for publication and leases, and `crab-cache-store` for
  read-through cache behavior. Do not create gateway-only reconstruction or
  publication paths.
- Serialized attributes, multipart records, catalog keys, and visibility
  evidence are persistent cross-version contracts. Schema changes require
  explicit compatibility or migration proof.

## Change Method

- Start from the public S3 operation, then trace its handler, authorization,
  namespace resolution, owner-crate call, persistence boundary, error mapping,
  and response body lifecycle.
- Read the whole changed function/module plus its callers, callees, sibling S3
  operations, focused tests, protocol contract, and current `origin/main`
  behavior before choosing a fix.
- Prefer one canonical path. Do not add compatibility aliases, silent
  fallbacks, duplicate policy, or special cases for one client unless the
  frozen protocol or an upstream dependency contract requires them.
- Keep `README.md`, the protocol contract, example configuration, deployment
  assets, alerts, and operations runbook synchronized with observable changes.
- For deployment edits, check Compose, Helm/EKS, ECS, container paths, ports,
  UID/GID, volumes, probes, limits, and credential mounts for the same invariant.

## Validation

Use a target directory on the mounted workspace that is dedicated to this
checkout. Replace `<worktree>` with a stable checkout name:

```sh
cargo fmt --check -p crab-s3-gateway
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-<worktree> \
  cargo check -p crab-s3-gateway --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-<worktree> \
  cargo test -p crab-s3-gateway --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-<worktree> \
  cargo clippy -p crab-s3-gateway --all-targets --locked -- -D warnings
```

- Run focused unit tests first. Use the checked Compose qualification or CI for
  signed real-client, packaged-image, restart, multi-instance, large-object,
  listing, fault, and performance proof.
- Qualification clients include unchanged AWS CLI and Boto3 paths. The wider
  accepted matrix lives in the protocol contract; do not claim a client or
  backend from implementation inference alone.
- Use isolated logical repositories, physical prefixes, and buckets for live
  tests. Never print credentials and never run bucket-wide GC.
- Do not edit evidence inventories, baselines, retained reports, or thresholds
  merely to make a gate pass. Fix the product or document the missing proof.

## Review Checklist

- Wire request and response remain compatible with the canonical S3 contract.
- Authentication happens before authorization; authorization covers bucket,
  ref, path, and action.
- Read view, content, attributes, and conditions use one pinned commit.
- Mutation acknowledgement follows durable publication and conflict fencing.
- Large reads/uploads remain bounded, verified, cancel-safe, and cleanup-safe.
- Multipart restart and multiple gateway instances preserve ownership and
  quotas.
- Error bodies and metrics contain no identities or secrets.
- Sibling operations and provider backends are proved unaffected or covered.
- Documentation and deployment artifacts match runtime behavior.
- The change is the best owner-boundary fix, not only a passing local patch.
