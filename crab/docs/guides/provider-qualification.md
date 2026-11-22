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
receive an authorization denial. Use `--second-viewer-file` with a distinct
grant issued to a different authorized principal for the same object. The
collector requires the second principal's full read to hit the cache and match
the approved bytes.
It records a separate cache-reuse trace when the first read was an observed
miss. Start with an uncached dedicated fixture to collect both traces. Retain
the test principals' grant provenance separately from the viewer credentials.
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
prefix, issued to a principal that lacks access to the target. Retain the grant
provenance without its credential. The collector uses the same grant to read
and verify the control before and after denied full and ranged GETs on the warm target.
It also verifies an authorized full cache hit on the target before and after
the denials.

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
- `CRAB_CDN_TRANSPORT_SECOND_VIEWER` is the optional environment **secret** for
  the separately issued second private viewer grant.
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

The canary requests the GitHub token and viewer grant after passing the
deployment preflight. It checks the provider, native origin, viewer authority,
physical view, signed object-family prefix, and expiry before the first edge
request. The grant stays in the collector process. The native inspection role
continues to supply the separate native read. Same-object cache reuse with
another credential still needs issuer evidence to establish a second principal.

Download `provider-cdn-transport-<provider>-<run-id>-<attempt>` for the redacted
traces. For manual warm expiry and parallel range denials on the same approved
fixture, run the standalone collector with `--viewer-file` and
`--viewer-expires-at`. The hosted workflow selects `--github-viewer`.

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

Set the public fixture's `source_placement` to the exact object from the
approved inventory. The canary identity needs native placement metadata
access on the source as well as the export. Grant source bucket location
access for S3, source bucket metadata access for GCS, or source storage
account and container read access in Azure Resource Manager.

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

### Verify viewer signing-key retirement

On the protected Linux measurement worker, register the policy digest for
the final deployment with only the replacement key trusted. Inspect and
approve the overlap digest separately while the same deployment trusts both
keys. Policy snapshots now include a digest of the sorted viewer key IDs.
Recollect any earlier snapshot and approve its updated digest before use.

