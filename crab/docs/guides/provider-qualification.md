# Provider Qualification and Canonical V1 Reset

Crab treats an object-store provider as supported only after the exact release
candidate passes the retained contract matrix. Successful credentials, a basic
PUT, or another `object_store` consumer are not qualification evidence.

## Support matrix

| Provider | Current evidence | Release status |
| --- | --- | --- |
| S3-compatible RustFS | The canonical harness passes locally against RustFS with create/CAS, multipart, range, pagination, cancellation, and receipt proof | Development-qualified; retain the CI artifact for a release claim |
| Amazon S3 | Opt-in OIDC job exists; no retained real-service artifact | Unqualified |
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
dispatch. The three real-cloud jobs require explicit enablement.

The self-contained job pins the immutable RustFS 1.0.0 GA image index for
Linux amd64 and arm64, verifies the running binary's version, and retains
`service-version.txt` beside its provider report. These storage-contract
checks do not replace the capsule replay, Xet, or product-parity gates.

Amazon S3 requires repository variable `CRAB_PROVIDER_S3_ENABLED=true` and a
protected GitHub environment named `provider-qualification-s3`.

- Set `CRAB_PROVIDER_S3_BUCKET`, `CRAB_PROVIDER_S3_REGION`,
  `CRAB_PROVIDER_S3_ACCOUNT_ID`, and `CRAB_PROVIDER_S3_ROLE_ARN` for a dedicated
  test bucket in the role's account.
- Trust the repository's GitHub OIDC environment subject and `sts.amazonaws.com`
  audience. Restrict the environment's permitted branches and tags. Scope the
  role to prefix-filtered `s3:ListBucket` and the object and multipart operations
  under `crab-provider-qualification/`. The runner confirms that the expected
  account owns the bucket and that the generated prefix is empty before it
  starts the contract test. [AWS IAM guidance](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles_create_for-idp_oidc.html)

Use an unversioned bucket for full qualification cleanup. The harness aborts
before any writes if the requested prefix is nonempty or unavailable. Real
cloud labels require the native endpoint for their provider.

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

## CDN release evidence

Keep the native `report.json`, the `cdn-report.json` supplement, and a
`traces/` directory together for each CDN release row. A row names the edge
provider, access mode, consumer, read family, direct or managed profile, and
platform. Generate both reports against real cloud fixtures in the same
trusted CI run. The native report uses service `amazon-s3`,
`google-cloud-storage`, or `azure-blob-storage` for the matching edge.
Retain configuration and benchmark registration approved before the run as
separate inputs. Verify the row by passing those expected values from the
release job.

```bash
python3 crab/scripts/verify-provider-qualification-report.py "$origin_report" \
  --provider s3 --source-sha "$GITHUB_SHA" \
  --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT" \
  --cdn-report "$cdn_report" \
  --cdn-scope cloudfront/private/cli/git/direct \
  --cdn-configuration-sha256 "$approved_configuration_sha256" \
  --cdn-platform linux-x86_64 \
  --cdn-benchmark-registration "$approved_benchmark_registration"
```

For a release build, add
`--cdn-release-policy crates/crab-storage/src/cdn/qualified-scopes.txt` to the
verification command. This requires an exact match to the runtime's edge,
access mode, consumer, read family and direct or managed profile. The platform
is verified separately in the report. Git pack layers and repository capsule
metadata use distinct read families. The policy is empty until real provider
and product evidence qualifies a row.

The supplement binds the native report by SHA-256. It records exact output
bytes, a native write, immediate ref visibility, an observed edge GET, and
fixture cleanup. The qualification runner hashes its approved configuration
without credentials, including the native and viewer targets and the
deployed access, route and cache policies. Each check retains a JSON trace with
the same provenance and typed request observations. A trace records the key
fingerprint, source, status, cache disposition, bytes and latency. Keep raw
URLs, cookies, tokens and headers out of retained traces. For warm-hit denial,
record the authorized warm hit before the denial for the same key. Keep the
approved benchmark registration separate from the artifacts it evaluates.

The v1 contract lives in `crab/scripts/cdn_qualification.py`. Retain at least
30 paired measurements in each of two different client regions, covering
origin-only, cold edge, warm edge and warm local. The verifier applies the
registered warm-edge p95 and per-action cost budgets and computes a conservative
95% lower bound for median paired savings in the distant region. A trusted CI runner must
produce the measurements and verify deployment behavior. The command checks
retained evidence and its linkage. Cloud resource and product runs remain
release inputs, including CloudFront traffic alongside the Amazon S3 job and
the production managed issuer.

### Collect transport traces

Run `crab/scripts/e2e/run_cdn_transport_checks.py` against an existing dedicated
CloudFront, Cloud CDN, or Front Door fixture after publishing a known immutable
object. The approved fixture JSON includes `scope`, `base_url`, `native_origin`,
`fixture_prefix`, `neighbor_prefix`, `view_suffix`, `object_path`, `size`,
`sha256`, and `deployment_policy_sha256`. Use two test-owned
`crab-cdn-qualification/<name>` repository prefixes, a repository-relative
object key of 2 bytes to 1 MiB, and the SHA-256 of the approved deployment
policy snapshot. The exact SHA-256 of the fixture JSON must match the
preregistered `configuration_sha256`. Set `view_suffix` to the empty string
for a repository-local fixture or use the exact materialized suffix for a
private protected view. Public fixtures also require `export_origin`, the
distinct same-provider native target for the public copy. Both origins use
the same relative object key. Set `native_origin` to the live repository's
native endpoint.

```bash
python3 crab/scripts/e2e/run_cdn_transport_checks.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$approved_configuration_sha256" \
  --trace-directory "$transport_trace_directory" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" \
  --run-attempt "$GITHUB_RUN_ATTEMPT"
```

For private delivery, add `--viewer-file` pointing to a protected file with the
Cookie header value for CloudFront or Cloud CDN, or the raw bearer token for
Front Door. The canary requires a byte-verified cache hit before and after
anonymous full and ranged GETs to the same key. Both anonymous requests must
receive an authorization denial. For cross-principal cache reuse, use the
authenticated cache-reuse collector below before warming the object with
the transport canary.
To capture warm expiry and parallel range denials, also pass the first
credential's Unix expiry with `--viewer-expires-at` when it has 5 to 120 seconds
remaining. The timestamp must match the credential's signed policy or token.
The collector verifies two concurrent single-byte ranges on the approved
object before expiry. It repeats both ranges concurrently with the same
credential two seconds after expiry, then requests the full object. All
three requests must be denied. The retained range trace includes the expiry,
byte offsets, and request intervals. The checks exercise the deployed edge.
Product refresh, cancellation, and origin retry behavior need product traces.

For the required unrelated-viewer trace, add `unrelated_control` to the
approved private fixture JSON before recording its digest. Specify the
control's `object_path`, `size`, and `sha256` as for the target. Choose an
immutable key under a separate viewer-grant prefix in the same repository or
protected view, such as an LFS object for a Git pack fixture. Publish both
objects and pass `--unrelated-viewer-file` with a grant scoped to the control's
exact prefix, issued to a principal that lacks access to the target. Use a
live grant with at most 15 minutes remaining. The collector validates the
signed scope and uses the same grant to verify the control bytes before and
after two denials on the warm target, one for a full GET and one for a
single-byte range. It also verifies an authorized full cache hit on the
target before and after the denials. The trace retains the control body digest,
grant fingerprint, signed expiry, request intervals, and range offset. All
four control-grant probes must finish before the signed expiry.

For public and private delivery, the collector verifies the full SHA-256 and
compares a one-byte range. On the same warm key, it requires a denial for HEAD
and for representative version, generation, snapshot, and response-type
override queries supported by the native provider. It verifies full cache
hits with the approved bytes before and after these requests. The release
verifier requires this trace alongside the deployment policy snapshot. The
collector also records mutable and neighboring path denials. Traces retain
key fingerprints, probe labels, and normalized cache results. Cloud CDN
fixtures expose the load balancer's `cdn_cache_status` through
`X-Crab-CDN-Cache`.

The collector also requires an anonymous GET of the same object at the live
native origin to fail. For public delivery, it probes the export separately.
The S3 export must deny anonymous GETs. GCS and Azure Blob exports must return
the approved bytes to anonymous clients. All three export origins must deny
anonymous listing. Origin probes use the providers' native HTTPS APIs and
send no viewer credentials. The release verifier pairs the live-origin
denial with a separate authenticated native read of the same complete object.

