variable "project_id" {
  type        = string
  description = "Dedicated qualification project with Compute Engine and Cloud Storage enabled."
}

variable "name" {
  type        = string
  description = "Resource name prefix of this public export."
  default     = "crab-public-export"
  validation {
    condition     = can(regex("^[a-z]([a-z0-9-]{0,38}[a-z0-9])?$", var.name))
    error_message = "Use a lowercase DNS label of up to 40 characters."
  }
}

variable "hostname" {
  type        = string
  description = "Dedicated HTTPS viewer hostname."
  validation {
    condition     = length(var.hostname) <= 253 && can(regex("^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$", var.hostname))
    error_message = "Use a lowercase fully qualified DNS name without a trailing dot."
  }
}

variable "bucket_name" {
  type        = string
  description = "Globally unique new export bucket name."
}

variable "bucket_location" {
  type        = string
  description = "Cloud Storage location for the new export bucket."
}

variable "source_origin" {
  type        = string
  description = "Native GCS origin of the live repository."
  validation {
    condition     = can(regex("^gs://[a-z0-9][a-z0-9._-]{1,220}[a-z0-9]$", var.source_origin))
    error_message = "Use the live bucket's native gs:// origin."
  }
}

variable "repository_prefix" {
  type        = string
  description = "Canonical repository prefix preserved in the export."
  validation {
    condition = (
      length(var.repository_prefix) <= 255 &&
      can(regex("^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*$", var.repository_prefix)) &&
      alltrue([for segment in split("/", var.repository_prefix) : !contains([".", ".."], segment)])
    )
    error_message = "Use up to 255 characters in slash-delimited canonical ASCII segments."
  }
}

variable "approved_inventory_sha256" {
  type        = string
  description = "SHA-256 of the exact reviewed immutable inventory."
  validation {
    condition     = can(regex("^[a-f0-9]{64}$", var.approved_inventory_sha256))
    error_message = "Use a lowercase hexadecimal SHA-256 digest."
  }
}

variable "writer_service_account" {
  type        = string
  description = "Email of the service account dedicated to publishing this export."
}

variable "enable_public_delivery" {
  type        = bool
  default     = false
  description = "Enable anonymous GET after the approved inventory is present."
}
