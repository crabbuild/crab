locals {
  auth_prefix = "${var.root_prefix}/.crab/http-server/v1/auth/"
}

resource "aws_s3_bucket" "repositories" {
  bucket = var.bucket_name

  lifecycle {
    prevent_destroy = true
  }
}

resource "aws_s3_bucket_public_access_block" "repositories" {
  bucket = aws_s3_bucket.repositories.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "repositories" {
  bucket = aws_s3_bucket.repositories.id

  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "repositories" {
  bucket = aws_s3_bucket.repositories.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_versioning" "repositories" {
  bucket = aws_s3_bucket.repositories.id

  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "repositories" {
  bucket = aws_s3_bucket.repositories.id

  depends_on = [aws_s3_bucket_versioning.repositories]

  rule {
    id     = "expire-crab-auth-state"
    status = "Enabled"

    filter {
      prefix = local.auth_prefix
    }

    expiration {
      days = 1
    }

    noncurrent_version_expiration {
      noncurrent_days = 1
    }
  }
}

data "aws_iam_policy_document" "pod_identity_trust" {
  statement {
    actions = ["sts:AssumeRole", "sts:TagSession"]
    effect  = "Allow"

    principals {
      identifiers = ["pods.eks.amazonaws.com"]
      type        = "Service"
    }
  }
}

resource "aws_iam_role" "server" {
  name               = var.role_name
  assume_role_policy = data.aws_iam_policy_document.pod_identity_trust.json
}

data "aws_iam_policy_document" "storage" {
  statement {
    sid = "ListApplicationRoot"
    actions = [
      "s3:ListBucket",
      "s3:ListBucketMultipartUploads",
    ]
    resources = [aws_s3_bucket.repositories.arn]

    condition {
      test     = "StringLike"
      values   = [var.root_prefix, "${var.root_prefix}/*"]
      variable = "s3:prefix"
    }
  }

  statement {
    sid = "ManageApplicationObjects"
    actions = [
      "s3:AbortMultipartUpload",
      "s3:DeleteObject",
      "s3:GetObject",
      "s3:ListMultipartUploadParts",
      "s3:PutObject",
    ]
    resources = ["${aws_s3_bucket.repositories.arn}/${var.root_prefix}/*"]
  }
}

resource "aws_iam_role_policy" "storage" {
  name   = "crab-http-server-storage"
  role   = aws_iam_role.server.id
  policy = data.aws_iam_policy_document.storage.json
}

resource "aws_eks_pod_identity_association" "server" {
  cluster_name    = var.cluster_name
  namespace       = var.namespace
  service_account = var.service_account_name
  role_arn        = aws_iam_role.server.arn
}
