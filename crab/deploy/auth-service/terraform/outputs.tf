output "api_endpoint" {
  description = "The URL of the crab-auth API"
  value       = aws_apigatewayv2_api.auth.api_endpoint
}

output "function_name" {
  description = "Name of the deployed Lambda function"
  value       = aws_lambda_function.auth.function_name
}

output "function_arn" {
  description = "ARN of the deployed Lambda function"
  value       = aws_lambda_function.auth.arn
}

output "api_id" {
  description = "ID of the HTTP API Gateway"
  value       = aws_apigatewayv2_api.auth.id
}

output "crab_config_snippet" {
  description = "Configuration snippet for crab CLI users"
  value       = <<-EOT
    # Add to ~/.config/crab/config.toml
    [auth]
    provider = "crab-auth"
    issuer_url = "${var.issuer}"
    client_id = "${var.audience}"
    auth_endpoint = "${aws_apigatewayv2_api.auth.api_endpoint}/v1/credentials"
  EOT
}
