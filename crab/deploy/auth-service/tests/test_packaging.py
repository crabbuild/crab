"""Tests for crab-auth deployment packaging contracts."""

from __future__ import annotations

from pathlib import Path

import yaml


AUTH_DIR = Path(__file__).resolve().parents[1]


def _read(relative: str) -> str:
    return (AUTH_DIR / relative).read_text()


def test_dockerfiles_build_receive_helper_inside_linux_image():
    for dockerfile in ["Dockerfile", "cloudrun/Dockerfile"]:
        text = _read(dockerfile)

        assert "FROM rust:1-slim-bookworm AS receive-helper" in text
        assert "COPY crates/ crates/" in text
        assert "-p crab-auth-server" in text
        assert "--bin crab-auth-receive" in text
        assert "--bin crab-auth-view" in text
        assert "--no-default-features" in text
        assert (
            "COPY --from=receive-helper /workspace/target/release/crab-auth-receive "
            "/usr/local/bin/crab-auth-receive"
        ) in text
        assert (
            "COPY --from=receive-helper /workspace/target/release/crab-auth-view "
            "/usr/local/bin/crab-auth-view"
        ) in text
        assert "COPY bin/crab-auth-receive" not in text
        assert "COPY bin/crab-auth-view" not in text


def test_docker_compose_uses_repo_root_build_context():
    config = yaml.safe_load(_read("docker-compose.yaml"))

    crab_auth_build = config["services"]["crab-auth"]["build"]
    assert crab_auth_build == {
        "context": "../../..",
        "dockerfile": "crab/deploy/auth-service/Dockerfile",
    }


def test_receive_helper_build_script_supports_host_and_lambda_targets():
    text = _read("scripts/build-receive-helper.sh")

    assert "--host" in text
    assert "--linux-amd64" in text
    assert "--linux-arm64" in text
    assert 'TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"' in text
    assert "--platform" in text
    assert '-p crab-auth-server' in text
    assert '--manifest-path "$WORKSPACE_ROOT/Cargo.toml"' in text
    assert "$CRAB_ROOT/target/release/crab-auth-receive" not in text
    assert "$CRAB_ROOT/target/release/crab-auth-view" not in text
    assert "--bin crab-auth-view" in text


def test_lambda_docs_require_linux_receive_helper_before_packaging():
    readme = _read("README.md")
    guide = _read("GUIDE.md")

    assert "../scripts/build-receive-helper.sh --linux-amd64" in guide
    assert "docker build -f crab/deploy/auth-service/Dockerfile -t crab-auth ." in readme
    assert "docker build -f crab/deploy/auth-service/cloudrun/Dockerfile" in readme
    assert "Terraform builds the Lambda zip locally" in readme
    assert "Terraform builds the Lambda zip locally" in guide


def test_generated_receive_helper_binary_is_ignored():
    assert "bin/" in _read(".gitignore").splitlines()
    assert "terraform/.terraform-build/" in _read(".gitignore").splitlines()


def test_terraform_lambda_package_installs_dependencies_before_zip():
    main_tf = _read("terraform/main.tf")
    variables_tf = _read("terraform/variables.tf")
    tfvars = _read("terraform/terraform.tfvars.example")

    assert 'source  = "hashicorp/archive"' in main_tf
    assert 'resource "terraform_data" "lambda_build"' in main_tf
    assert "triggers_replace = {" in main_tf
    assert "package_nonce = timestamp()" in main_tf
    assert "receive_hash = local.receive_helper_source_hash" in main_tf
    assert "source_hash  = local.lambda_source_hash" in main_tf
    assert "fileset(\"${local.crab_root}/src\", \"**/*.rs\")" in main_tf
    assert "Cargo.lock" in main_tf
    assert "crab/Cargo.toml" in main_tf
    assert "./scripts/build-receive-helper.sh ${local.receive_helper_build_arg}" in main_tf
    assert "requirements-lambda.txt" in main_tf
    assert "--platform '${local.lambda_pip_platform}'" in main_tf
    assert "cp -R src config bin '${local.lambda_build_dir}/'" in main_tf
    assert "source_dir  = local.lambda_build_dir" in main_tf
    assert "architectures = [var.lambda_architecture]" in main_tf
    assert "layers        = [var.git_layer_arn]" in main_tf
    assert 'PATH                       = "/opt/bin:/var/task/bin:' in main_tf
    assert 'CRAB_AUTH_VIEW_HELPER      = "/var/task/bin/crab-auth-view"' in main_tf
    assert "CRAB_AUTH_AWS_EXTERNAL_ID" in main_tf
    assert 'variable "lambda_architecture"' in variables_tf
    assert 'variable "auth_external_id"' in variables_tf
    assert 'variable "git_layer_arn"' in variables_tf
    assert 'lambda_architecture = "x86_64"' in tfvars
    assert 'auth_external_id = "crab-auth"' in tfvars
    assert 'git_layer_arn = "arn:aws:lambda:us-west-2:123456789012:layer:git:1"' in tfvars


def test_zip_lambda_deployments_require_git_layer():
    sam = _read("sam/template.yaml")
    readme = _read("README.md")
    guide = _read("GUIDE.md")

    assert "GitLayerArn:" in sam
    assert "Layers:" in sam
    assert "- !Ref GitLayerArn" in sam
    assert "PATH: /opt/bin:/var/task/bin:" in sam
    assert "CRAB_AUTH_VIEW_HELPER: /var/task/bin/crab-auth-view" in sam
    assert "AuthExternalId:" in sam
    assert "CRAB_AUTH_AWS_EXTERNAL_ID: !Ref AuthExternalId" in sam
    assert "/opt/bin/git" in readme
    assert "git_layer_arn" in guide


def test_serverless_deployments_expose_readiness_endpoint():
    main_tf = _read("terraform/main.tf")
    sam = _read("sam/template.yaml")
    readme = _read("README.md")
    guide = _read("GUIDE.md")

    assert 'route_key = "GET /ready"' in main_tf
    assert "Path: /ready" in sam
    assert "curl http://localhost:8080/ready" in readme
    assert '"auth_config":"ok"' in readme
    assert '"view_helper":"ok"' in readme
    assert "curl $AUTH_URL/ready" in guide
    assert '"provider_config":"ok"' in guide
    assert '"view_helper":"ok"' in guide