Use `--native-identity-read` to authenticate the exact native GET with the
current provider identity. Set `AWS_PROFILE=crab` to use that local AWS profile.
For S3, the collector signs a 60-second GET in memory. For Cloud Storage and
Azure Blob Storage, it retrieves a bearer token from the current `gcloud` or
`az` session and sends it only to the approved native endpoint. The collector
accepts the observation after comparing its full response with the verified
edge bytes. [S3 presign](https://docs.aws.amazon.com/cli/latest/reference/s3/presign.html) · [Cloud Storage authentication](https://docs.cloud.google.com/storage/docs/authentication) · [Azure Storage Microsoft Entra authorization](https://learn.microsoft.com/en-us/rest/api/storageservices/authorize-with-azure-active-directory)

For a separately delegated manual native read, use `--native-read-url-file`
with a short-lived signed HTTPS GET for the exact
live-origin fixture object. The collector validates the host and physical key,
keeps the signature out of the traces, and compares the entire native response
with the digest-verified CDN response. It accepts V4 signed URLs for S3 and GCS,
and a read-only SAS for one Azure blob. Generate the URL using the fixture's
dedicated native reader. For an S3 fixture readable by the `crab` AWS profile,
run the following with the fixture's object URI and region. [S3 presign command](https://docs.aws.amazon.com/cli/latest/reference/s3/presign.html)

```bash
umask 077
aws s3 presign "$live_s3_object_uri" --profile crab --region "$aws_region" \
  --endpoint-url "https://s3.$aws_region.amazonaws.com" --expires-in 300 \
  > "$native_read_url_file"
```

Keep the exact fixture path, including any protected-view suffix. Use the GCS
XML endpoint or the canonical Azure Blob endpoint. [GCS signed URL command](https://docs.cloud.google.com/sdk/gcloud/reference/storage/sign-url) · [Azure blob SAS command](https://learn.microsoft.com/en-us/cli/azure/storage/blob#az-storage-blob-generate-sas)

### Verify authenticated cache reuse

Use an uncached private fixture with the approved `viewer_issuer` settings
and two independently authorized OIDC test principals. The principal
descriptor maps `primary` and `second` to objects containing `issuer`,
`subject` and `policy_identity`. Use each ID token's `iss` and `sub` values
and the identity evaluated by Crab Auth, which is the nonempty email claim
or the subject. The collector compares policy identities
case-insensitively, matching the reference service.

Run `collect_cdn_cache_reuse.py` with the first three options below to obtain
`cache_reuse_sha256`. Register that digest for the private row. It binds the
issuer endpoint, CDN approval, the two principals and the exact fixture
object. On the protected measurement worker, collect the trace before any
other reader warms the fixture object. Pass each principal's short-lived
OIDC ID token in its own protected file.

```bash
python3 -B crab/scripts/e2e/collect_cdn_cache_reuse.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$approved_configuration_sha256" \
  --principals "$approved_principals_json" \
  --cache-reuse-sha256 "$approved_cache_reuse_sha256" \
  --primary-token-file "$primary_oidc_file" --second-token-file "$second_oidc_file" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT" \
  --output "$row_directory/traces/authorized_cache_reuse.json"
```

Crab Auth authenticates and authorizes each token before issuing the
corresponding read grant. The collector verifies a first miss and a second
hit on the same complete bytes while the grants are live. It requires
different signed expiration times to prove the edge saw distinct viewer
credentials. The trace retains
fingerprints and request observations. OIDC tokens stay in the issuer
exchange. Only the viewer credentials go to the CDN.

To collect the same trace in GitHub Actions, dispatch **Provider contract
qualification** with `cdn_transport` and `cdn_cache_reuse` for the selected
private provider. Attach the `cdn-cache-reuse` label to one protected Linux
runner in the `provider-cdn-qualification` group. Both jobs use that runner
and its stable network egress so they can reach the same edge cache. Restrict
the runner group to trusted qualification workflows and install the cloud
provider CLI used by the deployment inspector.

Configure `provider-cdn-qualification-<provider>-cache-primary` in addition
to `provider-cdn-qualification-<provider>`. Use the same approved fixture JSON
and digest in both environments. Set the environment secret
`CRAB_CDN_CACHE_REUSE_PRINCIPALS` to the approved principal descriptor and
the variable `CRAB_CDN_CACHE_REUSE_SHA256` to the registered digest in both
environments. Configure each environment's read-only
provider inspection identity. In Crab Auth, authorize the environments'
distinct GitHub OIDC subjects as the primary and second test readers of the
same isolated repository. Restrict both environments to trusted refs.
[GitHub environment OIDC subjects](https://docs.github.com/en/actions/reference/security/oidc#filtering-for-a-specific-environment)

The primary job reads the uncached object first. The second job downloads its
redacted observation, validates the commit, attempt, runner, and fixture, and
uses its own live GitHub ID token to obtain a separate viewer grant. It
requires a cache hit for the same verified bytes before running the ordinary
transport canary. The transport artifact supplies
`authorized_cache_reuse.json` to release assembly. For another attempt,
start with an uncached fixture object and rerun both jobs.

### Verify scoped-view isolation

For a private release row, prepare a protected-view fixture for its exact
provider, consumer, read family and profile. Use the row's fixture if it
already targets a materialized view. For a repository-local row, approve a
companion fixture on a dedicated protected-view deployment. Materialize the
target and a sibling generation or scope under the same test repository and
native origin. The sibling descriptor contains `view_suffix`, `object_path`,
`size`, and `sha256`. Use the same read family in both views, including the
projected `.crab` prefix for bucket-global families.

Run `collect_cdn_scoped_view.py` with the first three options below before
preregistration. Copy its `scoped_view_sha256` into the private row's
benchmark registration. The digest binds the deployment approval, source
repository, both view generations or scopes, immutable prefixes, and object
digests. Supply the target's short-lived viewer grant and a read-only signed
native GET URL for the sibling's exact physical object. The native URL uses
the same provider signing requirements described above.

```bash
python3 -B crab/scripts/e2e/collect_cdn_scoped_view.py \
  --fixture "$approved_protected_fixture_json" \
  --configuration-sha256 "$approved_protected_fixture_sha256" \
  --sibling "$sibling_descriptor_json" \
  --scoped-view-sha256 "$approved_scoped_view_sha256" \
  --row-configuration-sha256 "$approved_configuration_sha256" \
  --viewer-file "$protected_viewer_file" --sibling-read-url-file "$sibling_read_url_file" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT" \
  --output "$row_directory/traces/scoped_view_isolation.json"
```

The collector checks the deployed policy before and after the probes. It
verifies the exact target bytes on warm cache hits and the exact sibling
bytes through its native read grant on both sides of the denial. The target
viewer must receive 401 or 403 on both full and ranged sibling GETs. Keep
the credentials in protected files on the measurement worker. The trace
retains only fingerprints and redacted observations.

### Run the hosted transport canary

These traces cover part of release evidence. Product actions and two-region
measurements have their own requirements.

To collect deployment policy, GET, range, route denial, and origin access traces in GitHub Actions,
dispatch **Provider contract qualification** with `cdn_transport` enabled and
select the native provider. The canary requires a passing native report from the same
commit, run, and attempt. Create the corresponding `provider-cdn-qualification-s3`,
`provider-cdn-qualification-gcs`, or `provider-cdn-qualification-azure` GitHub
environment and restrict deployments to trusted refs. Set these environment
variables using the existing approved test fixture:

- `CRAB_CDN_TRANSPORT_FIXTURE` contains the exact approved fixture JSON.
  Its `deployment` object contains the provider's control-plane identifiers.
- `CRAB_CDN_TRANSPORT_FIXTURE_SHA256` contains the digest of that exact JSON.
  The job writes the variable bytes without adding a newline.
- `CRAB_CDN_TRANSPORT_VIEWER` is an environment **secret** for private fixtures
  that use an externally issued primary grant. Use a scoped credential valid
  through the probes. Leave it unset for public fixtures and private fixtures
  that configure `viewer_issuer`.
- `CRAB_CDN_TRANSPORT_UNRELATED_VIEWER` is the optional environment **secret**
  for the private control grant. Set it to collect the required unrelated-viewer
  trace using the fixture's approved `unrelated_control` object.

| Provider | `deployment` object keys | Read-only OIDC identity variables |
| --- | --- | --- |
| Amazon S3 | `distribution_id`, `region` | `CRAB_CDN_AWS_INSPECTOR_ROLE_ARN` |
| Google Cloud Storage | `project`, `forwarding_rule` | `CRAB_CDN_GCP_WORKLOAD_IDENTITY_PROVIDER`, `CRAB_CDN_GCP_INSPECTOR_SERVICE_ACCOUNT` |
| Azure Blob | `subscription`, `resource_group`, `profile` | `CRAB_CDN_AZURE_INSPECTOR_CLIENT_ID`, `CRAB_CDN_AZURE_TENANT_ID` |

Scope each identity's federation trust to the repository's corresponding
`provider-cdn-qualification-<provider>` environment. Grant read access to
the designated fixture's control plane and the approved object at its live
native origin. Use `s3:GetObject` on the exact S3 object ARN,
`storage.objects.get` scoped to the approved GCS object, or a Blob Data Reader
assignment scoped to the fixture container and the approved blob path. The
private CloudFront inspector also needs `s3:GetBucketLocation` on the fixture
bucket. It reads the location with the distribution's account as the expected
bucket owner. [S3 bucket location and expected owner](https://docs.aws.amazon.com/cli/latest/reference/s3api/get-bucket-location.html)

The hosted canary includes the authenticated native read in its artifact. It
records the registered deployment policy digest before and after transport
probes and uploads both
kinds of evidence only when all checks pass. A requested canary must finish
successfully before the same attempt can publish the release evidence.
[GitHub OIDC subjects](https://docs.github.com/en/actions/reference/security/oidc) · [Google Workload Identity Federation](https://github.com/google-github-actions/auth) · [Azure OIDC login](https://github.com/Azure/login)

For a hosted private fixture, add a `viewer_issuer` object to the approved
fixture JSON. Set `endpoint` to the dedicated Crab Auth HTTPS
`/v1/credentials` endpoint and `audience` to that service's
`CRAB_AUTH_AUDIENCE`. Set `approval_sha256` to the reviewed binding digest
pinned by the issuer. Configure the service for GitHub's issuer and JWKS using
its OIDC discovery document. Authorize the exact protected workflow subject
for `fetch` on only the test repository, with read permissions. Use the
repository's actual subject format, including immutable owner and repository
IDs where configured. Keep the service's cloud role, signing key, and viewer
export scoped to that fixture. [GitHub OIDC discovery](https://token.actions.githubusercontent.com/.well-known/openid-configuration) · [GitHub environment subjects](https://docs.github.com/en/actions/reference/security/oidc#filtering-for-a-specific-environment) · [Crab Auth configuration](../../deploy/auth-service/README.md)

The canary requests the GitHub token and primary viewer grant after passing the
deployment preflight. It checks the provider, native origin, viewer authority,
physical view, signed object-family prefix, and expiry before the first edge
request. After the transport and native reads, it requests a separate viewer
grant for the expiry check. It verifies an exact full cache hit and two
overlapping range reads of the same object, waits through the signed expiry,
then checks that all three reads are denied with the same grant. Both traces
contain the signed expiry and a common grant fingerprint. Viewer credentials
stay in the collector process. The native inspection role supplies the
separate native read. The two-principal check uses the protected viewer jobs
or the standalone collector on the measurement worker.

Download `provider-cdn-transport-<provider>-<run-id>-<attempt>` for the redacted
traces. For an approved fixture using an externally issued grant, collect
the expiry pair with the standalone collector and pass `--viewer-file` and
`--viewer-expires-at` with the exact signed expiry.

### Record the deployed policy

Run the provider's collector before preregistration to inspect the candidate
digest. In the draft fixture, use 64 zeros as `deployment_policy_sha256`.
Compute `configuration_sha256` from those exact draft bytes. After reviewing
the deployment, set `deployment_policy_sha256` to the observed digest in both
the fixture and benchmark registration, then recompute `configuration_sha256`
for the final fixture.

For the CloudFront reference fixtures, use the read-only deployment collector
with the `crab` AWS profile or a dedicated inspection role. Supply the
distribution ID and the S3 bucket region. The collector checks the control
plane for the approved default hostname and S3 bucket, the active viewer
function, signed origin access, cache and response policies, viewer keys, and
S3 access policy. For public delivery it checks the export bucket and its
create-only writer policy. It emits component hashes and a policy digest.
[CloudFront configuration reads](https://docs.aws.amazon.com/cli/latest/reference/cloudfront/get-distribution.html) · [S3 bucket policy reads](https://docs.aws.amazon.com/cli/latest/reference/s3api/get-bucket-policy.html)

```bash
python3 -B crab/scripts/e2e/collect_cloudfront_deployment_policy.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$configuration_sha256" \
  --profile crab --region "$bucket_region" --distribution-id "$distribution_id"
```

For Cloud CDN, use a `gcloud` configuration with read-only permissions for
the test project, its global load balancer, edge policy, bucket metadata and
conditional IAM policy. Public export also requires read access to the two
custom IAM roles. The collector checks DNS, the HTTPS proxy and certificate,
the URL map and backend bucket, the complete enforced Cloud Armor rules,
and bucket IAM. Review inherited access to the test project and bucket as
part of deployment approval. [Compute Engine resources](https://docs.cloud.google.com/compute/docs/reference/rest/v1) · [Cloud Storage conditional IAM](https://docs.cloud.google.com/storage/docs/json_api/v1/buckets/getIamPolicy)

```bash
python3 -B crab/scripts/e2e/collect_cloud_cdn_deployment_policy.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$configuration_sha256" \
  --project "$test_project" --forwarding-rule "$forwarding_rule" \
  --configuration "$gcloud_configuration"
```

For Front Door, sign in to the Azure CLI with an identity that has read
access to the fixture resource group, including the APIM viewer policy and
the Front Door identity's role assignments. The collector reads the
deployed route, WAF association and rules, and Blob access settings. For
private delivery, it compares the active APIM XML with the reviewed policy
and checks that WAF restricts socket addresses to the gateway and that the
Front Door identity is scoped to the Blob container. Verify the gateway's
observed egress and direct Front Door denial during live qualification.
[APIM policy read](https://learn.microsoft.com/en-us/rest/api/apimanagement/api-policy/get?view=rest-apimanagement-2024-05-01) · [Front Door security policies](https://learn.microsoft.com/en-us/rest/api/frontdoor/azurefrontdoorstandardpremium/security-policies/list-by-profile) · [Azure role assignment scope](https://learn.microsoft.com/en-us/rest/api/authorization/role-assignments/list-for-scope?view=rest-authorization-2022-04-01)

```bash
python3 -B crab/scripts/e2e/collect_front_door_deployment_policy.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$configuration_sha256" \
  --subscription "$subscription_id" --resource-group "$resource_group" \
  --profile "$front_door_profile"
```

Repeat during the measurement attempt with
`--output "$row_directory/traces/deployment_policy.json"`, `--source-sha`,
`--run-id`, and `--run-attempt` from the qualification workflow. The collector
writes the trace only when the observed digest matches the approved fixture.
Keep deployments fixed during measurement.

Each provider's release row requires a `crab.cdn-deployment-policy/v1`
trace with the same `source_sha`, `scope`, `configuration_sha256`,
`workflow_run_id`, and `workflow_run_attempt` as the row, plus
`deployment_policy_sha256` and a `components` object of named SHA-256 digests.
Run the provider's collector on the protected measurement worker. The
aggregate is the SHA-256 of compact sorted-key JSON containing `edge`,
`access`, and `components`, as computed by `deployment_policy_digest` in
`crab/scripts/cdn_qualification.py`. The release verifier recomputes it and
compares it to the registered digest. Run the transport probe against the
same fixture for independent data-plane evidence.

### Inspect a private deployment binding

Each private policy digest includes a `private-binding` component. It covers
the observed CDN resource, the native bucket or container identity and
location, the repository and view prefixes, and the HTTPS viewer URL.
Recompute existing private fixture digests and benchmark registrations before
running canaries with this policy component.

Run `crab/scripts/e2e/run_cdn_transport_checks.py` with
`--attest-deployment --native-identity-read --private-binding-candidate <file>`
and the normal private fixture, viewer and run inputs. The candidate file must
be new and outside the trace directory. This collector takes fixtures in the
`crab-cdn-qualification/` namespace. Crab writes the file after the native and
edge return the approved bytes, the route probes pass, and the deployment
still matches the preregistered policy. The file contains provider resource
identifiers and digests. Keep it with the operator's deployment records. It
contains no viewer grants. Review and pin the file in Crab Auth's trusted
policy before running hosted private canaries with issuer-granted viewer
credentials. Bootstrap with the test deployment's operator-signed viewer
credential file. [Crab Auth approval and required permissions](../../deploy/auth-service/README.md#cdn-viewer-grants)

### Collect approval for a customer repository

Use `crab/scripts/e2e/collect_private_cdn_binding.py` on an operator
workstation to attest a private deployment using the reviewed provider
policies. The operator can supply the logical repository prefix that the
deployment actually serves. Choose a separate, unused
`crab-cdn-qualification/<name>` prefix for the denial probe. Use an existing
immutable object of 2 bytes to 1 MiB and compute its SHA-256 from known-good
bytes. For a materialized view, set `view_suffix` to its exact deployed
suffix and select an object in that view.

This example describes an LFS object containing `abc`. Substitute the
deployment IDs, origin and object details for your deployment.

```json
{
  "scope": "cloudfront/private/cli/lfs/direct",
  "base_url": "https://fixture.cloudfront.net",
  "native_origin": "s3://example-repository-bucket",
  "fixture_prefix": "org/models",
  "neighbor_prefix": "crab-cdn-qualification/models-denial-probe",
  "view_suffix": "",
  "object_path": "lfs/objects/ba/78/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  "size": 3,
  "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  "deployment": {"distribution_id": "EXAMPLE", "region": "us-west-2"}
}
```

For GCS use `cloud-cdn` with the active gcloud identity and the
`project` and `forwarding_rule` deployment fields. For Azure Blob use
`front-door` with the active Azure CLI identity and the `subscription`,
`resource_group` and `profile` deployment fields. The examples above
describe the required controls for every provider.

First record the SHA-256 of the reviewed binding JSON. With the AWS profile
`crab`, run the inspection below. Review the observed distribution, origin,
repository and viewer URL and retain `deployment_policy_sha256`.

```sh
AWS_PROFILE=crab python3 crab/scripts/e2e/collect_private_cdn_binding.py \
  --binding operator-binding.json --configuration-sha256 "$binding_sha256"
```

Repeat with the approved digest and a short-lived viewer credential.
Use `--viewer-file` with a credential scoped to the declared object family,
or install the Crab Auth Python dependencies and sign the probe locally.
The local key is opened only after the deployment policy matches. The
probe credential stays in memory. The probe uses the provider login to
read the exact native object, or accepts a signed native URL from
`--native-read-url-file`.

```sh
AWS_PROFILE=crab python3 crab/scripts/e2e/collect_private_cdn_binding.py \
  --binding operator-binding.json --configuration-sha256 "$binding_sha256" \
  --deployment-policy-sha256 "$policy_sha256" \
  --signing-key-file "$viewer_key_file" --signing-key-id "$viewer_key_id" \
  --output private-binding.json
```

Review the candidate and pin the reported `approval_sha256` in Crab Auth
and in the client's private binding.
The native and edge reads, warm authorization and out-of-scope denial
probes must pass, and the deployment policy must still match afterward.
The release workflow keeps its separate fixture namespace and workflow
provenance. [Crab Auth approval setup](../../deploy/auth-service/README.md#cdn-viewer-grants)

### Bind public export evidence to the verified object

Keep the exact approved `inventory.json` and the publisher's
`publication.json` under `<scope>/<platform>/public-export/` in the measured
inputs. Use the publisher-reported `publication_sha256` in that row's
benchmark registration. The assembler keeps both files in the release
artifact. Use the dedicated qualification export's approved public inventory.

Use separate buckets for S3 and GCS. Use a separate Azure storage account
for the Front Door export so the export's anonymous access setting applies
only to that account. [Azure anonymous access isolation](https://learn.microsoft.com/en-us/azure/storage/blobs/anonymous-read-access-overview)

Set the public fixture's `source_placement` to the exact object from the
approved inventory. The canary identity needs native placement metadata
access on the source as well as the export. Grant source bucket location
access for S3, source bucket metadata access for GCS, or source storage
account and container read access in Azure Resource Manager.

Public S3 activation calls `HeadBucket` with the selected credentials and the
approved bucket owner. Grant `s3:ListBucket` on the source bucket to the
reader used by the qualification action. Public GCS activation calls
`buckets.get` with the selected credentials. Grant `storage.buckets.get`
on the source bucket to that reader. The activation verifies the location
from the same call and rejects a missing or mismatched proof before sending
an edge request.

Public Azure activation uses the runtime Azure Entra credential chain to
obtain a token for Azure Resource Manager. Give that identity
`Microsoft.Storage/storageAccounts/read` and
`Microsoft.Storage/storageAccounts/blobServices/containers/read` at the
approved source. Crab verifies the account and container resource IDs,
primary location, and Blob endpoint. Use `AZURE_CREDENTIAL_KIND` to select
the existing credential kind when the runtime has multiple identities.
Blob bearer tokens, SAS tokens, and account keys remain scoped to data access.
For an explicit-credential SDK checkout, the Git helper can obtain its ARM
identity from Azure CLI login or managed identity after the SDK scopes the
child's storage environment. Use `azure_from_env` when the helper needs the
launcher-provided Entra environment credential chain.
[S3 HeadBucket](https://docs.aws.amazon.com/AmazonS3/latest/API/API_HeadBucket.html) · [GCS Buckets: get](https://docs.cloud.google.com/storage/docs/json_api/v1/buckets/get) · [Azure storage account properties](https://learn.microsoft.com/en-us/rest/api/storagerp/storage-accounts/get-properties?view=rest-storagerp-2025-08-01) · [Azure container properties](https://learn.microsoft.com/en-us/rest/api/storagerp/blob-containers/get?view=rest-storagerp-2025-08-01)

Recollect public deployment snapshots before preregistration. They include
an export component that binds the actual distribution or backend and its
deployed inventory digest to the independently observed resource identities
and locations of the source and destination, repository, and viewer hostname.
The transport collector records `approved_public_export.json`
from the same successful full GET as `exact_get.json`, with the observed
SHA-256 of the body.

The release verifier checks the registered receipt digest, compares its
whole object list with the original inventory, and checks its binding
against the attested deployment. The full and ranged edge reads, native
read and denial, and export access probe must describe the same receipted
object. The recorded body digest and length must match its inventory entry.
The cleanup trace must name the receipt's source and destination.

### Verify a public export transition

Retain the active inventory and sealed receipt under
`public-export/before/` before the native publish. Register its digest as
`previous_publication_sha256`. Keep the current inventory and sealed receipt
at the top level of `public-export/`, with its digest in `publication_sha256`.
The publisher records its seal time after destination readback.

For direct CLI Git, capsule and LFS qualification, use the Git workload
to publish the test ref and retain its publication receipt. For Git and
capsule, identify the new repository-relative immutable object selected
by that ref. For LFS, place the matched `crab` executable beside the
Git helper. The worker stages the payload with `crab lfs clean`, publishes
its pointer with ordinary Git push and derives the exact LFS object key
from the publication receipt. Omit `--transition-object` on the LFS row.
Fetch while the earlier receipt is active and excludes that object. Use the same
ref, object and approved fixture on both fetches. Each fetch starts with
fresh Git and Crab caches.

```bash
git_workload_args=(
  --fixture "$approved_fixture_json" --configuration-sha256 "$configuration_sha256"
  --receipt "$row_directory/git-product/publication.json"
  --scratch "$scratch_directory" --helper "$qualification_helper"
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT"
)
python3 -B crab/scripts/e2e/run_cdn_git_workload.py fetch-origin "${git_workload_args[@]}" \
  --transition-object "$new_object_path" --export-approval "$row_directory/public-export/before" \
  --transition-output "$native_only_observation"
```

Stage the updated inventory with public delivery disabled. Preserve existing
approved entries and add the new object. Use the provider's public publisher
to verify and seal it. Re-enable the dedicated export, activate the new
receipt in the trusted Crab configuration and repeat the normal fetch.

```bash
python3 -B crab/scripts/e2e/run_cdn_git_workload.py fetch "${git_workload_args[@]}" \
  --transition-object "$new_object_path" --export-approval "$row_directory/public-export" \
  --before-transition "$native_only_observation" \
  --transition-output "$row_directory/traces/public_export_transition.json" \
  > "$row_directory/git-product/fetch.json"
```

The worker checks the active receipt digest and exact object fingerprint
reported by Crab. The first fetch must see the object as unlisted and
make zero CDN payload requests. The second fetch must accept verified edge
bytes for that object. On the LFS row, the worker first fetches the pointer
commit, then measures `crab lfs fetch` in the fresh repository and hashes
the resulting local object. After checking the complete SHA-256, Crab
reports the fingerprint of the physical LFS key for edge delivery or a
verified local cache hit. The CLI and service measurement workers match
that fingerprint to the object approved for the run. The release verifier
checks both receipts, the native ref, the seal time, and the verified result.

### Verify retained native versions

Use a dedicated public qualification source with retained versions of the
fixture object. Keep earlier fixture bytes private and export only the
approved current bytes. Record each retained selector in an operator-reviewed
`history.json` with a `versions` array. Each entry has `kind`, `selector`,
`size`, and `sha256`. Use `versionId` for S3, `generation` for GCS, and
`versionid` or `snapshot` for Azure. The collector checks one to eight
retained versions of up to 1 MiB each.

Run the collector with the fixture and history inputs, omitting `--output`
to print the candidate digest without network requests. Preregister it in
the benchmark row as `historical_origin_sha256`. Run the collector again on
the protected measurement worker and retain the trace with the other
same-attempt observations.

```bash
python3 -B crab/scripts/e2e/collect_cdn_historical_origin.py \
  --fixture "$approved_fixture_json" --configuration-sha256 "$configuration_sha256" \
  --history "$approved_history_json" --historical-origin-sha256 "$historical_origin_sha256" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT" \
  --output "$row_directory/traces/historical_origin.json"
```

Use the source inspection role. S3 needs `s3:GetObjectVersion` on the
approved fixture object and can select a local profile with `--aws-profile`.
GCS uses `storage.objects.get`; Azure uses Blob data read authority. The
collector checks the complete historical bytes using native authorization
and then requires anonymous denial for the same selector. Retained evidence
contains digests and status codes. [S3 version reads](https://docs.aws.amazon.com/AmazonS3/latest/userguide/RetrievingObjectVersions.html) · [GCS generation reads](https://docs.cloud.google.com/storage/docs/xml-api/get-object-download) · [Azure version and snapshot reads](https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob)

### Verify viewer signing-key retirement

In the private benchmark registration, record `deployment_policy_sha256`
for the deployment with only the replacement key trusted. Record
`rotation_overlap_policy_sha256` for the same deployment while it trusts both
keys. Use this approved overlap digest when starting the collector on the
protected measurement worker.

Issue two short-lived credentials for the same fixture object, one per key.
The collector accepts grants with up to 900 seconds remaining.
Keep the files private and use the current provider's read-only inspection
identity. Start the collector during overlap. After both credentials return
the approved bytes, the collector tells the operator to retire the old key.
It checks the deployed policy until the approved retired state is observed
or either grant expires. CloudFront attestation also requires the
distribution's active viewer keys to match the configured key group. The
profile stays unqualified if the edge probes cannot finish within both
signed lifetimes. [CloudFront active trusted keys](https://docs.aws.amazon.com/cloudfront/latest/APIReference/API_ActiveTrustedKeyGroups.html)

```bash
python3 -B crab/scripts/e2e/collect_cdn_key_rotation.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$configuration_sha256" \
  --overlap-policy-sha256 "$overlap_policy_sha256" \
  --retired-viewer-file "$old_viewer_file" \
  --replacement-viewer-file "$new_viewer_file" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" \
  --run-attempt "$GITHUB_RUN_ATTEMPT" \
  --output "$row_directory/traces/key_rotation.json"
```

The collector confirms a full cache hit with the replacement key before and
after denying both a full GET and a one-byte range with the old grant. It
attests the retired policy on both sides of these probes. The release verifier
checks both registered policy digests, the object against the ordinary
transport trace, and the signed lifetime of each tested grant. The operator
performs the provider change while the collector reads policy and object
bytes and retains redacted observations. [CloudFront key rotation](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-trusted-signers.html) · [Cloud CDN key retirement](https://docs.cloud.google.com/cdn/docs/using-signed-cookies) · [APIM signer policy](https://learn.microsoft.com/en-us/azure/api-management/validate-jwt-policy)

### Collect managed issuer observations

Use the issuer owner's protected repository-local fixture with the approved
native placement. Preregister the `managed_issuer` identity, service version
and private binding approval in that row's benchmark registration. The
identity digest is SHA-256 of the UTF-8 authority, the parsed API base URL
with its trailing slash, and the advertised OIDC issuer, joined by NUL bytes.
Use the service version from the authority-bound discovery document.

Run the same product read in a fresh process and cache for each role.

- `approved_v2` uses the approved binding and verifies a CDN payload.
- `v1_peer` requests v1 and reads the same fixture bytes from the approved
  native target.
- `denied_v2` receives HTTP 403 from the issuer after authenticated
  resolution.
- `mismatched_v2` receives HTTP 201 with a different binding approval from
  the issuer's controlled fixture. The client rejects that grant.

For private CLI Git, include the exact managed URL (for example,
`crab://issuer.example/org/repo`) as `managed_url` in the approved fixture JSON.
Leave `view_suffix` empty. Register `managed_issuer.repository_sha256` as the
SHA-256 of the 16 UUID bytes returned by authenticated managed resolution.
The native repository's own UUID and epoch are observed independently.
Publish the fixture once with
`run_cdn_git_workload.py publish` and use that receipt across all four arms.
The worker publishes and retires the temporary ref through the native bucket
and runs ordinary `git fetch` against the managed URL. The guard checks the
logical URL, native origin, physical prefix, active viewer binding, and
isolated cache before admitting repository reads.

Run `run_cdn_git_workload.py` with the same fixture, receipt, candidate, and
workflow identity for every arm on the protected worker. Configure the
issuer's protected test policy for each role before the corresponding
invocation.

| Role | Workload action | Extra argument |
|---|---|---|
| `approved_v2` | `fetch` | `--managed-issuer-capture <roles>/approved_v2` |
| `v1_peer` | `fetch-origin` | `--managed-issuer-capture <roles>/v1_peer` |
| `denied_v2` | `fetch-rejected` | `--managed-issuer-capture <roles>/denied_v2` |
| `mismatched_v2` | `fetch-rejected` | `--managed-issuer-capture <roles>/mismatched_v2` |

The v1 policy must return a direct native grant for the same physical
repository. Each action creates its role directory with the allowlisted
trace in `stderr.jsonl`, the Git process status in `exit-code`, the redacted
`client.json`, and the verified `payload` for a successful fetch.
Copy the exact captured `approved_v2/client.json` to `git-product/fetch.json`
and `v1_peer/client.json` to `traces/rollback.json` in the measured row.
Use those same bytes when collecting the product fetch's provider viewer
requests. For another managed consumer,
capture those same files from its normal product operation with
`CRAB_LOG=crab_auth::managed::cdn=debug,crab_storage::cdn::transfer=debug,crab_storage::native_read=debug`.

```bash
python3 -B crab/scripts/e2e/collect_cdn_managed_issuer.py \
  --fixture "$approved_fixture_json" --configuration-sha256 "$configuration_sha256" \
  --benchmarks "$registration_file" --benchmarks-sha256 "$benchmarks_sha256" \
  --observations "$row_directory/observations.json" \
  --deployment-policy "$row_directory/traces/deployment_policy.json" \
  --actions "$protected_role_directory" \
  --output "$row_directory/traces/managed_v2_issuer.json"
```

The collector requires one authenticated resolution and one grant exchange
per role. It checks the issuer, logical repository and native placement,
then derives the four outcomes from the observed HTTP status, client's
decision, transfer events and verified product bytes. Keep the working logs
and payload files on the protected worker. Retain the redacted issuer trace
with the row. For managed CLI Git, release verification matches the exact
fetch and rollback digests and their transfer counters. A protected production
issuer run is still required. Other managed consumers need their own
product-specific verifier.

### Measure CDN transfers during product actions

Capture stderr from the normal product operation with
`CRAB_LOG=crab_storage::cdn::transfer=debug` and a redirected, noninteractive
stderr. The CDN transport emits one redacted event per product attempt.
The event identifies the provider, access mode, consumer, read family and
authorization profile. It includes HTTP status, received bytes, elapsed time,
cache disposition, the expected object and request lengths, and whether the
request used a range. Standalone doctor probes do not produce these events.

```bash
CRAB_LOG=crab_storage::cdn::transfer=debug git fetch origin 2>"$fetch_stderr"
python3 -B crab/scripts/e2e/collect_cdn_product_metrics.py \
  --stderr "$fetch_stderr" --scope cloudfront/private/cli/git/direct
```

Retain only the reduced records. When a protected worker joins product reads
with provider logs, the per-request record preserves the read family, size,
range flag, and elapsed time alongside the result. A completed transfer proves
exact HTTP framing and delivery. Git payload readers emit a separate
observation after accepting an authenticated pack digest or a reconstructed object ID. A
discarded edge batch reports no verified payloads. For xorb evidence, pass
`--xorb-key-sha256` with the SHA-256 of the approved physical object key,
including the projected view's prefix and without a leading slash. The
xorb reader reports this fingerprint after it verifies nonempty requested
chunks from the edge. Retain the action's separately verified output bytes
or digest with the transfer evidence. The sum of parallel
transfer times differs from elapsed action time. Front Door's response
header describes the first 8 MB chunk, so Crab records an unknown cache
disposition for larger objects. Use provider logs for their cache and
origin-fill evidence. The benchmark counts a completed request's provider
cache result when the client reports an unknown result. A partial Front Door
hit remains unknown. [Front Door caching documentation](https://learn.microsoft.com/en-us/azure/frontdoor/front-door-caching#response-headers) ·
[Front Door access log fields](https://learn.microsoft.com/en-us/azure/frontdoor/standard-premium/how-to-monitor-metrics)

Record a verified release scope as five space-separated columns in
`crates/crab-storage/src/cdn/qualified-scopes.txt`. After the scope, record the
maximum full-GET bytes, the maximum object size for ranged requests, the
maximum bytes in one range, and the operation deadline in milliseconds. Zero
in a byte column disables that request form. Set both range columns to zero
for full GETs only. The verifier checks the candidate's compiled policy digest
and compares its provider-correlated benchmark requests with the row's limits.
On every release platform, the retained `warm_edge` samples must contain a
provider-confirmed cache hit that completed at the exact byte limit for each
enabled form. A ranged hit must reach both the object-size and single-range
limits in the same request. The hit may come from either registered region.
Both regions still need the full set of performance measurements.
Use the qualification build's request records to choose the candidate limits,
then repeat the workload on the ordinary release build with that policy.

For Crab Server and the S3 gateway, start the candidate in a dedicated process with
`RUST_LOG=crab_storage::cdn::transfer=debug,crab_storage::cdn::read_target=debug,crab_storage::cdn::service_build=debug,crab_storage::native_read=debug`.
For LFS, append `,crab_storage::cdn::service_lfs_cache=debug` to capture the
actual disk cache root selected for each CDN-enabled read.
On Cloud CDN, append `,crab_storage::cdn::gcs_viewer_log=trace` to correlate
each request with Cloud Logging. Redirect stderr to a protected append-only
file from startup. The worker matches the authenticated read's process to its
startup record and retains the executable SHA-256 and embedded build metadata.
Keep the process idle except for the measured request because the worker
uses the bytes appended during that request to isolate storage observations.
Use a fresh process and cache for each arm.

Use the native `publish` action below with the direct service fixture. For
LFS, pass `--service-lfs-payload` with the file whose size and SHA-256 the
fixture approves. The writer checks the bytes before pushing and commits their
LFS pointer. For Git, prepare a layered checkpoint that places the fixture
blob in a physical pack layer. Run `collect_cdn_git_native_ref.py published`
against the same fixture and publication receipt before either service read.

Seal a `crab.cdn-service-workload/v1` JSON file with `schema`,
`configuration_sha256`, `publication_sha256`, the service HTTPS `endpoint`,
logical `repository`, the full Git `commit`, `payload_sha256`,
`payload_bytes`, and the SHA-256 of the strong ETag including its quotes
in `etag_sha256`. Set `public_export_sha256` to the digest of the independently
approved export's `publication.json` for a public row, or the empty string for
a private row. Publish the fixture file as `cdn-qualification.bin`.
Use the Crab Server repository name as `owner/repo` or the S3 gateway bucket
name. Put the dedicated viewer credential in a separate protected JSON file
with `cookie` for a Crab browser session, `authorization` for the Git LFS
Basic header, or `access_key`, `secret_key`, and `region` for S3 gateway SigV4.

For a public transition, enable
`crab_storage::cdn::public_transition=debug` in both service processes. Seal
the prior export before publishing the native ref. Use a workload with that
receipt's digest in `public_export_sha256` and run `prior_export` with the
prior `--export-approval` directory and `--transition-object` set to the exact
new repository-relative pack-layer or LFS key. The worker verifies that the
active old receipt omits the key and that the ordinary request returns the
approved native bytes with zero CDN attempts. Keep the result as
`service-product/before-export.json`. After this read, publish and seal the
new export. Keep the endpoint, logical repository, published commit, payload,
and ETag the same, and seal a second workload with the new receipt digest.
Start a fresh service with the new receipt and pass the same
`--transition-object` to the edge action. Retain viewer logs that confirm
delivery of that exact object.

```bash
python3 -B crab/scripts/e2e/run_cdn_service_read.py edge \
  --fixture "$fixture" --configuration-sha256 "$configuration_sha256" \
  --workload "$workload" --workload-sha256 "$workload_sha256" \
  --publication "$publication_receipt" --native-published "$native_published" \
  --credentials "$credentials" --service-log "$service_log" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" \
  --run-attempt "$GITHUB_RUN_ATTEMPT" >"$edge_read"
```

Pass the edge report as `--client` to `collect_cdn_viewer_requests.py` with
the approved provider log exports. For a public edge read, also pass
`--export-approval` to the read worker with the directory containing the
sealed export receipt and inventory. The worker verifies the service's
observed native origin, physical prefix, viewer binding, and loaded public
receipt. The log collector checks every viewer request and matches LFS reads
to the exact approved storage key. Withdraw the CDN binding, restart the
service with a fresh cache, and run the same workload with `origin` and
without `--export-approval`. The rollback worker requires the same native
target and payload and an empty viewer binding. After rollback, retire the
temporary ref, collect the native `retired` observation, then remove the
isolated fixture and retain the cleanup inventory. Bind the records with
`verify_cdn_service_lifecycle.py`:

```bash
python3 -B crab/scripts/e2e/verify_cdn_service_lifecycle.py \
  --fixture "$fixture" --configuration-sha256 "$configuration_sha256" \
  --workload "$workload" --workload-sha256 "$workload_sha256" \
  --publication "$publication_receipt" --retirement "$retirement" \
  --native-published "$native_published" --native-retired "$native_retired" \
  --edge "$edge_read" --origin "$origin_read" --viewer-requests "$viewer_requests" \
  --cleanup "$cleanup" --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" \
  --run-attempt "$GITHUB_RUN_ATTEMPT" >"$service_lifecycle"
```

The verifier requires a distinct tombstone in the same epoch, matching storage
targets, and cleanup after both reads and retirement. Use the verified files
as assembly inputs for the service fixture row.

- Keep `edge.json`, `viewer-requests.json`, `publication.json`,
  `retirement.json`, `provider-published.json`, and `provider-retired.json` in
  `service-product/`. Keep the origin read in `traces/rollback.json` and the
  cleanup inventory in `traces/cleanup.json`.
- Register `workload: "service-http-read"` and `viewer_binding_sha256` in
  the benchmark row. Set `service_workload` to the approved workload file's
  `sha256`, the native origin hash in `origin_sha256`, the hash of the physical
  prefix with its trailing slash in `prefix_sha256`, and the approved
  `etag_sha256` and `payload_bytes`.
- For public rows, register the digest of the prior workload as
  `service_prior_workload_sha256`, retain both sealed export inventories, and
  pass `--before-export`, `--prior-workload`, and `--prior-workload-sha256`
  to the lifecycle verifier. Assembly rebuilds the transition trace from the
  retained prior and edge reads.
- Register `service_candidate` with the approved executable's
  `binary_sha256`, `source_sha`, `target_triple`, `cargo_profile`,
  `cargo_features`, `qualification_feature`, and `release_policy_sha256`.
  Compare the startup record with the SHA-256 of the binary from the approved
  container image before registration. Container builds pass the commit using
  the `GIT_SHA` build argument and compile the regular `provider-cdn` feature.
  For source builds, set `GIT_SHA` to the full checked-out revision when
  invoking Cargo. Use `provider-cdn-qualification` for fixture tests.
  Service release verification requires the release profile and rejects
  the qualification feature.
- Assembly and bundle verification derive `product_action` from the
  retained lifecycle records and performance from the regional records.

For each service Git or LFS pair, run the same commit between the observed
publication and retirement. Use separate fresh service processes for the
`origin`, `cold_edge`, and `warm_edge` arms. Wait for expiry or invalidate
only the fixture objects before each cold edge sample. Keep the warm-edge service
running and repeat the request with `warm_local --primer "$warm_edge_record"`
and the same workload and service log. Retain each `client.json`, runner
`measurement.json`, and provider viewer log proof for the edge arms under
`git-benchmarks/<location>/<pair>/<arm>/`. The reducer requires a distinct
service instance for each fresh arm and checks the exact verified Git blob
or LFS object and zero edge requests in the local repeat. To validate a service
benchmark separately, pass `--service-row` to `collect_cdn_git_performance.py`
with the row directory holding `service-product/` and `traces/`.

For LFS, use a separate disk cache root for the lifecycle edge read and each
fresh edge arm. Set the gateway's cache directory separately or give Crab
Server a separate data directory for each process. The cached read and its
primer must report the same fingerprint of the opened directory. The native
rollback and prior-export read bypass the LFS cache and report no cache root.

### Verify native publication and CDN Git fetch

Use `crab/scripts/e2e/run_cdn_git_workload.py` for direct CLI Git, capsule, and
LFS scopes on an initialized test-only repository. Use the candidate's
`git-remote-crab` and the same approved fixture JSON as the transport
collector. `native_origin` identifies the S3 bucket, GCS bucket, or Azure
account and container. Configure the writer and reader credentials for that
origin and the exact `fixture_prefix`. The helper checks the effective
repository path, including any scoped view, before reading its root.
Replica candidates covered by the read policy must pass the same check
before readiness probes. Each product command reports the approved origin
and prefix as digests. The workload requires this confirmation on a
read-only preflight before sending a push. Each phase uses a fresh scratch
directory and cache.

For a larger or smaller CLI workload, set `cli_payload_bytes` in the approved
fixture and use the same value in its benchmark registration. The default is
262,144 bytes and the qualification worker accepts up to 512 MiB. The
publication receipt records the payload size and SHA-256. The worker checks
both against the fetched bytes. The size controls the logical Git blob or LFS
payload. The fixture's separate `size` field describes the transport canary
object. Use the retained viewer response sizes to set edge limits for
physical objects such as compressed Git pack layers.

Run the command below with `inspect` in place of `publish` before preregistering
the benchmark. Omit `--receipt` for inspection. It reads the exact native
fixture and returns the executable SHA-256, compiled source SHA, target
triple, Cargo profile and features, qualification feature state, and
compiled CDN policy SHA-256 under `candidate`. The policy digest uses LF line
endings on every platform. Copy `candidate`, `viewer_binding_sha256` and the
selected `cli_payload_bytes` into each direct CLI benchmark registration.
The viewer digest binds the
approved HTTPS base and access mode. The worker confirms the same binary
identity on the native publication, fetches, and ref retirement. Every timed
arm uses the registered binary for its platform. [Cargo build-script environment](https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts)

Promotion requires the packaged target triple, the ordinary release profile,
the qualification feature disabled, and the exact release checkout's policy
embedded. Use native writer credentials restricted to the approved origin
and prefix. The qualification workload publishes directly to one native
store. Set up the reader separately when measuring a private CDN that uses
Crab Auth viewer grants.
Retain provider observations for the publication and retirement in addition
to the Git records.

```bash
python3 -B crab/scripts/e2e/run_cdn_git_workload.py publish \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$approved_configuration_sha256" \
  --receipt "$publication_receipt" --scratch "$scratch" --helper "$git_remote_crab" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" \
  --run-attempt "$GITHUB_RUN_ATTEMPT"
```

`publish` creates a unique test ref with an absence lease and checks its
advertisement immediately. Keep the receipt even if the push fails. Confirm
the committed ref independently on the native provider before the fetch.
Build the `cdn-native-ref` example from the approved commit using the
measurement worker's Cargo target directory. Point `native_ref_probe` at
the resulting executable and run it with credentials
allowed to read the native fixture. On S3, the probe uses the AWS SDK's
credential chain, including `AWS_PROFILE` for a named profile. On GCS and
Azure, configure the storage client's native credentials for the fixture.
[AWS SDK for Rust credential chain](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/credproviders.html)

For a local run backed by an `aws login --profile crab` session, configure a
separate AWS process profile and set `AWS_PROFILE=crab-cdn-local` for the
Crab writer and probe. Set the fixture bucket's region in the process profile.
The AWS CLI refreshes temporary credentials while the login session is valid.
Protected CI uses workload identity. [AWS CLI login credentials](https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sign-in.html)

```ini
[profile crab-cdn-local]
credential_process = aws configure export-credentials --profile crab --format process
region = us-east-1
```

```bash
CARGO_TARGET_DIR="$measurement_cargo_target" cargo build --locked -p crab --example cdn-native-ref
phase=published
python3 -B crab/scripts/e2e/collect_cdn_git_native_ref.py "$phase" \
  --fixture "$approved_fixture_json" --configuration-sha256 "$approved_configuration_sha256" \
  --publication "$publication_receipt" --probe "$native_ref_probe" \
  --output "$row_directory/git-product/provider-$phase.json" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT"
```

Prepare any checkpoint, protected view, or public export required by the registered
read family after publication. For public runs, add the export publisher's
receipt and SHA-256 to the worker's trusted CDN binding before measuring the
edge arm. Supply the same receipt directory with `--export-approval` on public
`fetch` and `fetch-warm-local` calls. Crab compares the loaded receipt digest
with the worker's approved digest, even when the viewer endpoint is unchanged.
The workload's separate receipt records the native Git publication.
Then run `fetch` with the same arguments and
the configured reader credentials. The workload verifies the advertised
commit and blob SHA-256 and requires both a completed CDN GET and an
accepted edge Git payload for the approved scope. It emits the measured
results as redacted JSON.

Use `fetch-origin` with the same receipt on a worker configured for native
origin access. Before returning a measured payload, Crab confirms the
selected store has no viewer binding for the origin arm and the registered
viewer binding for the edge and warm-local arms. The worker verifies the
local cache root before the fetch, and Crab rejects a configured shared
cache service. The origin arm rejects edge traffic for any scope. The public
export transition uses the previous active binding in its native-only phase
and proves the approved CDN skips the unlisted object.
`fetch-warm-local` primes a fresh Crab cache with a verified edge fetch,
then measures a second fetch from an empty Git object database against that
cache. LFS keeps the primed worktree and runs a second `crab lfs fetch` for the
same verified pointer. It verifies the SHA-256 and length of the local LFS
object and records the local hit. Both controls require zero edge requests
for the registered payload. Native metadata and sidecar reads still run.
The benchmark collector rejects a local cache root reused across any two
measured arms. Join the results with provider request and fill measurements
before assembly.

For the direct CLI rollback check, withdraw the CDN binding on the same
worker after retaining the successful edge fetch. Keep the same native
origin, prefix, reader authorization, and unretired publication receipt. Run
`fetch-origin` with the same workload arguments and save its stdout as
`traces/rollback.json`. For a public row, omit the transition options so the
worker verifies that the binding is absent. The verifier checks the same
payload and native target before and after the change, separate local cache
roots, and zero edge requests and cached payload hits after rollback. It
matches the selected native target to the independent ref observation.

Run `retire` with the same arguments after the rollback fetch. It verifies the
exact ref is absent after retirement and refuses to remove a ref whose tip
changed. Retain the publication receipt unchanged as `git-product/publication.json`
under the measured row, and capture the `fetch` and `retire` stdout as
`git-product/fetch.json` and `git-product/retirement.json`. Pass that exact
edge fetch file as `--client` to `collect_cdn_viewer_requests.py` and retain
the output as `git-product/viewer-requests.json`. The collector accepts
the approved provider log exports or the AWS companion stack. For a public
transition it confirms delivery of the exact newly approved object.
Use a single worker for these ordered product phases so their timestamps
share a clock. Both phase
reports bind the exact publication receipt by digest. Record storage and CDN
fixture cleanup separately in the `cleanup` check. Immediately after `retire`,
repeat the collector with `phase=retired` before deleting any fixture objects.
The release verifier requires a committed provider tombstone with a new
transaction and revision in the same repository epoch. It checks that the
publication observation preceded the fetch and the retirement observation
preceded cleanup. The probe reads the root and exact single-ref head through
the provider SDK and uses the canonical v2 decoder to resolve the visible tip.
[Git's explicit push lease](https://git-scm.com/docs/git-push#Documentation/git-push.txt---force-with-leaseltrefnamegtltexpectgt)

### Capture SDK Git snapshot, clone, and fetch evidence

Use a direct SDK Git fixture on the repository prefix. The same native
`publish` and `retire` actions work with that fixture. Between them, run
the edge and native snapshot, clone, and fetch pairs. Build the reader
from the checkout used for the native publication.

```bash
CRAB_CDN_PROBE_SOURCE_SHA="$GITHUB_SHA" CARGO_TARGET_DIR="$measurement_cargo_target" \
  cargo build --locked -p crab-sdk --features local,service --example cdn-sdk-read

"$sdk_read_probe" --build-info

python3 -B crab/scripts/e2e/run_cdn_sdk_workload.py edge \
  --fixture "$approved_fixture_json" --configuration-sha256 "$approved_configuration_sha256" \
  --receipt "$publication_receipt" --probe "$sdk_read_probe" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT" \
  --private-reader "$sdk_reader_settings" > "$sdk_edge_observation"
```

For a private row, the reader settings contain `oidc_issuer`, `oidc_client_id`,
and the absolute `token_cache_directory`. The approved fixture supplies the
issuer endpoint and the edge approval. Run the same command with `origin`
for the native control. For a public row, replace `--private-reader` with
`--export-approval "$approved_export_directory"` for the edge read and omit
both options for the native control.

The probe opens the repository through the SDK, checks the snapshot commit,
verifies the Git blob's SHA-256 and length, and closes the client. Each arm
confirms the native origin and prefix captured by the SDK after authorization.
The edge arm confirms the attached viewer and, for public delivery, the
loaded publication receipt. Edge delivery requires the verified Git object
ID fingerprint for the exact blob returned by the SDK. The origin control
requires native body bytes, an inactive viewer and zero edge attempts.
Use the independent native ref probe described above for both publication and
retirement on the same SDK fixture. The record order is native publication,
independent ref observation, edge snapshot, origin snapshot, edge clone,
origin clone, edge fetch, origin fetch, native retirement, independent tombstone
observation, and cleanup.

Preregister `sdk-git-snapshot` as the workload. Set `sdk_candidate` to the
build info plus the SHA-256 of the probe executable as `binary_sha256`.
Register `viewer_binding_sha256` for the selected URL and access mode. A
private row also registers the issuer's `viewer_approval_sha256`. A public
row uses the registered `publication_sha256`. Fixture builds can add
`provider-cdn-qualification` to exercise unqualified HTTPS rows. The build
record captures the actual feature configuration.

Keep the edge record at `git-product/snapshot.json` and the origin record at
`traces/rollback.json`. Retain `publication.json`, `retirement.json`,
`provider-published.json` and `provider-retired.json` in `git-product`. The
assembler derives `product_action` from these records, and the release
verifier recomputes it.

For both clone arms, pass `--action clone`, `--git`, and `--crab` with the
executable paths and keep stdout as `git-product/clone-edge.json` or
`git-product/clone-origin.json`. Use the same Crab build as the native
publisher. Each invocation calls `Client::clone_local` in a fresh worktree.
The probe checks HEAD and the visible file, identifies the packs containing
the blob through their local Git indexes, and fingerprints the installed
pack bytes. The edge arm requires a matching digest verified by the CDN
pack reader. The origin arm proves the same pack is available natively
with the binding withdrawn. Use an authenticated layered checkpoint that
covers the test ref so clone and fetch have eligible immutable pack ranges.

For both fetch arms, use `--action fetch` with the same executable paths.
Keep stdout as `git-product/fetch-edge.json` or `git-product/fetch-origin.json`.
The probe starts with an identical local commit and an empty remote-tracking
namespace in each worktree. `Local::fetch` must publish the expected
remote-tracking ref and preserve local HEAD and the worktree. The probe then
checks out that ref and verifies the visible file and installed pack. The
origin fetch must install the same pack verified by the edge arm.

Pass each edge action record as `--client` to
`collect_cdn_viewer_requests.py`. Save the snapshot, clone, and fetch proof
as `git-product/viewer-requests.json`,
`git-product/clone-viewer-requests.json`, and
`git-product/fetch-viewer-requests.json`, respectively. The verifier joins
each action to its own provider requests and checks for successful delivery
with body bytes. A viewer request cannot be reused across those actions or in
a regional benchmark.

For each registered regional pair, start a fresh probe for `origin`,
`cold_edge`, and `warm_edge`. The timed window covers repository open,
snapshot capture, and reading and verifying the blob. For the warm edge
command, pass `--warm-local-output` with the path to
`git-benchmarks/<location>/<pair>/warm_local/client.json`. Redirect stdout
from that same command to its `warm_edge/client.json`. The probe times a
second call to the blob API on the pinned snapshot and retains a fingerprint
of its warm edge record. This arm measures the verified in-memory blob
cache. Native metadata reads still count if the repeat opens a catalog.

Collect the provider viewer and cache-fill evidence for both edge arms
with `collect_cdn_viewer_requests.py`. Keep the independently measured
`measurement.json` beside each `client.json` and use
`collect_cdn_git_performance.py` for the paired summary. An SDK probe can
report zero measured disk bytes. The verifier checks all four reads used the
same publication, blob, and approved native target. Assembly reconstructs
the performance summary from these files.

For a public SDK row, seal the prior export before publishing the test
ref. After observing the native ref, run `prior_export` with
`--export-approval` for that prior directory and `--transition-object` for
the new repository-relative pack-layer key. Keep stdout in
`git-product/before-export.json`. The read uses the prior viewer binding
and verifies that the absent pack key was read natively with zero edge
requests. Seal the new export after that read. Use its approval for the
`edge` arm and pass the same `--transition-object`, retaining stdout as
`git-product/snapshot.json`. Both reads prove the exact Git blob on the
specified layer. The release verifier joins the snapshot to a successful
provider request for that same pack-layer key. Assembly
builds the transition trace from those records and verifies both retained
inventories and their sealing order. Run the separate `origin` rollback
with the public binding withdrawn before retirement.

### Capture a desktop Git read

Use the direct desktop Git fixture and the dedicated runner's normal user
configuration in `~/.config/crab/config.toml`. Publish with
`run_cdn_git_workload.py publish` and confirm the native ref with
`collect_cdn_git_native_ref.py`. Set `GIT_SHA` to the full commit of the
sidecar's checkout when building locally. Hosted desktop builds set it from
the checked-out ref. Run the exact sidecar with `--cdn-build-info` and
preregister its binary SHA-256 alongside the compiled build metadata.
The workload checks the executable digest, source commit, and host target
before opening the remote. The record includes the Cargo profile, features,
qualification feature state, and compiled policy digest.
[Cargo build-script environment](https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts)

```sh
python3 -B crab/scripts/e2e/run_cdn_desktop_workload.py edge \
  --fixture "$fixture" --configuration-sha256 "$configuration_sha256" \
  --receipt "$receipt" --sidecar "$sidecar" --sidecar-sha256 "$sidecar_sha256" \
  --scratch "$scratch" --source-sha "$source_sha" \
  --run-id "$run_id" --run-attempt "$run_attempt" \
  --private-reader "$private_reader" > "$edge_record"
```

For a private fixture, use the same trusted issuer document as the SDK
worker and set the runner's `[auth]` values to that identity. Set the matching
`[[cdn]]` entry's `approval_sha256` for the edge arm and remove that field for
the `origin` arm. For a public fixture, use `--export-approval` and pin its
receipt with `cdn.publication` for the edge arm. Omit both for the origin arm.
Keep the same sidecar, native origin, repository and commit for both reads.
Each invocation starts a fresh cache and verifies the actual RPC payload and
read target. Retain the redacted records and pass the edge record as `--client`
to `collect_cdn_viewer_requests.py`.

For regional measurements, register the `desktop-git-read` workload with
the writer's `candidate`, the `desktop_candidate`, and `desktop_native_target`.
The target contains `origin_sha256` and `prefix_sha256` from the approved
native origin and effective repository prefix, including its trailing slash.
Register the viewer binding and private issuer approval or public export as
for other direct workloads. Follow the [paired measurements layout](#pair-regional-cli-fetch-measurements)
and keep `publication.json` and `provider-published.json` at the root of each
pair. Publish and independently observe a fresh commit before either timed
read path starts.

Use a fresh invocation for the native, cold edge, and warm edge arms. On the
warm edge invocation, add `--warm-local-output "$pair/warm_local/client.json"`
and write stdout to `warm_edge/client.json` without reformatting it. The
worker repeats the read in the same open sidecar and records its verified
local Git blob and zero edge requests. The local record pins that exact warm
edge record. Collect provider logs for the edge arms and runner measurements
for all four arms. `collect_cdn_git_performance.py` recomputes the regional
pairs and checks the registered latency and cost budgets.

For a release row, retain a separate product publication and run the edge
and origin arms in that order. Keep the edge result as `git-product/read.json`
and the origin result as `traces/rollback.json`. Retain `publication.json`,
`provider-published.json`, `viewer-requests.json`, `retirement.json`, and
`provider-retired.json` in `git-product/`. Observe the ref before the product
read, retire the same ref after rollback, and observe the tombstone before
fixture cleanup. The regional reads must use their own commits, cache roots,
and viewer requests, and finish before cleanup.

For a public row, seal the prior export before publishing the product ref.
Run `prior_export` with that export's approval and `--transition-object`
set to the repository-relative pack-layer key. Retain the result as
`git-product/before-export.json`. Seal the current export after that read.
Use the current export's approval and the same `--transition-object` on the
product edge read. The verifier derives the transition from these two reads,
the retained inventory receipts, and the matching viewer logs. Run the usual
origin rollback with the approval withdrawn and omit `--transition-object`.

The assembler recomputes the product and regional summaries from the retained
records. Promotion requires the ordinary writer and sidecar builds and the
matching compiled CDN policy for every published desktop platform. Execute
the live provider qualification before adding a desktop scope to release policy.

### Capture protected SDK hydration evidence

Use a private direct SDK xorb fixture with an independently materialized view.
Keep the ordinary Crab pointer for a file reconstructed entirely from the
single approved xorb.
The workload JSON has schema `crab.cdn-sdk-hydration-workload/v1` and the
fields `configuration_sha256`, `pointer_sha256`, `payload_sha256`,
`payload_bytes`, `crab_sha256`, `publication_sha256`, and
`global_prefix_sha256`. Compute the
payload digest and length from the original file. Hash the physical shared
prefix of the projected view, ending in `/.crab` with no trailing slash.
Preregister `sdk-xorb-hydration` as the workload and the document digest
as `hydration_workload_sha256`. Also register the SDK probe build and the
approved viewer and issuer binding digests.

Use a repository-prefix private direct SDK Git fixture for the native
publication. Run the native Git worker's `publish` action with
`--hydration-pointer` and `--hydration-pointer-sha256`. Materialize and
approve the xorb view after that publication so its generation covers the
published pointer. Set `publication_sha256` to the digest of the native
publication receipt. Observe the source ref with the independent native ref
probe using the approved xorb fixture before either hydration arm. Use the
same Crab executable for publication and hydration.

```bash
python3 -B crab/scripts/e2e/run_cdn_sdk_hydration.py edge \
  --fixture "$approved_fixture_json" --configuration-sha256 "$approved_configuration_sha256" \
  --workload "$hydration_workload" --workload-sha256 "$approved_workload_sha256" \
  --pointer "$fixture_pointer" --probe "$sdk_read_probe" --git "$git_executable" --crab "$crab_executable" \
  --private-reader "$sdk_reader_settings" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT" > "$hydration_edge"
```

Run the same command with `origin` for the paired control. The worker creates
a fresh worktree and cache for each arm and calls `Local::hydrate` on the
tracked pointer. It verifies the file written to disk, the child's captured
origin and physical view, and the xorb chunk verification event. The origin
arm withdraws viewer approval, keeps native issuer authorization, and
requires zero edge attempts. Keep the edge record at
`xorb-product/hydration.json` and the native record at `traces/rollback.json`.
Pass the edge record as `--client` to `collect_cdn_viewer_requests.py`
and save its output as `xorb-product/viewer-requests.json`. The verifier
requires successful delivery of the approved xorb and rejects regional
samples that reuse a lifecycle viewer request.

Collect the preregistered `origin`, `cold_edge`, `warm_edge`, and `warm_local`
arms in each region before retiring the ref. Use separate invocations for
origin, cold edge, and warm edge. Expire or invalidate only the fixture
xorb between cold samples. Confirm a miss and fill in the provider logs.
Keep the cold and warm CloudFront arms in disjoint S3 log seconds.
[CloudFront invalidation](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/invalidation-specifying-objects.html) · [Cloud CDN invalidation](https://docs.cloud.google.com/cdn/docs/invalidating-cached-content) · [Front Door cache purge](https://learn.microsoft.com/azure/frontdoor/cache-purge)

For `warm_edge`, add `--warm-local-output` pointing to the paired
`warm_local/client.json`. The probe restores the pointer and invokes ordinary
hydration again in the same worktree with the same cache. It checks the
output file again and proves that the approved xorb chunks came from the
local cache with zero edge requests. The record includes a digest of its
warm edge primer. Use the `git-benchmarks/<location>/<pair>/<arm>/` layout,
with `client.json` and independently measured `measurement.json` in every
arm. Join the provider viewer and origin logs for both edge arms with
`collect_cdn_viewer_requests.py`. Assembly and release verification bind those
logs to the benchmark's exact xorb and rebuild the regional summary.

Retire the same ref after rollback and the regional measurements with the
native source fixture. Observe its tombstone with the xorb fixture before
cleanup. Keep `workload.json`, `publication.json`, `retirement.json`,
`provider-published.json`, and `provider-retired.json` with the hydration
and viewer records in `xorb-product`.
Assembly derives the product result from these records, the transport and
scoped view observations, and the cleanup trace.

### Capture projected NFS mount evidence

Use a `private/mount/xorb/direct` fixture and the same native publication,
independent ref observations, exact scoped-view xorb, and cleanup lifecycle
as SDK hydration. Include `cdn-qualification.bin` in the filtered paths.
Preregister `mount-xorb-read`, the packaged Crab `candidate`, the viewer
binding and approval digests, and `hydration_workload_sha256`. Use schema
`crab.cdn-mount-xorb-workload/v1` with the hydration workload fields above
and `mounted_oid`, the exact Git commit in the approved projected view.

The dedicated worker needs the ordinary native NFS prerequisites and a
Crab Auth login authorized for `mount` and Git metadata reads of that view.
Pass the issuer and absolute token cache path in the same format as
`--private-reader` for SDK hydration. Use scratch outside a Git or Crab
repository. On Windows, pass `--drive X:` with an unused measurement drive.

```bash
python3 -B crab/scripts/e2e/run_cdn_mount_workload.py edge \
  --fixture "$approved_fixture_json" --configuration-sha256 "$approved_configuration_sha256" \
  --workload "$mount_workload" --workload-sha256 "$approved_workload_sha256" \
  --receipt "$native_publication" --helper "$git_remote_crab" --scratch "$scratch" \
  --private-reader "$mount_reader_settings" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT" > "$mount_edge"
```

Compare a fresh `origin` mount of that commit with the viewer binding
withdrawn. Keep `xorb-product/mount.json`, `traces/rollback.json`, and the
exact xorb join at `xorb-product/viewer-requests.json`. Use the same regional
layout as SDK hydration. For `warm_edge`, pass `--warm-local-output` for
the paired fresh mount. Its Git metadata and kernel mount are new, and it
reuses the primer's Crab cache. The worker requires verified local chunk
hits and zero edge attempts for that arm. The timer covers startup, the
filesystem read, and clean detach. The xorb transfer counters cover the
filesystem read after the live commit and pointer checks.

### Verify fixture cleanup

After retiring the test ref and completing fixture cleanup, collect
`traces/cleanup.json` with the approved fixture configuration. Run this probe
on the Git product worker so its clock can prove the inventory was checked
after ref retirement. For Amazon S3, stop fixture writers and use an AWS
profile with `s3:ListBucketVersions` and `s3:ListBucketMultipartUploads`
on the dedicated fixture buckets, for example `AWS_PROFILE=crab`.
GCS uses the active `gcloud` identity with bucket metadata and object
listing access. Azure uses
the active Azure CLI identity with Blob account information and listing access.

```bash
python3 -B crab/scripts/e2e/collect_cdn_fixture_cleanup.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$approved_configuration_sha256" \
  --output "$row_directory/traces/cleanup.json" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" \
  --run-attempt "$GITHUB_RUN_ATTEMPT"
```

The probe checks the slash-terminated repository prefix in the native origin
and the separate public export where applicable. S3 lists incomplete multipart
uploads before checking committed versions and delete markers. GCS lists
generations and checks soft-deleted objects when the bucket
policy enables recovery. Use GCS cleanup fixtures with soft delete disabled
before test writes or allow the applicable retention period to elapse.
Azure includes uncommitted blobs when listing current paths on hierarchical
accounts or versions and snapshots on flat accounts. It checks soft-deleted
paths on hierarchical accounts separately. All listings must complete with an
empty result. The retained trace contains scope fingerprints and inventory
outcomes. Retain CDN deployment and purge evidence under the deployment and
rollback checks. [S3 multipart listing](https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListMultipartUploads.html) · [S3 version listing](https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectVersions.html) · [GCS object listing](https://docs.cloud.google.com/storage/docs/json_api/v1/objects/list) · [Azure Blob listing](https://learn.microsoft.com/en-us/rest/api/storageservices/list-blobs)

### Pair regional CLI fetch measurements

Collect one `client.json` from the scoped Git, capsule, or LFS workload for
each timed arm and retain the provider and runner measurements for its
reported fetch interval. Each pair uses the same published 256 KiB payload.
Use this layout under `<scope>/<platform>/git-benchmarks/` for the preregistered number
of pairs in each region. Set `workload` to `git-fetch` for Git or capsule
scopes and `lfs-fetch` for LFS. Each registration includes `regions`, an object with
the exact `near` and `distant` client region labels. Set them before approving
the registration digest.

```text
near/0001/publication.json
near/0001/origin/client.json
near/0001/origin/measurement.json
near/0001/cold_edge/...
near/0001/warm_edge/...
near/0001/warm_local/...
distant/0001/...
```

Retain the receipt written by `run_cdn_git_workload.py publish` as
`publication.json` at the root of the pair. Use that exact file as
`--receipt` for all four arms. Verification checks the registered writer
build, committed tip, payload digest, and ref visibility before the first
timed read. Measure cold edge before warm edge and warm local.
The origin control can run at any point in
the pair. Start the next pair after the previous pair finishes. The LFS timer
starts after Git fetches and verifies the pointer, so the recorded interval
measures LFS payload delivery and its native metadata reads.
Assembly and release verification match the native origin and repository
prefix of every CLI arm to the independently verified product fixture.

For private managed Git, use the registered managed URL for the timed reads.
Use the approved v2 grant for edge and warm-local arms and the direct v1
grant for the origin arm. The worker records the observed issuer, repository,
and grant version in each result. Use a fresh commit per pair and collect
the independent `provider-published.json` with
`collect_cdn_git_native_ref.py published` before the first arm.
Retain that observation beside the pair's publication receipt.
All pairs must observe the same native repository and epoch as the product
fetch and finish before fixture cleanup. To check the retained pairs locally
with `collect_cdn_git_performance.py`, pass `--managed-row` and `--fixture`.

For public benchmarks, stage the immutable objects for every benchmark pair
before sealing the current export approval. Use that same approved export
for the timed cold edge, warm edge, and warm-local arms. The release verifier
checks its digest independently of the viewer URL and the native Git
publication receipt. The origin control has the public binding withdrawn,
so omit `--export-approval` for that arm.

Each `measurement.json` uses schema `crab.cdn-git-measurement/v1` and
contains `client_report_sha256`, `provider` (`s3`, `gcs`, or `azure`),
`region`, `client_bytes`, `origin_gets`, `origin_fill_bytes`,
`rejected_bytes`, `peak_memory_bytes`, `peak_disk_bytes`, and
`cost_microusd`. Hash the exact `client.json` bytes. Use provider logs to
attribute request and fill counts. Measure client and resource totals on the
runner. Each timed fetch records native HTTPS GET and HEAD responses and
consumed body bytes, including reads of refs and metadata on the cloud storage
endpoint. The origin control requires native GET and body evidence. External
`origin_gets` must cover every observed native GET response, including listing
requests. External `client_bytes` must cover both native and edge body bytes,
including incomplete edge bodies. The rejected total also needs any bytes
discarded by higher-level verification. Measure protocol overhead and
provider-side origin fills independently.

For each timed fetch that made edge attempts, collect provider viewer
evidence beside `client.json`. For CloudFront, write
`cloudfront-requests.json` from the approved distribution's standard
access logs in W3C format. Keep the `#Fields`
header and the request ID, viewer host, URI stem, method, protocol, response
status, sent bytes and response result type fields. Also supply the approved
origin bucket's plain-text S3 server access logs with `--s3-origin-log`. Use
the private repository bucket or the public export bucket as applicable.
The AWS fixture includes a companion template for both log streams in
`us-east-1`.
The collector joins CloudFront's signed S3 object GETs to viewer keys and
the timed fetch window. The cold arm needs a successful S3 response with
body bytes. Use a fresh publication for each pair, keep other CloudFront
traffic outside the measured UTC seconds, and start the warm edge arm
in a later UTC second than the cold arm's finish. Leave cookie logging
disabled and keep raw exports on the protected worker.
[CloudFront standard logging](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/standard-logs-reference.html) · [S3 server access log format](https://docs.aws.amazon.com/AmazonS3/latest/userguide/LogFormat.html)

```bash
python3 -B crab/scripts/e2e/collect_cdn_viewer_requests.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$approved_configuration_sha256" \
  --client "$arm_directory/client.json" --log "$decompressed_cloudfront_log" \
  --s3-origin-log "$s3_origin_access_log" \
  --output "$arm_directory/cloudfront-requests.json"
```

For the AWS companion, replace `--log` and `--s3-origin-log` with
`--aws-log-stack "$approved_logs_stack"`. Add `--aws-profile` for local
inspection or use the protected worker's default AWS credentials. The
collector checks the companion outputs and the origin logging target. It
fetches only from the two private log prefixes, using expected-owner and
ETag checks, and retains only the redacted proof. To wait for delivery, pass
`--aws-log-wait-seconds` with the polling window in seconds. Pass
`--require-origin-fill` for the cold arm so the collector also waits for a
matching successful S3 response with body bytes. With local exports, the
same flag reports a missing origin fill immediately. Use time-filtered local
exports when a log prefix exceeds the collector's download bounds.

For Cloud CDN, pass `--gcp-logging` in place of `--log` to query the
approved project and forwarding rule through the Cloud Logging API. The
active `gcloud` identity needs `logging.logEntries.list` on the fixture
project. The collector includes a minute on either side of the timed read
for clock skew and origin fills. It follows bounded pagination and writes
only the verified redacted proof. Retry after log delivery if the required
records have yet to arrive. [Cloud Logging entries.list](https://docs.cloud.google.com/logging/docs/reference/v2/rest/v2/entries/list)

The workload sends a random per-request User-Agent tag for qualification.
The attested backend
bucket keeps that header out of the cache key. The collector joins viewer
records by the tag and retains Google's `cacheFillBytes` from each matched
viewer record. It joins Cloud CDN's range-fill and validation requests on
`parentInsertId` and retains their hashed IDs, methods, status codes, and fill
bytes. Include both kinds of log entry, and give delayed validation entries
time to arrive. The benchmark requires external measurements to cover the
logged origin GETs in addition to client-native GETs. The fill-byte minimum is
the higher of the total from distinct origin records and the largest viewer
record, since viewer and origin records can describe the same fill.
Every cold arm must show positive fill bytes on a completed viewer response
with payload bytes or on its associated successful origin GET. Error
responses still count toward the reported byte floor.
[Cloud CDN logging](https://docs.cloud.google.com/cdn/docs/logging) · [Backend bucket cache keys](https://docs.cloud.google.com/cdn/docs/caching#cache_keys)

```bash
python3 -B crab/scripts/e2e/collect_cdn_viewer_requests.py \
  --fixture "$approved_fixture_json" --configuration-sha256 "$approved_configuration_sha256" \
  --client "$arm_directory/client.json" --gcp-logging --require-origin-fill \
  --output "$arm_directory/cloud-cdn-requests.json"
```

If the direct query exceeds its bounds, supply a time-filtered JSON export.

```bash
gcloud logging read \
  "resource.type=\"http_load_balancer\" AND resource.labels.forwarding_rule_name=\"$forwarding_rule\" AND (httpRequest.userAgent:\"crab-cdn-qualification/\" OR httpRequest.userAgent=\"Cloud-CDN-Google\") AND timestamp>=\"$window_start\" AND timestamp<=\"$window_end\"" \
  --project "$project_id" --format=json > "$cloud_cdn_log"
```

Use `--log "$cloud_cdn_log" --output "$arm_directory/cloud-cdn-requests.json"`
with the collector above. Add `--require-origin-fill` for a cold arm.
Keep raw Cloud Logging exports on the protected worker.

For Front Door, enable Azure Monitor access logs on the approved profile and
`StorageRead` logs on the approved account's `blobServices/default` resource.
The private path forwards through APIM and removes the `/crab` prefix.
The collector matches the Front Door tracking reference and resource profile
and excludes duplicate traffic from origin-shield logs. It joins Blob GETs to
the viewer object keys and measurement window. The cold arm needs a successful
Blob response with body bytes. Use distinct fixture publications and keep
other traffic to those keys outside the measurement window.
[Front Door access logs](https://learn.microsoft.com/en-us/azure/frontdoor/front-door-diagnostics) · [Blob resource log format](https://learn.microsoft.com/en-us/azure/storage/blobs/monitor-blob-storage-reference#resource-logs)

To collect the archives directly, set both diagnostic streams to archive to
Azure Storage and use `--azure-monitor` in place of the viewer and Blob log
file flags. Use an Azure CLI identity that can read the resources' diagnostic
settings and read blobs from the configured log containers. The collector
looks up the enabled destination for each category and reads its hourly
archives around the timed action. Private collection reads the profile's
managed identity from ARM. Public collection still takes the Azure Public
service-tags file through `--azure-origin-authority`.
[Diagnostic settings API](https://learn.microsoft.com/en-us/rest/api/monitor/diagnostic-settings/list?view=rest-monitor-2021-05-01-preview) · [Azure Monitor archive layout](https://learn.microsoft.com/en-us/azure/azure-monitor/logs/resource-logs)

```bash
python3 -B crab/scripts/e2e/collect_cdn_viewer_requests.py \
  --fixture "$approved_fixture_json" --configuration-sha256 "$approved_configuration_sha256" \
  --client "$arm_directory/client.json" --azure-monitor --require-origin-fill \
  --output "$arm_directory/front-door-requests.json"
```

This command shows a private cold arm. Use the file-based command below
for preselected exports or when the direct read reaches its archive limits.

For private delivery, capture the approved profile's system-assigned identity
for comparison against the Blob log requester's OAuth object ID.

```bash
az afd profile show --subscription "$subscription" --resource-group "$resource_group" \
  --profile-name "$profile" --output json > "$azure_origin_authority"
```

For public delivery, use Microsoft's current Azure Public service-tags JSON
as `azure_origin_authority`. The collector requires anonymous reads of the
dedicated export container from addresses in `AzureFrontDoor.Backend`. The
fixture's isolated keys and time window establish which benchmark arm to
credit. Keep the authority and both raw archives on the protected worker.
[Azure Front Door managed identity](https://learn.microsoft.com/en-us/azure/frontdoor/origin-authentication-with-managed-identities) · [Azure Public service tags](https://www.microsoft.com/en-us/download/details.aspx?id=56519)

```bash
python3 -B crab/scripts/e2e/collect_cdn_viewer_requests.py \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$approved_configuration_sha256" \
  --client "$arm_directory/client.json" --log "$front_door_access_archive" \
  --azure-blob-log "$blob_storage_read_archive" --azure-origin-authority "$azure_origin_authority" \
  --output "$arm_directory/front-door-requests.json"
```

Repeat `--log`, `--s3-origin-log`, or `--azure-blob-log` for additional files. Every viewer attempt needs a matching
access log record for its hashed request ID. Missing logs,
conflicting records, or disagreements with the client's status or cache
classification fail the check. Viewer response bytes include HTTP overhead
and must cover the observed payload bytes. Collect native request counts and
origin fill bytes separately. AWS delivers S3 server access logs on a
best-effort basis. Treat the matched CloudFront GET counts and response
body bytes as minimums. Corroborate zero-origin and cost claims with
independent provider measurements. [S3 server access logging](https://docs.aws.amazon.com/AmazonS3/latest/userguide/ServerLogs.html)

```bash
python3 -B crab/scripts/e2e/collect_cdn_git_performance.py \
  --inputs "$paired_measurements" --benchmarks "$approved_benchmarks_json" \
  --benchmarks-sha256 "$approved_benchmarks_sha256" --row "$scope/$platform" \
  --run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT"
```

This command previews the summary and applies the release verifier's sample,
latency, cost, and paired savings rules. Assembly uses the underlying files
and derives the summary. The collector checks the source, workflow attempt,
runner platform, preregistered region, published commit, and verified payload.

### Assemble the release artifact

Put the measured inputs in a run-owned directory. Include the approved
`benchmarks.json`, each requested provider's native `report.json`, and both
files and traces for every registered scope and platform.

```text
benchmarks.json
s3/report.json
cloudfront/private/cli/git/direct/linux-x86_64/observations.json
cloudfront/private/cli/git/direct/linux-x86_64/traces/<check>.json
cloudfront/private/cli/git/direct/linux-x86_64/git-product/publication.json
cloudfront/private/cli/git/direct/linux-x86_64/git-product/fetch.json
cloudfront/private/cli/git/direct/linux-x86_64/git-product/viewer-requests.json
cloudfront/private/cli/git/direct/linux-x86_64/git-product/retirement.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/near/0001/publication.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/near/0001/origin/client.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/near/0001/origin/measurement.json
```

Each `observations.json` uses schema `crab.cdn-observations/v1` and records
`source_sha`, `scope`, `configuration_sha256`, `workflow_run_id`,
`workflow_run_attempt`, `platform`, `deployment`, and `fixture_prefix`.
It also contains `check_results`, a map from required check names to measured
boolean outcomes. For direct CLI Git, capsule, and LFS scopes and direct SDK
Git and private xorb scopes, assembly derives `product_action` from the
product records in `git-product/` or `xorb-product/` and the rollback and
cleanup traces. It derives `performance` from `git-benchmarks/`. Supply the
measured `product_action` and `performance` fields in `observations.json`
for fixture scopes with no product-specific collector. Direct Crab Server and
S3 gateway rows derive `product_action` from `service-product/`, rollback,
and cleanup. Their Git rows also derive `performance` from `git-benchmarks/`.
Release verification requires a
product-specific verifier for each enabled row. Git and capsule scopes require
`product_action.verified_edge_payloads` alongside the completed GET count.
Each benchmark arm records `local_payload_hits`. The warm-local comparison
requires a positive local payload hit and zero edge payload bytes.

When using protected canary artifacts, supply `--transport-traces` with a
directory containing each downloaded artifact under its provider name
(`s3`, `gcs`, or `azure`). The assembler reuses its verified transport traces
for rows with the same scope and approved fixture digest. Those traces may
be omitted from the row's `traces/` directory and `check_results`. If a row
also supplies a trace for the same check, its bytes must match the canary.
Supply the remaining required checks through the measured inputs.

```bash
python3 -B crab/scripts/e2e/assemble_cdn_qualification.py \
  --inputs "$measured_inputs" --output "$artifact_directory" \
  --benchmarks-sha256 "$approved_benchmarks_sha256" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" \
  --run-attempt "$GITHUB_RUN_ATTEMPT"
```

Use a new output directory under an existing scratch directory. The assembler
checks coverage for the compiled release policy, verifies each report and its
traces, then exposes the artifact directory. It retains the approved benchmark
file, the native provider reports, and the verified CDN reports and trace files.
CLI and SDK workload rows also retain the bounded, redacted product
and paired benchmark records. Release verification checks their build
identity and recomputes the summaries from the retained measurements.

### Release workflow

Release preparation checks the enabled scopes before building CLI and desktop
packages. An empty policy needs no artifact or approval. For a nonempty policy,
the release job expects a bundle from one successful manual run of
`.github/workflows/pb-provider-qualification.yml` on the exact release commit.

For publication, use a dedicated Linux runner in the
`provider-cdn-qualification` runner group. Restrict the group to the provider
qualification workflow on a trusted branch and restrict the
`provider-cdn-release-qualification` environment to the same trusted refs.
Configure these variables in that environment. [GitHub runner group access](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/manage-access)

- `CRAB_CDN_MEASURED_INPUTS_ROOT` points to the measurement directory mounted
  on the assembly runner. Give the assembly runner read access to the inputs.
- `CRAB_CDN_BENCHMARKS_SHA256` contains the independently approved SHA-256
  of the exact `benchmarks.json` bytes for the release candidate.

Dispatch **Provider contract qualification** at the release candidate with
`publish_cdn_evidence` enabled and select the providers in the approved
registration. The product, deployment, and regional workers must deposit the
measured row directories and `benchmarks.json` under
`<measurement-root>/<run-id>/<run-attempt>/` before the assembly job starts.
Use that dispatch's commit, run ID, and attempt in every record. Finalize the
files before making the run directory available to the assembly job. Those
workers need their own cloud fixtures and credentials. Enable `cdn_transport`
on the same dispatch to include transport and deployment traces from the
configured fixture.

The assembly job downloads the native reports and any successful CDN canary
artifacts from the exact attempt. It verifies the measurements and
uploads `cdn-release-evidence-<run-id>-<attempt>` only after assembly succeeds.
When repeating an attempt, rerun the native jobs and collect fresh records for
that attempt. The release job looks for this layout.

```text
benchmarks.json
s3/report.json
cloudfront/private/cli/git/direct/linux-x86_64/cdn-report.json
cloudfront/private/cli/git/direct/linux-x86_64/traces/<check>.json
cloudfront/private/cli/git/direct/linux-x86_64/git-product/<phase>.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/<location>/<pair>/publication.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/<location>/<pair>/<arm>/client.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/<location>/<pair>/<arm>/measurement.json
```

Use `gcs/report.json` and `azure/report.json` for the corresponding native
provider reports. Each scope and platform has its own supplement and traces.
The `benchmarks.json` object maps `<scope>/<platform>` to its preregistered
`crab.cdn-benchmark/v1` record, including `deployment_policy_sha256`. Private
rows also require `rotation_overlap_policy_sha256`. Public
rows also require `publication_sha256` and retain
`<scope>/<platform>/public-export/{inventory,publication}.json`. Approve
the SHA-256 of that exact file before
measuring performance. Set the repository variable `CRAB_CDN_RELEASE_EVIDENCE`
with the successful run ID and attempt as decimal strings and the approved
digest as `benchmarks_sha256`. The release job checks the run via GitHub and
compares the downloaded registration file to the separately approved digest.

Every published CLI platform requires its own CLI and mount evidence when the
corresponding scope is enabled. Desktop requires evidence for the published
installers and remote runtimes, including Linux arm64. SDK, Server and S3
gateway scopes currently require Linux x86_64 evidence in this gate. Each
future product release must cover its own published platform matrix.

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
