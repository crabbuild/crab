mock_provider "google" {}

variables {
  project_id        = "crab-cdn-test"
  hostname          = "downloads.example.com"
  bucket_name       = "crab-cdn-test-fixture"
  bucket_location   = "US"
  repository_prefix = "org/repo.v2"
}

run "private_viewer_policy" {
  command = plan

  assert {
    condition = (
      google_storage_bucket.origin.public_access_prevention == "enforced" &&
      google_storage_bucket.origin.uniform_bucket_level_access &&
      google_compute_backend_bucket.origin.enable_cdn &&
      google_compute_backend_bucket.origin.compression_mode == "DISABLED" &&
      contains(google_compute_backend_bucket.origin.custom_response_headers, "Cache-Control: private, no-store") &&
      contains(google_compute_backend_bucket.origin.custom_response_headers, "X-Crab-CDN-Cache: {cdn_cache_status}")
    )
    error_message = "The fixture must use a private bucket and preserve the representation and viewer cache policy."
  }

  assert {
    condition = (
      google_compute_security_policy.viewer.type == "CLOUD_ARMOR_EDGE" &&
      one([for rule in google_compute_security_policy.viewer.rule : rule.action if rule.priority == 2147483647]) == "deny(403)" &&
      length([for rule in google_compute_security_policy.viewer.rule : rule if rule.action == "allow"]) == 2 &&
      alltrue([for rule in google_compute_security_policy.viewer.rule : rule.priority >= 1000 if rule.action == "allow"]) &&
      alltrue([for rule in google_compute_security_policy.viewer.rule : rule.priority < 1000 if rule.action == "deny(403)" && rule.priority != 2147483647])
    )
    error_message = "The pre-cache policy must reject invalid credentials and paths before checking an allow rule."
  }

  assert {
    condition     = google_compute_global_forwarding_rule.viewer.port_range == "443"
    error_message = "The viewer fixture must expose HTTPS."
  }
}

run "canonical_object_keys" {
  command = plan

  assert {
    condition = alltrue([
      for path in [
        for pattern in [
          "/org/repo.v2/v2/capsules/ab/{hash}",
          "/org/repo.v2/v2/history/source-roots/ab/{hash}",
          "/org/repo.v2/lfs/objects/ab/cd/{hash}",
        ] : replace(pattern, "{hash}", format("abcd%060d", 0))
        ] : anytrue([
          for route in values(local.routes) :
          can(regex(route.shape, path)) && alltrue([for digit in route.digits : can(regex(digit, path))])
      ])
    ])
    error_message = "The edge must admit the canonical capsule, source-history and LFS paths."
  }

  assert {
    condition = alltrue([
      for path in [
        for pattern in [
          "/org/repo.v2/v2/root",
          "/org/repo.v2/.crab/xorbs/ab/{hash}",
          "/org/repo.v20/v2/capsules/ab/{hash}",
          "/org/repo-v2/v2/capsules/ab/{hash}",
          "/org/repo.v2/v2/capsules/aa/{hash}",
          "/org/repo.v2/v2/capsules/AB/{hash}",
          "/org/repo.v2/v2/capsules/%61b/{hash}",
          "/org/repo.v2/v2/capsules/ab/{hash}/tail",
          "/org/repo.v2/lfs/objects/aa/cd/{hash}",
          "/org/repo.v2/lfs/objects/ab/cc/{hash}",
        ] : replace(pattern, "{hash}", format("abcd%060d", 0))
        ] : !anytrue([
          for route in values(local.routes) :
          can(regex(route.shape, path)) && alltrue([for digit in route.digits : can(regex(digit, path))])
      ])
    ])
    error_message = "The edge must reject mutable, neighboring and noncanonical object keys."
  }
}

run "expression_length" {
  command = plan

  variables {
    # Periods require an additional escaping character in the path regex.
    repository_prefix     = format("a%sb", join("", [for position in range(253) : "."]))
    protected_view_suffix = "/acl-views/v2/${format("%064d", 1)}/18446744073709551615-${format("%064d", 2)}"
  }

  assert {
    condition = (
      alltrue([for expression in local.deny_checks : length(expression) <= 2048]) &&
      alltrue(flatten([
        for route in values(local.routes) : [
          for expression in concat([route.shape], route.digits) :
          length(format("request.path.matches(R\"%s\")", expression)) <= 1024
        ]
      ]))
    )
    error_message = "Generated Cloud Armor expressions must fit the provider limits at the maximum prefix length."
  }
}

run "protected_view_paths" {
  command = plan

  variables {
    protected_view_suffix = "/acl-views/v2/${format("%064d", 1)}/7-${format("%064d", 2)}"
  }

  assert {
    condition = alltrue([
      for path in [
        for kind in ["xorbs", "shards", "capsule-chunk-pages"] :
        "/${local.physical_repository_prefix}/.crab/${kind}/ab/${format("abcd%060d", 0)}"
      ] : can(regex(local.routes.xet.shape, path)) && alltrue([for digit in local.routes.xet.digits : can(regex(digit, path))])
    ])
    error_message = "The authorized physical view admits each canonical Xet family."
  }

  assert {
    condition = alltrue([
      for path in [
        for pattern in [
          "/.crab/shards/ab/{hash}",
          "/org/repo.v2/.crab/xorbs/ab/{hash}",
          "/org/repo.v2/v2/capsules/ab/{hash}",
          "/${replace(local.physical_repository_prefix, "/7-", "/8-")}/.crab/xorbs/ab/{hash}",
          "/${replace(local.physical_repository_prefix, format("%064d", 1), format("%064d", 3))}/.crab/shards/ab/{hash}",
          "/${local.physical_repository_prefix}/.crab/shards/aa/{hash}",
          "/${local.physical_repository_prefix}/.crab/shards/%61b/{hash}",
          "/${local.physical_repository_prefix}/.crab/shards/ab/{hash}/tail",
          "/${local.physical_repository_prefix}/v2/root",
        ] : replace(pattern, "{hash}", format("abcd%060d", 0))
        ] : !anytrue([
          for route in values(local.routes) :
          can(regex(route.shape, path)) && alltrue([for digit in route.digits : can(regex(digit, path))])
      ])
    ])
    error_message = "Sibling generations, other principals, noncanonical keys and native roots stay outside the viewer policy."
  }

  assert {
    condition = (
      output.repository_prefix == "org/repo.v2" &&
      output.physical_repository_prefix == "org/repo.v2${var.protected_view_suffix}" &&
      output.protected_global_prefix == "${output.physical_repository_prefix}/.crab" &&
      alltrue([for family in local.fill_prefixes : strcontains(output.cache_fill_condition, "/objects/${output.physical_repository_prefix}/${family}/'")]) &&
      !strcontains(output.cache_fill_condition, "/objects/.crab/")
    )
    error_message = "Viewer paths and fill identity must use the same physical view."
  }
}

run "reject_malformed_view_suffix" {
  command = plan

  variables {
    protected_view_suffix = "/acl-views/v1/${format("%064d", 1)}/7-${format("%064d", 2)}"
  }

  expect_failures = [var.protected_view_suffix]
}
