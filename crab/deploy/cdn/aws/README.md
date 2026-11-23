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

## Qualification access logs

Deploy `qualification-logs.yaml` as a companion to either AWS fixture in
`us-east-1`. Pass the fixture's `DistributionId` and its actual origin bucket
as `OriginBucketName`. Use the `Bucket` output for the private fixture or
remove the `s3://` prefix from the public fixture's `ExportOrigin` output.
The origin and log bucket must belong
to the same account and Region. The companion sets up CloudFront standard
logging v2 with UTC hourly partitions and the tab-separated W3C fields the
evidence collector expects. It records the response's Content-Length when
present and the inclusive Content-Range endpoints. Qualification checks the
range width and object bounds against the client's completed read. CloudFront's
total response bytes also include headers. [Standard log fields](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/standard-logs-reference.html)
It selects neither cookies nor query strings. Keep CloudFront cookie logging
disabled at the distribution. [CloudFront logging v2](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/standard-logging.html)

After the companion is ready, check the fixture origin's current server
access logging configuration. If it has no destination, enable logging to
the companion's `LogBucket` with target prefix `origin/` and
`PartitionDateSource: EventTime`. If it already has a destination, verify
that it matches these settings or use a new
fixture. Set the expected bucket owner on both API calls. For example,
after verifying these values against the two stacks' outputs:

```sh
aws s3api get-bucket-logging --profile crab --region us-east-1 \
  --bucket "$fixture_origin" --expected-bucket-owner "$fixture_account"

# Run only for the dedicated fixture with an empty logging configuration.
logging_status=$(jq -n --arg bucket "$fixture_log_bucket" \
  '{LoggingEnabled: {TargetBucket: $bucket, TargetPrefix: "origin/",
    TargetObjectKeyFormat: {PartitionedPrefix: {PartitionDateSource: "EventTime"}}}}')
aws s3api put-bucket-logging --profile crab --region us-east-1 \
  --bucket "$fixture_origin" --expected-bucket-owner "$fixture_account" \
  --bucket-logging-status "$logging_status"
```

Allow time for both CloudFront and S3 delivery. Confirm both feeds arrive
before collecting benchmark samples. Pass decompressed objects from the
`ViewerLogPrefix` output as `--log` and plain-text objects from
`OriginLogPrefix` as `--s3-origin-log` to
`crab/scripts/e2e/collect_cdn_viewer_requests.py`. Keep these raw files on
the protected worker. Only the collector's redacted output belongs in the
bundle. The log bucket expires raw logs after 14 days.
[S3 access logging](https://docs.aws.amazon.com/AmazonS3/latest/userguide/enable-server-access-logging.html)

To collect from the companion directly, pass `--aws-log-stack` to the
collector in place of the local log flags. Use the read-only identity that
can describe the given stack, read the origin bucket's logging configuration,
and list and read both prefixes of its log bucket. The worker checks the
stack's distribution and bucket against the approved fixture. It lists
the relevant UTC viewer hours and S3 event days and removes raw downloads
after each collection attempt. Use `--log-wait-seconds`
to poll for delayed records and `--require-origin-fill` for a cold sample.
S3 and CloudFront log delivery is best effort. A sample with missing
required records remains unqualified after the wait. [CloudFront log delivery](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/standard-logs-reference.html) · [S3 log delivery](https://docs.aws.amazon.com/AmazonS3/latest/userguide/ServerLogs.html)

To retire a fixture, stop its origin log delivery before removing the
companion. Retain the redacted report and empty only the dedicated log
bucket named in the companion's output.

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
   role `s3:GetObject`, `s3:PutObject`, and `s3:AbortMultipartUpload` access
   to the export families. The bucket policy requires that role, conditional
   creation, and a separate cleanup
   principal. [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes-enforce.html)
2. Run the publisher with AWS CLI v2 and Python 3.11 or later. Give the
   operator profile read access to the stack and the export bucket's location.
   Use an independent source-reader profile with `s3:GetBucketLocation` and
   object read access, and use the dedicated export-writer profile.
   Specify private scratch space for one object plus one multipart part at
   a time. The publisher uses single PUTs through 5,000,000,000 bytes and
   multipart uploads through Crab's 50 TB S3 object limit. Use an AWS CLI v2
   build that supports the multipart completion options `--if-none-match`,
   `--checksum-type`, and `--mpu-object-size`.

   ```sh
   python3 crab/deploy/cdn/aws/publish_export.py approved.json \
     --stack crab-public-export --region us-east-1 \
     --operator-profile crab --source-profile export-source-reader \
     --writer-profile export-writer --scratch-directory /path/to/scratch \
     --receipt /path/to/publication.json
   ```

   The publisher uses create-only PUT or multipart completion. It supplies
   SHA-256 for each part and checks the digest of the uploaded bytes against
   the approved inventory before making the completed object visible. It
   aborts failed uploads, and S3 lifecycle cleans up uploads left incomplete
   after seven days. It reads back the body and representation of each
   destination, including an existing key on retry. It checks the disabled
   distribution and both bucket owners and regions before and after the copy.
   It uses the approved source region
   and expected owner for every source read. It writes the complete publication receipt
   to a new path and returns that file's SHA-256. The receipt binds the
   approved inventory digest, source, export, prefix, deployment, viewer
   hostname, and every verified object. The inventory and receipt each have
   an 8 MiB limit. Retain the exact receipt and printed digest for approval.
   A failed run can be retried while public delivery is disabled.
   [S3 checksums](https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity-upload.html) · [Conditional multipart completion](https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html)
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
