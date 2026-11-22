# Private Cloud CDN qualification fixture

The [public export fixture](public-export/README.md) uses a separate backend and export bucket.

This Terraform module creates a dedicated GCS bucket and global HTTPS viewer
endpoint for one test-owned Crab repository prefix. Cloud Armor checks the
hostname, method, canonical key, and a single signed cookie before Cloud CDN
can serve cached content. Cloud CDN verifies the cookie's signature, expiry
and URL prefix. The bucket enforces public access prevention.

Use a Crab build with `--features provider-cdn-qualification` for live tests.
Default builds serve reads from the authorized native origin.

The default profile covers repository-local Git and LFS objects. Setting
`protected_view_suffix` pins the viewer routes and the GCS cache-fill policy
to one materialized v2 view and its view-local Xet families. In both profiles,
use native Crab GCS credentials for writes and control reads. Use an isolated
GCP project because Cloud CDN uses a project-wide cache-fill identity.

## Set up the fixture

1. Enable the Compute Engine and Cloud Storage APIs in the test project.
   Set the module's `project_id`, `bucket_name`, `bucket_location`,
   `repository_prefix`, and `hostname` variables. Review the Terraform plan,
   check that the project's Cloud Armor quota covers its custom rules, and
   apply it in the test project. [Cloud Armor quotas](https://docs.cloud.google.com/armor/quotas)

   For the protected-view profile, materialize the view in this bucket and
   set `protected_view_suffix` to the part of Crab Auth's
   `storage_scope.repo_prefix` after the logical `repository_prefix`,
   starting with `/acl-views/v2/`. Confirm `physical_repository_prefix` and
   `protected_global_prefix` match the resolved storage scope.
2. Point the viewer hostname's DNS A record at the `viewer_ip` output and
   wait for the Google-managed certificate to become active.
3. Mount a Cloud CDN key containing 16 random bytes encoded as base64url in
   the reference Crab Auth service. Keep the key material in the secret
   manager and outside Terraform state.
4. Register the same key with the backend bucket, which creates the Cloud
   CDN fill identity. Grant that identity conditional access to the immutable
   key families. Run the following commands from the deployed module
   directory. Set `CDN_PROJECT`, `CDN_KEY_NAME`, and `CDN_KEY_FILE` for the
   test deployment.

   ```sh
   cdn_backend=$(terraform output -raw backend_bucket)
   cdn_bucket=$(terraform output -raw bucket)
   cdn_project_number=$(gcloud projects describe "$CDN_PROJECT" --format='value(projectNumber)')
   cdn_fill_scope=$(terraform output -raw cache_fill_condition)

   gcloud compute backend-buckets add-signed-url-key "$cdn_backend" \
     --project "$CDN_PROJECT" --key-name "$CDN_KEY_NAME" --key-file "$CDN_KEY_FILE"
   gcloud storage buckets add-iam-policy-binding "gs://$cdn_bucket" \
     --project "$CDN_PROJECT" \
     --member "serviceAccount:service-$cdn_project_number@cloud-cdn-fill.iam.gserviceaccount.com" \
     --role roles/storage.objectViewer \
     --condition "title=CrabImmutableReads,expression=$cdn_fill_scope"
   ```

5. Configure a `cloud_cdn` export in Crab Auth with the fixture's origin,
   logical `repository_prefix`, `base_url`, and key name. Set the issuer's
   `protected_view_suffix` to the deployed suffix for a protected view. For
   a repository-local fixture, approve `allow_unfiltered`. Configure the
   matching private `[[cdn]]` entry using the logical repository prefix.
   [Crab Auth setup](../../auth-service/README.md) · [Crab storage configuration](../../../docs/architecture/storage-layer.md)

## Validate and qualify

Run the local configuration and policy tests with Terraform 1.7 or later.

```sh
terraform -chdir=crab/deploy/cdn/gcp init -backend=false
terraform -chdir=crab/deploy/cdn/gcp validate
terraform -chdir=crab/deploy/cdn/gcp test
```

For live qualification, push real Crab data to the test repository. Clone
and download LFS from a fresh client. Check exact range framing, object
digests and cache-hit evidence. Repeat from a second authorized client at
the same edge. After warming the cache, require denial for missing, expired,
duplicate and wrong-prefix cookies, and for an unmatched host, invalid key
shape, mutable route or query. Probe direct anonymous bucket access and
verify `Cache-Control: private, no-store` on successful warm responses.
Record origin GETs and fill bytes alongside client timings. For key rotation,
test both keys during the overlap and the old key after deletion.
Use `collect_cdn_viewer_requests.py --gcp-logging` on the protected
worker to collect viewer and origin records for the approved forwarding
rule. The active `gcloud` identity needs permission to list log entries in
the fixture project. [Cloud Logging entries.list](https://docs.cloud.google.com/logging/docs/reference/v2/rest/v2/entries/list)

For the protected fixture, hydrate projected Xet content and require denial
for keys from a different scope or generation, the source repository, and the
bucket-global namespace.

Retain fixture outputs and redacted evidence. For cleanup, remove the
dedicated viewer DNS record, remove the fixture's signing keys and fill IAM
binding, and empty only its test-owned bucket before destroying the module.
Use the [deployment collector](../../../docs/guides/provider-qualification.md#record-the-deployed-policy)
to bind the live HTTPS route, edge policy and bucket IAM to the release row.

[Cloud CDN signed cookies](https://docs.cloud.google.com/cdn/docs/using-signed-cookies)
· [Cloud Armor edge policies](https://docs.cloud.google.com/armor/docs/security-policy-overview)
· [Cloud Storage IAM conditions](https://docs.cloud.google.com/storage/docs/access-control/iam#iam-conditions)
