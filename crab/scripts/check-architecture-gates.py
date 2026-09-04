#!/usr/bin/env python3
"""Validate multi-crate architecture guardrails."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path


OBJECT_STORE_IMPLEMENTATION_FEATURES = {
    "crab": {"aws", "gcp", "azure", "fs"},
    "crab-storage": {"aws", "gcp", "azure", "fs"},
    "crab-workflow": {"aws", "gcp", "azure", "fs", "http"},
}
XET_OWNER_PACKAGE = "crab-xet"
XET_FORBIDDEN_PATTERNS = ("xet_core_structures", "xet-core-structures")
XET_MODULE_REQUIRED_NORMAL_PACKAGES = {
    "blake3",
    "bytes",
    "rayon",
    "thiserror",
    "tracing",
    "xet-core-structures",
}
XET_MODULE_OPTIONAL_PACKAGES = {
    "tokio",
    "xet-client",
    "xet-data",
    "xet-runtime",
}
XET_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "AuthServerError",
    "BuiltObjectStore",
    "CacheServerConfig",
    "Config::resolve_local",
    "CrabError",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "build_url_object_store",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-workflow",
    "crab::",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache",
    "crab_coordination",
    "crab_diff",
    "crab_git",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_storage",
    "crab_workflow",
    "eprintln!",
    "git2::",
    "gix::",
    "object_store",
    "object_store_options_from_env",
    "parse_url_opts",
    "println!",
    "reqwest",
    "rusqlite",
    "slatedb",
    "std::env",
    "std::io::stdin",
    "std::io::stdout",
    "std::process",
}
XET_MODULE_SCAN_PATHS = ("crates/crab-xet/src",)
STORAGE_PROVIDER_HELPER_SCAN_PATHS = (
    "crab/src/auth/mod.rs",
    "crab/src/cmd/auth_status.rs",
    "crab/src/cmd/config.rs",
    "crab/src/cmd/init.rs",
    "crab/src/storage/resolver.rs",
    "crab/src/tier/runtime.rs",
)
STORAGE_PROVIDER_DIRECT_VARIANT_PATTERNS = (
    "StorageProvider::S3",
    "StorageProvider::Gcs",
    "StorageProvider::Azure",
)
STORAGE_PROVIDER_REQUIRED_HELPERS = (
    "pub fn parse_config_value",
    "pub fn toml_value",
    "pub fn label",
    "pub fn credential_discovery_scheme",
    "pub fn storage_provider_kind",
    "pub fn from_storage_provider_kind",
)
CACHE_SERVER_ORIGIN_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "aws-config",
    "aws-sdk-s3",
    "aws-sdk-s3control",
    "aws_config",
    "aws_sdk_s3",
    "aws_sdk_s3control",
    "azure_core",
    "azure_identity",
    "azure_mgmt_storage",
    "azure_storage",
    "azure_storage_blobs",
    "google-cloud-storage",
    "google-cloud-token",
    "google_cloud_storage",
    "google_cloud_token",
    "normalize_env_option_key",
    "object_store::aws",
    "object_store::azure",
    "object_store::gcp",
    "object_store::parse_url",
    "object_store_options_from_env",
    "parse_url_opts",
}
CACHE_SERVER_ORIGIN_SCAN_PATHS = (
    "crates/crab-cache-server/Cargo.toml",
    "crates/crab-cache-server/src",
    "crates/crab-cache-server/tests",
)
CACHE_SERVER_ALLOWED_INTERNAL_NORMAL_PACKAGES = {
    "crab-cache",
    "crab-storage",
    "crab-xet",
}
CACHE_SERVER_SOURCE_FORBIDDEN_PATTERNS = {
    "AuthServerError",
    "BuiltObjectStore",
    "Config::resolve_local",
    "CredentialProvider",
    "ProviderClient",
    "TokenCache",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-workflow",
    "crab::cmd",
    "crab::core",
    "crab::diff",
    "crab::git",
    "crab::lfs",
    "crab::metadata",
    "crab::replication",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache_store",
    "crab_coordination",
    "crab_diff",
    "crab_git",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_workflow",
    "extern crate crab",
    "git2::",
    "gix::",
    "pub use crab::",
    "slatedb",
    "use crab::",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
CACHE_SERVER_SCOPE_SCAN_PATHS = (
    "crates/crab-cache-server/Cargo.toml",
    "crates/crab-cache-server/src",
)
COORDINATION_RUNTIME_PACKAGES = {
    "aws-config",
    "aws-sdk-dynamodb",
    "azure_core",
    "azure_identity",
    "google-cloud-storage",
    "google-cloud-token",
    "reqwest",
    "urlencoding",
}
COORDINATION_OBJECT_STORE_LOCK_PACKAGES = {
    "bytes",
    "futures-util",
    "object_store",
    "tokio-util",
    "tracing",
    "uuid",
}
COORDINATION_MODULE_REQUIRED_NORMAL_PACKAGES = {
    "async-trait",
    "blake3",
    "schemars",
    "serde",
    "serde_json",
    "thiserror",
    "tokio",
}
COORDINATION_MODULE_ALLOWED_OPTIONAL_PACKAGES = (
    COORDINATION_RUNTIME_PACKAGES | COORDINATION_OBJECT_STORE_LOCK_PACKAGES
)
COORDINATION_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "AuthServerError",
    "BuiltObjectStore",
    "CacheServerConfig",
    "CrabError",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "build_static_env",
    "build_url_object_store",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-workflow",
    "crab-xet",
    "crab::",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache",
    "crab_diff",
    "crab_git",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_storage",
    "crab_workflow",
    "crab_xet",
    "eprintln!",
    "git2::",
    "gix::",
    "object_store_options_from_env",
    "parse_url_opts",
    "println!",
    "rusqlite",
    "slatedb",
    "std::io::stdin",
    "std::io::stdout",
    "std::process::Command",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
COORDINATION_MODULE_SCAN_PATHS = ("crates/crab-coordination/src",)
CACHE_DEFAULT_FORBIDDEN_PACKAGES = {
    "crab-cache-server",
    "crab-storage",
    "filetime",
    "object_store",
    "reqwest",
    "rusqlite",
    "tokio",
    "xet-client",
}
CACHE_MODULE_DIRECT_FORBIDDEN_PACKAGES = {
    "aws-config",
    "aws-sdk-dynamodb",
    "aws-sdk-iam",
    "aws-sdk-s3",
    "aws-sdk-s3control",
    "aws-sdk-sts",
    "azure_core",
    "azure_identity",
    "azure_mgmt_storage",
    "azure_storage",
    "azure_storage_blobs",
    "clap",
    "crab",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-workflow",
    "gix",
    "git2",
    "google-cloud-storage",
    "google-cloud-token",
    "hyper",
    "object_store",
    "slatedb",
    "xet-core-structures",
    "xet-data",
}
CACHE_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "CRAB_AUTH",
    "CRAB_REPLICA",
    "CRAB_STORAGE",
    "CrabError",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_identity",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-workflow",
    "crab::",
    "crab::core::config",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache_server",
    "crab_cache_store",
    "crab_coordination",
    "crab_diff",
    "crab_git",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_storage",
    "crab_workflow",
    "eprintln!",
    "git2::",
    "google-cloud",
    "google_cloud",
    "object_store",
    "object_store_options_from_env",
    "parse_url_opts",
    "println!",
    "slatedb::",
    "std::io::stdin",
    "std::io::stdout",
    "token_cache",
    "xet-core-structures",
    "xet-data",
    "xet_core_structures",
    "xet_data",
}
CACHE_MODULE_XET_RUNTIME_PATTERNS = {
    "xet-client",
    "xet-runtime",
    "xet_client",
    "xet_runtime",
}
CACHE_MODULE_SCAN_PATHS = ("crates/crab-cache/src",)
CACHE_STORE_DIRECT_FORBIDDEN_PACKAGES = {
    "aws-config",
    "aws-sdk-dynamodb",
    "aws-sdk-iam",
    "aws-sdk-s3",
    "aws-sdk-s3control",
    "aws-sdk-sts",
    "azure_core",
    "azure_identity",
    "azure_mgmt_storage",
    "azure_storage",
    "azure_storage_blobs",
    "clap",
    "crab",
    "crab-auth",
    "crab-auth-server",
    "crab-cache-server",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-workflow",
    "gix",
    "git2",
    "google-cloud-storage",
    "google-cloud-token",
    "hyper",
    "reqwest",
    "rusqlite",
    "slatedb",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
}
CACHE_STORE_SOURCE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "BuiltObjectStore",
    "CRAB_",
    "CacheDb",
    "CacheMetrics",
    "CacheObjectKind",
    "CacheServerConfig",
    "CrabError",
    "CredentialProvider",
    "DedupScope",
    "GoogleCloudStorageBuilder",
    "Hydrator",
    "MicrosoftAzureBuilder",
    "MutablePathMode",
    "OriginClient",
    "ProviderClient",
    "ReadReplica",
    "ReadRoutingPolicy",
    "ReadSource",
    "ReadStoreChoice",
    "ReadStoreSelection",
    "ReadinessCheckOptions",
    "ReadinessProbeStats",
    "StaticProvider",
    "StoreClient",
    "TermResolver",
    "StorageProviderKind",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "axum::",
    "azure_identity",
    "build_router",
    "build_static_env",
    "build_url_object_store",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-cache-server",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-workflow",
    "crab::",
    "crab::core::config",
    "crab_auth",
    "crab_auth_server",
    "crab_cache_server",
    "crab_coordination",
    "crab_diff",
    "crab_git",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_workflow",
    "eprintln!",
    "env::var",
    "git2::",
    "google-cloud",
    "google_cloud",
    "hyper::",
    "normalize_env_option_key",
    "object_store::aws",
    "object_store::azure",
    "object_store::gcp",
    "object_store::parse_url",
    "object_store_options_from_env",
    "parse_cache_object_path",
    "parse_url_opts",
    "path_to_cache_key",
    "println!",
    "reqwest::",
    "rusqlite",
    "select_read_replicas",
    "select_read_store_choice",
    "select_read_store",
    "select_ready_read_replica",
    "slatedb::",
    "start_evictor_task",
    "std::env",
    "std::io::stdin",
    "std::io::stdout",
    "std::process",
    "token_cache",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
CACHE_STORE_SCAN_PATHS = ("crates/crab-cache-store/src",)
METADATA_DEFAULT_FORBIDDEN_PACKAGES = {
    "crab-storage",
    "futures-util",
    "object_store",
    "rusqlite",
    "slatedb",
    "tokio",
}
AUTH_DEFAULT_FORBIDDEN_PACKAGES = {
    "crab-auth-server",
    "crab-cache-server",
    "crab-storage",
    "object_store",
    "reqwest",
}
AUTH_CLIENT_FEATURE_FORBIDDEN_PACKAGES = {
    "crab-auth-server",
    "crab-cache-server",
    "crab-storage",
    "object_store",
}
AUTH_CLIENT_FEATURE_DEFINITIONS = {
    "aws-oidc-client": ["dep:sha2", "dep:tokio", "oidc-client"],
    "azure-entra-client": ["dep:tokio", "oidc-client"],
    "crab-auth-client": ["dep:tokio", "oidc-client"],
    "gcp-workload-identity-client": ["dep:tokio", "oidc-client"],
    "oidc-client": ["dep:reqwest", "dep:tokio"],
}
AUTH_CLIENT_FEATURE_REQUIRED_PACKAGES = {
    "aws-oidc-client": {"reqwest", "sha2", "tokio"},
    "azure-entra-client": {"reqwest", "tokio"},
    "crab-auth-client": {"reqwest", "tokio"},
    "gcp-workload-identity-client": {"reqwest", "tokio"},
    "oidc-client": {"reqwest", "tokio"},
}
AUTH_MODULE_DIRECT_FORBIDDEN_PACKAGES = {
    "aws-config",
    "aws-sdk-dynamodb",
    "aws-sdk-iam",
    "aws-sdk-s3",
    "aws-sdk-s3control",
    "aws-sdk-sts",
    "axum",
    "azure_core",
    "azure_identity",
    "azure_mgmt_storage",
    "azure_storage",
    "azure_storage_blobs",
    "clap",
    "crab",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-diff",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-workflow",
    "crab-xet",
    "git2",
    "google-cloud-storage",
    "google-cloud-token",
    "gix",
    "hyper",
    "object_store",
    "rusqlite",
    "slatedb",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
}
AUTH_MODULE_SOURCE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthServerError",
    "BuiltObjectStore",
    "CacheServerConfig",
    "CrabError",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "axum::",
    "azure_identity",
    "build_router",
    "build_static_env",
    "build_url_object_store",
    "clap::",
    "crab = {",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-coordination::dynamodb",
    "crab-diff",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-workflow",
    "crab-xet",
    "crab::",
    "crab::core::config",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache",
    "crab_coordination::dynamodb",
    "crab_diff",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_storage",
    "crab_workflow",
    "crab_xet",
    "eprintln!",
    "git2::",
    "google_cloud",
    "hyper::",
    "normalize_env_option_key",
    "object_store",
    "object_store_options_from_env",
    "parse_url_opts",
    "println!",
    "rusqlite",
    "select_read_store",
    "slatedb::",
    "std::io::stdin",
    "std::io::stdout",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
AUTH_MODULE_SCAN_PATHS = (
    "crates/crab-auth/Cargo.toml",
    "crates/crab-auth/src",
)
DELETED_AUTH_PROVIDER_REEXPORT_ADAPTER_PATHS = (
    "crab/src/auth/aws_oidc.rs",
    "crab/src/auth/azure_entra.rs",
    "crab/src/auth/crab_auth.rs",
    "crab/src/auth/gcp_federation.rs",
)
DELETED_AUTH_PROVIDER_REEXPORT_ADAPTER_SCAN_PATHS = (
    "crab/src/auth",
    "crab/src/git",
    "crab/src/cmd",
    "crab/tests",
)
DELETED_AUTH_PROVIDER_REEXPORT_ADAPTER_FORBIDDEN_PATTERNS = {
    "crate::auth::aws_oidc::",
    "crate::auth::azure_entra::",
    "crate::auth::crab_auth::",
    "crate::auth::gcp_federation::",
    "super::aws_oidc::",
    "super::azure_entra::",
    "super::crab_auth::",
    "super::gcp_federation::",
    "auth::aws_oidc::",
    "auth::azure_entra::",
    "auth::crab_auth::",
    "auth::gcp_federation::",
    "pub mod aws_oidc;",
    "pub mod azure_entra;",
    "pub mod crab_auth;",
    "pub mod gcp_federation;",
}
AUTH_PROVIDER_DISPATCH_OWNER_REQUIRED_PATTERNS = {
    "crates/crab-auth/src/client_config.rs": (
        "pub enum CredentialProviderConfig",
        "pub fn kind(&self) -> AuthProviderKind",
    ),
    "crates/crab-auth/src/credential_provider.rs": (
        "pub fn create_credential_provider",
        "CredentialProviderConfig::AwsOidc",
        "CredentialProviderConfig::GcpWorkloadIdentity",
        "CredentialProviderConfig::AzureEntra",
        "CredentialProviderConfig::CrabAuth",
        "CredentialProviderConfig::Static",
        "CredentialProviderConfig::None",
    ),
}
AUTH_PROVIDER_DISPATCH_CALLER_REQUIRED_PATTERNS = {
    "crab/src/auth/mod.rs": (
        "::crab_auth::create_credential_provider",
        "fn credential_provider_config",
        "CredentialProviderConfig::AwsOidc",
        "CredentialProviderConfig::GcpWorkloadIdentity",
        "CredentialProviderConfig::AzureEntra",
        "CredentialProviderConfig::CrabAuth",
        "CredentialProviderConfig::Static",
        "CredentialProviderConfig::None",
    ),
    "crab/src/git/protected_push.rs": (
        "crab_auth::create_crab_auth_provider",
        "crate::auth::crab_auth_client_config",
    ),
}
AUTH_PROVIDER_DISPATCH_CALLER_FORBIDDEN_PATTERNS = {
    "AwsOidcProvider::new(",
    "GcpFederationProvider::new(",
    "AzureEntraProvider::new(",
    "CrabAuthProvider::new(",
    "StaticProvider::new(",
}
AUTH_STORE_FEATURE_DEFINITIONS = {
    "default": [],
    "refreshing-store": [
        "dep:async-trait",
        "dep:blake3",
        "dep:futures-util",
        "dep:object_store",
        "dep:reqwest",
        "dep:tokio",
        "dep:tracing",
        "dep:url",
        "dep:uuid",
    ],
}
AUTH_STORE_REFRESHING_REQUIRED_PACKAGES = {
    "async-trait",
    "blake3",
    "futures-util",
    "object_store",
    "reqwest",
    "tokio",
    "tracing",
    "url",
    "uuid",
}
AUTH_STORE_DIRECT_FORBIDDEN_PACKAGES = {
    "crab",
    "crab-auth-server",
    "crab-cache",
    "crab-cache-server",
    "crab-read",
    "rusqlite",
    "slatedb",
}
AUTH_STORE_SOURCE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "AuthProvider",
    "CrabError",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_identity",
    "crab = {",
    "crab-auth-server",
    "crab-cache-server",
    "crab::",
    "crab_auth_server",
    "device_code",
    "google-cloud",
    "google_cloud",
    "object_store::aws",
    "object_store::azure",
    "object_store::gcp",
}
AUTH_STORE_SCAN_PATHS = (
    "crates/crab-auth-store/Cargo.toml",
    "crates/crab-auth-store/src",
)
AUTH_SERVER_RUNTIME_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "CrabError",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_core",
    "azure_identity",
    "azure_mgmt_storage",
    "azure_storage",
    "azure_storage_blobs",
    "crab = {",
    "crab::",
    "device_code",
    "google-cloud",
    "google_cloud",
    "normalize_env_option_key",
    "object_store::aws",
    "object_store::azure",
    "object_store::gcp",
    "object_store::parse_url",
    "object_store_options_from_env",
    "parse_url_opts",
    "reqwest::Client",
    "token_cache",
}
AUTH_SERVER_RUNTIME_SCAN_PATHS = (
    "crates/crab-auth-server/Cargo.toml",
    "crates/crab-auth-server/src",
)
AUTH_SERVER_PACK_FILENAME_SCAN_PATHS = (
    "crates/crab-auth-server/src/receive/git_workspace.rs",
)
AUTH_SERVER_PACK_FILENAME_FORBIDDEN_PATTERNS = (
    'strip_prefix("pack-")',
    'strip_suffix(".pack")',
    "pack object filename hash",
)
AUTH_SERVER_PACK_ENTRY_SCAN_PATHS = (
    "crates/crab-auth-server/src/receive.rs",
    "crates/crab-auth-server/src/receive/git_workspace.rs",
)
AUTH_SERVER_PACK_ENTRY_FORBIDDEN_PATTERNS = (
    "validate_pack_entry",
    "invalid pack metadata JSON",
    "pack metadata content_hash must match pack_id",
    "pack metadata entry must describe a non-empty pack",
    "pack metadata object_count does not match pack entry",
    "pack metadata ref_tips do not match pack entry",
)
AUTH_SERVER_SEGMENT_INDEX_SCAN_PATHS = (
    "crates/crab-auth-server/src/receive.rs",
    "crates/crab-auth-server/src/receive/git_workspace.rs",
)
AUTH_SERVER_SEGMENT_INDEX_FORBIDDEN_PATTERNS = (
    "pub fn validate_segment_index_shape",
    "pub fn validate_append_only_index",
    "from_jsonl::<ShardSegmentEntry>",
    "from_jsonl::<PackManifestEntry>",
    "parse_segment_records::<PackManifestEntry>",
    "validate_pack_manifest_entry(&entry)",
    "invalid segment index JSON",
    "metadata segment path does not match its hash",
    "metadata index totals do not match segments",
    "metadata index dropped base segments",
    "metadata index rewrote base segments",
    "metadata index changed without appended segments",
    "metadata index generation moved backwards",
    "shard metadata segment record count mismatch",
    "pack metadata segment record count mismatch",
    "shard segment entry hash",
)
AUTH_SERVER_MANIFEST_PAYLOAD_SCAN_PATHS = (
    "crates/crab-auth-server/src/receive.rs",
    "crates/crab-auth-server/src/receive/git_workspace.rs",
)
AUTH_SERVER_MANIFEST_PAYLOAD_FORBIDDEN_PATTERNS = (
    "candidate manifest must use version 1",
    "candidate manifest HEAD does not resolve to a ref",
    "candidate manifest index hash",
)
READ_MODULE_DIRECT_FORBIDDEN_PACKAGES = {
    "crab",
    "crab-auth",
    "crab-auth-server",
    "crab-cache-server",
    "crab-coordination",
}
READ_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "CRAB_REPLICA",
    "CrabError",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_identity",
    "crab = {",
    "crab::core::config",
    "crab-auth-server",
    "crab-auth",
    "crab-cache-server",
    "crab-coordination",
    "crab::",
    "crab_auth_server",
    "crab_auth",
    "crab_cache_server",
    "crab_coordination",
    "google-cloud",
    "google_cloud",
    "object_store::aws",
    "object_store::azure",
    "object_store::gcp",
    "object_store::parse_url",
    "parse_url_opts",
    "std::env::var(\"CRAB_REPLICA",
    "token_cache",
    "xet-core-structures",
    "xet_core_structures",
}
READ_MODULE_SCAN_PATHS = (
    "crates/crab-read/Cargo.toml",
    "crates/crab-read/src",
)
READ_REPLICA_CANDIDATE_SCAN_PATHS = (
    "crab/src/replication/mod.rs",
)
READ_REPLICA_CANDIDATE_FORBIDDEN_PATTERNS = (
    re.compile(
        r"ReadReplicaCandidate::new\s*\(\s*replica\.name(?:\.clone\(\)|\.to_owned\(\)|\.to_string\(\))?\s*,\s*replica\.read",
        re.MULTILINE,
    ),
)
READ_PROBE_RESULT_SCAN_PATHS = (
    "crab/src/replication/mod.rs",
)
READ_PROBE_RESULT_FORBIDDEN_PATTERNS = (
    "ReadReplicaProbeResult::Ready(",
    "ReadReplicaProbeResult::Fallback(",
    "ReadyReadReplica {",
)
READ_FETCH_ADMISSION_SCAN_PATHS = (
    "crab/src/git/remote_helper.rs",
)
READ_FETCH_ADMISSION_FORBIDDEN_PATTERNS = (
    "manifest_reachable_objects",
    "let tip_set",
    "let mut reachable_set",
    "std::collections::HashSet<&str>",
)
READ_REF_ADVERTISEMENT_SCAN_PATHS = (
    "crab/src/git/remote_helper.rs",
)
READ_REF_ADVERTISEMENT_FORBIDDEN_PATTERNS = (
    "globset::GlobSet",
    "GlobSetBuilder",
    "manifest.peeled_refs",
    "manifest HEAD does not match any live ref",
)
GIT_MODULE_DIRECT_FORBIDDEN_PACKAGES = {
    "crab",
    "crab-auth",
    "crab-auth-server",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-xet",
    "object_store",
    "reqwest",
    "rusqlite",
    "slatedb",
    "tokio",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
}
GIT_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "CrabError",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_identity",
    "crab = {",
    "crab-auth",
    "crab-cache",
    "crab-coordination",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-xet",
    "crab::",
    "crab::core::config",
    "crab_auth",
    "crab_cache",
    "crab_coordination",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_storage",
    "crab_xet",
    "google-cloud",
    "google_cloud",
    "object_store",
    "parse_url_opts",
    "reqwest",
    "rusqlite",
    "slatedb",
    "env::var(\"CRAB_",
    "std::env::var(\"CRAB_",
    "tokio::",
    "token_cache",
    "xet-core-structures",
    "xet-client",
    "xet-data",
    "xet-runtime",
    "xet_core_structures",
    "xet_client",
    "xet_data",
    "xet_runtime",
}
GIT_MODULE_SCAN_PATHS = (
    "crates/crab-git/Cargo.toml",
    "crates/crab-git/src",
)
GIT_DELETED_CLI_OWNER_PATHS = (
    "crab/src/git/facade.rs",
    "crab/src/git/filter_attr_cache.rs",
    "crab/src/git/odb_adapter.rs",
    "crab/src/git/push_state.rs",
    "crab/src/git/refname.rs",
    "crab/src/git/reject_reason.rs",
    "crab/src/git/walk.rs",
)
GIT_HEAD_SYMREF_SCAN_PATHS = (
    "crab/src/git/push.rs",
)
GIT_HEAD_SYMREF_FORBIDDEN_PATTERNS = (
    "try_find(\"HEAD\")",
    "resolve_local_head_at(",
)
GIT_TAG_PEEL_REQUIRED_DELEGATIONS = {
    "crab/src/git/push.rs": "crab_git::tag::peeled_tag_refs_at",
    "crab/src/git/push_native.rs": "crab_git::tag::annotated_tag_refs_strict_at",
}
GIT_TAG_PEEL_FORBIDDEN_PATTERNS = (
    "peel_to_commit",
    "TargetRef::Object",
    "TargetRef::Symbolic",
    "find_object(",
)
STORAGE_PACK_LAYOUT_REQUIRED_DELEGATIONS = {
    "crab/src/git/push.rs": ("pack_path(", "pack_metadata_path("),
    "crab/src/git/remote_helper.rs": ("pack_path(", "pack_index_path("),
    "crab/src/read/mod.rs": ("pack_path(",),
    "crab/src/cmd/gc/mod.rs": ("pack_path(", "pack_metadata_path("),
    "crab/src/cmd/fsck_store.rs": (
        "repo_pack_path(",
        "pack_path(",
        "pack_metadata_path(",
    ),
    "crab/src/cmd/repack.rs": (
        "repo_pack_path(",
        "repo_pack_index_path(",
        "repo_pack_reverse_index_path(",
    ),
    "crab/src/replication/mod.rs": ("pack_path(", "pack_metadata_path("),
    "crates/crab-auth-server/src/receive.rs": (
        "pack_path(",
        "pack_metadata_path(",
    ),
    "crates/crab-auth-server/src/receive/finalize.rs": (
        "pack_path(",
        "pack_metadata_path(",
    ),
    "crates/crab-auth-server/src/receive/git_workspace.rs": (
        "pack_path(",
        "pack_metadata_path(",
    ),
    "crates/crab-auth-server/src/view.rs": ("pack_path(", "pack_metadata_path("),
    "crates/crab-read/src/selection.rs": ("pack_path(", "pack_metadata_path("),
}
STORAGE_PACK_LAYOUT_FORBIDDEN_PATTERNS = (
    'repo_path(&format!("packs/pack-',
    'ObjectPath::from(format!("{prefix}/packs/pack-',
)
DIFF_MODULE_DIRECT_FORBIDDEN_PACKAGES = {
    "crab",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-workflow",
    "gix",
    "gix-diff",
    "gix-hash",
    "gix-object",
    "gix-packetline",
    "gix-ref",
    "gix-revision",
    "gix-traverse",
    "gix-url",
    "object_store",
    "reqwest",
    "rusqlite",
    "serde_json",
    "slatedb",
    "tokio",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
}
DIFF_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "CRAB_",
    "Config::",
    "CrabError",
    "CredentialProvider",
    "File::open",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_identity",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-workflow",
    "crab::",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache",
    "crab_cache_server",
    "crab_cache_store",
    "crab_coordination",
    "crab_git",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_storage",
    "crab_workflow",
    "eprintln!",
    "env::var",
    "gix-",
    "gix::",
    "gix_",
    "google-cloud",
    "google_cloud",
    "object_store",
    "parse_url_opts",
    "println!",
    "reqwest",
    "rusqlite",
    "serde_json",
    "slatedb",
    "std::env",
    "std::fs",
    "tokio::",
    "token_cache",
    "xet-core-structures",
    "xet-client",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
DIFF_MODULE_SCAN_PATHS = (
    "crates/crab-diff/Cargo.toml",
    "crates/crab-diff/src",
)
LFS_MODULE_DIRECT_FORBIDDEN_PACKAGES = {
    "axum",
    "clap",
    "crab",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-metadata",
    "crab-read",
    "crab-workflow",
    "crab-xet",
    "gix",
    "git2",
    "hyper",
    "reqwest",
    "rusqlite",
    "slatedb",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
}
LFS_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "CRAB_",
    "Config::",
    "CrabError",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_identity",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-metadata",
    "crab-read",
    "crab-workflow",
    "crab-xet",
    "crab::",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache",
    "crab_cache_server",
    "crab_cache_store",
    "crab_coordination",
    "crab_metadata",
    "crab_read",
    "crab_workflow",
    "crab_xet",
    "eprintln!",
    "env::var",
    "git2::",
    "google-cloud",
    "google_cloud",
    "object_store::aws",
    "object_store::azure",
    "object_store::gcp",
    "object_store::parse_url",
    "parse_url_opts",
    "println!",
    "reqwest",
    "rusqlite",
    "slatedb",
    "std::env",
    "std::io::stdin",
    "std::io::stdout",
    "token_cache",
    "tokio::io::stdin",
    "tokio::io::stdout",
    "xet-core-structures",
    "xet-client",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
LFS_MODULE_SCAN_PATHS = (
    "crates/crab-lfs/Cargo.toml",
    "crates/crab-lfs/src",
)
STORAGE_MODULE_DIRECT_FORBIDDEN_PACKAGES = {
    "axum",
    "clap",
    "crab",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-workflow",
    "crab-xet",
    "gix",
    "git2",
    "rusqlite",
    "slatedb",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
}
STORAGE_MODULE_FORBIDDEN_PATTERNS = {
    "AuthConfig",
    "CrabError",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_identity",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-workflow",
    "crab-xet",
    "crab::",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache",
    "crab_cache_server",
    "crab_cache_store",
    "crab_coordination",
    "crab_diff",
    "crab_git",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_workflow",
    "crab_xet",
    "eprintln!",
    "git2::",
    "google-cloud",
    "google_cloud",
    "lfs/objects",
    "println!",
    "rusqlite",
    "slatedb",
    "std::io::stdin",
    "std::io::stdout",
    "std::process",
    "token_cache",
    "xet-core-structures",
    "xet-client",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
STORAGE_MODULE_SCAN_PATHS = (
    "crates/crab-storage/Cargo.toml",
    "crates/crab-storage/src",
)
METADATA_MODULE_DIRECT_FORBIDDEN_PACKAGES = {
    "axum",
    "clap",
    "crab",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-read",
    "crab-workflow",
    "gix",
    "git2",
    "hyper",
    "reqwest",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
}
METADATA_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "CRAB_",
    "CrabError",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "azure_identity",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-lfs",
    "crab-read",
    "crab-workflow",
    "crab::",
    "crab::core::config",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_cache",
    "crab_cache_server",
    "crab_cache_store",
    "crab_coordination",
    "crab_diff",
    "crab_git",
    "crab_lfs",
    "crab_read",
    "crab_workflow",
    "eprintln!",
    "git2::",
    "google-cloud",
    "google_cloud",
    "object_store::aws",
    "object_store::azure",
    "object_store::gcp",
    "object_store::parse_url",
    "parse_url_opts",
    "println!",
    "reqwest",
    "std::env",
    "std::io::stdin",
    "std::io::stdout",
    "std::process",
    "token_cache",
    "tracing::error",
    "xet-core-structures",
    "xet-client",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
METADATA_MODULE_SCAN_PATHS = (
    "crates/crab-metadata/Cargo.toml",
    "crates/crab-metadata/src",
)
DELETED_METADATA_REEXPORT_ADAPTER_PATHS = (
    "crab/src/metadata/file_index_lookup.rs",
)
DELETED_METADATA_REEXPORT_ADAPTER_SCAN_PATHS = (
    "crab/src",
    "crab/tests",
    "crates",
)
DELETED_METADATA_REEXPORT_ADAPTER_FORBIDDEN_PATTERNS = {
    "crate::metadata::file_index_lookup",
    "crate::metadata::{file_index_lookup",
    "crab::metadata::file_index_lookup",
    "use crate::metadata::file_index_lookup",
    "use crab::metadata::file_index_lookup",
}
WORKFLOW_MODULE_ALLOWED_NORMAL_PACKAGES = {
    "blake3",
    "bytes",
    "crab-coordination",
    "crab-storage",
    "crab-types",
    "futures-util",
    "fs4",
    "gix",
    "gix-discover",
    "gix-hash",
    "gix-object",
    "gix-odb",
    "globset",
    "libc",
    "md-5",
    "nix",
    "notify",
    "object_store",
    "petgraph",
    "reqwest",
    "rusqlite",
    "serde",
    "serde_json",
    "serde_yaml",
    "tempfile",
    "thiserror",
    "toml",
    "tokio",
    "tokio-util",
    "tracing",
    "unicode-normalization",
    "url",
    "uuid",
}
WORKFLOW_MODULE_FORBIDDEN_PATTERNS = {
    "AmazonS3Builder",
    "AuthConfig",
    "AuthServerError",
    "BuiltObjectStore",
    "CacheServerConfig",
    "CredentialProvider",
    "GoogleCloudStorageBuilder",
    "MicrosoftAzureBuilder",
    "ProviderClient",
    "TokenCache",
    "aws-config",
    "aws-sdk",
    "aws_config",
    "aws_sdk",
    "axum::",
    "azure_identity",
    "build_static_env",
    "build_url_object_store",
    "clap::",
    "crab = {",
    "crab-auth",
    "crab-auth-server",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-server",
    "crab-cache-store",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-xet",
    "crab::",
    "crab_auth",
    "crab_auth_server",
    "crab_auth_store",
    "crab_diff",
    "crab_git",
    "crab_lfs",
    "crab_metadata",
    "crab_read",
    "crab_xet",
    "git2::",
    "google-cloud",
    "google_cloud",
    "hyper::",
    "object_store_options_from_env",
    "slatedb",
    "std::io::stdin",
    "std::io::stdout",
    "xet-client",
    "xet-core-structures",
    "xet-data",
    "xet-runtime",
    "xet_client",
    "xet_core_structures",
    "xet_data",
    "xet_runtime",
}
WORKFLOW_MODULE_SCAN_PATHS = (
    "crates/crab-workflow/Cargo.toml",
    "crates/crab-workflow/src",
)
DELETED_WORKFLOW_REEXPORT_ADAPTER_PATHS = (
    "crab/src/workflow/template/mod.rs",
    "crab/src/workflow/graph.rs",
    "crab/src/workflow/lockfile.rs",
    "crab/src/workflow/retry.rs",
    "crab/src/workflow/run_state.rs",
    "crab/src/workflow/state.rs",
    "crab/src/workflow/status.rs",
    "crab/src/workflow/yaml.rs",
    "crab/src/workflow/migrate_dvc.rs",
)
DELETED_WORKFLOW_REEXPORT_ADAPTER_SCAN_PATHS = (
    "crab/src/workflow",
    "crab/src/cmd",
    "crab/tests",
)
DELETED_WORKFLOW_REEXPORT_ADAPTER_FORBIDDEN_PATTERNS = {
    "crate::workflow::template",
    "crate::workflow::graph",
    "crate::workflow::lockfile::",
    "crate::workflow::retry",
    "crate::workflow::run_state",
    "crate::workflow::state",
    "crate::workflow::status",
    "crate::workflow::yaml::",
    "crate::workflow::migrate_dvc",
    "crate::workflow::{yaml",
    "crate::workflow::{params, yaml",
    "use crate::workflow::yaml",
    "use crate::workflow::migrate_dvc",
    "crate::workflow::parse_yaml",
    "super::template",
    "super::graph",
    "super::lockfile::",
    "super::retry",
    "super::run_state",
    "super::state",
    "super::status",
    "workflow::template",
    "workflow::graph",
    "crab::workflow::lockfile::",
    "crab::workflow::graph",
    "workflow::retry",
    "workflow::run_state",
    "workflow::state",
    "workflow::status",
    "workflow::migrate_dvc",
    "crab::workflow::yaml::",
    "use crab::workflow::yaml",
    "crab::workflow::parse_yaml",
    "pub mod template;",
    "pub mod graph;",
    "pub mod retry;",
    "pub mod run_state;",
    "pub mod state;",
    "pub mod yaml;",
    "pub mod migrate_dvc;",
    "pub use state::StageState",
    "pub use yaml::",
}
PRIVATE_INTERNAL_PACKAGES = {
    "crab-auth",
    "crab-auth-store",
    "crab-cache",
    "crab-cache-store",
    "crab-coordination",
    "crab-diff",
    "crab-git",
    "crab-lfs",
    "crab-metadata",
    "crab-read",
    "crab-storage",
    "crab-types",
    "crab-workflow",
    "crab-xet",
}
SHIPPED_BINARY_PACKAGES = {
    "crab": {"crab"},
    "crab-auth-server": {"crab-auth-receive", "crab-auth-view"},
    "crab-cache-server": {"crab-cache-server"},
}
SERVER_PACKAGES = {"crab-auth-server", "crab-cache-server"}
ALLOWED_SERVER_DEV_FIXTURES = {
    "crab-auth-server": set(),
    "crab-cache-server": {"crab", "crab-cache-store"},
}
WORKSPACE_DEPENDENCY_POLICY = {
    "crab": {
        "normal": {
            "crab-auth",
            "crab-auth-store",
            "crab-cache",
            "crab-cache-store",
            "crab-coordination",
            "crab-diff",
            "crab-git",
            "crab-lfs",
            "crab-metadata",
            "crab-read",
            "crab-remote-git",
            "crab-staging",
            "crab-storage",
            "crab-types",
            "crab-workflow",
            "crab-xet",
            "crab-vfs",
        },
        "dev": {"crab-cache-server", "crab-workflow"},
    },
    "crab-auth": {"normal": {"crab-coordination", "crab-git", "crab-types"}},
    "crab-auth-server": {
        "normal": {
            "crab-auth",
            "crab-cache",
            "crab-cache-store",
            "crab-coordination",
            "crab-git",
            "crab-lfs",
            "crab-metadata",
            "crab-read",
            "crab-staging",
            "crab-storage",
            "crab-types",
            "crab-xet",
        },
    },
    "crab-auth-store": {
        "normal": {"crab-auth", "crab-git", "crab-storage", "crab-types"},
    },
    "crab-cache": {"normal": {"crab-types", "crab-xet"}},
    "crab-cache-server": {
        "normal": {"crab-cache", "crab-storage", "crab-xet"},
        "dev": {"crab-cache"},
    },
    "crab-cache-store": {
        "normal": {"crab-cache", "crab-storage", "crab-xet"},
        "dev": {"crab-cache-server"},
    },
    "crab-coordination": {},
    "crab-diff": {"normal": {"crab-types", "crab-xet"}},
    "crab-git": {"normal": {"crab-types"}},
    "crab-lfs": {"normal": {"crab-git", "crab-storage"}},
    "crab-metadata": {"normal": {"crab-storage", "crab-xet"}},
    "crab-read": {
        "normal": {
            "crab-cache",
            "crab-cache-store",
            "crab-diff",
            "crab-metadata",
            "crab-remote-git",
            "crab-storage",
            "crab-types",
            "crab-xet",
        },
    },
    "crab-remote-git": {
        "normal": {"crab-git", "crab-metadata", "crab-storage", "crab-xet"},
        "dev": {"crab-metadata"},
    },
    "crab-staging": {"normal": {"crab-diff", "crab-xet"}},
    "crab-storage": {"normal": {"crab-types"}},
    "crab-types": {},
    "crab-vfs": {
        "normal": {
            "crab-cache",
            "crab-cache-store",
            "crab-git",
            "crab-read",
            "crab-staging",
            "crab-storage",
            "crab-types",
            "crab-xet",
        },
    },
    "crab-workflow": {
        "normal": {"crab-coordination", "crab-storage", "crab-types"}
    },
    "crab-xet": {},
}
WORKSPACE_DEPENDENCY_PATHS = {
    "crab-auth": "crates/crab-auth",
    "crab-auth-server": "crates/crab-auth-server",
    "crab-auth-store": "crates/crab-auth-store",
    "crab-cache": "crates/crab-cache",
    "crab-cache-server": "crates/crab-cache-server",
    "crab-cache-store": "crates/crab-cache-store",
    "crab-coordination": "crates/crab-coordination",
    "crab-diff": "crates/crab-diff",
    "crab-git": "crates/crab-git",
    "crab-lfs": "crates/crab-lfs",
    "crab-metadata": "crates/crab-metadata",
    "crab-read": "crates/crab-read",
    "crab-remote-git": "crates/crab-remote-git",
    "crab-staging": "crates/crab-staging",
    "crab-storage": "crates/crab-storage",
    "crab-types": "crates/crab-types",
    "crab-vfs": "crates/crab-vfs",
    "crab-workflow": "crates/crab-workflow",
    "crab-xet": "crates/crab-xet",
}
WORKSPACE_XET_DEPENDENCY_VERSIONS = {
    "xet-client": "1.6.0",
    "xet-core-structures": "1.6.0",
    "xet-data": "1.6.0",
    "xet-runtime": "1.6.0",
}
WORKSPACE_GITOXIDE_DEPENDENCIES = {
    "gix": "0.83.0",
    "gix-attributes": "0.33.0",
    "gix-config": "0.56.0",
    "gix-credentials": "0.38.0",
    "gix-diff": {"version": "0.63.0", "default-features": False},
    "gix-dir": "0.25.0",
    "gix-discover": "0.51.0",
    "gix-features": "0.48.0",
    "gix-filter": "0.30.0",
    "gix-fsck": "0.21.0",
    "gix-glob": "0.26.0",
    "gix-hash": "0.25.0",
    "gix-ignore": "0.21.0",
    "gix-index": "0.51.0",
    "gix-lock": "23.0.0",
    "gix-negotiate": "0.31.0",
    "gix-object": "0.60.0",
    "gix-odb": "0.80.0",
    "gix-pack": "0.70.0",
    "gix-packetline": "0.21.3",
    "gix-pathspec": "0.18.0",
    "gix-prompt": "0.15.0",
    "gix-protocol": "0.61.0",
    "gix-ref": "0.63.0",
    "gix-refspec": "0.41.0",
    "gix-revwalk": "0.31.0",
    "gix-shallow": "0.12.0",
    "gix-status": "0.30.0",
    "gix-tempfile": "23.0.0",
    "gix-transport": "0.57.0",
    "gix-traverse": "0.57.0",
    "gix-url": "0.36.0",
    "gix-validate": "0.11.1",
    "gix-worktree": "0.52.0",
    "gix-worktree-state": "0.30.0",
}
WORKSPACE_PROVIDER_SDK_DEPENDENCIES = {
    "aws-config": "1",
    "aws-sdk-dynamodb": "1",
    "aws-sdk-iam": "1",
    "aws-sdk-s3": "1",
    "aws-sdk-s3control": "1",
    "aws-sdk-sts": "1",
    "azure_core": "0.21",
    "azure_identity": "0.21",
    "azure_mgmt_storage": "0.21",
    "azure_storage": "0.21",
    "azure_storage_blobs": "0.21",
    "google-cloud-storage": "0.24",
    "google-cloud-token": "0.1.2",
}
WORKSPACE_THIRD_PARTY_DEPENDENCIES = {
    "async-trait": "0.1",
    "blake3": "1.8",
    "bytes": "1.11",
    "futures-util": "0.3",
    "object_store": {"version": "0.14.1", "default-features": False},
    "reqwest": {"version": "0.12", "default-features": False},
    "rusqlite": {"version": "0.34", "features": ["bundled"]},
    "schemars": "0.8",
    "serde": {"version": "1", "features": ["derive"]},
    "serde_json": "1",
    "serde_yaml": "0.9",
    "tempfile": "3",
    "thiserror": "2.0",
    "tokio": "1.49",
    "tokio-util": "0.7",
    "toml": "0.8",
    "tracing": "0.1",
}


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def run(args: list[str], root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )


def cargo_metadata(root: Path, cargo: str) -> dict:
    result = run([cargo, "metadata", "--format-version", "1", "--no-deps"], root)
    if result.returncode != 0:
        raise RuntimeError(result.stdout)
    return json.loads(result.stdout)


def package_roots(metadata: dict) -> dict[str, Path]:
    roots = {}
    for package in metadata["packages"]:
        roots[package["name"]] = Path(package["manifest_path"]).parent
    return roots


def package_by_name(metadata: dict, name: str) -> dict:
    for package in metadata["packages"]:
        if package["name"] == name:
            return package
    raise KeyError(name)


def normal_dependency(package: dict, name: str) -> dict | None:
    for dependency in package["dependencies"]:
        if dependency["name"] == name and dependency["kind"] is None:
            return dependency
    return None


def dev_dependency(package: dict, name: str) -> dict | None:
    for dependency in package["dependencies"]:
        if dependency["name"] == name and dependency["kind"] == "dev":
            return dependency
    return None


def feature(package: dict, name: str) -> list[str]:
    return sorted(package["features"].get(name, []))


def binary_targets(package: dict) -> set[str]:
    return {
        target["name"]
        for target in package["targets"]
        if "bin" in target["kind"]
    }


def check_feature_definitions(package: dict, expected: dict[str, list[str]]) -> list[str]:
    violations: list[str] = []
    for name, expected_values in expected.items():
        actual = feature(package, name)
        expected_sorted = sorted(expected_values)
        if actual != expected_sorted:
            violations.append(
                f"{package['name']}: feature {name} is {actual}; expected {expected_sorted}"
            )
    return violations


def check_object_store_features(metadata: dict) -> bool:
    violations: list[str] = []
    checked = 0
    for package in sorted(metadata["packages"], key=lambda package: package["name"]):
        for dependency in package["dependencies"]:
            if dependency["name"] != "object_store":
                continue
            checked += 1
            name = package["name"]
            features = set(dependency["features"])
            if dependency["uses_default_features"]:
                violations.append(f"{name}: object_store uses upstream default features")

            allowed = OBJECT_STORE_IMPLEMENTATION_FEATURES.get(name, set())
            if features != allowed:
                expected = ", ".join(sorted(allowed)) or "(none)"
                actual = ", ".join(sorted(features)) or "(none)"
                violations.append(
                    f"{name}: object_store features are {actual}; expected {expected}"
                )

    if not violations:
        print(f"ok: {checked} direct object_store dependencies have explicit feature ownership")
        return True

    print("error: object_store feature ownership drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_package_release_policy(metadata: dict) -> bool:
    violations: list[str] = []
    product_version = package_by_name(metadata, "crab")["version"]

    for name in sorted(PRIVATE_INTERNAL_PACKAGES):
        package = package_by_name(metadata, name)
        if package["version"] != "0.1.0":
            violations.append(f"{name}: private split crate version is {package['version']}; expected 0.1.0")
        if package["publish"] != []:
            violations.append(f"{name}: private split crate must set publish = false")

    for name, expected_bins in sorted(SHIPPED_BINARY_PACKAGES.items()):
        package = package_by_name(metadata, name)
        if package["version"] != product_version:
            violations.append(
                f"{name}: shipped package version is {package['version']}; expected {product_version}"
            )
        actual_bins = binary_targets(package)
        missing = sorted(expected_bins - actual_bins)
        if missing:
            violations.append(f"{name}: missing shipped binary target(s): {', '.join(missing)}")

    if not violations:
        print(
            f"ok: {len(PRIVATE_INTERNAL_PACKAGES)} private split crates are unpublished and shipped package versions align"
        )
        return True

    print("error: package release policy drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_server_fixture_dependencies(metadata: dict) -> bool:
    violations: list[str] = []
    seen_dev_fixtures: dict[str, set[str]] = {
        name: set() for name in SERVER_PACKAGES
    }

    for package in metadata["packages"]:
        package_name = package["name"]
        for dependency in package["dependencies"]:
            server_name = dependency["name"]
            if server_name not in SERVER_PACKAGES:
                continue

            if dependency["kind"] == "dev":
                seen_dev_fixtures[server_name].add(package_name)
                allowed = ALLOWED_SERVER_DEV_FIXTURES[server_name]
                if package_name not in allowed:
                    violations.append(
                        f"{package_name}: dev-depends on {server_name}; expected one of {sorted(allowed)}"
                    )
                continue

            kind = dependency["kind"] or "normal"
            violations.append(
                f"{package_name}: {kind}-depends on server package {server_name}"
            )

    for server_name, expected in sorted(ALLOWED_SERVER_DEV_FIXTURES.items()):
        actual = seen_dev_fixtures[server_name]
        if actual != expected:
            violations.append(
                f"{server_name}: dev fixture consumers are {sorted(actual)}; expected {sorted(expected)}"
            )

    if not violations:
        fixture_count = sum(len(consumers) for consumers in seen_dev_fixtures.values())
        print(
            f"ok: server packages stay production-isolated with {fixture_count} approved dev fixture edges"
        )
        return True

    print("error: server fixture dependency policy drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def dependency_kind(dependency: dict) -> str:
    return dependency["kind"] or "normal"


def check_workspace_dependency_policy(metadata: dict) -> bool:
    violations: list[str] = []
    workspace_packages = {package["name"] for package in metadata["packages"]}

    missing_policy = sorted(workspace_packages - set(WORKSPACE_DEPENDENCY_POLICY))
    for name in missing_policy:
        violations.append(f"{name}: missing workspace dependency policy entry")

    stale_policy = sorted(set(WORKSPACE_DEPENDENCY_POLICY) - workspace_packages)
    for name in stale_policy:
        violations.append(f"{name}: policy exists for package missing from cargo metadata")

    checked_edges = 0
    for package in sorted(metadata["packages"], key=lambda package: package["name"]):
        package_name = package["name"]
        allowed_by_kind = WORKSPACE_DEPENDENCY_POLICY.get(package_name, {})
        for dependency in package["dependencies"]:
            dependency_name = dependency["name"]
            if dependency_name not in workspace_packages:
                continue
            checked_edges += 1
            kind = dependency_kind(dependency)
            allowed = allowed_by_kind.get(kind, set())
            if dependency_name not in allowed:
                expected = {
                    allowed_kind: sorted(dependencies)
                    for allowed_kind, dependencies in sorted(allowed_by_kind.items())
                    if dependencies
                }
                violations.append(
                    f"{package_name}: {kind}-depends on {dependency_name}; expected policy {expected}"
                )

    if not violations:
        print(
            f"ok: {checked_edges} direct workspace dependency edges match production/dev policy"
        )
        return True

    print("error: workspace dependency production/dev policy drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def manifest_dependency_tables(manifest: dict) -> list[tuple[str, dict]]:
    tables: list[tuple[str, dict]] = []
    for table_name in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = manifest.get(table_name)
        if isinstance(table, dict):
            tables.append((table_name, table))

    target = manifest.get("target")
    if isinstance(target, dict):
        for target_name, target_manifest in sorted(target.items()):
            if not isinstance(target_manifest, dict):
                continue
            for table_name in ("dependencies", "dev-dependencies", "build-dependencies"):
                table = target_manifest.get(table_name)
                if isinstance(table, dict):
                    tables.append((f"target.{target_name}.{table_name}", table))

    return tables


def rel(root: Path, path: Path) -> str:
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return path.as_posix()


def normalized_workspace_entry(entry: object) -> object:
    if not isinstance(entry, dict):
        return entry

    normalized: dict[str, object] = {}
    for key, value in sorted(entry.items()):
        if key == "features" and isinstance(value, list):
            normalized[key] = sorted(value)
            continue
        normalized[key] = value
    return normalized


def check_workspace_dependency_sources(root: Path, metadata: dict) -> bool:
    violations: list[str] = []
    inherited_refs = 0

    root_manifest_path = root / "Cargo.toml"
    root_manifest = tomllib.loads(root_manifest_path.read_text(encoding="utf-8"))
    workspace_dependencies = root_manifest.get("workspace", {}).get("dependencies", {})

    for name, expected_path in sorted(WORKSPACE_DEPENDENCY_PATHS.items()):
        entry = workspace_dependencies.get(name)
        if not isinstance(entry, dict):
            violations.append(f"Cargo.toml: workspace dependency {name} must be a table")
            continue
        if entry.get("path") != expected_path:
            violations.append(
                f"Cargo.toml: workspace dependency {name} path is {entry.get('path')!r}; expected {expected_path!r}"
            )
        if entry.get("default-features") is not False:
            violations.append(
                f"Cargo.toml: workspace dependency {name} must set default-features = false"
            )

    for package in sorted(metadata["packages"], key=lambda package: package["name"]):
        manifest_path = Path(package["manifest_path"])
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        for table_name, table in manifest_dependency_tables(manifest):
            for dependency_name, dependency in sorted(table.items()):
                if dependency_name not in WORKSPACE_DEPENDENCY_PATHS:
                    continue
                inherited_refs += 1
                label = f"{rel(root, manifest_path)}:{table_name}.{dependency_name}"
                if not isinstance(dependency, dict):
                    violations.append(f"{label}: internal Crab dependency must inherit workspace")
                    continue
                if "path" in dependency:
                    violations.append(
                        f"{label}: inline path {dependency['path']!r} must move to workspace.dependencies"
                    )
                if dependency.get("workspace") is not True:
                    violations.append(f"{label}: missing workspace = true")

    if not violations:
        print(
            f"ok: {len(WORKSPACE_DEPENDENCY_PATHS)} internal workspace dependencies are centralized with {inherited_refs} inherited refs"
        )
        return True

    print("error: internal workspace dependency source policy drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_workspace_xet_dependency_sources(root: Path, metadata: dict) -> bool:
    violations: list[str] = []
    inherited_refs = 0

    root_manifest_path = root / "Cargo.toml"
    root_manifest = tomllib.loads(root_manifest_path.read_text(encoding="utf-8"))
    workspace_dependencies = root_manifest.get("workspace", {}).get("dependencies", {})

    for name, expected_version in sorted(WORKSPACE_XET_DEPENDENCY_VERSIONS.items()):
        entry = workspace_dependencies.get(name)
        if isinstance(entry, str):
            actual_version = entry
        elif isinstance(entry, dict):
            actual_version = entry.get("version")
            if "path" in entry:
                violations.append(
                    f"Cargo.toml: workspace Xet dependency {name} must use crates.io, not path {entry['path']!r}"
                )
        else:
            violations.append(
                f"Cargo.toml: workspace Xet dependency {name} must declare version {expected_version!r}"
            )
            continue

        if actual_version != expected_version:
            violations.append(
                f"Cargo.toml: workspace Xet dependency {name} version is {actual_version!r}; expected {expected_version!r}"
            )

    for package in sorted(metadata["packages"], key=lambda package: package["name"]):
        manifest_path = Path(package["manifest_path"])
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        for table_name, table in manifest_dependency_tables(manifest):
            for dependency_name, dependency in sorted(table.items()):
                if dependency_name not in WORKSPACE_XET_DEPENDENCY_VERSIONS:
                    continue
                inherited_refs += 1
                label = f"{rel(root, manifest_path)}:{table_name}.{dependency_name}"
                if not isinstance(dependency, dict):
                    violations.append(f"{label}: Xet dependency must inherit workspace")
                    continue
                if "path" in dependency:
                    violations.append(
                        f"{label}: inline path {dependency['path']!r} must move to workspace.dependencies"
                    )
                if dependency.get("workspace") is not True:
                    violations.append(f"{label}: missing workspace = true")

    if not violations:
        print(
            f"ok: {len(WORKSPACE_XET_DEPENDENCY_VERSIONS)} published Xet dependencies are centralized with {inherited_refs} inherited refs"
        )
        return True

    print("error: Xet workspace dependency source policy drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_workspace_gitoxide_dependency_sources(root: Path, metadata: dict) -> bool:
    violations: list[str] = []
    inherited_refs = 0

    root_manifest_path = root / "Cargo.toml"
    root_manifest = tomllib.loads(root_manifest_path.read_text(encoding="utf-8"))
    workspace_dependencies = root_manifest.get("workspace", {}).get("dependencies", {})

    for name, expected_entry in sorted(WORKSPACE_GITOXIDE_DEPENDENCIES.items()):
        entry = workspace_dependencies.get(name)
        if normalized_workspace_entry(entry) != normalized_workspace_entry(expected_entry):
            violations.append(
                f"Cargo.toml: workspace Gitoxide dependency {name} is {entry!r}; expected {expected_entry!r}"
            )

    for package in sorted(metadata["packages"], key=lambda package: package["name"]):
        manifest_path = Path(package["manifest_path"])
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        for table_name, table in manifest_dependency_tables(manifest):
            for dependency_name, dependency in sorted(table.items()):
                if dependency_name not in WORKSPACE_GITOXIDE_DEPENDENCIES:
                    continue
                inherited_refs += 1
                label = f"{rel(root, manifest_path)}:{table_name}.{dependency_name}"
                if not isinstance(dependency, dict):
                    violations.append(f"{label}: Gitoxide dependency must inherit workspace")
                    continue
                if "path" in dependency:
                    violations.append(
                        f"{label}: inline path {dependency['path']!r} must move to workspace.dependencies"
                    )
                if "version" in dependency:
                    violations.append(
                        f"{label}: local version {dependency['version']!r} must move to workspace.dependencies"
                    )
                if dependency.get("workspace") is not True:
                    violations.append(f"{label}: missing workspace = true")

    if not violations:
        print(
            f"ok: {len(WORKSPACE_GITOXIDE_DEPENDENCIES)} Gitoxide dependencies are centralized with {inherited_refs} inherited refs"
        )
        return True

    print("error: Gitoxide workspace dependency source policy drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_workspace_provider_sdk_dependency_sources(root: Path, metadata: dict) -> bool:
    violations: list[str] = []
    inherited_refs = 0

    root_manifest_path = root / "Cargo.toml"
    root_manifest = tomllib.loads(root_manifest_path.read_text(encoding="utf-8"))
    workspace_dependencies = root_manifest.get("workspace", {}).get("dependencies", {})

    for name, expected_entry in sorted(WORKSPACE_PROVIDER_SDK_DEPENDENCIES.items()):
        entry = workspace_dependencies.get(name)
        if normalized_workspace_entry(entry) != normalized_workspace_entry(expected_entry):
            violations.append(
                f"Cargo.toml: workspace provider SDK dependency {name} is {entry!r}; expected {expected_entry!r}"
            )

    for package in sorted(metadata["packages"], key=lambda package: package["name"]):
        manifest_path = Path(package["manifest_path"])
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        for table_name, table in manifest_dependency_tables(manifest):
            for dependency_name, dependency in sorted(table.items()):
                if dependency_name not in WORKSPACE_PROVIDER_SDK_DEPENDENCIES:
                    continue
                inherited_refs += 1
                label = f"{rel(root, manifest_path)}:{table_name}.{dependency_name}"
                if not isinstance(dependency, dict):
                    violations.append(f"{label}: provider SDK dependency must inherit workspace")
                    continue
                if "version" in dependency:
                    violations.append(
                        f"{label}: local version {dependency['version']!r} must move to workspace.dependencies"
                    )
                if "default-features" in dependency:
                    violations.append(
                        f"{label}: local default-features must move to workspace.dependencies"
                    )
                if dependency.get("workspace") is not True:
                    violations.append(f"{label}: missing workspace = true")

    if not violations:
        print(
            f"ok: {len(WORKSPACE_PROVIDER_SDK_DEPENDENCIES)} cloud/provider SDK dependency versions are centralized with {inherited_refs} inherited refs"
        )
        return True

    print("error: provider SDK workspace dependency source policy drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_workspace_third_party_dependency_sources(root: Path, metadata: dict) -> bool:
    violations: list[str] = []
    inherited_refs = 0

    root_manifest_path = root / "Cargo.toml"
    root_manifest = tomllib.loads(root_manifest_path.read_text(encoding="utf-8"))
    workspace_dependencies = root_manifest.get("workspace", {}).get("dependencies", {})

    for name, expected_entry in sorted(WORKSPACE_THIRD_PARTY_DEPENDENCIES.items()):
        entry = workspace_dependencies.get(name)
        if normalized_workspace_entry(entry) != normalized_workspace_entry(expected_entry):
            violations.append(
                f"Cargo.toml: workspace dependency {name} is {entry!r}; expected {expected_entry!r}"
            )

    for package in sorted(metadata["packages"], key=lambda package: package["name"]):
        manifest_path = Path(package["manifest_path"])
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        for table_name, table in manifest_dependency_tables(manifest):
            for dependency_name, dependency in sorted(table.items()):
                if dependency_name not in WORKSPACE_THIRD_PARTY_DEPENDENCIES:
                    continue
                inherited_refs += 1
                label = f"{rel(root, manifest_path)}:{table_name}.{dependency_name}"
                if not isinstance(dependency, dict):
                    violations.append(f"{label}: third-party dependency must inherit workspace")
                    continue
                if "version" in dependency:
                    violations.append(
                        f"{label}: local version {dependency['version']!r} must move to workspace.dependencies"
                    )
                if "default-features" in dependency:
                    violations.append(
                        f"{label}: local default-features must move to workspace.dependencies"
                    )
                if dependency.get("workspace") is not True:
                    violations.append(f"{label}: missing workspace = true")

    if not violations:
        print(
            f"ok: {len(WORKSPACE_THIRD_PARTY_DEPENDENCIES)} shared third-party dependency versions are centralized with {inherited_refs} inherited refs"
        )
        return True

    print("error: shared third-party dependency source policy drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def cargo_tree_lines(root: Path, cargo: str, args: list[str]) -> list[str]:
    # Keep Cargo's download/progress chatter out of the machine-parsed tree.
    result = run([cargo, "tree", "--quiet", *args], root)
    if result.returncode != 0:
        raise RuntimeError(result.stdout)
    return [
        line
        for line in result.stdout.splitlines()
        if line.strip() and "Blocking waiting for file lock" not in line
    ]


def tree_contains_package(lines: list[str], package: str) -> bool:
    marker = f"{package} v"
    return any(marker in line for line in lines)


def check_tree_packages(
    root: Path,
    cargo: str,
    label: str,
    args: list[str],
    *,
    required: set[str] | None = None,
    forbidden: set[str] | None = None,
) -> bool:
    required = required or set()
    forbidden = forbidden or set()

    try:
        lines = cargo_tree_lines(root, cargo, args)
    except RuntimeError as error:
        print(f"error: cargo tree for {label} failed:", file=sys.stderr)
        print(error, file=sys.stderr)
        return False

    missing = sorted(package for package in required if not tree_contains_package(lines, package))
    present_forbidden = sorted(
        package for package in forbidden if tree_contains_package(lines, package)
    )

    if not missing and not present_forbidden:
        print(f"ok: {label}")
        return True

    print(f"error: dependency budget drifted for {label}:", file=sys.stderr)
    for package in missing:
        print(f"  missing expected package: {package}", file=sys.stderr)
    for package in present_forbidden:
        print(f"  unexpected package: {package}", file=sys.stderr)
    return False


def check_reverse_dependency_is_self(root: Path, cargo: str, package: str) -> bool:
    try:
        lines = cargo_tree_lines(
            root,
            cargo,
            ["-i", package, "--edges", "normal", "--depth", "2"],
        )
    except RuntimeError as error:
        print(f"error: cargo tree -i {package} failed:", file=sys.stderr)
        print(error, file=sys.stderr)
        return False

    if len(lines) == 1 and lines[0].startswith(f"{package} "):
        print(f"ok: {package} has no normal reverse consumers")
        return True

    print(f"error: {package} has unexpected normal reverse consumers:", file=sys.stderr)
    for line in lines:
        print(f"  {line}", file=sys.stderr)
    return False


def check_xet_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-xet")
    violations: list[str] = []

    for name in XET_MODULE_REQUIRED_NORMAL_PACKAGES:
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-xet: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-xet: {name} must stay non-optional")

    for dependency in package["dependencies"]:
        if dependency["kind"] is not None:
            continue
        name = dependency["name"]
        if dependency["optional"]:
            if name not in XET_MODULE_OPTIONAL_PACKAGES:
                violations.append(f"crab-xet: optional dependency {name} is not admitted")
        elif name not in XET_MODULE_REQUIRED_NORMAL_PACKAGES:
            violations.append(f"crab-xet: normal dependency {name} is not admitted")

    tokio = normal_dependency(package, "tokio")
    if tokio is not None and sorted(tokio["features"]) != ["sync"]:
        violations.append(
            f"crab-xet: optional tokio features must stay ['sync'], got {sorted(tokio['features'])}"
        )

    for scan_path in XET_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in XET_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")
                if candidate.name != "chunker.rs" and "xet_data::" in line:
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")
                if candidate.name != "upload_concurrency.rs" and (
                    "xet_client::" in line or "xet_runtime::" in line or "tokio::" in line
                ):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-xet stays Xet data-plane owned without domain policy")
        return True

    print("error: crab-xet data-plane scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_xet_feature_budget(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-xet")
    violations = check_feature_definitions(
        package,
        {
            "default": [],
            "chunker": ["dep:xet-data"],
            "upload-concurrency": ["dep:tokio", "dep:xet-client", "dep:xet-runtime"],
        },
    )

    for name in ("tokio", "xet-client", "xet-data", "xet-runtime"):
        dependency = normal_dependency(package, name)
        if dependency is None or not dependency["optional"]:
            violations.append(f"crab-xet: {name} must stay optional")

    checks = [
        check_tree_packages(
            root,
            cargo,
            "default crab-xet exposes only the Xet compatibility tax",
            ["-p", "crab-xet", "--edges", "normal", "--depth", "2"],
            required={"xet-core-structures", "xet-runtime"},
            forbidden={"xet-data", "xet-client"},
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-xet/chunker exposes the CDC chunker cost",
            [
                "-p",
                "crab-xet",
                "--features",
                "chunker",
                "--edges",
                "normal",
                "--depth",
                "2",
            ],
            required={"xet-data", "xet-client"},
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-xet/upload-concurrency exposes upload controller cost",
            [
                "-p",
                "crab-xet",
                "--features",
                "upload-concurrency",
                "--edges",
                "normal",
                "--depth",
                "2",
            ],
            required={"xet-client", "xet-runtime"},
            forbidden={"xet-data"},
        ),
    ]

    if not violations:
        print("ok: crab-xet feature budget is explicit")
        return all(checks)

    print("error: crab-xet manifest feature budget drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_coordination_feature_budget(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-coordination")
    violations = check_feature_definitions(
        package,
        {
            "default": [],
            "object-store-lock": [
                "dep:bytes",
                "dep:futures-util",
                "dep:object_store",
                "dep:tokio-util",
                "dep:tracing",
                "dep:uuid",
                "tokio/macros",
                "tokio/rt",
                "tokio/time",
            ],
            "coordinator-cosmosdb": [
                "dep:azure_core",
                "dep:azure_identity",
                "dep:reqwest",
                "dep:urlencoding",
            ],
            "coordinator-dynamodb": ["dep:aws-config", "dep:aws-sdk-dynamodb"],
            "coordinator-spanner": [
                "dep:google-cloud-storage",
                "dep:google-cloud-token",
                "dep:reqwest",
            ],
        },
    )

    for name in COORDINATION_RUNTIME_PACKAGES:
        dependency = normal_dependency(package, name)
        if dependency is None or not dependency["optional"]:
            violations.append(f"crab-coordination: {name} must stay optional")

    for name in COORDINATION_OBJECT_STORE_LOCK_PACKAGES:
        dependency = normal_dependency(package, name)
        if dependency is None or not dependency["optional"]:
            violations.append(f"crab-coordination: {name} must stay optional")

    tree_ok = check_tree_packages(
        root,
        cargo,
        "default crab-coordination excludes provider runtimes",
        ["-p", "crab-coordination", "--edges", "normal", "--depth", "2"],
        forbidden=COORDINATION_RUNTIME_PACKAGES | COORDINATION_OBJECT_STORE_LOCK_PACKAGES,
    )

    if not violations:
        print("ok: crab-coordination optional runtimes are feature-gated")
        return tree_ok

    print("error: crab-coordination feature budget drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_coordination_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-coordination")
    violations: list[str] = []

    for name in COORDINATION_MODULE_REQUIRED_NORMAL_PACKAGES:
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-coordination: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-coordination: {name} must stay non-optional")

    tokio = normal_dependency(package, "tokio")
    if tokio is not None and sorted(tokio["features"]) != ["sync"]:
        violations.append(
            f"crab-coordination: default tokio features must stay ['sync'], got {sorted(tokio['features'])}"
        )

    for dependency in package["dependencies"]:
        if dependency["kind"] is not None:
            continue
        name = dependency["name"]
        if dependency["optional"]:
            if name not in COORDINATION_MODULE_ALLOWED_OPTIONAL_PACKAGES:
                violations.append(f"crab-coordination: optional dependency {name} is not admitted")
        elif name not in COORDINATION_MODULE_REQUIRED_NORMAL_PACKAGES:
            violations.append(f"crab-coordination: normal dependency {name} is not admitted")

    for scan_path in COORDINATION_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in COORDINATION_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-coordination stays coordination-owned without cross-domain policy")
        return True

    print("error: crab-coordination coordination scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_cache_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-cache")
    violations: list[str] = []

    for name in ("crab-types", "crab-xet", "serde", "thiserror", "tracing"):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-cache: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-cache: {name} must stay non-optional")

    xet_dependency = normal_dependency(package, "crab-xet")
    if xet_dependency is not None and xet_dependency["features"]:
        violations.append("crab-cache: crab-xet dependency must not enable chunker/client features")

    for name in CACHE_MODULE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-cache: {name} must not be a normal dependency")

    for scan_path in CACHE_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in CACHE_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")
                if candidate.name != "xet_chunk_cache.rs" and any(
                    pattern in line for pattern in CACHE_MODULE_XET_RUNTIME_PATTERNS
                ):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-cache stays client/shared cache without server or storage policy")
        return True

    print("error: crab-cache client/shared cache scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_cache_feature_budget(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-cache")
    violations = check_feature_definitions(
        package,
        {
            "default": [],
            "active-probe": ["dep:reqwest"],
            "local-cache": ["dep:filetime", "dep:fs4", "dep:rusqlite", "dep:tokio", "dep:tokio-util"],
            "remote-client": ["active-probe", "dep:futures-util", "dep:tokio"],
            "xet-chunk-cache": [
                "dep:base64",
                "dep:crc32fast",
                "dep:tokio",
                "dep:tokio-util",
                "dep:xet-client",
                "dep:xet-runtime",
            ],
        },
    )

    for name in (
        "base64",
        "crc32fast",
        "filetime",
        "fs4",
        "reqwest",
        "rusqlite",
        "tokio",
        "tokio-util",
        "xet-client",
    ):
        dependency = normal_dependency(package, name)
        if dependency is None or not dependency["optional"]:
            violations.append(f"crab-cache: {name} must stay optional")

    if normal_dependency(package, "crab-cache-server") is not None:
        violations.append("crab-cache: crab-cache-server must not be a normal dependency")

    checks = [
        check_tree_packages(
            root,
            cargo,
            "default crab-cache excludes persistence, HTTP, storage, and server runtime",
            ["-p", "crab-cache", "--edges", "normal", "--depth", "2"],
            forbidden=CACHE_DEFAULT_FORBIDDEN_PACKAGES,
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-cache/local-cache exposes only local persistence cost",
            [
                "-p",
                "crab-cache",
                "--features",
                "local-cache",
                "--edges",
                "normal",
                "--depth",
                "2",
            ],
            required={"filetime", "fs4", "rusqlite", "tokio", "tokio-util"},
            forbidden={"crab-cache-server", "crab-storage", "object_store", "reqwest", "xet-client"},
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-cache/active-probe exposes only active probe HTTP cost",
            [
                "-p",
                "crab-cache",
                "--features",
                "active-probe",
                "--edges",
                "normal",
                "--depth",
                "2",
            ],
            required={"reqwest"},
            forbidden={"crab-cache-server", "crab-storage", "filetime", "object_store", "rusqlite", "xet-client"},
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-cache/remote-client exposes only HTTP client cost",
            [
                "-p",
                "crab-cache",
                "--features",
                "remote-client",
                "--edges",
                "normal",
                "--depth",
                "2",
            ],
            required={"reqwest", "tokio"},
            forbidden={"crab-cache-server", "crab-storage", "filetime", "object_store", "rusqlite", "xet-client"},
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-cache/xet-chunk-cache exposes only Xet range-cache cost",
            [
                "-p",
                "crab-cache",
                "--features",
                "xet-chunk-cache",
                "--edges",
                "normal",
                "--depth",
                "2",
            ],
            required={
                "base64",
                "crc32fast",
                "tokio",
                "tokio-util",
                "xet-client",
                "xet-runtime",
            },
            forbidden={"crab-cache-server", "crab-storage", "filetime", "object_store", "rusqlite"},
        ),
    ]

    if not violations:
        print("ok: crab-cache feature costs are explicit")
        return all(checks)

    print("error: crab-cache feature budget drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_cache_store_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-cache-store")
    violations: list[str] = []

    for name in (
        "async-trait",
        "bytes",
        "crab-cache",
        "crab-storage",
        "crab-xet",
        "futures-util",
        "object_store",
        "thiserror",
        "tracing",
    ):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-cache-store: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-cache-store: {name} must stay non-optional")

    cache_dependency = normal_dependency(package, "crab-cache")
    if cache_dependency is not None and sorted(cache_dependency["features"]) != ["local-cache"]:
        violations.append(
            "crab-cache-store: normal crab-cache dependency must enable only local-cache"
        )

    object_store = normal_dependency(package, "object_store")
    if object_store is not None:
        if object_store["uses_default_features"]:
            violations.append("crab-cache-store: object_store must disable default features")
        if object_store["features"]:
            violations.append("crab-cache-store: object_store must stay featureless")

    for name in CACHE_STORE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-cache-store: {name} must not be a normal dependency")

    if dev_dependency(package, "crab-cache-server") is None:
        violations.append("crab-cache-store: cache-server fixture dependency should stay dev-only")

    for scan_path in CACHE_STORE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            production_text = text.partition("\n#[cfg(test)]")[0]
            for index, line in enumerate(production_text.splitlines(), start=1):
                if any(pattern in line for pattern in CACHE_STORE_SOURCE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-cache-store stays a cache/storage Adapter without server policy")
        return True

    print("error: crab-cache-store Adapter scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_cache_store_budget(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-cache-store")
    violations = check_feature_definitions(
        package,
        {
            "default": [],
            "remote-client": ["crab-cache/remote-client"],
        },
    )

    cache_dependency = normal_dependency(package, "crab-cache")
    if cache_dependency is None:
        violations.append("crab-cache-store: missing normal crab-cache dependency")
    elif sorted(cache_dependency["features"]) != ["local-cache"]:
        violations.append(
            "crab-cache-store: normal crab-cache dependency must enable only local-cache"
        )

    object_store = normal_dependency(package, "object_store")
    if object_store is None:
        violations.append("crab-cache-store: missing object_store Adapter dependency")
    else:
        if object_store["uses_default_features"]:
            violations.append("crab-cache-store: object_store must disable default features")
        if object_store["features"]:
            violations.append("crab-cache-store: object_store must stay featureless")

    if normal_dependency(package, "crab-cache-server") is not None:
        violations.append("crab-cache-store: crab-cache-server must not be a normal dependency")
    if dev_dependency(package, "crab-cache-server") is None:
        violations.append("crab-cache-store: cache-server fixture dependency should stay dev-only")

    checks = [
        check_tree_packages(
            root,
            cargo,
            "crab-cache-store no-default build excludes cache server runtime",
            [
                "-p",
                "crab-cache-store",
                "--no-default-features",
                "--edges",
                "normal",
                "--depth",
                "3",
            ],
            forbidden={"crab-cache-server"},
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-cache-store/remote-client keeps cache server runtime out of production",
            [
                "-p",
                "crab-cache-store",
                "--no-default-features",
                "--features",
                "crab-cache-store/remote-client",
                "--edges",
                "normal",
                "--depth",
                "3",
            ],
            required={"reqwest"},
            forbidden={"crab-cache-server"},
        ),
    ]

    if not violations:
        print("ok: crab-cache-store keeps cache/storage Adapter costs explicit")
        return all(checks)

    print("error: crab-cache-store budget drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_metadata_feature_budget(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-metadata")
    violations = check_feature_definitions(
        package,
        {
            "default": [],
            "file-index-reader": [
                "dep:futures-util",
                "dep:object_store",
                "dep:slatedb",
                "dep:tokio",
                "storage",
            ],
            "local-index": ["dep:rusqlite"],
            "remote-index": [
                "dep:futures-util",
                "dep:object_store",
                "dep:slatedb",
                "storage",
            ],
            "storage": [
                "dep:base64",
                "dep:crab-storage",
                "dep:futures-util",
                "dep:object_store",
            ],
        },
    )

    for name in (
        "base64",
        "crab-storage",
        "futures-util",
        "object_store",
        "rusqlite",
        "slatedb",
        "tokio",
    ):
        dependency = normal_dependency(package, name)
        if dependency is None or not dependency["optional"]:
            violations.append(f"crab-metadata: {name} must stay optional")

    checks = [
        check_tree_packages(
            root,
            cargo,
            "default crab-metadata excludes persistence and SlateDB runtimes",
            ["-p", "crab-metadata", "--edges", "normal", "--depth", "2"],
            forbidden=METADATA_DEFAULT_FORBIDDEN_PACKAGES,
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-metadata/local-index exposes only SQLite index cost",
            [
                "-p",
                "crab-metadata",
                "--features",
                "local-index",
                "--edges",
                "normal",
                "--depth",
                "1",
            ],
            required={"rusqlite"},
            forbidden={"crab-storage", "object_store", "slatedb"},
        ),
    ]

    if not violations:
        print("ok: crab-metadata runtime costs are feature-gated")
        return all(checks)

    print("error: crab-metadata feature budget drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-auth")
    violations: list[str] = []

    for name in ("crab-coordination", "crab-types", "serde", "serde_json", "thiserror", "tracing"):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-auth: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-auth: {name} must stay non-optional")

    coordination = normal_dependency(package, "crab-coordination")
    if coordination is not None and coordination["features"]:
        violations.append("crab-auth: crab-coordination dependency must stay payload-only")

    for dependency in package["dependencies"]:
        name = dependency["name"]
        if name in AUTH_MODULE_DIRECT_FORBIDDEN_PACKAGES:
            kind = dependency_kind(dependency)
            violations.append(f"crab-auth: {kind}-depends on forbidden package {name}")

    for scan_path in AUTH_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in AUTH_MODULE_SOURCE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-auth stays client/shared auth without server or storage policy")
        return True

    print("error: crab-auth client/shared scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_default_budget(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-auth")
    violations = check_feature_definitions(package, {"default": []})
    for name in ("crab-storage", "object_store"):
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-auth: {name} must not be a normal dependency")

    tree_ok = check_tree_packages(
        root,
        cargo,
        "default crab-auth excludes storage, HTTP, and server runtime",
        ["-p", "crab-auth", "--edges", "normal", "--depth", "2"],
        forbidden=AUTH_DEFAULT_FORBIDDEN_PACKAGES,
    )

    if not violations:
        print("ok: crab-auth default stays client/shared and storage-free")
        return tree_ok

    print("error: crab-auth default budget drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_client_feature_budget(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-auth")
    violations = check_feature_definitions(package, AUTH_CLIENT_FEATURE_DEFINITIONS)

    for name in ("reqwest", "sha2", "tokio"):
        dependency = normal_dependency(package, name)
        if dependency is None or not dependency["optional"]:
            violations.append(f"crab-auth: {name} must stay optional")

    for name in AUTH_CLIENT_FEATURE_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-auth: {name} must not be a normal dependency")

    checks = [
        check_tree_packages(
            root,
            cargo,
            f"crab-auth/{feature_name} keeps server and storage runtime out",
            [
                "-p",
                "crab-auth",
                "--features",
                feature_name,
                "--edges",
                "normal",
                "--depth",
                "2",
            ],
            required=required,
            forbidden=AUTH_CLIENT_FEATURE_FORBIDDEN_PACKAGES,
        )
        for feature_name, required in sorted(AUTH_CLIENT_FEATURE_REQUIRED_PACKAGES.items())
    ]

    if not violations:
        print("ok: crab-auth client feature costs are explicit")
        return all(checks)

    print("error: crab-auth client feature budget drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_xet_owner_imports(metadata: dict) -> bool:
    roots = package_roots(metadata)
    violations: list[str] = []

    for name, root in sorted(roots.items()):
        if name == XET_OWNER_PACKAGE:
            continue
        candidates = [root / "Cargo.toml"]
        candidates.extend(path for path in root.rglob("*.rs") if "target" not in path.parts)
        for path in candidates:
            if not path.is_file():
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in XET_FORBIDDEN_PATTERNS):
                    violations.append(f"{path}:{index}: {line.strip()}")

    if not violations:
        print("ok: direct xet-core-structures imports stay inside crab-xet")
        return True

    print("error: direct xet-core-structures imports escaped crab-xet:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_storage_provider_identity_helpers(root: Path) -> bool:
    violations: list[str] = []
    config_path = root / "crab/src/core/config.rs"
    config_text = config_path.read_text(encoding="utf-8")

    for helper in STORAGE_PROVIDER_REQUIRED_HELPERS:
        if helper not in config_text:
            violations.append(f"crab/src/core/config.rs: missing StorageProvider helper {helper}")

    for scan_path in STORAGE_PROVIDER_HELPER_SCAN_PATHS:
        path = root / scan_path
        text = path.read_text(encoding="utf-8")
        production_text = text.partition("\n#[cfg(test)]")[0]
        for index, line in enumerate(production_text.splitlines(), start=1):
            if any(pattern in line for pattern in STORAGE_PROVIDER_DIRECT_VARIANT_PATTERNS):
                violations.append(f"{scan_path}:{index}: {line.strip()}")

    if not violations:
        print("ok: CLI StorageProvider identity conversions stay helper-owned")
        return True

    print("error: StorageProvider concrete identity leaked outside helpers:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_cache_server_origin_construction(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in CACHE_SERVER_ORIGIN_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in CACHE_SERVER_ORIGIN_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: cache-server origin construction stays behind crab-storage")
        return True

    print("error: cache-server regained storage-provider construction:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_cache_server_runtime_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-cache-server")
    violations: list[str] = []

    for dependency in package["dependencies"]:
        if dependency["kind"] is not None:
            continue
        name = dependency["name"]
        if (name == "crab" or name.startswith("crab-")) and (
            name not in CACHE_SERVER_ALLOWED_INTERNAL_NORMAL_PACKAGES
        ):
            violations.append(f"crab-cache-server: internal dependency {name} is not admitted")
        if name in {"xet-client", "xet-core-structures", "xet-data", "xet-runtime"}:
            violations.append(
                f"crab-cache-server: direct upstream Xet dependency {name} is not admitted"
            )

    object_store = normal_dependency(package, "object_store")
    if object_store is not None:
        if object_store["features"]:
            violations.append(
                f"crab-cache-server: object_store features must stay empty, got {sorted(object_store['features'])}"
            )
        if object_store["uses_default_features"]:
            violations.append("crab-cache-server: object_store default features must stay disabled")

    crab_cache = normal_dependency(package, "crab-cache")
    if crab_cache is not None and sorted(crab_cache["features"]) != ["active-probe"]:
        violations.append(
            f"crab-cache-server: crab-cache features must stay ['active-probe'], got {sorted(crab_cache['features'])}"
        )

    crab_xet = normal_dependency(package, "crab-xet")
    if crab_xet is not None and crab_xet["features"]:
        violations.append("crab-cache-server: crab-xet dependency must not enable chunker/client features")

    for scan_path in CACHE_SERVER_SCOPE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in CACHE_SERVER_SOURCE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: cache-server runtime stays server-owned")
        return True

    print("error: cache-server runtime scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_store_adapter_scope(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-auth-store")
    violations = check_feature_definitions(package, AUTH_STORE_FEATURE_DEFINITIONS)

    for name in ("crab-auth", "crab-storage", "thiserror"):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-auth-store: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-auth-store: {name} must stay non-optional")

    for name in AUTH_STORE_REFRESHING_REQUIRED_PACKAGES:
        dependency = normal_dependency(package, name)
        if dependency is None or not dependency["optional"]:
            violations.append(f"crab-auth-store: {name} must stay optional")

    object_store = normal_dependency(package, "object_store")
    if object_store is None:
        violations.append("crab-auth-store: missing optional object_store refresh dependency")
    else:
        if object_store["uses_default_features"]:
            violations.append("crab-auth-store: object_store must disable default features")
        if object_store["features"]:
            violations.append("crab-auth-store: object_store must stay featureless")

    for name in AUTH_STORE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-auth-store: {name} must not be a normal dependency")

    for scan_path in AUTH_STORE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in AUTH_STORE_SOURCE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    checks = [
        check_tree_packages(
            root,
            cargo,
            "default crab-auth-store stays a narrow auth/storage Adapter",
            ["-p", "crab-auth-store", "--edges", "normal", "--depth", "1"],
            required={"crab-auth", "crab-storage", "thiserror"},
            forbidden=AUTH_STORE_REFRESHING_REQUIRED_PACKAGES
            | AUTH_STORE_DIRECT_FORBIDDEN_PACKAGES,
        ),
        check_tree_packages(
            root,
            cargo,
            "crab-auth-store/refreshing-store exposes only refresh-wrapper cost",
            [
                "-p",
                "crab-auth-store",
                "--features",
                "refreshing-store",
                "--edges",
                "normal",
                "--depth",
                "1",
            ],
            required=AUTH_STORE_REFRESHING_REQUIRED_PACKAGES
            | {"crab-auth", "crab-storage", "thiserror"},
            forbidden=AUTH_STORE_DIRECT_FORBIDDEN_PACKAGES,
        ),
    ]

    if not violations:
        print("ok: crab-auth-store Adapter scope is narrow and feature-gated")
        return all(checks)

    print("error: crab-auth-store Adapter scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_server_runtime_scope(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in AUTH_SERVER_RUNTIME_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in AUTH_SERVER_RUNTIME_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: auth-server runtime stays separate from CLI and provider construction")
        return True

    print("error: auth-server runtime scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_server_pack_filename_validation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in AUTH_SERVER_PACK_FILENAME_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in AUTH_SERVER_PACK_FILENAME_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: validate pack object filenames through crab-git"
                )

    if not violations:
        print("ok: auth-server pack filename validation stays in crab-git")
        return True

    print("error: auth-server pack filename validation drifted out of crab-git:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_server_pack_entry_validation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in AUTH_SERVER_PACK_ENTRY_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in AUTH_SERVER_PACK_ENTRY_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: validate PackManifestEntry through crab-metadata"
                )

    if not violations:
        print("ok: auth-server pack manifest entry validation stays in crab-metadata")
        return True

    print(
        "error: auth-server pack manifest entry validation drifted out of crab-metadata:",
        file=sys.stderr,
    )
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_server_segment_index_validation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in AUTH_SERVER_SEGMENT_INDEX_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in AUTH_SERVER_SEGMENT_INDEX_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: validate segmented metadata through crab-metadata"
                )

    if not violations:
        print("ok: auth-server segmented metadata validation stays in crab-metadata")
        return True

    print(
        "error: auth-server segmented metadata validation drifted out of crab-metadata:",
        file=sys.stderr,
    )
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_server_manifest_payload_validation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in AUTH_SERVER_MANIFEST_PAYLOAD_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in AUTH_SERVER_MANIFEST_PAYLOAD_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: validate manifest payloads through crab-metadata"
                )

    if not violations:
        print("ok: auth-server manifest payload validation stays in crab-metadata")
        return True

    print(
        "error: auth-server manifest payload validation drifted out of crab-metadata:",
        file=sys.stderr,
    )
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_read_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-read")
    violations: list[str] = []

    object_store = normal_dependency(package, "object_store")
    if object_store is None:
        violations.append("crab-read: missing direct object_store Interface dependency")
    else:
        if object_store["uses_default_features"]:
            violations.append("crab-read: object_store must disable default features")
        if object_store["features"]:
            violations.append("crab-read: object_store must stay featureless")

    for name in READ_MODULE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-read: {name} must not be a normal dependency")

    for scan_path in READ_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in READ_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-read stays shared read orchestration without CLI/auth/server policy")
        return True

    print("error: crab-read shared read scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_read_replica_candidate_derivation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in READ_REPLICA_CANDIDATE_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        for pattern in READ_REPLICA_CANDIDATE_FORBIDDEN_PATTERNS:
            for match in pattern.finditer(text):
                line = text.count("\n", 0, match.start()) + 1
                violations.append(
                    f"{rel(root, path)}:{line}: derive ReplicaConfig read candidates through crab-read"
                )

    if not violations:
        print("ok: read replica candidates derive ReplicaConfig fields in crab-read")
        return True

    print("error: read replica candidate derivation drifted out of crab-read:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_read_probe_result_construction(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in READ_PROBE_RESULT_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in READ_PROBE_RESULT_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: construct read probe results through crab-read"
                )

    if not violations:
        print("ok: read probe result construction stays in crab-read")
        return True

    print("error: read probe result construction drifted out of crab-read:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_read_fetch_admission_delegation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in READ_FETCH_ADMISSION_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        if "crab_read::validate_fetch_wants_with_manifest" not in text:
            violations.append(
                f"{rel(root, path)}: remote-helper fetch admission must delegate to crab-read"
            )
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in READ_FETCH_ADMISSION_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: fetch admission policy belongs in crab-read"
                )

    if not violations:
        print("ok: remote-helper fetch admission delegates to crab-read")
        return True

    print("error: remote-helper fetch admission ownership drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_read_ref_advertisement_delegation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in READ_REF_ADVERTISEMENT_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        if "crab_read::manifest_ref_advertisement" not in text:
            violations.append(
                f"{rel(root, path)}: remote-helper manifest ref advertisement must delegate to crab-read"
            )
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in READ_REF_ADVERTISEMENT_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: manifest ref advertisement policy belongs in crab-read"
                )

    if not violations:
        print("ok: remote-helper manifest ref advertisement delegates to crab-read")
        return True

    print("error: remote-helper manifest ref advertisement ownership drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_git_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-git")
    violations: list[str] = []

    for name in GIT_MODULE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-git: {name} must not be a normal dependency")

    for relative_path in GIT_DELETED_CLI_OWNER_PATHS:
        if (root / relative_path).exists():
            violations.append(
                f"{relative_path}: Git contract implementation belongs in crab-git"
            )

    for scan_path in GIT_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in GIT_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-git stays low-dependency Git and URL shape logic")
        return True

    print("error: crab-git low-dependency scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_git_head_symref_delegation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path in GIT_HEAD_SYMREF_SCAN_PATHS:
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        if "crab_git::ref_resolve::resolve_head_symref" not in text:
            violations.append(
                f"{rel(root, path)}: local HEAD symref resolution must delegate to crab-git"
            )
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in GIT_HEAD_SYMREF_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: local HEAD symref lookup belongs in crab-git"
                )

    if not violations:
        print("ok: push local HEAD symref resolution delegates to crab-git")
        return True

    print("error: push local HEAD symref ownership drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_git_tag_peel_delegation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path, required in GIT_TAG_PEEL_REQUIRED_DELEGATIONS.items():
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        if required not in text:
            violations.append(f"{rel(root, path)}: annotated tag peeling must delegate to crab-git")
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in GIT_TAG_PEEL_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: annotated tag object peeling belongs in crab-git"
                )

    if not violations:
        print("ok: push annotated tag peeling delegates to crab-git")
        return True

    print("error: push annotated tag peeling ownership drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_storage_pack_layout_delegation(root: Path) -> bool:
    violations: list[str] = []

    for scan_path, required_patterns in STORAGE_PACK_LAYOUT_REQUIRED_DELEGATIONS.items():
        path = root / scan_path
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        for required in required_patterns:
            if required not in text:
                violations.append(
                    f"{rel(root, path)}: pack object layout must delegate to crab-storage"
                )
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in STORAGE_PACK_LAYOUT_FORBIDDEN_PATTERNS):
                violations.append(
                    f"{rel(root, path)}:{index}: pack object layout belongs in crab-storage"
                )

    if not violations:
        print("ok: pack object layout delegates to crab-storage")
        return True

    print("error: pack object layout ownership drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_diff_module_scope(root: Path, cargo: str, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-diff")
    violations: list[str] = []

    for name in ("crab-types", "crab-xet", "serde", "tracing"):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-diff: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-diff: {name} must stay non-optional")

    xet_dependency = normal_dependency(package, "crab-xet")
    if xet_dependency is not None and xet_dependency["features"]:
        violations.append("crab-diff: crab-xet dependency must not enable chunker/client features")

    for name in DIFF_MODULE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-diff: {name} must not be a normal dependency")

    for scan_path in DIFF_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in DIFF_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    tree_check = check_tree_packages(
        root,
        cargo,
        "crab-diff avoids Xet chunker/client runtime stacks",
        ["-p", "crab-diff", "--edges", "normal", "--depth", "3"],
        required={"crab-types", "crab-xet"},
        forbidden={"xet-client", "xet-data"},
    )

    if not violations and tree_check:
        print("ok: crab-diff stays pure comparison without runtime or policy deps")
        return True

    print("error: crab-diff pure diff scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_lfs_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-lfs")
    violations: list[str] = []

    for name in ("crab-git", "crab-storage", "object_store", "sha2", "thiserror"):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-lfs: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-lfs: {name} must stay non-optional")

    object_store = normal_dependency(package, "object_store")
    if object_store is not None:
        if object_store["uses_default_features"]:
            violations.append("crab-lfs: object_store must disable default features")
        if object_store["features"]:
            violations.append("crab-lfs: object_store must stay featureless")

    for name in LFS_MODULE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-lfs: {name} must not be a normal dependency")

    object_store_rs = root / "crates/crab-lfs/src/object_store.rs"
    if object_store_rs.is_file():
        object_store_text = object_store_rs.read_text(encoding="utf-8")
        if "pub fn object_path_for_prefix(" not in object_store_text:
            violations.append(
                "crates/crab-lfs/src/object_store.rs: LFS layout helper must stay crate-owned"
            )

    for scan_path in LFS_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in LFS_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-lfs stays LFS object storage without CLI/provider/server policy")
        return True

    print("error: crab-lfs object-storage scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_storage_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-storage")
    violations: list[str] = []

    for name in ("crab-types", "object_store", "thiserror", "tokio", "tracing"):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-storage: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-storage: {name} must stay non-optional")

    object_store = normal_dependency(package, "object_store")
    if object_store is not None:
        if object_store["uses_default_features"]:
            violations.append("crab-storage: object_store must disable default features")
        expected_features = {"aws", "gcp", "azure", "fs"}
        observed_features = set(object_store["features"])
        if observed_features != expected_features:
            violations.append(
                "crab-storage: object_store features must stay "
                f"{sorted(expected_features)}, got {sorted(observed_features)}"
            )

    for name in STORAGE_MODULE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-storage: {name} must not be a normal dependency")

    for scan_path in STORAGE_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in STORAGE_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-storage stays provider construction without cross-domain policy")
        return True

    print("error: crab-storage provider/storage scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_metadata_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-metadata")
    violations: list[str] = []

    for name in ("crab-xet", "serde", "serde_json", "thiserror", "tracing"):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-metadata: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-metadata: {name} must stay non-optional")

    xet_dependency = normal_dependency(package, "crab-xet")
    if xet_dependency is not None and xet_dependency["features"]:
        violations.append("crab-metadata: crab-xet dependency must not enable chunker/client features")

    for name in (
        "crab-storage",
        "futures-util",
        "object_store",
        "rusqlite",
        "slatedb",
        "tokio",
    ):
        dependency = normal_dependency(package, name)
        if dependency is None or not dependency["optional"]:
            violations.append(f"crab-metadata: {name} must stay optional")

    object_store = normal_dependency(package, "object_store")
    if object_store is not None:
        if object_store["uses_default_features"]:
            violations.append("crab-metadata: object_store must disable default features")
        if object_store["features"]:
            violations.append("crab-metadata: object_store must stay featureless")

    for name in METADATA_MODULE_DIRECT_FORBIDDEN_PACKAGES:
        if normal_dependency(package, name) is not None:
            violations.append(f"crab-metadata: {name} must not be a normal dependency")

    for scan_path in METADATA_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in METADATA_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-metadata stays metadata-owned without CLI or side-domain policy")
        return True

    print("error: crab-metadata metadata scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_deleted_metadata_reexport_adapters_removed(root: Path) -> bool:
    violations: list[str] = []

    for adapter_path in DELETED_METADATA_REEXPORT_ADAPTER_PATHS:
        if (root / adapter_path).exists():
            violations.append(f"{adapter_path}: remove CLI metadata re-export Adapter")

    metadata_mod_path = root / "crab/src/metadata/mod.rs"
    if metadata_mod_path.is_file():
        metadata_mod_text = metadata_mod_path.read_text(encoding="utf-8")
        if "pub mod file_index_lookup;" in metadata_mod_text:
            violations.append("crab/src/metadata/mod.rs: pub mod file_index_lookup;")

    for scan_path in DELETED_METADATA_REEXPORT_ADAPTER_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix == ".rs" and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            if rel(root, candidate) in DELETED_METADATA_REEXPORT_ADAPTER_PATHS:
                continue
            text = candidate.read_text(encoding="utf-8")
            for index, line in enumerate(text.splitlines(), start=1):
                if any(
                    pattern in line
                    for pattern in DELETED_METADATA_REEXPORT_ADAPTER_FORBIDDEN_PATTERNS
                ):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: deleted CLI metadata re-export Adapters stay removed")
        return True

    print("error: deleted CLI metadata re-export Adapter path regressed:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_workflow_module_scope(root: Path, metadata: dict) -> bool:
    package = package_by_name(metadata, "crab-workflow")
    violations: list[str] = []

    for name in ("crab-types", "serde", "serde_json", "serde_yaml", "thiserror"):
        dependency = normal_dependency(package, name)
        if dependency is None:
            violations.append(f"crab-workflow: missing normal {name} dependency")
        elif dependency["optional"]:
            violations.append(f"crab-workflow: {name} must stay non-optional")

    for dependency in package["dependencies"]:
        if dependency["kind"] is not None:
            continue
        name = dependency["name"]
        if name not in WORKFLOW_MODULE_ALLOWED_NORMAL_PACKAGES:
            violations.append(f"crab-workflow: normal dependency {name} is not admitted")

    for scan_path in WORKFLOW_MODULE_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix in {".rs", ".toml"} and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            try:
                text = candidate.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            for index, line in enumerate(text.splitlines(), start=1):
                if any(pattern in line for pattern in WORKFLOW_MODULE_FORBIDDEN_PATTERNS):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-workflow stays workflow contracts without runtime transport policy")
        return True

    print("error: crab-workflow workflow-contract scope drifted:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_deleted_workflow_reexport_adapters_removed(root: Path) -> bool:
    violations: list[str] = []

    for adapter_path in DELETED_WORKFLOW_REEXPORT_ADAPTER_PATHS:
        if (root / adapter_path).exists():
            violations.append(f"{adapter_path}: remove CLI workflow re-export Adapter")

    workflow_mod_path = root / "crab/src/workflow/mod.rs"
    if workflow_mod_path.is_file():
        workflow_mod_text = workflow_mod_path.read_text(encoding="utf-8")
        for module_name in (
            "template",
            "graph",
            "lockfile",
            "retry",
            "run_state",
            "state",
            "status",
            "yaml",
            "migrate_dvc",
        ):
            module_decl = f"pub mod {module_name};"
            if module_decl in workflow_mod_text:
                violations.append(f"crab/src/workflow/mod.rs: {module_decl}")

    for scan_path in DELETED_WORKFLOW_REEXPORT_ADAPTER_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix == ".rs" and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            if rel(root, candidate) in DELETED_WORKFLOW_REEXPORT_ADAPTER_PATHS:
                continue
            text = candidate.read_text(encoding="utf-8")
            for index, line in enumerate(text.splitlines(), start=1):
                if any(
                    pattern in line
                    for pattern in DELETED_WORKFLOW_REEXPORT_ADAPTER_FORBIDDEN_PATTERNS
                ):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: deleted CLI workflow re-export Adapters stay removed")
        return True

    print("error: deleted CLI workflow re-export Adapter path regressed:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_deleted_auth_provider_reexport_adapters_removed(root: Path) -> bool:
    violations: list[str] = []

    for adapter_path in DELETED_AUTH_PROVIDER_REEXPORT_ADAPTER_PATHS:
        if (root / adapter_path).exists():
            violations.append(f"{adapter_path}: remove CLI auth provider re-export Adapter")

    for scan_path in DELETED_AUTH_PROVIDER_REEXPORT_ADAPTER_SCAN_PATHS:
        path = root / scan_path
        if path.is_file():
            candidates = [path]
        elif path.is_dir():
            candidates = sorted(
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix == ".rs" and "target" not in candidate.parts
            )
        else:
            continue

        for candidate in candidates:
            if rel(root, candidate) in DELETED_AUTH_PROVIDER_REEXPORT_ADAPTER_PATHS:
                continue
            text = candidate.read_text(encoding="utf-8")
            for index, line in enumerate(text.splitlines(), start=1):
                if any(
                    pattern in line
                    for pattern in DELETED_AUTH_PROVIDER_REEXPORT_ADAPTER_FORBIDDEN_PATTERNS
                ):
                    violations.append(f"{rel(root, candidate)}:{index}: {line.strip()}")

    if not violations:
        print("ok: deleted CLI auth provider re-export Adapters stay removed")
        return True

    print("error: deleted CLI auth provider re-export Adapter path regressed:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def check_auth_provider_dispatch_owner(root: Path) -> bool:
    violations: list[str] = []

    for relative_path, required_patterns in AUTH_PROVIDER_DISPATCH_OWNER_REQUIRED_PATTERNS.items():
        path = root / relative_path
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        if not text:
            violations.append(f"{relative_path}: missing auth provider dispatch owner source")
            continue
        for pattern in required_patterns:
            if pattern not in text:
                violations.append(f"{relative_path}: missing {pattern!r}")

    for relative_path, required_patterns in AUTH_PROVIDER_DISPATCH_CALLER_REQUIRED_PATTERNS.items():
        path = root / relative_path
        text = path.read_text(encoding="utf-8") if path.exists() else ""
        if not text:
            violations.append(f"{relative_path}: missing auth provider dispatch caller source")
            continue
        for pattern in required_patterns:
            if pattern not in text:
                violations.append(f"{relative_path}: missing {pattern!r}")
        for index, line in enumerate(text.splitlines(), start=1):
            if any(pattern in line for pattern in AUTH_PROVIDER_DISPATCH_CALLER_FORBIDDEN_PATTERNS):
                violations.append(f"{relative_path}:{index}: {line.strip()}")

    if not violations:
        print("ok: auth provider dispatch stays behind crab-auth")
        return True

    print("error: auth provider dispatch escaped crab-auth:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate architecture guardrails for the Crab multi-crate split.",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="Cargo executable to use for dependency checks.",
    )
    args = parser.parse_args()

    root = repo_root()
    try:
        metadata = cargo_metadata(root, args.cargo)
    except RuntimeError as error:
        print("error: cargo metadata failed:", file=sys.stderr)
        print(error, file=sys.stderr)
        return 1

    checks = [
        check_object_store_features(metadata),
        check_package_release_policy(metadata),
        check_server_fixture_dependencies(metadata),
        check_workspace_dependency_policy(metadata),
        check_workspace_dependency_sources(root, metadata),
        check_workspace_xet_dependency_sources(root, metadata),
        check_workspace_gitoxide_dependency_sources(root, metadata),
        check_workspace_provider_sdk_dependency_sources(root, metadata),
        check_workspace_third_party_dependency_sources(root, metadata),
        check_reverse_dependency_is_self(root, args.cargo, "crab"),
        check_reverse_dependency_is_self(root, args.cargo, "crab-auth-server"),
        check_reverse_dependency_is_self(root, args.cargo, "crab-cache-server"),
        check_xet_owner_imports(metadata),
        check_storage_provider_identity_helpers(root),
        check_cache_server_origin_construction(root),
        check_cache_server_runtime_scope(root, metadata),
        check_auth_store_adapter_scope(root, args.cargo, metadata),
        check_auth_server_runtime_scope(root),
        check_auth_server_pack_filename_validation(root),
        check_auth_server_pack_entry_validation(root),
        check_auth_server_segment_index_validation(root),
        check_auth_server_manifest_payload_validation(root),
        check_read_module_scope(root, metadata),
        check_read_replica_candidate_derivation(root),
        check_read_probe_result_construction(root),
        check_read_fetch_admission_delegation(root),
        check_read_ref_advertisement_delegation(root),
        check_git_module_scope(root, metadata),
        check_git_head_symref_delegation(root),
        check_git_tag_peel_delegation(root),
        check_storage_pack_layout_delegation(root),
        check_diff_module_scope(root, args.cargo, metadata),
        check_lfs_module_scope(root, metadata),
        check_storage_module_scope(root, metadata),
        check_metadata_module_scope(root, metadata),
        check_deleted_metadata_reexport_adapters_removed(root),
        check_workflow_module_scope(root, metadata),
        check_deleted_workflow_reexport_adapters_removed(root),
        check_xet_module_scope(root, metadata),
        check_xet_feature_budget(root, args.cargo, metadata),
        check_coordination_module_scope(root, metadata),
        check_coordination_feature_budget(root, args.cargo, metadata),
        check_cache_module_scope(root, metadata),
        check_cache_feature_budget(root, args.cargo, metadata),
        check_cache_store_module_scope(root, metadata),
        check_cache_store_budget(root, args.cargo, metadata),
        check_metadata_feature_budget(root, args.cargo, metadata),
        check_auth_module_scope(root, metadata),
        check_deleted_auth_provider_reexport_adapters_removed(root),
        check_auth_provider_dispatch_owner(root),
        check_auth_default_budget(root, args.cargo, metadata),
        check_auth_client_feature_budget(root, args.cargo, metadata),
    ]
    return 0 if all(checks) else 1


if __name__ == "__main__":
    raise SystemExit(main())
