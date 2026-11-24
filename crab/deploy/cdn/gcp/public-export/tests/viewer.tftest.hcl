mock_provider "google" {}

variables {
  project_id                = "crab-cdn-test"
  hostname                  = "downloads.example.com"
  source_origin             = "gs://crab-live-models"
  bucket_name               = "crab-cdn-public-export"
  bucket_location           = "US"
  repository_prefix         = "org/repo.v2"
  approved_inventory_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  writer_service_account    = "export-writer@crab-cdn-test.iam.gserviceaccount.com"
}

run "stage_without_disclosure" {
  command = plan

  assert {
    condition = (
      google_storage_bucket.export.public_access_prevention == "enforced" &&
      google_storage_bucket.export.uniform_bucket_level_access &&
      one(google_storage_bucket.export.versioning).enabled == false &&
      one(google_storage_bucket.export.soft_delete_policy).retention_duration_seconds == 0 &&
      length(google_storage_bucket_iam_member.anonymous_get) == 0 &&
      length(google_compute_global_forwarding_rule.viewer) == 0 &&
      google_project_iam_custom_role.export_writer.permissions == toset(["storage.objects.create", "storage.objects.get"])
    )
    error_message = "The staged export must stay private with no forwarding rule or retained generations and a create-only writer."
  }
}

run "activate_get_without_listing" {
  command = plan

  variables {
    enable_public_delivery = true
  }

  assert {
    condition = (
      google_storage_bucket.export.public_access_prevention == "inherited" &&
      one(google_storage_bucket_iam_member.anonymous_get).member == "allUsers" &&
      one(google_storage_bucket_iam_member.anonymous_get).bucket == var.bucket_name &&
      google_project_iam_custom_role.anonymous_get.permissions == toset(["storage.objects.get"]) &&
      one(google_compute_global_forwarding_rule.viewer).port_range == "443" &&
      google_compute_backend_bucket.export.bucket_name == var.bucket_name &&
      google_compute_backend_bucket.export.enable_cdn &&
      google_compute_backend_bucket.export.compression_mode == "DISABLED" &&
      contains(google_compute_backend_bucket.export.custom_response_headers, "X-Crab-CDN-Cache: {cdn_cache_status}")
    )
    error_message = "Public access must grant object GET only and attach HTTPS Cloud CDN to the export bucket."
  }

  assert {
    condition = (
      google_compute_security_policy.viewer.type == "CLOUD_ARMOR_EDGE" &&
      anytrue([for check in local.deny_checks : strcontains(check, "request.query != ''")]) &&
      anytrue([for check in local.deny_checks : strcontains(check, "request.method != 'GET'")]) &&
      anytrue([for check in local.deny_checks : strcontains(check, "request.headers['host']")]) &&
      one([for rule in google_compute_security_policy.viewer.rule : rule.action if rule.priority == 2147483647]) == "deny(403)"
    )
    error_message = "The edge must deny queries, unexpected methods, host aliases and unclassified paths."
  }
}

run "exact_keys_and_fanout" {
  command = plan

  assert {
    condition = alltrue([
      for path in [
        "/org/repo.v2/v2/history/source-roots/ab/${format("abcd%060d", 0)}",
        "/org/repo.v2/lfs/objects/ab/cd/${format("abcd%060d", 0)}",
        ] : anytrue([
          for route in values(local.routes) :
          can(regex(route.shape, path)) && alltrue([for digit in route.digits : can(regex(digit, path))])
      ])
    ])
    error_message = "Both canonical Git and LFS objects must pass the public edge policy."
  }

  assert {
    condition = alltrue([
      for path in [
        "/org/repo.v2/v2/root",
        "/org/repo.v2/v2/history/%61b/${format("abcd%060d", 0)}",
        "/org/repo.v20/v2/history/ab/${format("abcd%060d", 0)}",
        "/org/repo.v2/lfs/objects/ab/cc/${format("abcd%060d", 0)}",
        "/org/repo.v2/.crab/xorbs/ab/${format("abcd%060d", 0)}",
        ] : !anytrue([
          for route in values(local.routes) :
          can(regex(route.shape, path)) && alltrue([for digit in route.digits : can(regex(digit, path))])
      ])
    ])
    error_message = "The public policy must reject neighboring, mutable, bucket-global and noncanonical keys."
  }
}

run "source_cannot_be_the_export" {
  command = plan

  variables {
    source_origin = "gs://crab-cdn-public-export"
  }

  expect_failures = [google_storage_bucket.export]
}
