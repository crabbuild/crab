# Public Azure Front Door qualification fixture

`main.bicep` creates a dedicated Azure storage account, export container and
Front Door Premium distribution for one repository's approved Git and LFS
objects. The distribution maps its only viewer route to the dedicated
container. It preserves the repository prefix and exact object bytes.

The default deployment disables public Blob access and the viewer route.
The route and public origin access share a gate for explicit activation,
a separate source account in Azure public cloud, a canonical repository
prefix, and the approved inventory SHA-256. The prefix limit is 64 characters
so the escaped WAF expressions fit Azure's limits. The storage account uses
hierarchical namespaces, disables shared-key access, and has no retained
soft deletes. [Azure WAF custom rules](https://learn.microsoft.com/en-us/azure/web-application-firewall/afds/waf-front-door-custom-rules) · [Front Door limits](https://learn.microsoft.com/en-us/azure/azure-resource-manager/management/azure-subscription-service-limits#azure-front-door-limits)

When enabled, anonymous callers can read the approved blobs directly.
`Blob` access exposes individual blobs without allowing container listing.
The separate export account limits the scope of that disclosure. Front Door
requires HTTPS on its default hostname. The attached WAF checks GET, an empty
query, the exact immutable key and matching hash fanout on cache hits and
misses. [Azure anonymous Blob access](https://learn.microsoft.com/en-us/azure/storage/blobs/anonymous-read-access-configure) · [Blob snapshots and hierarchical namespaces](https://learn.microsoft.com/en-us/azure/storage/blobs/snapshots-overview)

1. Deploy in a dedicated resource group with `enablePublicDelivery = false`.
   Supply the live `sourceOrigin`, `repositoryPrefix`, a new export account and
   container, and `approvedInventorySha256` from the exact reviewed inventory.
2. Use the [approved inventory format](../../aws/README.md#public-export-qualification)
   with the live Blob origin. Use Python 3.11 or later and the Azure CLI.
   The publisher accepts three local Azure CLI configuration directories
   for the deployment operator, source reader and dedicated Entra writer.
   The writer needs read and write data access scoped to the export
   container. Provide private scratch space for one object at a time.

   ```sh
   python3 crab/deploy/cdn/azure/public-export/publish_export.py approved.json \
     --deployment crab-public-export --subscription "$subscription_id" \
     --resource-group crab-public-cdn --operator-configuration /path/to/operator \
     --source-configuration /path/to/reader --writer-configuration /path/to/writer \
     --scratch-directory /path/to/scratch --receipt /path/to/publication.json
   ```

   It verifies the disabled Front Door route, viewer hostname and native
   account before and after copying the approved immutable keys. It uses
   conditional block blob creation and reads back destination bytes and public
   metadata. Retain the exact receipt and its printed SHA-256. Confirm that
   the receipt covers the accelerated objects before activation. The
   publisher admits objects through 50,000 blocks of
   4 MiB each. [Azure CLI Blob authorization](https://learn.microsoft.com/en-us/azure/storage/blobs/authorize-data-operations-cli) · [Azure Blob transfer limits](https://learn.microsoft.com/en-us/azure/storage/blobs/storage-blobs-tune-upload-download-python)
3. Check the deployment's `publicDeliveryEnabled` output after activation.
   Qualify warm GETs and exact ranges on the default hostname and check
   denial for every other method, hostname, mutable key and alternate query.
   Confirm the expected disclosure through the native Blob and DFS endpoints,
   verify native listing, snapshot and version requests cannot expose
   unapproved data, and keep the source's authorizations separate.
   Record the live route, WAF and Blob account settings using the
   [deployment collector](../../../../docs/guides/provider-qualification.md#record-the-deployed-policy).
4. Use the `liveSourceOrigin`, `exportOrigin`, `sourceRepositoryPrefix`
   and `baseUrl` outputs to configure `access = "public"` after qualification.
   In `publication`, set the absolute receipt `path` and publisher-reported
   `sha256`.

Compile and inspect the ARM template without provisioning resources.

```sh
bicep build crab/deploy/cdn/azure/public-export/main.bicep --outfile /tmp/crab-azure-public.json
python3 crab/deploy/cdn/azure/public-export/tests/check_fixture.py /tmp/crab-azure-public.json
python3 crab/deploy/cdn/azure/public-export/test_publish_export.py
```
