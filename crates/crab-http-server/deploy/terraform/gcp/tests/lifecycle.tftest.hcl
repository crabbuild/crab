mock_provider "google" {}

variables {
  project_id       = "test-project"
  region           = "us-west1"
  bucket_name      = "test-crab-http-server"
  bucket_location  = "us-west1"
  gke_cluster_mode = "standard"
}

run "renders_standard_metadata_server_placement" {
  command = plan

  assert {
    condition     = output.gke_node_selector["iam.gke.io/gke-metadata-server-enabled"] == "true"
    error_message = "GKE Standard Helm values must select metadata-server-enabled nodes."
  }
}

run "omits_the_standard_selector_for_autopilot" {
  command = plan

  variables {
    gke_cluster_mode = "autopilot"
  }

  assert {
    condition     = length(output.gke_node_selector) == 0
    error_message = "GKE Autopilot Helm values must omit the rejected Standard node selector."
  }
}

run "rejects_an_unknown_cluster_mode" {
  command = plan

  variables {
    gke_cluster_mode = "unknown"
  }

  expect_failures = [var.gke_cluster_mode]
}

run "bounds_recovery_versions_and_multipart_uploads" {
  command = plan

  assert {
    condition = one([
      for rule in google_storage_bucket.repositories.lifecycle_rule :
      one(rule.condition).days_since_noncurrent_time
      if one(rule.action).type == "Delete" && one(rule.condition).days_since_noncurrent_time != null
    ]) == 90
    error_message = "GCS must retain noncurrent Crab-root versions for the configured recovery window."
  }

  assert {
    condition = one([
      for rule in google_storage_bucket.repositories.lifecycle_rule :
      one(rule.condition).age
      if one(rule.action).type == "AbortIncompleteMultipartUpload"
    ]) == 1
    error_message = "GCS must abort Crab-root multipart uploads left incomplete for one day."
  }
}

run "grants_object_use_without_object_administration" {
  command = plan

  assert {
    condition     = google_storage_bucket_iam_member.server.role == "roles/storage.objectUser"
    error_message = "The workload identity must use Storage Object User without object IAM administration."
  }
}

run "rejects_an_unsafe_recovery_window" {
  command = plan

  variables {
    recovery_version_retention_days = 29
  }

  expect_failures = [var.recovery_version_retention_days]
}
