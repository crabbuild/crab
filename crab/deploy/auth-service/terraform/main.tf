terraform {
  required_version = ">= 1.5"

  required_providers {
    archive = {
      source  = "hashicorp/archive"
      version = "~> 2.4"
    }
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

locals {
  auth_root         = abspath("${path.module}/..")
  crab_root         = abspath("${path.module}/../../..")
  workspace_root    = abspath("${path.module}/../../../..")
  lambda_build_dir  = "${path.module}/.terraform-build/lambda"
  lambda_pip_platform = {
    x86_64 = "manylinux2014_x86_64"
    arm64  = "manylinux2014_aarch64"
  }[var.lambda_architecture]
  receive_helper_build_arg = var.lambda_architecture == "arm64" ? "--linux-arm64" : "--linux-amd64"

  lambda_source_files = concat(
    [for file in fileset("${path.module}/../src", "**") : "src/${file}"],
    [for file in fileset("${path.module}/../config", "**") : "config/${file}"],
    [
      "requirements-lambda.txt",
      "scripts/build-receive-helper.sh",
    ],
  )
  lambda_source_hash = sha256(join("\n", [
    for file in local.lambda_source_files :
    "${file}:${filesha256("${local.auth_root}/${file}")}"
  ]))

  receive_helper_source_files = concat(
    [for file in fileset("${local.crab_root}/src", "**/*.rs") : "crab/src/${file}"],
    [
      "Cargo.lock",
      "Cargo.toml",
      "crab/Cargo.toml",
      "crab/build.rs",
    ],
  )
  receive_helper_source_hash = sha256(join("\n", [
    for file in local.receive_helper_source_files :
    "${file}:${filesha256("${local.workspace_root}/${file}")}"
  ]))
}

# ---------------------------------------------------------------------------
# Lambda function
# ---------------------------------------------------------------------------

resource "terraform_data" "lambda_build" {
  triggers_replace = {
    architecture = var.lambda_architecture
    package_nonce = timestamp()
    receive_hash = local.receive_helper_source_hash
    source_hash  = local.lambda_source_hash
  }

  provisioner "local-exec" {
    working_dir = local.auth_root
    interpreter = ["/usr/bin/env", "bash", "-lc"]
    command     = <<-EOT
      set -euo pipefail
      ./scripts/build-receive-helper.sh ${local.receive_helper_build_arg}
      rm -rf '${local.lambda_build_dir}'
      mkdir -p '${local.lambda_build_dir}'
      python3 -m pip install \
        --upgrade \
        --platform '${local.lambda_pip_platform}' \
        --implementation cp \
        --python-version 3.12 \
        --only-binary=:all: \
        --target '${local.lambda_build_dir}' \
        -r requirements-lambda.txt
      cp -R src config bin '${local.lambda_build_dir}/'
      find '${local.lambda_build_dir}' -type d -name '__pycache__' -prune -exec rm -rf {} +
      find '${local.lambda_build_dir}' -type f -name '*.pyc' -delete
    EOT
  }
}

data "archive_file" "lambda_zip" {
  type        = "zip"
  source_dir  = local.lambda_build_dir
  output_path = "${path.module}/lambda.zip"

  depends_on = [terraform_data.lambda_build]
}

resource "aws_lambda_function" "auth" {
  function_name = var.function_name
  role          = aws_iam_role.lambda_exec.arn
  handler       = "src.lambda_handler.handler"
  runtime       = "python3.12"
  architectures = [var.lambda_architecture]
  timeout       = 300
  memory_size   = 512
  layers        = [var.git_layer_arn]

  filename         = data.archive_file.lambda_zip.output_path
  source_code_hash = data.archive_file.lambda_zip.output_base64sha256

  environment {
    variables = {
      CRAB_AUTH_JWKS_URL         = var.jwks_url
      CRAB_AUTH_ISSUER           = var.issuer
      CRAB_AUTH_AUDIENCE         = var.audience
      CRAB_AUTH_AWS_ROLE_ARN     = var.auth_role_arn
      CRAB_AUTH_AWS_EXTERNAL_ID  = var.auth_external_id
      CRAB_AUTH_AWS_REGION       = var.aws_region
      CRAB_AUTH_SESSION_DURATION = tostring(var.session_duration)
      CRAB_AUTH_LOG_LEVEL        = var.log_level
      CRAB_AUTH_POLICY_PATH      = "/var/task/config/policy.yaml"
      CRAB_AUTH_RECEIVE_HELPER   = "/var/task/bin/crab-auth-receive"
      CRAB_AUTH_VIEW_HELPER      = "/var/task/bin/crab-auth-view"
      PATH                       = "/opt/bin:/var/task/bin:/var/lang/bin:/usr/local/bin:/usr/bin:/bin"
    }
  }

  tags = var.tags
}

# ---------------------------------------------------------------------------
# IAM role for Lambda execution
# ---------------------------------------------------------------------------

resource "aws_iam_role" "lambda_exec" {
  name = "${var.function_name}-exec"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action = "sts:AssumeRole"
      Effect = "Allow"
      Principal = {
        Service = "lambda.amazonaws.com"
      }
    }]
  })

  tags = var.tags
}

