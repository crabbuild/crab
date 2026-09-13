output "storage_url" {
  description = "Crab storage.url value."
  value       = "gs://${google_storage_bucket.repositories.name}/${var.root_prefix}"
}

output "bucket_name" {
  description = "Dedicated GCS bucket name."
  value       = google_storage_bucket.repositories.name
}

output "gcp_service_account_email" {
  description = "Value for the Helm GKE ServiceAccount annotation."
  value       = google_service_account.server.email
}

output "service_account_name" {
  description = "Helm serviceAccount.name value."
  value       = var.service_account_name
}

output "recovery_version_retention_days" {
  description = "Configured noncurrent object-version recovery window."
  value       = var.recovery_version_retention_days
}

output "helm_values" {
  description = "Non-secret GKE Helm values generated from this infrastructure."
  value = yamlencode({
    config = {
      storageUrl = "gs://${var.bucket_name}/${var.root_prefix}"
    }
    serviceAccount = {
      name = var.service_account_name
      annotations = {
        "iam.gke.io/gcp-service-account" = google_service_account.server.email
      }
    }
  })
}
