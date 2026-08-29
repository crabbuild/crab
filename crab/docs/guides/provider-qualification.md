# Provider Qualification and Canonical V1 Reset

Crab treats an object-store provider as supported only after the exact release
candidate passes the retained contract matrix. Successful credentials, a basic
PUT, or another `object_store` consumer are not qualification evidence.

## Support matrix

| Provider | Current evidence | Release status |
| --- | --- | --- |
| S3-compatible RustFS | The canonical harness passes locally against RustFS with create/CAS, multipart, range, pagination, cancellation, and receipt proof | Development-qualified; retain the CI artifact for a release claim |
| Amazon S3 | No retained real-service artifact for the current commit | Unqualified |
| Google Cloud Storage | Secret-gated CI job exists; no retained real-service artifact | Unqualified |
| Azure Blob Storage | Secret-gated CI job exists; no retained real-service artifact | Unqualified |

An unqualified row means the adapter is implemented but must not be advertised
as release-supported. Update this table only after
`.github/workflows/pb-provider-qualification.yml` emits a report accepted by
the strict verifier for the exact release commit.

## Required contract rows

Every provider run must pass all of these independently:

| Row | Proof |
| --- | --- |
| Create-only | First writer succeeds; a second writer conflicts and cannot replace the body |
| Match-token and identity | A current ETag/version updates; the stale token conflicts; the provider returns a usable identity |
| Multipart completion | Three fixed-size parts complete and reconstruct the exact BLAKE3-verified body |
| Multipart abort | An uploaded part is aborted and no object becomes visible |
| File-backed staged multipart | A local xorb streams to the generated staging key, flush confirms durability, and the canonical key remains absent |
| Exact range | The provider returns exactly `[start, end)` with no prefix or suffix bytes |
| Pagination | Listing crosses the provider's default page boundary with no missing or duplicate keys |
| Retry and errors | Real missing/conflict responses map to Crab errors and the retry boundary reattempts transient failures |
| Cancellation | Cancellation aborts multipart state and publishes no object |
| Origin receipt | A canonical v1 receipt binds the payload digest to the current ETag/version and avoids a second body hash |
| Isolation | The generated prefix starts empty and is empty again after prefix-only cleanup |

The harness records logical request and byte counts, provider/service identity,
region, `object_store` version, Crab commit, workflow run identity, commands,
per-row duration, and results. It never records credential environment values.

## Local RustFS qualification

RustFS must already be running and the named bucket must exist. The example
uses the local development credentials and `beyond`; it creates and removes
only `crab-provider-qualification/<run-id>`.

```bash
export AWS_ACCESS_KEY_ID=crab
export AWS_SECRET_ACCESS_KEY=crab
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL=http://127.0.0.1:9000
export AWS_ALLOW_HTTP=true
export CRAB_S3_FORCE_PATH_STYLE=true
export CRAB_PROVIDER_QUALIFICATION_PROVIDER=s3
export CRAB_PROVIDER_QUALIFICATION_BUCKET=beyond
export CRAB_PROVIDER_QUALIFICATION_SERVICE=rustfs-local
export CRAB_PROVIDER_QUALIFICATION_REGION=local
export CRAB_PROVIDER_QUALIFICATION_SOURCE_SHA="$(git rev-parse HEAD)"
export CRAB_PROVIDER_QUALIFICATION_WORKFLOW_RUN_ID=local
export CRAB_PROVIDER_QUALIFICATION_WORKFLOW_RUN_ATTEMPT=1
export CRAB_PROVIDER_QUALIFICATION_RUN_ID="local-$(date +%Y%m%d%H%M%S)"
export CRAB_PROVIDER_QUALIFICATION_REPORT=/Volumes/Workspace/CrabBuild/provider-qualification/report.json

CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab --test provider_qualification --locked -- \
  --ignored --exact provider_contracts --nocapture

python3 crab/scripts/verify-provider-qualification-report.py \
  "$CRAB_PROVIDER_QUALIFICATION_REPORT" \
  --provider s3 \
  --source-sha "$CRAB_PROVIDER_QUALIFICATION_SOURCE_SHA" \
  --run-id "$CRAB_PROVIDER_QUALIFICATION_WORKFLOW_RUN_ID" \
  --run-attempt "$CRAB_PROVIDER_QUALIFICATION_WORKFLOW_RUN_ATTEMPT"
```

The test refuses cleanup unless the prefix has exactly the generated
`crab-provider-qualification/<run-id>` shape. It does not invoke GC and never
lists or deletes the bucket root.

## CI configuration

The RustFS job is self-contained and runs on scheduled or manual workflow
dispatch. Real GCS and Azure jobs are disabled until their repository variables
are explicitly enabled.

GCS requires:

- variable `CRAB_PROVIDER_GCS_ENABLED=true`;
- variable `CRAB_PROVIDER_GCS_BUCKET` naming a dedicated qualification bucket;
- variable `CRAB_PROVIDER_GCS_REGION`;
- secret `CRAB_PROVIDER_GCS_SERVICE_ACCOUNT_KEY` containing a scoped service
  account JSON key.

Azure requires:

- variable `CRAB_PROVIDER_AZURE_ENABLED=true`;
- variables `CRAB_PROVIDER_AZURE_ACCOUNT`,
  `CRAB_PROVIDER_AZURE_CONTAINER`, and `CRAB_PROVIDER_AZURE_REGION`;
- secret `CRAB_PROVIDER_AZURE_ACCOUNT_KEY` scoped to the dedicated container.

Each job uploads its report for 90 days. The verifier checks the exact commit,
run, attempt, dependency version, complete row inventory, provider page
threshold, positive request metrics, and absence of credential field names.

## Explicit development reset

Canonical v1 never deletes or translates pre-cutover state during normal open.
If a descriptor, staging database, prepared plan, manifest, shard, receipt, or
other Crab-owned format is not canonical v1, normal operation fails closed.

Use this procedure only for a dedicated development repository with no data
that needs preservation:

1. Record the exact bucket/container and repository prefix. Confirm the bucket
   is dedicated to this reset; if it is shared, create a new dedicated bucket
   instead of deleting shared `.crab` content.
2. Stop every Crab process, Git filter process, cache service, auth receive
   worker, and automation that can write the old repository.
3. Preserve source Git history and hydrated worktree files outside `.crab`.
   Do not use the Crab remote as the only copy.
4. Move `.crab/staging` aside or delete that exact directory. Remove the exact
   configured Crab cache directory separately; neither operation changes Git's
   source history.
5. Delete and recreate the dedicated development bucket/container, or delete
   only the explicitly verified repository prefix plus its dedicated global
   prefix. Never run `crab gc --scope=bucket` for this reset.
6. Run `crab init --storage-provider <provider> crab://<bucket>/<repo>` with the
   canonical v1 binary.
7. Run `crab add`, commit, and push every source file again. Do not copy old
   staging databases, manifests, xorbs, shards, or receipts into the new
   repository.
8. Clone into a new directory, hydrate all tracked files, run strict Git fsck,
   and compare byte digests with the preserved source.
9. Retain the commands, source/release SHA, provider identity, and digest
   results with the provider report.

If the scope is not isolated or any data owner cannot confirm deletion, stop.
There is deliberately no `--force-open`, legacy reader, migration command, or
automatic reset fallback.
