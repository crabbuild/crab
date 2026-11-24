output "bucket" {
  value = google_storage_bucket.origin.name
}

output "repository_prefix" {
  value = var.repository_prefix
}

output "physical_repository_prefix" {
  value = local.physical_repository_prefix
}

output "protected_global_prefix" {
  value = local.protected_view ? "${local.physical_repository_prefix}/.crab" : null
}

output "backend_bucket" {
  value = google_compute_backend_bucket.origin.name
}

output "viewer_ip" {
  value = google_compute_global_address.viewer.address
}

output "base_url" {
  value = "https://${var.hostname}"
}

output "cache_fill_condition" {
  description = "Restrict the project Cloud CDN fill identity to repository immutable families."
  value = format("resource.type == 'storage.googleapis.com/Object' && (%s)", join(" || ", [
    for prefix in local.fill_prefixes :
    "resource.name.startsWith('projects/_/buckets/${google_storage_bucket.origin.name}/objects/${local.physical_repository_prefix}/${prefix}/')"
  ]))
}
