# CloudFront qualification fixture

`cloudfront.yaml` defines a private S3 bucket and CloudFront distribution for
one test-owned Crab repository prefix. CloudFront uses Origin Access Control
for S3 reads and a trusted key group for signed viewer cookies. The viewer
function admits GET requests for the exact immutable capsule, checkpoint,
catalog, pack-layer, history and LFS paths.

Use a Crab build with `--features provider-cdn-qualification` for live tests.
Default builds serve reads from the authorized native origin.

Supply the issuer's PEM public key as `ViewerPublicKey`. Keep the matching
private key in the issuer's secret store. Use the stack's bucket for the
qualification repository and its `BaseUrl` and `ViewerKeyId` for signing.
Native S3 credentials perform publication and control reads. Use a separate
test role for those operations.

Leave `ProtectedViewSuffix` empty for repository-local Git and LFS reads.
For a protected view, materialize the view in the fixture bucket, then set
`ProtectedViewSuffix` to the suffix of its Crab Auth `storage_scope.repo_prefix`
after removing the logical `RepositoryPrefix`. The suffix starts with
`/acl-views/v2/` and identifies one policy scope, source generation and
digest. In this mode the viewer function and S3 policy serve the exact
`PhysicalRepositoryPrefix` and the xorb, shard and capsule chunk page
families under `ProtectedGlobalPrefix`. Configure the issuer and client with
the logical repo prefix. Configure the issuer's `protected_view_suffix` with
the same deployed suffix and leave `allow_unfiltered` disabled.
[Crab Auth setup](../../auth-service/README.md)

Test the distribution's own hostname with valid, expired and missing cookies
while the cache is warm. For the protected fixture, fetch Git and hydrate real
Xet content, then require denial for a different scope or generation and for
source repository and bucket-global keys. Record the edge response and S3
request evidence for each.

For key rotation, add the new public key in the unused slot. The output is
`ViewerKeyId` for `ViewerPublicKey` and `SecondaryViewerKeyId` for
`SecondaryViewerPublicKey`. After both are trusted at the edge, switch
the issuer's `key_file` and `key_id` together. Verify that a new cookie works
and that an unexpired cookie from the old key still works. Once the old
cookies expire, clear the old public key slot and verify the edge denies
cookies signed with that key. [CloudFront signing key rotation](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-trusted-signers.html)

Run the path-policy tests and validate the template without creating resources.

```sh
node crab/deploy/cdn/aws/check_fixture.mjs
aws cloudformation validate-template --profile crab --region us-east-1 \
  --template-body file://crab/deploy/cdn/aws/cloudfront.yaml
```

Retain the outputs and verification evidence for any deployed fixture. Before
stack deletion, empty only the bucket named in that stack's `Bucket` output.
Use the read-only [deployment collector](../../../docs/guides/provider-qualification.md#record-the-deployed-policy)
to bind the deployed policy to the qualification fixture and release row.

[Origin Access Control](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-restricting-access-to-s3.html)
· [CloudFront viewer cookies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-setting-signed-cookie-custom-policy.html)

## Public export qualification

`public-export.yaml` uses a separate stack and creates a new S3 bucket.
CloudFront reads that bucket through distribution-scoped Origin Access
Control. The export bucket blocks public S3 access. The viewer function
allows anonymous HTTPS GETs only for exact repository-local Git and LFS keys.
Queries, writes, listing, mutable keys and bucket-global content are denied.

Review an inventory of the immutable bytes approved for public disclosure.
Each `path` is relative to the source repository prefix and each `sha256`
identifies the exact full object body. An LFS path also contains that SHA-256.
For example, this inventory approves a three-byte LFS object containing `abc`.

```json
{
  "schema": "crab.cdn-export.v1",
  "source_origin": "s3://source-models",
  "source_placement": {
    "resource": "accounts/123456789012/buckets/source-models",
    "location": "us-west-2"
  },
  "repository_prefix": "org/models",
  "objects": [
    {
      "path": "lfs/objects/ba/78/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
      "size": 3,
      "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    }
  ]
}
```

1. Deploy a dedicated stack in the qualification account. Provide the
   source and prefix, the SHA-256 of the exact reviewed inventory file as
   `ApprovedInventorySha256`, and `ExportWriterRoleArn`. Leave
   `EnablePublicDelivery` at its default of false. Give the dedicated writer
   role S3 GetObject and PutObject access to the export families. The bucket
   policy requires that role, conditional creation, and a separate cleanup
   principal. [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes-enforce.html)
2. Run the publisher with AWS CLI v2 and Python 3.11 or later. Give the
   operator profile read access to the stack and the export bucket's location.
   Use an independent source-reader profile with `s3:GetBucketLocation` and
   object read access, and use the dedicated export-writer profile.
   Specify private scratch space for one object at a time. This fixture
   accepts objects through 5,000,000,000 bytes using single PUTs.

   ```sh
   python3 crab/deploy/cdn/aws/publish_export.py approved.json \
     --stack crab-public-export --region us-east-1 \
     --operator-profile crab --source-profile export-source-reader \
     --writer-profile export-writer --scratch-directory /path/to/scratch \
     --receipt /path/to/publication.json
   ```

   The publisher uses a conditional checksum-protected PUT and reads back
   the body and representation of each destination, including an existing
   key on retry. It checks the disabled distribution and both bucket owners
   and regions before and after the copy. It uses the approved source region
   and expected owner for every source read. It writes the complete publication receipt
   to a new path and returns that file's SHA-256. The receipt binds the
   approved inventory digest, source, export, prefix, deployment, viewer
   hostname, and every verified object. The inventory and receipt each have
   an 8 MiB limit. Retain the exact receipt and printed digest for approval.
   A failed run can be retried while public delivery is disabled.
   [S3 checksums](https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html) · [Conditional S3 readback](https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html)
3. Confirm the export covers the Git and LFS objects named by the captured
   source view, then enable public delivery. Probe the warm distribution's
   own host, exact ranges and digests, and denial of mutable paths, adjacent
   prefixes, version and response-override queries, and anonymous direct
   S3 reads. Retain the deployed control-plane settings and cache evidence.
4. After qualification, configure Crab with the values from that stack.
   Keep the source repository native for control reads, writes and maintenance.
   The approved receipt determines which objects can use the edge.
   Newly published objects use native storage until a receipt includes them.

   ```toml
   [[cdn]]
   origin = "s3://source-models"
   export_origin = "s3://dedicated-export-from-stack-output"
   repo_prefix = "org/models"
   base_url = "https://example.cloudfront.net"
   access = "public"
   publication = { path = "/path/to/publication.json", sha256 = "<publisher-reported digest>" }
   ```

Run the publisher tests and read-only template validation locally.

```sh
python3 crab/deploy/cdn/aws/test_publish_export.py
node crab/deploy/cdn/aws/check_fixture.mjs
aws cloudformation validate-template --profile crab --region us-east-1 \
  --template-body file://crab/deploy/cdn/aws/public-export.yaml
```
