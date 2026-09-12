output "storage_url" {
  description = "Crab storage.url value."
  value       = "s3://${aws_s3_bucket.repositories.id}/${var.root_prefix}"
}

output "bucket_name" {
  description = "Dedicated S3 bucket name."
  value       = aws_s3_bucket.repositories.id
}

output "role_arn" {
  description = "IAM role used through EKS Pod Identity."
  value       = aws_iam_role.server.arn
}

output "service_account_name" {
  description = "Helm serviceAccount.name value."
  value       = var.service_account_name
}

output "recovery_version_retention_days" {
  description = "Configured noncurrent object-version recovery window."
  value       = var.recovery_version_retention_days
}
