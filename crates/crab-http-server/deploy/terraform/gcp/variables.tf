variable "project_id" {
  description = "Google Cloud project containing the GKE cluster and GCS bucket."
  type        = string
}

variable "region" {
  description = "Default Google Cloud region for provider operations."
  type        = string
}

variable "bucket_name" {
  description = "Globally unique dedicated GCS bucket name."
  type        = string
}

variable "bucket_location" {
  description = "GCS bucket region or multi-region."
  type        = string
}

variable "root_prefix" {
  description = "Nonempty application root inside the bucket."
  type        = string
  default     = "repositories"

  validation {
    condition     = can(regex("^[A-Za-z0-9][A-Za-z0-9._/-]*[A-Za-z0-9]$", var.root_prefix)) && !startswith(var.root_prefix, "/") && !endswith(var.root_prefix, "/")
    error_message = "root_prefix must be a nonempty relative object prefix without leading or trailing slashes."
  }
}

variable "recovery_version_retention_days" {
  description = "Days to retain noncurrent object versions below the Crab root for point-in-time recovery."
  type        = number
  default     = 90

  validation {
    condition     = var.recovery_version_retention_days == floor(var.recovery_version_retention_days) && var.recovery_version_retention_days >= 30 && var.recovery_version_retention_days <= 3650
    error_message = "recovery_version_retention_days must be a whole number from 30 through 3650."
  }
}

variable "namespace" {
  description = "Kubernetes namespace used by the Helm release."
  type        = string
  default     = "crab"
}

variable "service_account_name" {
  description = "Kubernetes ServiceAccount used by crab-http-server."
  type        = string
  default     = "crab-http-server"
}

variable "gcp_service_account_id" {
  description = "Account ID for the Google service account."
  type        = string
  default     = "crab-http-server"
}

variable "labels" {
  description = "Labels applied to the GCS bucket."
  type        = map(string)
  default = {
    project   = "crab"
    component = "crab-http-server"
  }
}
