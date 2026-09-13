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

run "hardens_the_storage_boundary" {
  command = plan

  assert {
    condition = (
      aws_s3_bucket_public_access_block.repositories.block_public_acls == true &&
      aws_s3_bucket_public_access_block.repositories.block_public_policy == true &&
      aws_s3_bucket_public_access_block.repositories.ignore_public_acls == true &&
      aws_s3_bucket_public_access_block.repositories.restrict_public_buckets == true
    )
    error_message = "S3 public-access protections must all remain enabled."
  }

  assert {
    condition     = one(aws_s3_bucket_ownership_controls.repositories.rule).object_ownership == "BucketOwnerEnforced"
    error_message = "S3 object ownership must remain enforced by the bucket owner."
  }

  assert {
    condition     = one(one(aws_s3_bucket_server_side_encryption_configuration.repositories.rule).apply_server_side_encryption_by_default).sse_algorithm == "AES256"
    error_message = "S3 objects must retain default server-side encryption."
  }

  assert {
    condition     = one(aws_s3_bucket_versioning.repositories.versioning_configuration).status == "Enabled"
    error_message = "S3 object versioning must remain enabled."
  }
}

run "binds_pod_identity_to_the_server" {
  command = plan

  assert {
    condition = (
      aws_eks_pod_identity_association.server.cluster_name == var.cluster_name &&
      aws_eks_pod_identity_association.server.namespace == var.namespace &&
      aws_eks_pod_identity_association.server.service_account == var.service_account_name
    )
    error_message = "EKS Pod Identity must bind the expected cluster, namespace, and ServiceAccount."
  }
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

run "denies_insecure_storage_transport" {
  command = plan

  assert {
    condition = local.secure_transport_conditions == {
      "aws:PrincipalIsAWSService" = "false"
      "aws:SecureTransport"       = "false"
    }
    error_message = "The S3 bucket policy must deny non-service-principal requests made without TLS."
  }
}

run "rejects_an_unsafe_recovery_window" {
  command = plan

  variables {
    recovery_version_retention_days = 29
  }

  expect_failures = [var.recovery_version_retention_days]
}
