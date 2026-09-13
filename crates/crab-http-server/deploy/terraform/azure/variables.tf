variable "resource_group_name" {
  description = "Existing resource group for storage and managed identity."
  type        = string
}

variable "storage_account_name" {
  description = "Globally unique dedicated Azure Storage account name."
  type        = string

  validation {
    condition     = can(regex("^[a-z0-9]{3,24}$", var.storage_account_name))
    error_message = "storage_account_name must contain 3 to 24 lowercase letters or digits."
  }
}

variable "container_name" {
  description = "Dedicated private Blob container name."
  type        = string
  default     = "crab-repositories"
}

variable "root_prefix" {
  description = "Nonempty application root inside the container."
  type        = string
  default     = "repositories"

  validation {
    condition     = can(regex("^[A-Za-z0-9][A-Za-z0-9._/-]*[A-Za-z0-9]$", var.root_prefix)) && !startswith(var.root_prefix, "/") && !endswith(var.root_prefix, "/")
    error_message = "root_prefix must be a nonempty relative object prefix without leading or trailing slashes."
  }
}

variable "account_replication_type" {
  description = "Azure Storage replication type."
  type        = string
  default     = "ZRS"

  validation {
    condition     = contains(["LRS", "ZRS", "GRS", "GZRS"], var.account_replication_type)
    error_message = "account_replication_type must be LRS, ZRS, GRS, or GZRS."
  }
}

variable "public_network_access_enabled" {
  description = "Whether the storage public endpoint is reachable. Disable after configuring a private endpoint and cluster DNS."
  type        = bool
  default     = true
}

variable "aks_oidc_issuer_url" {
  description = "OIDC issuer URL from the existing AKS cluster."
  type        = string
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

variable "managed_identity_name" {
  description = "User-assigned managed identity name."
  type        = string
  default     = "crab-http-server"
}

variable "tags" {
  description = "Tags applied to Azure resources."
  type        = map(string)
  default = {
    Project   = "crab"
    Component = "crab-http-server"
  }
}
