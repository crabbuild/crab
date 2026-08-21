#!/usr/bin/env python3
"""Validate auth-helper deployment packaging contracts."""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class TextCheck:
    label: str
    path: Path
    contains: tuple[str, ...] = ()
    excludes: tuple[str, ...] = ()


def crab_root() -> Path:
    return Path(__file__).resolve().parents[2]


def auth_dir() -> Path:
    return crab_root() / "deploy" / "auth"


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def check_text(check: TextCheck) -> list[str]:
    errors: list[str] = []
    if not check.path.is_file():
        return [f"{check.label}: missing file {check.path}"]

    text = read_text(check.path)
    for needle in check.contains:
        if needle not in text:
            errors.append(f"{check.label}: expected {needle!r} in {check.path}")
    for needle in check.excludes:
        if needle in text:
            errors.append(f"{check.label}: unexpected {needle!r} in {check.path}")
    return errors


def dockerfile_checks(root: Path) -> list[TextCheck]:
    common_contains = (
        "FROM rust:1-slim-bookworm AS receive-helper",
        "COPY crates/ crates/",
        "-p crab-auth-server",
        "--bin crab-auth-receive",
        "--bin crab-auth-view",
        "--no-default-features",
        (
            "COPY --from=receive-helper /workspace/target/release/crab-auth-receive "
            "/usr/local/bin/crab-auth-receive"
        ),
        (
            "COPY --from=receive-helper /workspace/target/release/crab-auth-view "
            "/usr/local/bin/crab-auth-view"
        ),
    )
    common_excludes = (
        "COPY bin/crab-auth-receive",
        "COPY bin/crab-auth-view",
    )
    return [
        TextCheck(
            "auth Dockerfile packages helper binaries",
            root / "Dockerfile",
            contains=common_contains,
            excludes=common_excludes,
        ),
        TextCheck(
            "auth Cloud Run Dockerfile packages helper binaries",
            root / "cloudrun" / "Dockerfile",
            contains=common_contains,
            excludes=common_excludes,
        ),
    ]


def checks() -> list[TextCheck]:
    root = auth_dir()
    return [
        *dockerfile_checks(root),
        TextCheck(
            "docker-compose uses repo root build context",
            root / "docker-compose.yaml",
            contains=(
                "context: ../../..",
                "dockerfile: crab/deploy/auth/Dockerfile",
            ),
        ),
        TextCheck(
            "receive-helper build script builds package-owned binaries",
            root / "scripts" / "build-receive-helper.sh",
            contains=(
                "--host",
                "--linux-amd64",
                "--linux-arm64",
                'TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"',
                "--platform",
                "-p crab-auth-server",
                '--manifest-path "$WORKSPACE_ROOT/Cargo.toml"',
                "--bin crab-auth-receive",
                "--bin crab-auth-view",
                "--no-default-features",
            ),
            excludes=(
                "$CRAB_ROOT/target/release/crab-auth-receive",
                "$CRAB_ROOT/target/release/crab-auth-view",
            ),
        ),
        TextCheck(
            "lambda Terraform packages helper binaries",
            root / "terraform" / "main.tf",
            contains=(
                'source  = "hashicorp/archive"',
                'resource "terraform_data" "lambda_build"',
                "receive_hash = local.receive_helper_source_hash",
                "Cargo.lock",
                "crab/Cargo.toml",
                "./scripts/build-receive-helper.sh ${local.receive_helper_build_arg}",
                "requirements-lambda.txt",
                "cp -R src config bin '${local.lambda_build_dir}/'",
                'PATH                       = "/opt/bin:/var/task/bin:',
                'CRAB_AUTH_RECEIVE_HELPER   = "/var/task/bin/crab-auth-receive"',
                'CRAB_AUTH_VIEW_HELPER      = "/var/task/bin/crab-auth-view"',
                "CRAB_AUTH_AWS_EXTERNAL_ID",
            ),
        ),
        TextCheck(
            "lambda Terraform requires architecture and Git layer",
            root / "terraform" / "variables.tf",
            contains=(
                'variable "lambda_architecture"',
                'variable "auth_external_id"',
                'variable "git_layer_arn"',
            ),
        ),
        TextCheck(
            "lambda Terraform example binds deploy-time defaults",
            root / "terraform" / "terraform.tfvars.example",
            contains=(
                'lambda_architecture = "x86_64"',
                'auth_external_id = "crab-auth"',
                'git_layer_arn = "arn:aws:lambda:us-west-2:123456789012:layer:git:1"',
            ),
        ),
        TextCheck(
            "SAM zip deployments include helper path and Git layer",
            root / "sam" / "template.yaml",
            contains=(
                "GitLayerArn:",
                "Layers:",
                "- !Ref GitLayerArn",
                "PATH: /opt/bin:/var/task/bin:",
                "CRAB_AUTH_RECEIVE_HELPER: /var/task/bin/crab-auth-receive",
                "CRAB_AUTH_VIEW_HELPER: /var/task/bin/crab-auth-view",
                "CRAB_AUTH_AWS_EXTERNAL_ID: !Ref AuthExternalId",
            ),
        ),
        TextCheck(
            "auth deployment docs cover helper packaging",
            root / "README.md",
            contains=(
                "docker build -f crab/deploy/auth/Dockerfile -t crab-auth .",
                "docker build -f crab/deploy/auth/cloudrun/Dockerfile",
                "Terraform builds the Lambda zip locally",
                "./scripts/build-receive-helper.sh --linux-amd64",
                "/opt/bin/git",
            ),
        ),
        TextCheck(
            "auth deployment guide covers helper packaging",
            root / "GUIDE.md",
            contains=(
                "../scripts/build-receive-helper.sh --linux-amd64",
                "Terraform builds the Lambda zip locally",
                "/opt/bin/git",
            ),
        ),
        TextCheck(
            "generated helper binaries stay ignored",
            root / ".gitignore",
            contains=(
                "bin/",
                "terraform/.terraform-build/",
            ),
        ),
    ]


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate Crab Auth receive/view helper packaging contracts.",
    )
    parser.parse_args()

    errors: list[str] = []
    for check in checks():
        errors.extend(check_text(check))

    if errors:
        print("error: auth-helper packaging contract drifted:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    print(f"ok: {len(checks())} auth-helper packaging checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