# Allow Lambda to write CloudWatch logs.
resource "aws_iam_role_policy_attachment" "lambda_logs" {
  role       = aws_iam_role.lambda_exec.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

# Allow Lambda to assume the Crab Auth role (to generate scoped credentials).
resource "aws_iam_role_policy" "assume_auth_role" {
  name = "${var.function_name}-assume-crab-auth"
  role = aws_iam_role.lambda_exec.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = "sts:AssumeRole"
      Resource = var.auth_role_arn
    }]
  })
}

# ---------------------------------------------------------------------------
# API Gateway (HTTP API)
# ---------------------------------------------------------------------------

resource "aws_apigatewayv2_api" "auth" {
  name          = var.function_name
  protocol_type = "HTTP"
  tags          = var.tags
}

resource "aws_apigatewayv2_stage" "default" {
  api_id      = aws_apigatewayv2_api.auth.id
  name        = "$default"
  auto_deploy = true

  access_log_settings {
    destination_arn = aws_cloudwatch_log_group.api_logs.arn
    format = jsonencode({
      requestId      = "$context.requestId"
      ip             = "$context.identity.sourceIp"
      method         = "$context.httpMethod"
      path           = "$context.path"
      status         = "$context.status"
      responseLength = "$context.responseLength"
      latency        = "$context.integrationLatency"
    })
  }

  tags = var.tags
}

resource "aws_apigatewayv2_integration" "lambda" {
  api_id                 = aws_apigatewayv2_api.auth.id
  integration_type       = "AWS_PROXY"
  integration_uri        = aws_lambda_function.auth.invoke_arn
  payload_format_version = "2.0"
}

resource "aws_apigatewayv2_route" "credentials" {
  api_id    = aws_apigatewayv2_api.auth.id
  route_key = "POST /v1/credentials"
  target    = "integrations/${aws_apigatewayv2_integration.lambda.id}"
}

resource "aws_apigatewayv2_route" "push_prepare" {
  api_id    = aws_apigatewayv2_api.auth.id
  route_key = "POST /v1/push/prepare"
  target    = "integrations/${aws_apigatewayv2_integration.lambda.id}"
}

resource "aws_apigatewayv2_route" "push_finalize" {
  api_id    = aws_apigatewayv2_api.auth.id
  route_key = "POST /v1/push/finalize"
  target    = "integrations/${aws_apigatewayv2_integration.lambda.id}"
}

resource "aws_apigatewayv2_route" "health" {
  api_id    = aws_apigatewayv2_api.auth.id
  route_key = "GET /health"
  target    = "integrations/${aws_apigatewayv2_integration.lambda.id}"
}

resource "aws_apigatewayv2_route" "ready" {
  api_id    = aws_apigatewayv2_api.auth.id
  route_key = "GET /ready"
  target    = "integrations/${aws_apigatewayv2_integration.lambda.id}"
}

# Allow API Gateway to invoke the Lambda.
resource "aws_lambda_permission" "apigw" {
  statement_id  = "AllowAPIGateway"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.auth.function_name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${aws_apigatewayv2_api.auth.execution_arn}/*/*"
}

# ---------------------------------------------------------------------------
# CloudWatch Logs
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_log_group" "lambda_logs" {
  name              = "/aws/lambda/${var.function_name}"
  retention_in_days = 30
  tags              = var.tags
}

resource "aws_cloudwatch_log_group" "api_logs" {
  name              = "/aws/apigateway/${var.function_name}"
  retention_in_days = 30
  tags              = var.tags
}
