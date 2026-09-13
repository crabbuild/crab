mock_provider "aws" {}

override_data {
  target = data.aws_iam_policy_document.pod_identity_trust
  values = {
    json = "{}"
  }
}

override_data {
  target = data.aws_iam_policy_document.storage
  values = {
    json = "{}"
  }
}

variables {
  region       = "us-west-2"
  cluster_name = "test-cluster"
  bucket_name  = "test-crab-http-server"
}

run "bounds_recovery_versions_and_multipart_uploads" {
  command = plan

  assert {
    condition = one([
      for rule in aws_s3_bucket_lifecycle_configuration.repositories.rule :
      one(rule.noncurrent_version_expiration).noncurrent_days
      if rule.id == "expire-noncurrent-recovery-versions"
    ]) == 90
    error_message = "S3 must retain noncurrent Crab-root versions for the configured recovery window."
  }

  assert {
    condition = one([
      for rule in aws_s3_bucket_lifecycle_configuration.repositories.rule :
      one(rule.abort_incomplete_multipart_upload).days_after_initiation
      if rule.id == "abort-incomplete-multipart-uploads"
    ]) == 1
    error_message = "S3 must abort Crab-root multipart uploads left incomplete for one day."
  }
}

run "grants_only_runtime_storage_actions" {
  command = plan

  assert {
    condition = toset(local.bucket_list_actions) == toset([
      "s3:ListBucket",
    ])
    error_message = "The pod identity may list objects only through the prefix-restricted ListBucket action."
  }

  assert {
    condition = toset(local.object_actions) == toset([
      "s3:AbortMultipartUpload",
      "s3:DeleteObject",
      "s3:GetObject",
      "s3:PutObject",
    ])
    error_message = "The pod identity must grant exactly the object operations used by Crab and object_store multipart uploads."
  }
}

run "rejects_an_unsafe_recovery_window" {
  command = plan

  variables {
    recovery_version_retention_days = 29
  }

  expect_failures = [var.recovery_version_retention_days]
}