Issue two short-lived credentials for the same fixture object, one per key.
The collector accepts grants with up to 900 seconds remaining.
Keep the files private and use the current provider's read-only inspection
identity. Start the collector during overlap. After both credentials return
the approved bytes, the collector prompts the operator to retire the old
key. Confirm only after the provider has deployed the change. If the provider
cannot complete this transition while the old grant is live, the profile
stays unqualified.

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
checks the same object against the ordinary transport trace, confirms that the
old grant was still within its signed lifetime, and rejects a generic pass
trace. The rotation command reads policy and object bytes. Provider changes
are performed by the operator, and the output contains only redacted
observations. [CloudFront key rotation](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-trusted-signers.html) · [Cloud CDN key retirement](https://docs.cloud.google.com/cdn/docs/using-signed-cookies) · [APIM signer policy](https://learn.microsoft.com/en-us/azure/api-management/validate-jwt-policy)

### Measure CDN transfers during product actions

Capture stderr from the normal product operation with
`CRAB_LOG=crab_storage::cdn::transfer=debug` and a redirected, noninteractive
stderr. The CDN transport emits one redacted event per product attempt.
The event identifies the provider, access mode, consumer, read family and
authorization profile, with HTTP status, received bytes, elapsed time and
cache disposition. Standalone doctor probes do not produce these events.

```bash
CRAB_LOG=crab_storage::cdn::transfer=debug git fetch origin 2>"$fetch_stderr"
python3 -B crab/scripts/e2e/collect_cdn_product_metrics.py \
  --stderr "$fetch_stderr" --scope cloudfront/private/cli/git/direct
```

Retain only the reduced metrics. A completed transfer proves exact HTTP
framing and delivery. Git payload readers emit a separate observation after
accepting an authenticated pack digest or a reconstructed object ID. A
discarded edge batch reports no verified payloads. The sum of parallel
transfer times differs from elapsed action time. Front Door's response
header describes the first 8 MB chunk, so Crab records an unknown cache
disposition for larger objects. Use provider logs for their cache and
origin-fill evidence. [Front Door caching documentation](https://learn.microsoft.com/en-us/azure/frontdoor/front-door-caching#response-headers)

### Verify native publication and CDN Git fetch

Use `crab/scripts/e2e/run_cdn_git_workload.py` for direct CLI Git and capsule
scopes on an initialized test-only repository. Use the qualification build's
`git-remote-crab` and the same approved fixture JSON as the transport
collector. `native_origin` identifies the S3 bucket, GCS bucket, or Azure
account and container. Configure the writer and reader credentials for that
exact native target. The qualification helper verifies the actual native
origin for the resolved repository, any selected read replica, and the store
used for push. The workload requires the helper's confirmation in a read-only
preflight before sending a push. Each phase uses a fresh scratch directory and
cache.

```bash
python3 -B crab/scripts/e2e/run_cdn_git_workload.py publish \
  --fixture "$approved_fixture_json" \
  --configuration-sha256 "$approved_configuration_sha256" \
  --receipt "$publication_receipt" --scratch "$scratch" --helper "$git_remote_crab" \
  --source-sha "$GITHUB_SHA" --run-id "$GITHUB_RUN_ID" \
  --run-attempt "$GITHUB_RUN_ATTEMPT"
```

`publish` creates a unique test ref with an absence lease and checks its
advertisement immediately. Keep the receipt even if the push fails. Prepare
any checkpoint, protected view, or public export required by the registered
read family after publication. For public runs, add the export publisher's
receipt and SHA-256 to the worker's trusted CDN binding before measuring
the edge arm. The workload's own receipt records the native Git publication.
Then run `fetch` with the same arguments and
the configured reader credentials. The workload verifies the advertised
commit and blob SHA-256 and requires both a completed CDN GET and an
accepted edge Git payload for the approved scope. It emits the measured
results as redacted JSON.

Use `fetch-origin` with the same receipt on a worker configured for native
origin access without CDN bindings or a cache service. That arm starts with
empty local caches and rejects edge traffic for any scope.
`fetch-warm-local` primes a fresh Crab cache with a verified edge fetch,
then measures a second fetch from an empty Git object database against that
cache. It requires a verified local pack hit and zero edge requests for the
registered scope. Native metadata and sidecar reads still run. Join the
results with provider request and fill measurements before assembly.

Run `retire` with the same arguments after the workload. It verifies the
exact ref is absent after retirement and refuses to remove a ref whose tip
changed. Retain the publication receipt unchanged as `git-product/publication.json`
under the measured row, and capture the `fetch` and `retire` stdout as
`git-product/fetch.json` and `git-product/retirement.json`. Use a single worker
for these ordered product phases so their timestamps share a clock. Both phase
reports bind the exact publication receipt by digest. Record storage and CDN
fixture cleanup separately in the `cleanup` check.
[Git's explicit push lease](https://git-scm.com/docs/git-push#Documentation/git-push.txt---force-with-leaseltrefnamegtltexpectgt)

### Verify fixture cleanup

After retiring the test ref and completing fixture cleanup, collect
`traces/cleanup.json` with the approved fixture configuration. Run this probe
on the Git product worker so its clock can prove the inventory was checked
after ref retirement. For Amazon S3, use the AWS profile with list-version
access to the fixture, for example `AWS_PROFILE=crab`. GCS uses the active
`gcloud` identity with bucket metadata and object listing access. Azure uses
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
and the separate public export where applicable. S3 lists versions and delete
markers. GCS lists generations and checks soft-deleted objects when the bucket
policy enables recovery. Use GCS cleanup fixtures with soft delete disabled
before test writes or allow the applicable retention period to elapse.
Azure lists current and soft-deleted paths on hierarchical accounts, or
versions and snapshots on flat accounts. All listings must complete with an
empty result. The retained trace contains scope fingerprints and inventory
outcomes. Retain CDN deployment and purge evidence under the deployment and
rollback checks. [S3 version listing](https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectVersions.html) · [GCS object listing](https://docs.cloud.google.com/storage/docs/json_api/v1/objects/list) · [Azure Blob listing](https://learn.microsoft.com/en-us/rest/api/storageservices/list-blobs)

### Pair regional Git benchmark measurements

Collect one `client.json` from the Git workload for each timed arm and retain
the provider and runner measurements for its reported fetch interval. Use this
layout under `<scope>/<platform>/git-benchmarks/` for the preregistered number
of pairs in each region. Each registration includes `regions`, an object with
the exact `near` and `distant` client region labels. Set them before approving
the registration digest.

```text
near/0001/origin/client.json
near/0001/origin/measurement.json
near/0001/cold_edge/...
near/0001/warm_edge/...
near/0001/warm_local/...
distant/0001/...
```

Use the same publication receipt for all four arms in a pair. Measure cold edge
before warm edge and warm local. The origin control can run at any point in
the pair. Start the next pair after the previous pair finishes.

Each `measurement.json` uses schema `crab.cdn-git-measurement/v1` and
contains `client_report_sha256`, `provider` (`s3`, `gcs`, or `azure`),
`region`, `client_bytes`, `origin_gets`, `origin_fill_bytes`,
`rejected_bytes`, `peak_memory_bytes`, `peak_disk_bytes`, and
`cost_microusd`. Hash the exact `client.json` bytes. Use provider logs to
attribute request and fill counts and runner measurements for client and
resource totals. The transport counter includes incomplete response bodies;
the rejected total also needs any bytes discarded by higher-level verification.

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
cloudfront/private/cli/git/direct/linux-x86_64/git-product/retirement.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/near/0001/origin/client.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/near/0001/origin/measurement.json
```

Each `observations.json` uses schema `crab.cdn-observations/v1` and records
`source_sha`, `scope`, `configuration_sha256`, `workflow_run_id`,
`workflow_run_attempt`, `platform`, `deployment`, and `fixture_prefix`.
It also contains `check_results`, a map from required check names to measured
boolean outcomes. For direct CLI Git and capsule scopes, assembly derives
`product_action` from the files in `git-product/` plus the fixture cleanup
check. It derives `performance` from `git-benchmarks/`. Supply the measured
`product_action` and `performance` fields in `observations.json` for other
scopes. Git and capsule scopes require
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
Direct CLI Git and capsule rows also retain the bounded, redacted Git product
and benchmark records. Release verification recomputes their summaries and
requires an exact match to the assembled report.

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
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/<location>/<pair>/<arm>/client.json
cloudfront/private/cli/git/direct/linux-x86_64/git-benchmarks/<location>/<pair>/<arm>/measurement.json
```

Use `gcs/report.json` and `azure/report.json` for the corresponding native
provider reports. Each scope and platform has its own supplement and traces.
The `benchmarks.json` object maps `<scope>/<platform>` to its preregistered
`crab.cdn-benchmark/v1` record, including `deployment_policy_sha256`. Public
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
