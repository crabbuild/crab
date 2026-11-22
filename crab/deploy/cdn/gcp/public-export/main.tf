terraform {
  required_version = ">= 1.7"
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 7.0"
    }
  }
}

provider "google" {
  project = var.project_id
}

module "immutable_routes" {
  source = "../immutable-routes"

  physical_repository_prefix = var.repository_prefix
}

locals {
  routes = module.immutable_routes.routes
  deny_checks = concat([
    "request.scheme != 'https' || request.method != 'GET' || request.query != ''",
    "!has(request.headers['host']) || request.headers['host'] != '${var.hostname}'",
    "has(request.headers['authorization'])",
    "has(request.headers['range']) && !request.headers['range'].matches(R\"^bytes=[0-9]+-[0-9]+$\")",
  ], module.immutable_routes.fanout_denials)
  writer_prefixes = [
    for family in module.immutable_routes.fill_prefixes :
    "resource.name.startsWith('projects/_/buckets/${var.bucket_name}/objects/${var.repository_prefix}/${family}/')"
  ]
}

resource "google_storage_bucket" "export" {
  name                        = var.bucket_name
  location                    = var.bucket_location
  uniform_bucket_level_access = true
  public_access_prevention    = var.enable_public_delivery ? "inherited" : "enforced"
  force_destroy               = false

  labels = {
    crab_public_export = "true"
    crab_inventory_0   = substr(var.approved_inventory_sha256, 0, 32)
    crab_inventory_1   = substr(var.approved_inventory_sha256, 32, 32)
  }

  versioning {
    enabled = false
  }
  soft_delete_policy {
    retention_duration_seconds = 0
  }

  lifecycle {
    precondition {
      condition     = var.source_origin != "gs://${var.bucket_name}"
      error_message = "Create a separate export bucket for public delivery."
    }
  }
}

resource "google_project_iam_custom_role" "export_writer" {
  role_id     = "${replace(var.name, "-", "_")}_export_writer"
  title       = "Crab public export writer"
  permissions = ["storage.objects.create", "storage.objects.get"]
}

resource "google_storage_bucket_iam_member" "export_writer" {
  bucket = google_storage_bucket.export.name
  role   = google_project_iam_custom_role.export_writer.name
  member = "serviceAccount:${var.writer_service_account}"

  condition {
    title      = "CrabImmutableExport"
    expression = "resource.type == 'storage.googleapis.com/Object' && (${join(" || ", local.writer_prefixes)})"
  }
}

resource "google_project_iam_custom_role" "anonymous_get" {
  role_id     = "${replace(var.name, "-", "_")}_anonymous_get"
  title       = "Crab public export object GET"
  permissions = ["storage.objects.get"]
}

resource "google_storage_bucket_iam_member" "anonymous_get" {
  count = var.enable_public_delivery ? 1 : 0

  bucket = google_storage_bucket.export.name
  role   = google_project_iam_custom_role.anonymous_get.name
  member = "allUsers"
}

resource "google_compute_security_policy" "viewer" {
  name = "${var.name}-viewer"
  type = "CLOUD_ARMOR_EDGE"

  dynamic "rule" {
    for_each = local.deny_checks
    content {
      action   = "deny(403)"
      priority = 100 + rule.key
      match {
        expr {
          expression = rule.value
        }
      }
    }
  }

  dynamic "rule" {
    for_each = values(local.routes)
    content {
      action   = "allow"
      priority = 1000 + rule.key
      match {
        expr {
          expression = format("request.path.matches(R\"%s\")", rule.value.shape)
        }
      }
    }
  }

  rule {
    action   = "deny(403)"
    priority = 2147483647
    match {
      versioned_expr = "SRC_IPS_V1"
      config {
        src_ip_ranges = ["*"]
      }
    }
  }
}

resource "google_compute_backend_bucket" "export" {
  name                    = "${var.name}-export"
  bucket_name             = google_storage_bucket.export.name
  enable_cdn              = true
  compression_mode        = "DISABLED"
  edge_security_policy    = google_compute_security_policy.viewer.id
  custom_response_headers = ["X-Crab-CDN-Cache: {cdn_cache_status}"]

  cdn_policy {
    cache_mode        = "USE_ORIGIN_HEADERS"
    negative_caching  = false
    serve_while_stale = 0
  }
}

resource "google_compute_global_address" "viewer" {
  name       = "${var.name}-ip"
  ip_version = "IPV4"
}

resource "google_compute_managed_ssl_certificate" "viewer" {
  name = "${var.name}-certificate"
  managed {
    domains = [var.hostname]
  }
}

resource "google_compute_url_map" "viewer" {
  name            = "${var.name}-map"
  default_service = google_compute_backend_bucket.export.id
}

resource "google_compute_target_https_proxy" "viewer" {
  name             = "${var.name}-https"
  url_map          = google_compute_url_map.viewer.id
  ssl_certificates = [google_compute_managed_ssl_certificate.viewer.id]
}

resource "google_compute_global_forwarding_rule" "viewer" {
  count = var.enable_public_delivery ? 1 : 0

  name                  = "${var.name}-https"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  network_tier          = "PREMIUM"
  target                = google_compute_target_https_proxy.viewer.id
  ip_address            = google_compute_global_address.viewer.id
  port_range            = "443"

  depends_on = [google_storage_bucket_iam_member.anonymous_get]
}

output "source_origin" {
  value = var.source_origin
}

output "project_id" {
  value = var.project_id
}

output "export_origin" {
  value = "gs://${google_storage_bucket.export.name}"
}

output "repository_prefix" {
  value = var.repository_prefix
}

output "approved_inventory_sha256" {
  value = var.approved_inventory_sha256
}

output "public_delivery_enabled" {
  value = var.enable_public_delivery
}

output "backend_bucket" {
  value = google_compute_backend_bucket.export.name
}

output "viewer_ip" {
  value = google_compute_global_address.viewer.address
}

output "base_url" {
  value = "https://${var.hostname}"
}
