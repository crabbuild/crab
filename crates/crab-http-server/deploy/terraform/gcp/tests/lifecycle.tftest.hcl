mock_provider "google" {}

variables {
  project_id      = "test-project"
  region          = "us-west1"
  bucket_name     = "test-crab-http-server"
  bucket_location = "us-west1"
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

run "rejects_an_unsafe_recovery_window" {
  command = plan

  variables {
    recovery_version_retention_days = 29
  }

  expect_failures = [var.recovery_version_retention_days]
}
