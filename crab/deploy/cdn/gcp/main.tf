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

locals {
  physical_repository_prefix = "${var.repository_prefix}${var.protected_view_suffix}"
  protected_view             = var.protected_view_suffix != ""
  request_checks = [
    "request.scheme != 'https' || request.method != 'GET' || request.query != ''",
    "!has(request.headers['host']) || request.headers['host'] != '${var.hostname}'",
    "!has(request.headers['cookie']) || !request.headers['cookie'].matches(R\"^Cloud-CDN-Cookie=URLPrefix=[A-Za-z0-9_-]+={0,2}:Expires=[0-9]{1,20}:KeyName=[A-Za-z0-9_-]{1,63}:Signature=[A-Za-z0-9_-]{27}=$\")",
    "has(request.headers['authorization'])",
    "has(request.headers['range']) && !request.headers['range'].matches(R\"^bytes=[0-9]+-[0-9]+$\")",
  ]
  deny_checks   = concat(local.request_checks, module.immutable_routes.fanout_denials)
  routes        = module.immutable_routes.routes
  fill_prefixes = module.immutable_routes.fill_prefixes
}

module "immutable_routes" {
  source = "./immutable-routes"

  physical_repository_prefix = local.physical_repository_prefix
  protected_view             = local.protected_view
}

resource "google_storage_bucket" "origin" {
  name                        = var.bucket_name
  location                    = var.bucket_location
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  force_destroy               = false
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

resource "google_compute_backend_bucket" "origin" {
  name                 = "${var.name}-origin"
  bucket_name          = google_storage_bucket.origin.name
  enable_cdn           = true
  compression_mode     = "DISABLED"
  edge_security_policy = google_compute_security_policy.viewer.id
  custom_response_headers = [
    "Cache-Control: private, no-store",
    "X-Crab-CDN-Cache: {cdn_cache_status}",
  ]

  cdn_policy {
    cache_mode                   = "USE_ORIGIN_HEADERS"
    signed_url_cache_max_age_sec = 86400
    negative_caching             = false
    serve_while_stale            = 0
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
  name = "${var.name}-map"
  # Every host and path reaches the same default-deny pre-cache policy.
  default_service = google_compute_backend_bucket.origin.id
}

resource "google_compute_target_https_proxy" "viewer" {
  name             = "${var.name}-https"
  url_map          = google_compute_url_map.viewer.id
  ssl_certificates = [google_compute_managed_ssl_certificate.viewer.id]
}

resource "google_compute_global_forwarding_rule" "viewer" {
  name                  = "${var.name}-https"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  network_tier          = "PREMIUM"
  target                = google_compute_target_https_proxy.viewer.id
  ip_address            = google_compute_global_address.viewer.id
  port_range            = "443"
}
