variable "region" {
  description = "AWS region containing the EKS cluster and S3 bucket."
  type        = string
}

variable "cluster_name" {
  description = "Existing EKS cluster name with the Pod Identity Agent installed."
  type        = string
}

variable "bucket_name" {
  description = "Globally unique dedicated S3 bucket name."
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

variable "role_name" {
  description = "IAM role name for EKS Pod Identity."
  type        = string
  default     = "crab-http-server"
}

variable "tags" {
  description = "Tags applied through the AWS provider."
  type        = map(string)
  default = {
    Project   = "crab"
    Component = "crab-http-server"
  }
}
