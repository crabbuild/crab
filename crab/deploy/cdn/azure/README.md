# Private Front Door qualification fixture

The [public export fixture](public-export/README.md) uses a separate Front Door distribution and Blob account.

This Bicep template targets a dedicated resource group in the Azure public
cloud. The request path is Crab → classic Standard API Management (APIM) →
Front Door Premium → private Blob container.

APIM validates the Crab issuer's short-lived RS256 token on each GET before
forwarding any request to the cache. The token authorizes one physical key
family in the fixture repository. APIM sends the exact approved path to a
fixed HTTPS backend, strips viewer credentials, and streams the response with
`Cache-Control: private, no-store`. Front Door WAF blocks direct connections
outside the APIM instance's exclusive socket addresses. The Front Door
system-assigned identity has Blob Data Reader on the fixture container.

Use a Crab build with `--features provider-cdn-qualification` for live tests.
Default builds serve reads from the authorized native origin.

The default profile covers repository-local Git and LFS objects. Setting
`protectedViewSuffix` pins APIM viewer authorization to one materialized
v2 view and its view-local Xet families. The hierarchical namespace
supports the reference issuer's directory-scoped user delegation
SAS for native Blob operations. Blob anonymous access and Shared Key access
are disabled. The Blob endpoint remains reachable by authorized native
clients. Microsoft lists managed-identity origin authentication as preview.
Qualify its behavior in the test subscription before relying on it.

## Validate locally

```sh
cdn_arm=$(mktemp -d)
bicep build crab/deploy/cdn/azure/main.bicep --outfile "$cdn_arm/fixture.json"
python3 crab/deploy/cdn/azure/tests/check_fixture.py "$cdn_arm/fixture.json"
```

The tests check the compiled resource wiring, gateway policy structure, and
canonical Git, LFS and projected Xet path matching. Live APIM policy
compilation and cache behavior require an Azure deployment.

For timing evidence, archive the profile's `FrontDoorAccessLog` and the
fixture Blob service's `StorageRead` category to dedicated diagnostic
storage. The qualification collector can discover these archives and read
them using `--azure-monitor`. [Provider qualification guide](../../../docs/guides/provider-qualification.md#pair-regional-cli-fetch-measurements)

## Deploy and qualify

1. Provide the fixture's region, resource names, publisher contact, private
   container and repo prefix. Set `viewerSigningKeys` to the issuer's public
   RSA key ID, modulus and exponent. Use unpadded JWK base64url for the
   modulus and exponent. Keep the private RSA PEM in the issuer's secret
   store. The template supports an overlapping second key for rotation.
2. Apply the template with `enableViewerRoute=false`. The dedicated classic
   Standard gateway and Front Door Premium carry fixed service charges.
   Confirm the deployed WAF association covers `/*` on the default Front
   Door endpoint and its socket allowlist matches the `apimPublicIPs` output.
   Verify actual egress using a temporary APIM probe that targets a test-owned
   HTTPS endpoint. Remove the probe before enabling the viewer route.
3. Assign the Crab Auth service and test writers their native Blob permissions.
   The issuer needs permission to mint a user delegation key on the storage
   account and data roles for its approved scopes. For a protected fixture,
   materialize the view in the fixture container and set `protectedViewSuffix`
   to the part of Crab Auth's `storage_scope.repo_prefix` after the logical
   `repositoryPrefix`, starting with `/acl-views/v2/`. Confirm the template's
   physical repo and global prefix outputs match the resolved storage scope.
   Configure a `front_door` export on the issuer using `originUrl`, the
   logical `repositoryPrefix` and `baseUrl`. Set the issuer's
   `protected_view_suffix` to the deployed suffix for a protected view. For a
   repository-local fixture, approve `allow_unfiltered`. The issuer and
   the client's private `[[cdn]]` binding use the APIM URL ending in `/crab`
   and the logical repository prefix.
   [Crab Auth setup](../../auth-service/README.md) · [Crab storage configuration](../../../docs/architecture/storage-layer.md)
4. Reapply with `enableViewerRoute=true` after the APIM policy, WAF association
   and Blob role assignment have propagated. Test native write and control
   reads, then clone and download LFS from a fresh Crab client. Verify byte
   digests, exact 206 framing, and positive Front Door cache-hit evidence.
   Repeat from a second authorized client. Measure remote action times,
   APIM placement and throughput, cache-fill bytes and service charges.
5. While the cache is warm, require denial for missing or expired JWTs,
   changed origin or key scopes, mutable paths, invalid fanout, queries,
   alternate methods and direct access to `frontDoorHost`. Verify anonymous
   requests to the Blob endpoint fail. During rotation, test both trusted
   keys and then verify denial for the retired key. Record the observed
   cache headers, request counts and policy settings without viewer tokens.
   For the protected fixture, hydrate projected Xet content and require
   denial for keys from a different scope or generation, the source
   repository, and the bucket-global namespace.
6. Run the [deployment collector](../../../docs/guides/provider-qualification.md#record-the-deployed-policy)
   to bind the active APIM policy, WAF socket rule, and Blob role assignment
   to the release row.

For teardown, reapply with `enableViewerRoute=false` and verify the edge has
stopped serving the fixture before releasing the dedicated APIM instance and
its addresses. Delete the test-owned resources after retaining the outputs and
qualification evidence.

[APIM egress addresses](https://learn.microsoft.com/en-us/azure/api-management/api-management-howto-ip-addresses)
· [Front Door WAF IP restrictions](https://learn.microsoft.com/en-us/azure/web-application-firewall/afds/waf-front-door-configure-ip-restriction)
· [Front Door origin identity](https://learn.microsoft.com/en-us/azure/frontdoor/origin-authentication-with-managed-identities)
