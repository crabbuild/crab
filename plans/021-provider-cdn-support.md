# Provider CDN support for Crab v2

Proposed design. Audited 2026-11-21 against the in-progress worktree on
`482daa946a7`. Provider qualification is pending.

The storage key classifier, exact HTTPS reads and private viewer headers have
local coverage. The reference issuer and Rust client implement negotiated,
scope-bound CloudFront cookies, Cloud CDN cookies and Azure gateway tokens.
Direct CLI bindings now route verified history, checkpoint and capsule control
reads, clone pack files, scoped xorbs and full LFS file downloads through the
selected target. Pack and LFS file downloads use bounded parallel ranges and
verify whole-payload digests. The private
[CloudFront fixture](../crab/deploy/cdn/aws/README.md) passed template validation.
The private [Cloud CDN fixture](../crab/deploy/cdn/gcp/README.md) passed
Terraform validation and mocked route tests. The private
[Front Door fixture](../crab/deploy/cdn/azure/README.md) builds with Bicep and
has local ARM wiring and path-policy tests. Each fixture has a repo-local
profile and a protected profile pinned to one materialized view. The
reference issuer grants only the protected view's exact physical prefix.
The CloudFront and Front Door templates support overlapping signing keys.
The managed API client negotiates v2 direct read grants, and the managed
resolver renews their native and repo-local viewer credentials together.
Typed direct SDK bindings cover native remote reads, fetch, clone, and
worktree content reads. Git filters, hydration, and content prefetch receive
a temporary, nonsecret binding for the captured target, view, and private
principal. In an existing worktree the SDK delegates only when the worktree
explicitly configures the same content source. Git integration uses the
binding for the configured Crab filters. The child checks the worktree
source again before opening storage. Private delegated children run without
ambient cloud credential variables, explicit provider overrides, or dotenv
imports. The child checks the login and obtains its own private grant.
Live cross-process provider qualification remains open.

### Open implementation and release gates

| Gate | Current evidence and required closure |
|---|---|
| Activation and placement | Direct boundaries require an explicit private deployment digest agreed by the client and reference issuer. The issuer checks live native placement before signing for all three providers, and the client checks that the confirmed digest belongs to the returned viewer URL and origin. SDK checkout preserves the same digest in the child's operation binding. Managed clients capture the approved binding from authenticated repository resolution and check the digest, native placement, and viewer URL on every v2 grant and renewal. Qualify this contract with the production managed issuer. Operators can attest canonical customer repositories that use the reviewed private policies. Release qualification still requires the dedicated fixture namespace. Qualify placement-aware issuance for same-named origins and cross-region S3 replicas, and verify public source and export placement in each live fixture. |
| Transfer lifetime | Pack, LFS and xorb origin restarts now share the edge attempt's deadline. File downloads signal cancellation and drain local writes before returning. Qualify failed lease renewal, admission across real provider retries and denial during parallel ranges through the complete operations. |
| Authorization refresh | The direct issuer serves negotiated credentials with `Cache-Control: no-store`. Its client bounds responses and redacts issuer bodies from errors. Negotiated CLI, SDK and desktop direct reads and managed v2 direct reads share one origin and viewer refresh per rejected generation, including cancellation and failure. Online cache reads recheck the grant and reject out-of-scope paths. Local cache-store tests exercise separation by physical target and view, and denial on a warm disk cache. Failed edge-grant renewal also denies online cache reads. Private direct and managed renewal bind the original OIDC issuer and subject. Refresh cache writes cannot overwrite a newer login or undo sign-out. Private checkout passes a principal fingerprint to children. Qualify live renewals, namespace isolation, and terminal denial across the product entry points. |
| Reader coverage | Authenticated catalog pages, detached capsule controls, native layered payload windows, lazy Git index ranges, and catalog-bound shards can use the CDN. Layered Git point reads and coalesced object batches verify final object IDs after delta reconstruction and own the origin restart. Raw pack passthrough, recovery, consolidation and strict provenance checks use origin. Full LFS streams use bounded edge ranges and hold the final bytes until SHA-256 succeeds. Partial LFS streams use a verified local descriptor or the native version proof. Qualify each reader on live deployments. |
| Projected-view mounts | Direct Crab Auth mounts acquire and renew an issuer-owned GC claim for the exact projected catalog before admitting Xet reads. The issuer checks the resolved native target, and the mount checks both the catalog digest and the Git commit served by a remote-owned clone. Qualify deployed issuer permissions, actual mounted bytes, lease-loss behavior and clean teardown on S3, GCS and Azure. Managed projected mounts still require issuer integration. |
| Deployment and release | The private CloudFront, Cloud CDN and Front Door fixtures cover repository-local reads and one projected view's Xet content in local policy tests. Public bindings require a distinct same-cloud export origin. All three providers have separate public Git and LFS export templates with public delivery disabled by default, plus publishers that verify an approved inventory and use create-only writes. Local tests exercise staged configuration, canonical viewer paths, destination binding, corrupt bytes and idempotent publishing. Desktop remote browsing passes the trusted user binding into the native SDK. Crab Server and the S3 gateway have opt-in `provider-cdn-qualification` builds with exact per-repository bindings for verified Git and full LFS reads. A loopback gateway test covers a cold pack-layer fetch with ETag, range and access checks, then observes full LFS reads at the edge and partial LFS reads at origin. Live qualification, publication beyond fixture-specific limits, SDK checkout and desktop qualification, managed issuer rollout, and provider and performance evidence remain open. |
| Publication readiness | The AWS, GCP and Azure publishers seal a common `crab.cdn-export-publication.v1` receipt after verifying each destination object and rechecking the source and export resource identities and locations. Direct CLI, SDK and checkout, desktop, server and gateway bindings pin the operator's receipt digest. Public S3 and GCS activation verifies source placement once with the selected native credentials. Azure activation uses a read-only ARM identity to verify the selected account, container, primary location and endpoint. Storage sends only listed keys of the approved size to the edge. The release verifier checks the complete preregistered receipt and inventory against independently attested placement and the digest of the object actually read at the edge. Live publication transitions, historical native access checks and public performance qualification remain open. |
| Qualification integrity | The transport canary observes wire behavior. Private cache reuse requires two preregistered OIDC subjects and Crab Auth policy identities, separately authenticated and authorized by the approved issuer. Key rotation binds ordered reads to deployed key IDs and signed expiration. Scoped-view evidence requires authorized reads of the target and native sibling on both sides of full and ranged CDN denials. Managed issuer rows require their specific v1 and v2 observations. Live observations and matching product and performance evidence for other consumers remain open. |

