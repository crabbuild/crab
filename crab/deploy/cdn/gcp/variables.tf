variable "project_id" {
  type        = string
  description = "Dedicated qualification project with Compute Engine and Cloud Storage enabled."
}

variable "name" {
  type        = string
  description = "Resource name prefix for this fixture."
  default     = "crab-cdn-fixture"
  validation {
    condition     = can(regex("^[a-z]([a-z0-9-]{0,38}[a-z0-9])?$", var.name))
    error_message = "Use a lowercase DNS label of up to 40 characters."
  }
}

variable "hostname" {
  type        = string
  description = "Dedicated viewer hostname. Point its DNS A record at the viewer IP."
  validation {
    condition     = length(var.hostname) <= 253 && can(regex("^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$", var.hostname))
    error_message = "Use a lowercase fully qualified DNS name without a trailing dot."
  }
}

variable "bucket_name" {
  type        = string
  description = "Globally unique name for the new, private fixture bucket."
}

variable "bucket_location" {
  type        = string
  description = "Cloud Storage location for the fixture origin."
}

variable "repository_prefix" {
  type        = string
  description = "Canonical logical repository prefix in the new bucket."
  validation {
    condition = (
      length(var.repository_prefix) <= 255 &&
      can(regex("^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*$", var.repository_prefix)) &&
      alltrue([for segment in split("/", var.repository_prefix) : !contains([".", ".."], segment)])
    )
    error_message = "Use up to 255 characters in slash-delimited canonical ASCII segments."
  }
}

variable "protected_view_suffix" {
  type        = string
  default     = ""
  description = "Exact suffix of one materialized v2 view, including its leading slash. Empty selects the repository-local profile."
  validation {
    condition     = var.protected_view_suffix == "" || can(regex("^/acl-views/v2/[a-f0-9]{64}/(0|[1-9][0-9]{0,19})-[a-f0-9]{64}$", var.protected_view_suffix))
    error_message = "Use an exact materialized v2 view suffix from Crab Auth, including its leading slash."
  }
}
