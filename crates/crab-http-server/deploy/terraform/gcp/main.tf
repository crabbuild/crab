locals {
  auth_prefix = "${var.root_prefix}/.crab/http-server/v1/auth/"
  ksa_member  = "serviceAccount:${var.project_id}.svc.id.goog[${var.namespace}/${var.service_account_name}]"
}

resource "google_storage_bucket" "repositories" {
  name                        = var.bucket_name
  location                    = var.bucket_location
  project                     = var.project_id
  force_destroy               = false
  public_access_prevention    = "enforced"
  uniform_bucket_level_access = true
  labels                      = var.labels

  versioning {
    enabled = true
  }

  lifecycle_rule {
    action {
      type = "Delete"
    }

    condition {
      age            = 1
      matches_prefix = [local.auth_prefix]
      with_state     = "ANY"
    }
  }

  lifecycle {
    prevent_destroy = true
  }
}

resource "google_service_account" "server" {
  account_id   = var.gcp_service_account_id
  display_name = "Crab HTTP server"
  project      = var.project_id
}

resource "google_storage_bucket_iam_member" "server" {
  bucket = google_storage_bucket.repositories.name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.server.email}"
}

resource "google_service_account_iam_member" "workload_identity" {
  service_account_id = google_service_account.server.name
  role               = "roles/iam.workloadIdentityUser"
  member             = local.ksa_member
}
