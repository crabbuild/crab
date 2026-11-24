# Public Cloud CDN qualification fixture

This Terraform root creates a new GCS export bucket and a separate HTTPS
Cloud CDN backend for approved, repository-local Git and LFS objects. The
source bucket remains the native origin for Crab metadata and writes. The
export preserves the source repository prefix and its immutable object paths.

With the default `enable_public_delivery = false`, the export bucket enforces
public access prevention and the HTTPS forwarding rule is absent. The writer
service account has object creation and read permissions for the immutable
families in the export bucket. Use a dedicated identity with no other grants
on this bucket. Versioning and soft delete are disabled from creation.

Enabling public delivery lets anonymous readers fetch approved bytes and
object metadata directly from GCS. The bucket-scoped custom role grants
`storage.objects.get`. It has no list or write permission. The qualification
project must permit that disclosure. [Cloud Storage public access](https://docs.cloud.google.com/storage/docs/access-control/making-data-public) · [Cloud Storage versioning](https://docs.cloud.google.com/storage/docs/object-versioning)

Cloud Armor checks the exact configured hostname, HTTPS GET method, empty
query, canonical immutable key and hash fanout before serving cached data.
All other requests are denied. The distribution preserves raw bytes.
[Cloud Armor edge policies](https://docs.cloud.google.com/armor/docs/security-policy-overview) · [Cloud CDN backend bucket](https://docs.cloud.google.com/cdn/docs/setting-up-cdn-with-bucket)

1. Apply the module in an isolated qualification project with public delivery
   disabled. Specify a new `bucket_name`, the live `source_origin`, the
   `repository_prefix`, and the SHA-256 of the exact approved inventory.
   Allocate a dedicated viewer hostname and point its A record at `viewer_ip`.
2. Use the [approved inventory format](../../aws/README.md#public-export-qualification)
   with the live `gs://` source. Set `source_placement.resource` to
   `projects/<project-number>/buckets/<bucket>` and `location` to the bucket's
   Cloud Storage location. The operator needs bucket metadata access on both
   buckets and project metadata access on the export project.
   Run the publisher with Python 3.11 or later,
   `google-cloud-storage` 3.17.1, and the Google Cloud CLI. The operator's
   named configuration needs read access to the bucket settings, CDN
   backend and Cloud Armor viewer policy. Use separate Application Default
   Credentials files for the source reader and export writer. Provide private scratch space for
   one object at a time.

   ```sh
   terraform -chdir=crab/deploy/cdn/gcp/public-export output -json > staged-export.json
   python3 crab/deploy/cdn/gcp/public-export/publish_export.py approved.json \
     --deployment staged-export.json --operator-configuration export-operator \
     --source-adc /path/to/reader.json --writer-adc /path/to/writer.json \
     --scratch-directory /path/to/scratch --receipt /path/to/publication.json
   ```

   The publisher validates both bucket placements, the staged backend and viewer hostname
   before and after the copy. It hashes each source generation, uses
   `ifGenerationMatch=0` to create the matching export key, and verifies
   the exported bytes and public metadata. Retain the exact receipt and its
   printed SHA-256. Confirm that it covers the accelerated objects before
   activation. [GCS request preconditions](https://docs.cloud.google.com/storage/docs/request-preconditions) · [Application Default Credentials](https://docs.cloud.google.com/docs/authentication/application-default-credentials)
3. Enable public delivery and wait for the certificate to become active.
   Probe the warm CDN and anonymous native access. Capture GET and exact
   ranges, sibling and mutable-path denial, query and alternate host denial,
   anonymous listing denial, and the absence of retired generations. Include
   the exposed object metadata in the public approval. The repository's
   native bucket must keep its own access policy.
   Run the [deployment collector](../../../../docs/guides/provider-qualification.md#record-the-deployed-policy)
   to record the live export route and verify the permissions of both custom
   bucket roles.
4. Configure `access = "public"` with `origin` set to `source_origin`,
   `export_origin` set to `export_origin`, and the fixture's `base_url` and
   repository prefix after qualification. In `publication`, set the
   absolute receipt `path` and publisher-reported `sha256`.

Run local plan checks without creating cloud resources.

```sh
terraform -chdir=crab/deploy/cdn/gcp/public-export init -backend=false
terraform -chdir=crab/deploy/cdn/gcp/public-export validate
terraform -chdir=crab/deploy/cdn/gcp/public-export test
python3 crab/deploy/cdn/gcp/public-export/test_publish_export.py
```