For public Azure activation, the opt-in auth module obtains a sensitive ARM
authorization header through the workspace Azure Identity SDK. The auth-store
adapter supplies the header only when receipt verification selects Azure.
`crab-storage` checks the resource documents and binds the approved placement
to the selected Blob endpoint. [Azure Resource Manager authentication](https://learn.microsoft.com/azure/azure-resource-manager/management/manage-resources-rest)

The reference issuer's repository-local origin credentials and viewer grants
cover the repository prefix. Shared bucket-global Xet objects can contain
other repositories' content. Qualify protected private Xet reads on the
projected view's separate global prefix. Ordinary native credentials may
continue to read shared content where their existing origin policy allows it.

Use a nearby CDN edge for verified immutable downloads from the repository's
selected cloud bucket. Two developers fetching the same Git pack layer, or CI
runners hydrating the same Xet content, can share an edge fill even with empty
local caches. Repeated cross-region reads are the target workload. Cold fills,
short reads dominated by ref lookup, and existing local cache hits have different
latency profiles. Measure each separately.

Deliver CloudFront with S3 first, then Cloud CDN with GCS and Front Door with
Azure Blob. Support customer-managed distributions and reproducible templates.
Opt in per target. Preserve the object layout, on-disk format, publication
protocol, and origin credentials for ordinary Crab operations. Push, CAS, live
metadata reads, and maintenance go directly to the authorized origin. Cross-cloud
origins, browser redirects and automatic resource provisioning are out of scope.

## Routing and trust

1. Resolve the operation's principal, repository, view, permissions and physical
   read target through the existing access and replica selection path. Read the
   current root, ref authority, and any required generation or retention claim
   from origin for online actions that require them. Retain and renew existing
   leases through cache hits, slow downloads, cancellation and cleanup. A failed
   lease or lost authorization stops the read.
2. Let the owning reader select an immutable object from authenticated metadata.
   Check visibility, allowed key shape, **the entire physical object's access
   scope**, and the verifier for the requested bytes. A Git object ID or a
   requested byte range cannot restrict what a holder of a CDN credential can
   download. Co-packed objects, sibling keys covered by a cookie, and the
   bucket-global dedup prefix all count when issuing access.
3. Preserve local and existing Crab cache-service reads wherever the caller's
   current contract permits them. Resolve the protected view before cache lookup.
   Separate reusable private entries by physical target and the authorized view,
   preserve verification on reuse, and honor existing offline policy. Existing
   cache-service capabilities continue to govern its routes.
4. On a miss, use the selected target's CDN only when the key, proof, binding
   and viewer authorization all match. Otherwise an independently authorized
   in-scope read uses its existing origin route. An out-of-scope request fails
   before lookup or I/O. An explicit CDN authorization failure is terminal
   across caches, replicas and credential providers after the refresh rule below.
5. The format owner verifies bytes before publishing a file, admitting reusable
   bytes, or declaring the action successful. Preserve application-level streaming
   guarantees. Origin responses alone can establish canonical ETags, versions,
   write receipts, replication readiness and strict fsck/recovery results.

Protected views need a physically projected namespace containing only approved
bytes or proof that the principal may read *every* object exposed by the grant.
Bind separate grants for repository and global namespaces. When a prefix cannot
express policy, omit CDN authority for that prefix and use the existing scoped
origin. Public delivery requires explicit operator approval of everything
readable in that export, including history, unreachable objects and co-packed
data. A public head does not make the containing bucket or shared xorbs public.
The existing view materializer binds the physical prefix to source generation,
digest and policy scope in `crates/crab-auth-server/src/view.rs`. Never reassign
an exposed CDN hostname/path to a different private target or trust domain
while old credentials or cache entries can still serve it.

The private qualification fixtures and reference issuer approve one protected
scope and generation per logical repository. Materialize the view through
native credentials first, configure the exact suffix on the edge and issuer,
and compare their physical prefix with the resolved storage scope. A fresh
operation for another generation uses its scoped origin. A refresh within an
active operation must preserve the captured target and view or stop the read.
Treat changing which bytes an existing edge exposes as an explicit cutover.
Stop old grant issuance, remove access to the old path, and verify denial
and cache retirement before reusing its route. Include generation changes and
rotation in live qualification.

Private prefix authorization applies to all eligible bytes under that prefix
for the grant's lifetime. Ordinary GC removes origin objects and protects active
readers under the existing lease protocol. An edge may still serve an unreachable
object to an authorized viewer until its cache entry expires, including the
provider's stale window. If policy requires tighter retirement, use a separately
revocable physical export or leave that namespace on origin. For confidentiality
deletion, block viewer access before removing objects, then revoke grants, purge
edges and verify denial with old and freshly issued credentials. Qualify this
flow separately from routine GC.

### Eligible reads

The storage layout owns exact key matching. Match the resolved, case-sensitive
repo or global prefix on segment boundaries. For hash fanout require a canonical
lowercase 64-hex digest and its matching two-hex directory.

| Physical objects | Proof required for edge delivery |
|---|---|
| Repo `v2/{capsules,checkpoints,catalogs,pack-layers}/<hh>/<hash>` | Authenticated snapshot or descriptor plus the format's content verifier. For pack-layer ranges, verify the selected section or final Git object ID after delta reconstruction using trusted locator/index data. |
| Repo `v2/history/<hh>/<hash>` and `v2/history/source-roots/<hh>/<hash>` | Exact immutable segment, digest and current retention/visibility rules. |
| Global `{xorbs,shards,capsule-chunk-pages}/<hh>/<hash>` | Authorized physical namespace. Verify whole metadata/pages or authenticate xorb framing, reconstruction terms and all requested chunk hashes before publication. |
| Repo `lfs/objects/<aa>/<bb>/<sha256>` | Verify both fanouts, full declared length and SHA-256. Full downloads can use CDN. Serve a partial read from the same fully verified local file handle, or use the existing verified, strong-validator origin path. A digest for the whole payload cannot authenticate an arbitrary HTTP range. |
| Older Git packs, indexes and generated pack artifacts used by v2 | Enable each caller only after documenting its independently trusted identity, checksum and scope. A generated descriptor keyed by a request hash has no implicit byte digest. Unqualified paths use origin. |

Keep `v2/root`, ref and source heads, source floor, browse roots and reader
claims/descriptors, the chunk-hint root, locks, staging, transactions, config
and registry on origin. Unknown classes, mutable siblings, conditionals,
versioned reads, list, existence checks and strict readback use origin. A
content-shaped name alone never opts a raw read into CDN delivery.

## Implementation boundaries

| Owner and current evidence | Implementation requirement |
|---|---|
| Layout and transport in `crates/crab-storage/src/layout.rs`, `crates/crab-storage/src/store.rs` and `crates/crab-storage/src/store/parallel_read.rs` | Add opt-in immutable byte/range operations requiring the trusted selection and verification context. Reuse bounded transfer machinery. Keep generic `ObjectStore`, `Store::inner()`, conditional GET/HEAD, origin-pinned parallel reads and signed writes on their original contracts. |
| Native v2 readers in `crates/crab-metadata/src/capsule_protocol/store.rs`, `crates/crab-remote-git/src/reader.rs` and `crates/crab-remote-git/src/operation.rs` | Migrate eligible body reads individually. Preserve locator/snapshot identity across the admitted store and its raw inner handle. Keep current-state metadata fresh even on a warm CDN. |
| Cache composition in `crates/crab-cache-store/src/lib.rs`, `crates/crab-cache-store/src/git_pack.rs`, `crates/crab-cache-store/src/xorb_read.rs` and `crates/crab-cache/src/path_class.rs` | Preserve eligibility and cache namespace through wrapping. Route cold Git pack file downloads before the direct S3 signed-range shortcut. Give xorb attempts a distinct CDN source so chunk failures can discard the correct supplier. Carry typed terminal errors through higher-level retries. |
| Budgeting and retention in `crates/crab-storage/src/transport_read_admission.rs`, `crates/crab-storage/src/read_admission.rs` and `crates/crab-read/src/read_retention.rs` | Carry the operation's admission context into real HTTP requests and spawned range tasks. Charge each physical request and received bytes once, including rejected bodies and restarted attempts. Hold permits through EOF or cancellation. Preserve observers and lease cleanup. |
| LFS in `crates/crab-lfs/src/object_store.rs` and `crates/crab-lfs/src/object_store/origin.rs` | Preserve SHA-256 through EOF and existing terminal-chunk behavior. Isolate temporary files and session proof. CDN metadata cannot populate an origin verification receipt. Teach the existing LFS primary retry to propagate terminal failures. |
| Placement in `crab/src/replication/mod.rs` and credentials in `crates/crab-auth-store/src/refreshing_store.rs` and `crates/crab-auth-store/src/managed_repository.rs` | Resolve the CDN after the ready replica and authorized read route are known. An unconfigured replica uses its own origin. A malformed binding or scope change fails the operation. Keep the origin identity when constructing wrappers. |
| Product composition in `crates/crab-sdk/src/store_options.rs`, `crates/crab-sdk/src/local/fetch.rs` and `packages/desktop/agent/src/storage_auth.rs` | Thread typed options through SDK, sidecar and long-lived mount/server readers. Direct SDK clone and fetch transfer capsule packs in the SDK process. Git updates local refs and performs checkout, which can launch filters. Pass the nonsecret target binding to checkout and hydration through operation-scoped configuration so each child obtains its own grant. Keep secrets out of Git remotes, argv, environment, renderer IPC and unrelated children. |

Tagged `v2.0.0-alpha.1` includes the local/service/origin cache and provider
endpoint overrides. The service uses its own capability and object API. An S3
endpoint override also changes writes. CDN therefore needs an explicit read
binding. Extend `crates/crab-storage/src/signed_read_tests.rs` and the existing
`conditional_and_versioned_reads_bypass_a_warm_immutable_cache` and
`mutable_path_bypasses_cache_even_when_configured` regressions alongside the
managed-contract tests.

### Trusted configuration and credential lifecycle

For direct mode use an optional, repeatable mapping in user/operator-owned
configuration, available before clone. A private AWS target would use this entry.

```toml
[[cdn]]
origin = "s3://team-repos"
repo_prefix = "org/models"
base_url = "https://downloads.example.com"
access = "private"
approval_sha256 = "<reviewed private-binding receipt SHA-256>"
```

Infer the CDN provider from the native origin. Use `gs://<bucket>` for GCS
and `https://<account>.blob.core.windows.net/<container>` for Azure's standard
public-cloud endpoint. Normalize the origin to Crab's resolved placement and
verify the native endpoint/partition, bucket or account/container, repo prefix
and physical target identity. The mapping names the source repository. When
the issuer materializes a filtered view, match its authenticated source identity
and issue only for the returned generation-specific physical prefixes. Obtain
the global prefix from the authorized layout. Bind private grants to that
target, view, approved prefixes and exact CDN authority. Reject duplicate or
ambiguous mappings. Template outputs identify the distribution, origin and
path mapping. Bring-your-own deployments must provide equivalent control-plane
evidence and a matching authorized object probe before qualification. A
custom S3-compatible endpoint, sovereign endpoint or filesystem target has no
qualified CDN profile in this release. An explicit misbinding is a configuration
error. Multiple native buckets can use separate mappings in the same process.
Resolve the cloud, account or partition, and bucket/container before matching
a private mapping. If the repository locator lacks that identity, get it from
trusted placement or authorization before asking for viewer authority. An
unqualified replica stays on its selected origin.

Record the storage resource owner and location where the provider exposes
them. Use the S3 bucket owner and region, the GCS bucket's owning project and
location, or the Azure storage account resource and container. Verify them
through native or control-plane authority. A viewer hostname, a cloud profile
name and the identity of the inspecting caller do not identify who owns the
bucket. Exercise a mismatched owner and a replica with a different region.
[S3 expected bucket owner](https://docs.aws.amazon.com/AmazonS3/latest/API/API_HeadBucket.html) · [GCS bucket metadata](https://docs.cloud.google.com/storage/docs/json_api/v1/buckets) · [Azure Blob container resource scope](https://learn.microsoft.com/en-us/azure/storage/common/storage-auth-aad-rbac-cli)

Private is the default access mode for a mapping. Public requires explicit
selection and a qualified public-only `export_origin` separate from the live
`origin`. Both origins must be from the same cloud. Azure exports use a
separate storage account because enabling anonymous access is an account-level
setting. Absent mapping means existing
origin/cache behavior. A requested private mapping needs an existing trusted
issuer and a valid capability before transmitting credentials or reading via
CDN. Repositories cannot select an issuer, endpoint or public mode. Parse CDN
settings separately from the shared user/local/remote `ConfigOverlay` in
`crab/src/core/config.rs`. Reject CDN overrides from tracked project config,
repo-local overlays and remote repository JSON. SDK callers supply typed
options. Managed authority comes from authenticated discovery and grants.

- **Reference Crab Auth.** Extend the response to the existing
  `/v1/credentials` request after `_materialize_filtered_read_scope` and
  provider authorization succeed. Direct cloud credentials alone cannot mint
  viewer credentials. Use an opt-in capability header understood by
  the upgraded issuer to negotiate optional CDN response data. Old servers
  receive the original strict request body, and old clients receive the original
  response. A successful older response yields origin-only capability. Bind the
  optional credential's expiry to the parent read grant and the configured
  revocation window, whichever ends first. See
  `crab/deploy/auth-service/src/app.py` and `crates/crab-auth/src/credential_response.rs`.
- **Managed deployments.** Existing transfer grants reject unknown fields,
  discovery requires support for v1, and the client requests v1 explicitly.
  Advertise `[1, 2]` and request transfer-grant schema v2 only when mutually
  supported. Include a typed optional CDN envelope on direct read grants in v2.
  Old clients/issuers use the original v1 bytes. Gateway-only and upload grants
  retain their existing authority. The managed issuer selects the exact viewer
  endpoint for the authenticated placement. Grant v2 in this release carries
  only repository-local viewer prefixes. Require the response version requested.
  Reject malformed v2, mismatched identity and denied issuance without retrying
  v1. Preserve the existing non-replayed grant
  POST contract. Update OpenAPI, validators and old/new fixtures. Production
  managed issuer rollout requires its owner outside this repository. See
  `crates/crab-auth/src/managed/transfer.rs`, `crates/crab-auth/src/managed/client.rs`
  and `crates/crab-auth/tests/managed_contract.rs`.
- **Capabilities.** Use a bounded, provider-typed envelope of exact audience,
  selected deployment, origin, repo/view, operation, scope prefixes, expiry and
  corresponding short-lived cookie set or token. Look up the capability for
  each requested physical path. Sign only issuer-selected, segment-delimited
  scopes in this release. A narrower policy that cannot be represented stays
  on its authorized origin. Use separate signer trust per provider deployment
  and keep signing keys in the operator's secret manager. Bound and redact
  issuer responses and errors. Credential negotiation, reverse proxies,
  tracing, and HTTP diagnostics must prevent credential replay from logs or
  shared caches.
- **Refresh.** Keep viewer capabilities in memory. Never serialize them in the
  existing credential cache or reuse filtered grants across view generations.
  Coalesce refresh by principal, target, view and *observed credential
  generation*. Share the in-flight result, including failure, for the lifetime
  of the operation. A cancelled waiter must release its wait without reopening
  the rejected generation. Revalidate before later ranges.
  Expiry/key rotation with identical scope can resume at a verification boundary.
  Changed scope or placement cancels the operation and requires fresh resolution.
  The existing refresh path and cache policy live in
  `crates/crab-auth-store/src/refreshing_store.rs` and `crates/crab-auth/src/crab_auth_client.rs`.

## HTTP delivery and failure behavior

Construct one canonical HTTPS URL from the approved base and physical key. The
template maps that path to exactly one bucket/container and preserves its case.
Use the locked object-store path representation and its signing/encoding vectors.
The first profile accepts only canonical ASCII binding and key paths. Existing
repositories with paths outside that grammar use origin. Exercise spaces,
Unicode, and literal percent signs in the transport vectors to prove they cannot
alias admitted keys. Any later expansion must prove round trips to the same
origin object across issuer, proxy and edge. Reject ambiguous separators,
traversal, query/fragment/userinfo and all redirects. Attach only the
provider-specific viewer capability to that exact request. Disable ambient
cookie jars, automatic decompression and byte transformations. Never forward
origin credentials. [object_store 0.14.1 path contract](https://docs.rs/object_store/0.14.1/object_store/path/struct.Path.html)

For full GETs enforce object limits, expected identity, length and EOF. For a
nonempty single range require a matching 206, exact Content-Range including
the total length, and exactly the requested number of bytes. A 200 response
to a range is a failed attempt. Handle zero-byte files and empty ranges
locally when authorized metadata proves them. Bound parallelism, memory,
temporary disk and file offsets. Select origin upfront when the provider's
supported object/range sizes or the caller's verifier cannot serve the request.
Record each qualified profile's full-object, range and duration limits and
enforce them before sending the first edge request. Reuse trusted descriptor
sizes to avoid an extra origin HEAD per object.

| Outcome | Required behavior |
|---|---|
| Connection timeout, verified transient availability failure, throttling or qualified 5xx | At most one restart at the independently authorized origin of the selected target. Share the operation deadline, physical request/byte budgets and cancellation across both sources. Qualify statuses against the actual deployment before treating them as retryable. |
| 401/403 | Refresh or recheck once on the same distribution within deadline. If denial persists, surface it through all outer cache, xorb, Git, LFS and replica callers. Zero origin reads after denial. Never interpret an ambiguous 403 as a miss. |
| 404 or unclassified status | Terminal. Any future miss-based retry needs evidence that no viewer, gateway or origin denial can produce that status. |
| Framing or content verification fails | The verification owner discards that attempt and can restart once at selected origin if no bytes escaped, budget remains and the whole unit can be reset. Distinguish edge corruption from origin corruption. |
| Streaming failure after caller-visible bytes | Terminate the stream with its typed error. No transparent source switch or replay. For file downloads, cancel and join parallel writers, discard the private partial file and atomically publish only after verification. |
| Invalid binding, TLS identity failure, redirect, lost scope/lease, caller cancellation, local I/O failure or exhausted budget | Stop and drain. No alternate endpoint, primary-replica retry, cache success or authorization downgrade. |

A restart belongs to one owner per logical verification unit. A restart is
permitted only before caller-visible output. Stop and drain sibling reads when
any range reaches a terminal failure. An availability miss can authorize a
restart only after all in-flight siblings have either succeeded or returned
recoverable outcomes. Once the edge has denied a request, only a successful
same-edge recheck can restore the unit's eligibility. Existing backend retries,
verification, clean-up and file writes must respect that unit's remaining
deadline and budget. Preserve both sanitized failure causes and account for
discarded bytes. Origin restart preserves availability for the same selected
target when acceleration fails.

## Provider deployments

All templates expose only approved immutable routes and read methods. Use a
default-deny route on every exposed hostname, including the provider's default
domain. Reject alternate object APIs, listing, version/response-override
queries and unexpected methods before cached delivery. Isolate public/private
routes and cache keys by physical target, view namespace and representation.
Share across authorized users only where every request passes the applicable
authorization check. Set the immutable edge TTL independently of viewer expiry.
Exclude per-user credentials from the shared cache key only after authorization.
Prevent downstream shared HTTP caches from reusing private viewer responses.
Test both hits and misses.

For public delivery, publish verified immutable objects into a dedicated export
using an operator-approved inventory. Bind the source repository identity and
export origin separately, preserve the relative key and digest, and keep live
repository state and writes at source. The export writer may publish only
approved objects and must reject replacement with different bytes. Qualify
anonymous direct-origin access as part of the public disclosure. On GCS, grant
anonymous object GET on the export bucket only and deny listing. Any approved
metadata exposed by anonymous object reads is part of the inventory. Verify
version and snapshot access at the native origin so that retired bytes with a
different disclosure policy never become readable through a version query.
[Cloud Storage public access and IAM](https://docs.cloud.google.com/storage/docs/access-control/making-data-public)

Publish and verify every approved destination object before committing its
membership in an export inventory. Commit the inventory through the trusted
operator or issuer after readback from the export. Bind the inventory to the
source target, repository or view, export target and viewer path. Include the
canonical key, length and digest for each admitted object. At routing time,
check membership against the immutable object selected by native metadata. A
new ref can become visible before its objects are approved and exported. For
objects absent from the captured inventory, select the independently authorized
native store before making any edge request. A 404 on an admitted object is
still terminal. Exercise incomplete publication, retries, new refs and
concurrent inventory updates. A partially copied export must never be marked
complete or expose bytes outside the separately approved disclosure inventory.

At activation, attest that the native origin and distribution are correctly
paired, the same canonical key reaches the same verified bytes, and the
installed viewer policy denies out-of-scope requests before cache delivery.
Retain approval from the trusted operator or managed issuer for the exact
resolved target, view, HTTPS authority/path, access mode, export origin where
applicable, distribution resource identity and observed policy digest. A
compiled release row authorizes the class of deployment. The per-binding
approval authorizes the concrete deployment. Check both at the product's
activation boundary, before sending any viewer credential. Reuse the approval
for a captured operation without adding a control-plane request to each read.
An unapproved but otherwise valid target uses its authorized native route.
A conflicting approval or an attempted change to a captured binding fails the
operation. Keep deployment changes within the same operator trust boundary as
approval. Withdraw approval before changing the origin, DNS target, host/path
mapping, signer trust, query handling, or cache authorization. Verify the new
control-plane snapshot and rerun the affected probes before reactivation.
For a change that could expose new bytes to old grants, also deny the old
route at the edge and verify denial before moving traffic. Routine key
overlap can proceed through explicitly approved transition states.
Test a second HTTPS endpoint with a valid certificate but no matching approval,
as well as a previously approved endpoint after a trust-sensitive change.

| Origin | Public delivery | Private delivery to qualify |
|---|---|---|
| S3 | CloudFront with a restricted S3 REST origin and explicitly public export routes | Read-only Origin Access Control scoped to the distribution, plus CloudFront trusted key groups and signed cookies restricted to approved paths. Keep the bucket private. |
| GCS | External HTTPS load balancer, Cloud CDN backend bucket and isolated public export | Private GCS bucket, cache-fill service identity and short-lived Cloud CDN signed cookies. Use slash-terminated literal URL prefixes. Deny unsigned access on private routes and isolate the default backend. |
| Azure Blob | Front Door with an isolated public export and explicit container mapping | Customer-operated Azure API Management (APIM) before a restricted Front Door cache, then Blob. This extra hop and its cost are part of private Azure qualification. |

**AWS.** Check cookie expiry on each new range. Size-limit large transfers using
CloudFront's documented range behavior. Verify that actual objects written by
Crab cache at the edge while the response headers policy sends
`Cache-Control: private, no-store` to viewers. Set the lowest available error
TTL and disable origin error TTL enlargement and status rewriting. S3-backed CloudFront
can retain errors for at least one second, and a missing S3 key can return 403
when listing is disallowed. For a freshly referenced key, the single same-edge
recheck may wait out the configured error window within the deadline. Persistent
403 remains a denial. Test a miss immediately before publish followed by an
authorized fetch immediately after. [OAC](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-restricting-access-to-s3.html) · [Viewer cookies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-signed-cookies.html) · [Range GET](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/RangeGETs.html) · [Error TTL](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/custom-error-pages-expiration.html) · [S3 GetObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html)
Use overlapping trusted public keys during routine rotation. Verify a grant
signed with each key during the overlap and verify denial for the retired key
after removal. Exercise the cloud fill identity with customer-managed
encryption if the profile supports it. [Viewer response headers](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/modifying-response-headers.html)

**GCP.** Unsigned requests can pass through Cloud CDN to the backend. The
deployment must deny missing viewer capability on private routes before
delivery and deny anonymous bucket reads. Attach a default-deny Cloud Armor
edge policy to the private backend bucket. Require the expected host, method,
path and exactly one signed cookie, then let Cloud CDN verify its signature,
expiry and URLPrefix. Keep a default backend with equivalent denial on
unmatched hosts or paths. Cloud Armor uses RE2 without backreferences. Check
fanout equality with separate anchored rules that match the same hex digit
at each corresponding position. Keep the rules within provider size and
expression limits and compare their decisions against the storage classifier.
Grant the cache-fill identity read access only to the approved export, keep
keys out of infrastructure state, and set the edge TTL using the signed-request
cache policy. Verify the client-facing `private, no-store` header on a warm hit
and prove that it leaves the signed cache useful. Test absent, malformed,
duplicate and wrong-scope cookies on cold and warm paths. Confirm origin
validators, representation, and range-fill behavior on repeated reads. For
partial responses, Cloud CDN can use the first request to learn that the
origin supports ranges. Confirm a cache fill and a later hit with provider
logs before counting the warmed samples.
[Signed cookies](https://docs.cloud.google.com/cdn/docs/using-signed-cookies) · [Edge rule language](https://docs.cloud.google.com/armor/docs/create-security-policy-rules) · [URL map defaults](https://docs.cloud.google.com/load-balancing/docs/url-map) · [Byte-range caching](https://docs.cloud.google.com/cdn/docs/caching#support_for_byte_range_requests)

**Private Azure.** The proposed reference deployment is
`client → APIM → Front Door → private Blob container`. APIM validates a
short-lived Crab issuer JWT on every request, including cache hits. Enforce
issuer, audience, expiry, read method, physical view/prefix and exact canonical
object class. Strip viewer authorization before streaming to a fixed Front Door
backend. Disable APIM response caching and redirects. Use a dedicated classic
APIM tier with exclusive, deterministic public egress. Front Door WAF must
block traffic outside those exact addresses by observed socket address on every
domain. Restrict all gateway routes and policy authors that can use those
addresses to the same authorization boundary. Validate APIM's observed egress
before enabling the Front Door route. Remove obsolete allowlist entries before
releasing addresses or tearing down the gateway. Exercise direct alias and
alternate gateway operations in the deployment test. Use Front Door managed identity with narrowly
scoped Blob data-read permission and anonymous Blob access disabled. This
reference uses the authenticated Blob endpoint. Front Door managed-identity
origin auth currently excludes Front Door Private Link. Keep authorized native
Crab access to Blob working for metadata, writes and recovery. [Gateway JWT validation](https://learn.microsoft.com/en-us/azure/api-management/validate-jwt-policy) · [Gateway forwarding](https://learn.microsoft.com/en-us/azure/api-management/forward-request-policy) · [APIM IP ownership](https://learn.microsoft.com/en-us/azure/api-management/api-management-howto-ip-addresses) · [Front Door WAF](https://learn.microsoft.com/en-us/azure/web-application-firewall/afds/waf-front-door-configure-ip-restriction) · [Origin identity](https://learn.microsoft.com/en-us/azure/frontdoor/origin-authentication-with-managed-identities)

Front Door's native Entra JWT policy is documented as public preview. The
reference deployment uses APIM viewer validation. Microsoft also lists
Front Door managed-identity origin authentication as preview in its deployment
guidance. Treat that Azure fixture as qualification-only until the origin
authentication profile meets the release requirements. [Front Door security guidance](https://learn.microsoft.com/en-us/azure/frontdoor/secure-front-door)

Probe GET and range delivery directly. Front Door caches GETs, handles large objects
in chunks, and can serve stale content during origin outages. Measure end-to-end
from the remote client through APIM. In a single-region deployment the gateway
can remain far from the viewer even on an edge hit. Record gateway region, its
observed Front Door edge, throughput limits and charges. Release private Azure
only if that path meets the same security and performance gates. [Front Door authentication policy](https://learn.microsoft.com/en-us/azure/frontdoor/authentication-policy) · [Front Door caching](https://learn.microsoft.com/en-us/azure/frontdoor/front-door-caching) · [Blob integration](https://learn.microsoft.com/en-us/azure/frontdoor/scenario-storage-blobs)

## Delivery and acceptance

| Slice | Deliverable and exit evidence |
|---|---|
| 1. Freeze contracts | Storage, auth and format owners assign every read family a physical scope, verifier and source. Add layout, encoded-path, projected-view and old/new grant fixtures. Start with the private S3 deployment in an isolated test account. |
| 2. Build shared transport | Loopback HTTP against a real local object store exercises bounded reads, false ranges, truncated or transformed bodies, wrong bytes, zero length, cancellation, file cleanup, budgets and one-source restart. Extend signed-read, conditional-cache and parallel-read regressions. |
| 3. Qualify private CloudFront for direct CLI | Reference issuer, trusted direct config, OAC template and CLI wiring. Push to S3 and clone/fetch Git and LFS from a clean machine. Use a protected view to qualify private Xet hydration. Repeat with a second authorized principal at the same edge, verify Git IDs and payload hashes, and test newly published data and persistent denial. |
| 4. Qualify GCP and Azure | Provide provider templates and bring-your-own deployment checks. Repeat slice 3 per public/private profile, including default hostnames, rotated/expired credentials, range-fill amplification and anonymous origin policy. Record Azure gateway overhead separately. |
| 5. Complete consumer coverage | Exercise CLI/remote-helper clone and fetch, SDK browse/content and Git subprocesses, desktop visible reads, Xet hydrate/filter, VFS mount, LFS full and partial, and server/S3 gateway repository reads. Preserve client-facing HTTP auth, ETag and range semantics. Run packaging/feature combinations, including cross-OS subprocesses, in CI. Publish storage, auth, cache, desktop, SDK and web setup/doctor guides and correct earlier edge-caching advice. |
| 6. Release per qualified profile | Canary by physical target and consumer. Retain real-provider evidence for each advertised mode and entry point. A managed profile additionally needs the production issuer's v2 compatibility and authorization evidence. An unqualified mode remains disabled. |

Release eligibility is specific to provider, public/private mode, consumer,
read family and direct or managed profile. The release policy in
`crates/crab-storage/src/cdn/qualified-scopes.txt` lists the exact enabled rows.
An opted-in binding must also satisfy the native origin and immutable key checks
at the storage boundary. A qualified Git pack read leaves capsule, LFS and Xet
reads on native storage until their own rows qualify.

All provider routes currently have fixture-only status. Default builds keep
HTTPS bindings on origin, omit the direct viewer-grant request header, and
request managed transfer grant v1. Direct private bindings continue to use the
same principal and native scope. The `provider-cdn-qualification` Cargo feature
enables real-provider routing in dedicated builds. The release policy is empty
pending retained evidence. The report verifier can require that a verified
scope exactly match a row in the compiled policy.

The client negotiates private viewer authority only when every configured
provider the issuer can select for that repository has an eligible row for
the caller. Direct bindings retain the same principal and native scope during
a mixed-provider rollout. A provider-selection contract would be needed to
request a viewer grant for just one provider on a shared locator.

Promote private CloudFront direct CLI Git pack reads first. Qualify the
exact release candidate commit and the ordinary release build for that row.
Use the qualification feature to develop and test fixtures. The candidate's
ordinary build must demonstrate the promoted read and the origin behavior of
adjacent unpromoted families. Block promotion when required evidence still
depends on a generic trace or an externally supplied pass flag.

Start with the dedicated S3 repository and CloudFront distribution. Configure
`provider-qualification-s3` for the native contract job and
`provider-cdn-qualification-s3` for the read-only CDN inspector. Restrict both
GitHub environments to trusted refs. Use a fixture-scoped writer for the
product workload and a dedicated Crab Auth issuer for the independently
authorized viewers. Register the approved deployment and benchmark before
gathering release samples in both a near-origin and a distant region.
[Hosted canary setup](../crab/docs/guides/provider-qualification.md#run-the-hosted-transport-canary)

The AWS qualification fixtures can use the companion
`crab/deploy/cdn/aws/qualification-logs.yaml` stack for viewer and origin
logs in `us-east-1`. It selects the exact CloudFront fields needed by the
collector, gives the fixture's S3 origin its own log prefix, and expires
raw logs after the qualification window. Verify delivery before the first
benchmark sample.
The viewer evidence collector can read both feeds from that approved stack
using a read-only AWS role. It checks the distribution, origin, log bucket
owner, logging target, and object validators before joining the records.

The scripted Git workload guards the native origin and effective repository
prefix in both fixture and ordinary builds. The guard runs before repository
roots and replica readiness, and records origin and prefix digests. Its
read-only inspection records the binary SHA-256,
compiled commit, target, Cargo profile and features, qualification feature
state, and digest of the embedded policy. Direct CLI product records and Git
benchmark arms must match the registered build. Edge reads confirm the
attached viewer's access mode and HTTPS base against the registered digest.
Promotion rejects fixture builds and requires the exact checkout policy.
For the ordinary candidate run, restrict the writer to the preregistered
native target and test prefix.
The independent `cdn-native-ref` probe reads the provider's current root and
exact single-ref head using native credentials. The release verifier requires
the committed tip before the fetch and a distinct provider tombstone in the
same repository epoch before physical cleanup. Verify the selected replica
and edge payload through redacted observations.

### Required release evidence

- **Isolation and freshness.** Use two repositories, two users, a filtered view
  and shared physical content. Test prefix neighbors, multiple buckets in one
  process, same-name custom endpoints, scoped global objects, co-packed data,
  a stale local/service cache, replica selection, staged data, read retention
  during GC and expiry between parallel ranges. Exercise authorization failures
  through each outer retry owner. Assert the observable output and actual counts
  of edge, origin and primary requests. Cover CDN-off operation and offline cache
  behavior. Exercise the server and S3 gateway owners in
  `crates/crab-server/src/server/storage.rs` and `crates/crab-s3-gateway/src/repository.rs`.
  Positive cloud tests include a real write followed by an immediately
  visible new ref and a byte-identical fetch through the ordinary product.
  The direct CLI workload in `crab/scripts/e2e/run_cdn_git_workload.py`
  publishes a leased test ref, verifies bytes from a fresh fetch, requires
  a completed edge GET and a payload accepted after integrity verification,
  and retires that ref. For LFS it stages the pointer with `crab lfs clean`
  and verifies the object from an ordinary `crab lfs fetch` after fetching
  the commit. The release gate derives direct CLI Git, capsule, and LFS
  product evidence from the exact publication receipt and its
  ordered fetch and retirement records. The retained product fetch must
  match provider viewer logs, including delivery of the exact new object
  for public transitions. The fixture owner separately proves
  storage and export cleanup. The read-only cleanup collector verifies empty
  native inventories for the exact test prefix, records live and historical
  inventory results, and binds public exports separately. The release gate
  requires the direct Git inventory check after ref retirement. The helper
  checks the approved native origin against the repository, selected read
  replica, and prepared push store.
- **Deployments and operations.** Inspect actual route, IAM, cache key, TTL,
  key-rotation, DNS/TLS, error and logging settings. For public and private
  delivery, bracket HEAD and provider-specific version, generation, snapshot
  and response override denials with byte-verified full cache hits on the same
  object. Verify private viewer authorization on warm hits after expiry,
  unauthorized prefix changes and downstream cache behavior. Exercise supported
  origin encryption/key access where applicable. The release gate requires a
  deployment snapshot digest approved in the benchmark registration. The
  CloudFront, Cloud CDN and Front Door collectors verify read-only
  control-plane observations for the reference private and public deployments.
  Cloud CDN checks DNS, the forwarding rule, and conditional bucket IAM. Front
  Door checks the default endpoint, WAF, APIM viewer policy, and Blob role
  assignment or public export settings. `crab doctor --cdn-probe` uses the
  fetch resolver to report the primary or ready replica binding, eligibility,
  credential lifetime, origin availability,
  byte-verified GET and range delivery, and the mutable-root denial status. It
  uses an origin-verified Git object of at most 1 MiB referenced by the current
  root on the selected physical target, including a pack layer from a native
  checkpoint. Exercise it on existing authorized cloud fixtures.
- **Observability and rollback.** Count source, hit/miss/unknown, bytes, latency,
  rejected bytes, refresh and restart outcomes. Retain provider cache logs or
  headers plus origin fill evidence. Product edge attempts emit redacted,
  scope-specific transfer events. The qualification reducer counts completed
  bodies and incomplete bytes separately. Redact cookies, JWTs, signed URLs and
  sensitive headers in all client and infrastructure logs. An operator can stop
  issuance, remove the target binding and revert to authorized origin reads.
  For a disclosure or deletion, restrict the viewer route, revoke/rotate grants,
  purge edge entries, verify denial on every hostname and record propagation
  time. Already distributed bytes cannot be recalled. Ordinary pushes need no purge.
- **Performance and cost.** Pre-register workloads, sample size, thresholds
  and budgets before release measurements. Suggested initial gate: at least
  30 paired, interleaved samples per arm, a positive lower bound on a 95%
  confidence interval for distant-region warm-edge median action-time savings,
  and p95 within the agreed action budget. Compare origin-only, cold edge,
  warm edge with a fresh client, and warm local cache. Include near-origin and
  remote regions, clone/fetch, large hydration, small ranges and concurrent
  clients. Measure client bytes, origin GETs and fill bytes, memory, local disk,
  egress, CDN requests and any gateway charges per successful action. Include
  the gateway's fixed deployment cost at the expected traffic volume. Use
  provider hit evidence and the same commit, repo and workload in every comparison.
  The direct CLI Git, capsule and LFS workloads include native-origin and
  warm-local controls for the same publication receipt. Each pair retains
  the writer's exact receipt. Verification checks the candidate, commit,
  payload, and ref visibility before the first measurement. The origin arm
  verifies the binding is disabled.
  Each timed arm confirms its isolated local cache root, and qualification
  rejects a configured shared cache. Public edge and warm-local arms pin the
  active sealed export receipt alongside the viewer URL. Stage their immutable
  benchmark objects before sealing the preregistered receipt. The warm-local
  arm requires a verified pack cache hit from a fresh Git client sharing the
  cache of its own edge primer, or a SHA-256-verified LFS cache hit in the
  primed worktree.
  `crab/scripts/e2e/collect_cdn_git_performance.py` binds each timed fetch to its
  measured provider and runner metrics, checks ordered same-publication pairs
  across two preregistered regions, and applies the release budgets. Assembly
  and release verification match each direct CLI benchmark arm to the
  independently observed native origin and repository prefix. Assembly
  retains the direct CLI Git, capsule and LFS measurement inputs. The release
  gate recomputes each summary and checks the complete platform matrix.
  CloudFront, Cloud CDN and Front Door measurements match timed fetches to
  provider logs using hashed viewer request IDs and retain the verified
  status, cache result, and viewer response bytes. CloudFront verifies
  signed reads from the approved S3 bucket using the viewer keys and timed
  window. Its cold arm requires a successful origin body response. The
  collector can wait for both AWS log streams before returning a cold proof.
  Cloud CDN retains cache-fill bytes on viewer log records and joins
  range-fill and validation requests by the provider's parent log-entry ID.
  The collector can read those records directly from Cloud Logging for the
  approved project, forwarding rule, and time window.
  Every Cloud CDN cold arm must show a successful fill linked to a completed
  viewer response. The benchmark checks external origin GETs and fill bytes
  against these observations. Front Door separates viewer responses from
  origin-shield traffic and joins Blob reads to measured keys and time.
  The collector can read the hourly archives from the storage destinations
  reported by the two approved resources' diagnostic settings.
  Private rows match the profile's system-assigned identity. Public rows match
  anonymous reads of a dedicated export from the Front Door backend service tag.
  The cold arm requires a successful Blob response with body bytes.
  Each timed fetch records redacted native HTTPS GET and HEAD response counts
  and consumed body bytes. The origin control must observe a native GET with
  payload bytes, and external totals must cover the observed GETs and combined
  edge and native bytes in every arm. Provider-side origin fills and runner
  totals remain independently measured inputs. Live metric collection and
  provider runs still need configured fixtures.
- **Artifacts and sign-off.** Keep a versioned CDN evidence supplement beside
  the existing provider qualification report. The verifier at
  `crab/scripts/verify-provider-qualification-report.py` checks the native
  storage contract and the optional v1 CDN supplement. It binds both reports by
  commit, run, attempt and native report digest. The release job supplies the
  required row, platform, configuration digest and prior benchmark registration.
  Release preparation calls `crab/scripts/release/verify-cdn-release-evidence.py`
  before the CLI and desktop jobs. It uses the exact successful workflow run
  attempt and the previously approved benchmark digest, then verifies every
  enabled row on the required platforms against the compiled scope inventory.
  The provider workflow can qualify native S3 using a dedicated OIDC role.
  `crab/scripts/e2e/run_cdn_transport_checks.py` records the exact GET and range,
  mutable and neighbor denial, and optional warm viewer expiry traces from an
  existing approved CloudFront, Cloud CDN, or Front Door fixture. Private
  evidence requires an authorized cache hit on the same complete object
  before and after denied anonymous full and ranged GETs.
  An unrelated viewer must also be denied full and ranged GETs of that warm
  object. Its grant must read a separate, approved control object before and
  after the denials. The control uses a disjoint grant prefix on the same
  endpoint, so an expired or invalid credential cannot supply the evidence.
  The transport expiry probe checks the signed expiry and verifies two
  overlapping range reads of the same warm object on each side of expiry.
  Both early reads must return the approved bytes, and the expired grant
  must be denied both ranges. Product refresh and expiry during an in-flight
  action still require workload evidence.
  A second authorized viewer grant exercises same-object cache reuse. The
  collector verifies the second response's bytes and cache hit and records
  miss-to-hit evidence when the first response was cold. Distinct credential
  strings establish two grants. Prove two distinct principals with redacted
  issuer observations from two independently authorized subjects. Correlate
  issuance and probes within the same attempt and fixture. Record only
  pseudonymous principal labels, signing key IDs and scope identifiers.
  Use fresh grants from the real issuer after preflight attestation. Finish
  any native job or environment approval before minting them. Keep native
  inspection, the two authorized subjects and the unrelated control in
  their separate roles. A CI implementation may use protected workload
  identities if the issuer authorizes each subject for its fixture scope.
  Use a short, independently issued grant for the expiry window. Keep viewer
  credentials out of artifacts and avoid long-lived credentials to cover
  scheduling delays. An approved hosted private fixture can request its primary
  viewer grant from a dedicated Crab Auth issuer using GitHub OIDC after
  deployment preflight. The collector binds the response and signed prefix to
  the exact fixture. Live issuer setup, distinct-subject evidence, and the
  separate expiry window still require qualification.
  The collector also probes live-origin anonymity and the separate public
  export object and listing policies through the provider's native API.
  The verifier pairs live-origin denial with a successful authenticated read
  of the same complete object. The hosted canary uses the protected provider
  identity for the exact native GET. S3 uses an in-memory signed URL, and
  GCS and Azure use bearer tokens on their native APIs. The collector verifies
  the full response against the edge bytes. A separately issued signed URL
  remains available for standalone collection.
  Manual provider qualification uses read-only OIDC credentials in the
  protected canary environment to attest the approved deployment before and
  after the transport and origin probes. The canary requires native evidence
  from the same commit and attempt. Publication waits for requested canaries.
  `crab/scripts/e2e/assemble_cdn_qualification.py` combines measured results
  with native evidence and approved benchmarks, verifies every required
  platform, and stages the release artifact. Assembly imports same-attempt
  canary traces for the exact scope and fixture digest. Conflicting measured
  traces or failed observations stop publication. A manual provider qualification
  run can publish the verified bundle using same-attempt native reports and
  the measured inputs on a dedicated runner. The live product, deployment,
  and regional jobs or external workers still need configured fixtures.
  Record provider/profile, configuration, region, client
  and product action, expected/observed bytes, redacted traces, metrics and
  cleanup. Use test-owned prefixes and distributions in dedicated CI. Level 3
  requires user action, real cloud side effect and visible verified content.
  Release records must distinguish verified provider, access mode, consumer and
  managed issuer. A passing mock or another provider's evidence cannot fill a row.

The release verifier must enforce the following check-specific observations
before accepting a row. Use the existing report's commit, scope, configuration,
run and attempt to bind each observation.

| Gate | Required observation |
|---|---|
| Authenticated cache reuse | Preregister the exact private object, issuer endpoint and approval, and two distinct subjects and policy identities. Exchange each OIDC ID token with Crab Auth for the same read scope. Observe a cold miss and then a warm hit for the same complete bytes within the two signed grant lifetimes. Keep only identity and object fingerprints and request observations. |
| Key rotation and retirement | Before and after policy snapshots with the actual key IDs. Same-key verified reads by both signers during overlap. After the retired key has left the deployed policy, that grant must still be within its signed lifetime and must fail for both full and ranged warm reads. The replacement grant must still return the original verified bytes. Verify the bounded rollout and retirement window per provider and hostname. |
| Scoped view and rollback | Preregister the isolated view and sibling control. The collector attests the scoped deployment, verifies warm target bytes and native sibling bytes before and after full and ranged sibling denials, and retains source, scope, generation and immutable-prefix digests. Use the projected shared prefix when applicable. Direct CLI Git, capsule, and LFS release rows require an ordinary edge fetch followed by a native fetch with the binding withdrawn, before retiring the ref. The verifier checks identical verified output, the same independently observed native target and prefix, distinct local caches, and zero edge and cached payload reads after withdrawal. Live scoped-view and direct CLI evidence and ordinary rollback producers for other consumers remain open. Test terminal viewer denial separately. |
| Public release | Retain the prior and current publish/readback receipts and inventories. The direct CLI Git and LFS fetch workers, SDK and desktop Git read workers, and Git and LFS service workers verify the native ref, active prior approval, absent object membership and zero edge requests on the first read. After sealing the new export, they verify identical content and the exact object through the active edge binding. The release verifier checks both pinned receipts, immutable inventory preservation, readiness order and the retained product records. Other consumer and read-family transition producers remain open. The native history collector checks approved retained versions or snapshots using both an authorized read and anonymous denial. Release verification pins the observed selectors and distinct historical byte digests to the same fixture object as the current public export. Collect the traces on the protected measurement worker in the release attempt. |
| Managed release | Preregister the production issuer's identity digest and deployed version and the binding receipt digest in `managed_issuer`. The verifier requires a `crab.cdn-managed-issuer/v1` trace for the same platform and workflow attempt. It checks the authenticated resolution digest, attested private placement, matching v2 request and HTTP 201 grant, and the ordinary action's verified output and edge observations. A v1 peer must verify the same output through native reads. HTTP 403 and a v2 grant with a different approval must each terminate with a single v2 request and zero payload reads. The protected production observation job remains open. |
| Other consumers and read families | Retain action records, verified outputs and associated provider/runner measurements for the row's actual entry point. Recompute the product and performance summaries from those inputs, as the direct CLI Git reducer does. For native controls, compare normal authorization behavior and check that the candidate feature set matches the shipped build. |

The SDK Git snapshot collector uses the ordinary SDK open, snapshot and read
APIs through `crates/crab-sdk/examples/cdn-sdk-read.rs`. It binds redacted edge
and native controls to a confirmed native publication and an exact SDK probe
build. Each arm confirms the native origin and effective repository prefix
captured after SDK authorization. The edge arm confirms the active viewer,
the public receipt where applicable, and the Git object ID fingerprint for the
verified blob delivered to the caller. The independent native ref collector
also accepts the SDK fixture. Each SDK snapshot, clone, and fetch edge arm
requires its own provider viewer evidence. For public SDK Git, a successful
snapshot request must match the exact pack-layer key added to the approved
export. Assembly and verification
derive the product result from the registered SDK build, verified edge
snapshot, ordinary native rollback, independently observed publication and
tombstone, and fixture cleanup. The SDK Git release row also requires a fresh
`Client::clone_local` through the CDN and a native clone of the same ref.
The probe verifies the checked-out commit and file bytes and matches the
pack containing the file to a digest observed by the CDN pack reader.
The release row also requires paired `Local::fetch` evidence. From the same
local baseline, both arms must publish the new remote-tracking ref without
changing local HEAD or checking out the fixture file. The probe checks out
the fetched ref afterward and verifies the visible file. Both arms must
install the pack verified during the CDN fetch before native ref retirement.
Regional SDK inputs use a fresh probe for the native,
cold edge and warm edge arms. The local arm repeats the blob read on the
warm probe's pinned snapshot. It proves the exact checksum-verified cached
blob and zero edge attempts. Assembly and release verification rebuild the
paired summary from the SDK records and independent provider and runner
measurements. For public releases the worker also runs the ordinary SDK blob
read with each active approval. The prior read proves native delivery of
the exact blob from its unlisted pack layer. The current read proves CDN
delivery of that blob from the same layer. The release verifier reconstructs
the transition trace from the retained SDK records. Live runs still require
the protected provider fixtures. Xorb readers now report the physical key
fingerprint after verifying the requested chunks from the edge. The
collector can match that fingerprint to the approved projected view.
The SDK hydration worker captures allowlisted child observations and checks
the file written by the ordinary hydration API. It binds the projected
view, exact xorb, local cache and file digest to the approved fixture.
The native control keeps the same issuer and withdraws viewer approval.
Assembly derives the SDK xorb product result from the independently
observed native pointer publication and retirement, both SDK hydration
records, provider viewer evidence for the exact xorb, and the scope and
cleanup observations. Regional samples cannot reuse the lifecycle viewer
requests. Release verification
recomputes that result and requires both arms to use the same approved
view and separate caches. The source pointer is published before the
projected view is materialized, then the xorb workload pins that publication
receipt. SDK regional hydration pilots use four arms. The warm-local arm
restores the tracked pointer and hydrates again with its own warm edge
primer's cache. It verifies the exact reconstructed file and local xorb key
with zero edge requests. Assembly and release verification rebuild the
regional summary from the retained samples, require isolated cache roots for
the other arms, and match viewer requests to the exact approved xorb.
Scopes with generic workload summaries can produce fixture reports. Release
verification requires a product-specific verifier that rebuilds the summary
from individual runs.

Crab Server's loopback HTTP smoke verifies complete Git and LFS bodies through
the ordinary read handlers with those native bodies blocked. It checks that
partial LFS reads stay on origin and withdrawing the CDN layout gives a fresh
native Git read with identical bytes and ETag. The service qualification
worker verifies ordinary Crab Server and S3 gateway Git and LFS responses
against a digest-pinned workload and captures allowlisted storage observations
from the dedicated service process. The authorized read handlers report the
selected native origin and repository prefix, active viewer and loaded public
export receipt. The worker checks those fingerprints against the approved
fixture and confirms the binding was withdrawn for native rollback. The
provider log collector joins every attempt and matches LFS reads to the exact
approved object. The native writer can publish these service fixtures and
verifies LFS bytes against the approved fixture before pushing. Both service
read arms require the same independently observed native publication. The
lifecycle verifier binds the edge and rollback records, provider viewer logs,
distinct native retirement tombstone, and final storage inventories. Assembly
and bundle verification derive the service product result from the retained
records and the preregistered payload size, ETag, and storage fingerprints.
Service fixture assembly rebuilds each benchmark pair from a native
control, distinct cold and warm edge processes, and an exact re-read using
the warm edge process's verified in-memory Git blob or disk-cached LFS
object. The verifier joins provider
requests and proves each arm used the same published fixture before
retirement. Both service images embed the checked-out commit and compile the
ordinary CDN feature. Each read pins the executable hash, target, build
features, and embedded policy reported by its own process. Service
promotion requires the registered release binary and per-run evidence. For
public service Git and LFS, assembly derives the transition from ordinary
reads in fresh processes with the prior and current receipt. It checks the
same native publication, bytes, ETag, and physical key against each approved
inventory and joins the new object's successful delivery to its provider log.
Full LFS verification and CLI local-cache observations also carry the exact
physical key fingerprint. The service and CLI workers require the approved key
when counting those observations.
For eligible full service LFS reads, the gateway uses its existing disk cache
budget and Crab Server reserves a capped share of local disk capacity.
Namespaces bind the physical target, repository, and active CDN approval.
The service worker measures the repeat in the primer's process and proves
the exact verified local LFS key, its opened cache root, and zero edge
requests. The regional verifier rejects cache-root reuse by fresh arms
and requires the local repeat to match its own edge primer's root. Live
regional measurements remain open.

Desktop headless qualification can launch the ordinary sidecar with
`--log-json` to collect the SDK target and provider transfer events from
stderr alongside the existing RPC results on stdout. The packaged Linux
runtime check exercises both outputs, including a byte read and repository
close. `crab/scripts/e2e/run_cdn_desktop_workload.py` checks the registered
sidecar digest, compiled source and platform, and the worker's user binding
before opening the remote through RPC. Its record retains the build profile,
features and compiled release policy digest. It reads the published commit
in a fresh cache and verifies the returned bytes, captured native target,
and edge request fingerprints. Its native arm
checks that the same user binding has its viewer approval withdrawn. The
provider log collector can join the desktop edge read to the actual responses.
The regional collector verifies each fresh desktop publication against the
independent native ref observation, the preregistered candidate and target,
and provider logs. The warm-local arm repeats the verified blob in the same
open sidecar without edge requests and pins its warm edge primer's record.
Release assembly and verification recompute desktop product and benchmark
results from those records. A product read requires the ordinary edge and
native rollback results from separate caches, provider logs, independent ref
publication and retirement, and fixture cleanup. A public row also retains
the native-only read under the prior export and the provider-confirmed edge
read under the current export. Live measurements and promotion remain open.

The protected workflow or its dedicated workers own producing these raw
observations. The assembly and release jobs own rejecting missing, mismatched
or semantically incomplete records. Until each affected check has both a live
producer and a verifier, leave its scope out of `qualified-scopes.txt`.

### Mounted reads in projected views

Interactive and daemon mounts protect captured Xet readers against GC with an
immutable capture and a renewing claim. A direct Crab Auth mount asks for a
`mount` grant. Its AWS session policy grants `GetObject`, its GCS access
boundary grants Object Viewer, and Azure issues a read/list SAS. Issuer-owned
retention handles claim writes for these scopes, including when an operator
withdraws CDN viewer approval and the mount reads from origin.

Service-owned retention binds mount admission to the same principal, native
transport target, and materialized view. The GC collector
validates the descriptor for every live claim before sweeping. Giving a
viewer raw write access to even one reader directory would let it submit
invalid capture metadata into that decision. The service must build the
capture from the authorized view and own conditional claim creation, renewal,
and release. Viewer storage credentials stay read-only. The native helper checks
both GC barriers and the client drains admitted reads before release.

The mount and the native `crab-auth-view retention` helper share capture and
both GC barriers. The helper derives the claim owner from an issuer-bound
reader ID and the selected native transport target. It checks view readiness
on acquisition and uses backend age plus the last acknowledged incarnation
and sequence for conditional renewal. Stale revisions cannot release a newer
claim or revive a retired one. The reference issuer also verifies the exact
projected namespace before granting credentials. `POST /v1/views/retention`
checks the mount policy on every request and derives the reader from the verified
issuer and subject. The native helper verifies the client's transport target
digest before touching the claim. Acquisition confirms the current source
without rematerializing the view. The direct client opens the catalog named by
the acknowledgement and protects the last acknowledged revision with a local
deadline. A remote-owned mount checks the selected Git ref and commit against
that catalog and requires `--read-only` because the mount overlay cache is keyed
by the logical URL. A local macOS NFS test with Crab Auth and RustFS verified a
2 MiB projected Xet file, sibling exclusion, native claim renewal and release,
rejection of a broader Git clone, and a denied new Xet read after renewal loss.

Credential refresh preserves the selected native target and physical scope after
the source advances. A direct mount presents its retained view and claim
incarnation to the credentials endpoint. The issuer rechecks the mount policy
and verifies the active incarnation on the selected native target using backend
age. The check leaves the claim revision intact when lease renewal races
credential issuance. A macOS NFS run against RustFS advanced the repository,
observed a distinct current projection, let the mount's original storage grant
expire, and verified a cold 2 MiB Xet read from the retained projection. The
original claim was released on unmount. Managed mounts request `hydrate` under
the managed grant contract. Managed issuer retention, deployed direct issuer
access, and live provider proof remain open.

The qualification guard checks the native target in the Git clone child and
the viewer binding and projected content prefix in the mount's own read
context. `run_cdn_mount_workload.py` checks the live NFS session's projected
Git revision, the Xet pointer in that commit, mounted file bytes, and clean
detach. Assembly and release verification require the exact xorb request in
provider logs, rollback through a fresh native mount, independently observed
publication and cleanup, and regional benchmarks. The warm local arm remounts
with a fresh Git cache and requires verified Crab chunk-cache hits with zero
edge attempts. Live provider runs and measurements on the packaged platform
matrix remain open.
