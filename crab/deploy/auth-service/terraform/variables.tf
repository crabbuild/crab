variable "aws_region" {
  description = "AWS region for the Lambda function and API Gateway"
  type        = string
  default     = "us-east-1"
}

variable "function_name" {
  description = "Name of the Lambda function"
  type        = string
  default     = "crab-auth"
}

variable "lambda_architecture" {
  description = "Lambda CPU architecture. Terraform builds the packaged receive helper for this architecture."
  type        = string
  default     = "x86_64"

  validation {
    condition     = contains(["x86_64", "arm64"], var.lambda_architecture)
    error_message = "Lambda architecture must be one of: x86_64, arm64."
  }
}

variable "jwks_url" {
  description = "OIDC JWKS endpoint URL for token verification"
  type        = string
}

variable "issuer" {
  description = "Expected issuer (iss) claim in ID tokens"
  type        = string
}

variable "audience" {
  description = "Expected audience (aud) claim in ID tokens"
  type        = string
}

variable "auth_role_arn" {
  description = "IAM role ARN that the auth endpoint assumes to generate scoped credentials"
  type        = string
}

variable "auth_external_id" {
  description = "Optional STS ExternalId to send when assuming auth_role_arn"
  type        = string
  default     = ""
  sensitive   = true
}

variable "git_layer_arn" {
  description = "Lambda layer ARN that provides a git executable at /opt/bin/git for crab-auth-receive"
  type        = string

  validation {
    condition     = can(regex("^arn:aws[a-zA-Z-]*:lambda:[a-z0-9-]+:[0-9]{12}:layer:[A-Za-z0-9-_]+:[0-9]+$", var.git_layer_arn))
    error_message = "Git layer ARN must be a versioned AWS Lambda layer ARN."
  }
}

variable "session_duration" {
  description = "Credential lifetime in seconds (900-43200)"
  type        = number
  default     = 3600

  validation {
    condition     = var.session_duration >= 900 && var.session_duration <= 43200
    error_message = "Session duration must be between 900 and 43200 seconds."
  }
}

variable "log_level" {
  description = "Application log level"
  type        = string
  default     = "INFO"

  validation {
    condition     = contains(["DEBUG", "INFO", "WARNING", "ERROR"], var.log_level)
    error_message = "Log level must be one of: DEBUG, INFO, WARNING, ERROR."
  }
}

variable "tags" {
  description = "Tags to apply to all resources"
  type        = map(string)
  default = {
    Project   = "crab"
    Component = "crab-auth"
  }
}
