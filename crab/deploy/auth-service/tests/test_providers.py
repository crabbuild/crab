"""Tests for cloud credential providers."""

from datetime import datetime, timedelta, timezone
import json
import sys
import types

import pytest

from src.providers import (
    _providers,
    get_provider,
    normalize_permissions,
    validate_protected_push_permissions,
)
from src.providers.azure import (
    AzureProvider,
    _directory_depth,
    _parse_repo_url as _parse_azure_repo_url,
    _sas_permission_string,
)
from src.providers.aws import (
    AwsProvider,
    _parse_repo_url,
    _session_name,
    _build_session_policy,
)
from src.providers.gcp import GcpProvider
from src.providers.gcp import (
    _build_access_boundary_rules,
    _gcs_prefix_expression,
)
from src.providers.s3 import S3Provider


class TestProviderPermissionValidation:
    def test_normalize_permissions_strips_and_lowercases_tokens(self):
        assert normalize_permissions([" read ", "Immutable-Write", ""]) == [
            "read",
            "immutable-write",
        ]

    def test_push_permissions_reject_case_insensitive_canonical_write(self):
        with pytest.raises(ValueError, match="canonical write"):
            validate_protected_push_permissions(
                "PUSH",
                ["Read", "Write"],
                "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )

    def test_push_permissions_reject_unknown_permission(self):
        with pytest.raises(ValueError, match="unsupported permission"):
            validate_protected_push_permissions(
                "push",
                ["read", "immutable-write", "admin"],
                "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )

    def test_push_permissions_reject_read_with_immutable_write(self):
        with pytest.raises(ValueError, match="read permission"):
            validate_protected_push_permissions(
                "push",
                ["read", "immutable-write"],
                "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )


class TestAwsRepoUrlParsing:
    """Test S3 bucket/prefix extraction from repo URLs."""

    def test_crab_scheme(self):
        bucket, prefix = _parse_repo_url("crab://my-bucket/team/repo")
        assert bucket == "my-bucket"
        assert prefix == "team/repo"

    def test_s3_scheme(self):
        bucket, prefix = _parse_repo_url("s3://my-bucket/path/to/repo")
        assert bucket == "my-bucket"
        assert prefix == "path/to/repo"

    def test_bucket_only(self):
        with pytest.raises(ValueError, match="repo prefix"):
            _parse_repo_url("crab://my-bucket")

    def test_deep_path(self):
        bucket, prefix = _parse_repo_url("crab://bucket/a/b/c/d/e")
        assert bucket == "bucket"
        assert prefix == "a/b/c/d/e"

    def test_whitespace_stripped(self):
        bucket, prefix = _parse_repo_url("  crab://bucket/repo  ")
        assert bucket == "bucket"
        assert prefix == "repo"


class TestSessionName:
    """Test STS session name generation."""

    def test_deterministic(self):
        assert _session_name("alice@example.com") == _session_name("alice@example.com")

    def test_different_identities_differ(self):
        assert _session_name("alice@example.com") != _session_name("bob@example.com")

    def test_starts_with_crab(self):
        name = _session_name("alice@example.com")
        assert name.startswith("crab-")

    def test_valid_sts_characters(self):
        """STS session names must match [a-zA-Z0-9+=,.@_-]."""
        name = _session_name("user+special@example.com")
        import re
        assert re.match(r'^[a-zA-Z0-9+=,.@_-]+$', name)

    def test_length_within_sts_limits(self):
        """STS session names must be 2-64 characters."""
        name = _session_name("a" * 200 + "@example.com")
        assert 2 <= len(name) <= 64


class TestSessionPolicy:
    """Test IAM inline session policy generation."""

    def test_read_only_policy(self):
        policy = _build_session_policy("my-bucket", "team/repo", ["read"])
        statements = policy["Statement"]

        # Should have object-level read + ListBucket.
        actions = []
        for stmt in statements:
            action = stmt["Action"]
            if isinstance(action, list):
                actions.extend(action)
            else:
                actions.append(action)

        assert "s3:GetObject" in actions
        assert "s3:ListBucket" in actions
        assert "s3:PutObject" not in actions
        assert "s3:DeleteObject" not in actions

    def test_read_write_policy(self):
        policy = _build_session_policy("my-bucket", "team/repo", ["read", "write"])
        statements = policy["Statement"]

        actions = []
        for stmt in statements:
            action = stmt["Action"]
            if isinstance(action, list):
                actions.extend(action)
            else:
                actions.append(action)

        assert "s3:GetObject" in actions
        assert "s3:PutObject" in actions
        assert "s3:DeleteObject" in actions

    def test_immutable_write_policy_rejects_read_permission(self):
        with pytest.raises(ValueError, match="read permission"):
            _build_session_policy(
                "my-bucket",
                "team/repo",
                ["read", "immutable-write"],
                "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )

    def test_immutable_write_only_policy_has_no_canonical_reads(self):
        policy = _build_session_policy(
            "my-bucket",
            "team/repo",
            ["immutable-write"],
            "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        statements = policy["Statement"]
        actions = []
        for stmt in statements:
            action = stmt["Action"]
            actions.extend(action if isinstance(action, list) else [action])

        assert "s3:GetObject" not in actions
        assert "s3:ListBucket" not in actions
        assert "s3:PutObject" in actions
        assert statements == [
            {
                "Effect": "Allow",
                "Action": [
                    "s3:PutObject",
                    "s3:AbortMultipartUpload",
                    "s3:ListMultipartUploadParts",
                ],
                "Resource": (
                    "arn:aws:s3:::my-bucket/team/repo/staging/"
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/*"
                ),
            }
        ]

    def test_immutable_write_policy_normalizes_permissions(self):
        policy = _build_session_policy(
            "my-bucket",
            "team/repo",
            [" Immutable-Write "],
            "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        write_stmt = next(
            s for s in policy["Statement"]
            if "s3:PutObject" in s["Action"]
        )

        assert write_stmt["Resource"] == (
            "arn:aws:s3:::my-bucket/team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/*"
        )

    def test_immutable_write_requires_exact_upload_prefix(self):
        with pytest.raises(ValueError, match="upload_prefix"):
            _build_session_policy(
                "my-bucket", "team/repo", ["immutable-write"]
            )

    def test_immutable_write_upload_prefix_must_be_repo_staging_prefix(self):
        with pytest.raises(ValueError, match="repo staging"):
            _build_session_policy(
                "my-bucket",
                "team/repo",
                ["immutable-write"],
                "team/other/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )

    def test_immutable_write_upload_prefix_must_use_generated_push_id(self):
        with pytest.raises(ValueError, match="push_id"):
            _build_session_policy(
                "my-bucket",
                "team/repo",
                ["immutable-write"],
                "team/repo/staging/push-123",
            )

    def test_repo_prefix_with_iam_wildcard_is_rejected(self):
        with pytest.raises(ValueError, match="unsupported characters"):
            _build_session_policy(
                "my-bucket",
                "team/*",
                ["read"],
            )

    def test_policy_scoped_to_prefix(self):
        policy = _build_session_policy("my-bucket", "team/repo", ["read"])
        statements = policy["Statement"]

        # Object-level statement should reference the prefix.
        object_stmt = next(
            s for s in statements if isinstance(s["Action"], list)
        )
        assert "arn:aws:s3:::my-bucket/team/repo/*" == object_stmt["Resource"]

    def test_read_policy_for_acl_view_excludes_source_repo_and_global_prefix(self):
        view_prefix = "team/repo/acl-views/v1/" + "a" * 64 + "/7-deadbeef"
        policy = _build_session_policy("my-bucket", view_prefix, ["read"])
        statements = policy["Statement"]
        object_stmt = next(s for s in statements if isinstance(s["Action"], list))
        list_stmt = next(s for s in statements if s.get("Action") == "s3:ListBucket")

        assert object_stmt["Resource"] == f"arn:aws:s3:::my-bucket/{view_prefix}/*"
        assert object_stmt["Resource"] != "arn:aws:s3:::my-bucket/team/repo/*"
        assert ".crab/xorbs" not in object_stmt["Resource"]
        assert list_stmt["Condition"]["StringLike"]["s3:prefix"] == [
            f"{view_prefix}/*",
            view_prefix,
        ]

    def test_list_bucket_has_prefix_condition(self):
        policy = _build_session_policy("my-bucket", "team/repo", ["read"])
        statements = policy["Statement"]

        list_stmt = next(
            s for s in statements if s.get("Action") == "s3:ListBucket"
        )
        condition = list_stmt["Condition"]["StringLike"]["s3:prefix"]
        assert "team/repo/*" in condition

    def test_policy_is_valid_json(self):
        policy = _build_session_policy("bucket", "prefix", ["read", "write"])
        # Should serialize without error.
        json_str = json.dumps(policy)
        # Should parse back.
        parsed = json.loads(json_str)
        assert parsed["Version"] == "2012-10-17"

    def test_prefix_with_leading_trailing_slashes_normalized(self):
        policy = _build_session_policy("bucket", "/team/repo/", ["read"])
        statements = policy["Statement"]
        object_stmt = next(
            s for s in statements if isinstance(s["Action"], list)
        )
        # Should not have double slashes.
        assert "//" not in object_stmt["Resource"]

    def test_empty_prefix_is_rejected_to_avoid_bucket_wide_credentials(self):
        with pytest.raises(ValueError, match="repo prefix"):
            _build_session_policy("bucket", "", ["read", "write"])


class TestAwsProviderIntegration:
    """Integration test using moto to mock STS."""

    @pytest.fixture(autouse=True)
    def setup_env(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_AWS_ROLE_ARN", "arn:aws:iam::123456789012:role/test")
        monkeypatch.setenv("CRAB_AUTH_AWS_REGION", "us-east-1")
        monkeypatch.setenv("CRAB_AUTH_SESSION_DURATION", "3600")
        monkeypatch.setenv("AWS_ACCESS_KEY_ID", "testing")
        monkeypatch.setenv("AWS_SECRET_ACCESS_KEY", "testing")
        monkeypatch.setenv("AWS_SECURITY_TOKEN", "testing")
        monkeypatch.setenv("AWS_DEFAULT_REGION", "us-east-1")

    @pytest.mark.asyncio
    async def test_generate_returns_credentials(self):
        """Test credential generation with mocked STS."""
        from moto import mock_aws

        with mock_aws():
            provider = AwsProvider()
            result = await provider.generate(
                identity="alice@example.com",
                repo_url="crab://my-bucket/team/repo",
                operation="gc",
                permissions=["read", "write"],
            )

            assert "access_key_id" in result.credentials
            assert "secret_access_key" in result.credentials
            assert "session_token" in result.credentials
            assert "region" in result.credentials
            assert result.expires_at  # Non-empty ISO timestamp

    @pytest.mark.asyncio
    async def test_generate_passes_configured_external_id(self):
        calls = []

        class FakeSts:
            def assume_role(self, **kwargs):
                calls.append(kwargs)
                return {
                    "Credentials": {
                        "AccessKeyId": "AKIA",
                        "SecretAccessKey": "secret",
                        "SessionToken": "token",
                        "Expiration": datetime.now(timezone.utc) + timedelta(hours=1),
                    }
                }

        provider = AwsProvider.__new__(AwsProvider)
        provider._role_arn = "arn:aws:iam::123456789012:role/test"
        provider._region = "us-east-1"
        provider._session_duration = 3600
        provider._external_id = "crab-auth"
        provider._dry_run = False
        provider._sts = FakeSts()

        await provider.generate(
            identity="alice@example.com",
            repo_url="crab://my-bucket/team/repo",
            operation="fetch",
            permissions=["read"],
        )

        assert calls[0]["ExternalId"] == "crab-auth"

    @pytest.mark.asyncio
    async def test_generate_omits_external_id_when_unset(self):
        calls = []

        class FakeSts:
            def assume_role(self, **kwargs):
                calls.append(kwargs)
                return {
                    "Credentials": {
                        "AccessKeyId": "AKIA",
                        "SecretAccessKey": "secret",
                        "SessionToken": "token",
                        "Expiration": datetime.now(timezone.utc) + timedelta(hours=1),
                    }
                }

        provider = AwsProvider.__new__(AwsProvider)
        provider._role_arn = "arn:aws:iam::123456789012:role/test"
        provider._region = "us-east-1"
        provider._session_duration = 3600
        provider._external_id = ""
        provider._dry_run = False
        provider._sts = FakeSts()

        await provider.generate(
            identity="alice@example.com",
            repo_url="crab://my-bucket/team/repo",
            operation="fetch",
            permissions=["read"],
        )

        assert "ExternalId" not in calls[0]

    @pytest.mark.asyncio
    async def test_push_read_write_permissions_fail_closed(self):
        provider = AwsProvider()
        with pytest.raises(ValueError, match="canonical write"):
            await provider.generate(
                identity="alice@example.com",
                repo_url="crab://my-bucket/team/repo",
                operation="push",
                permissions=["read", "write"],
            )

    def test_cleanup_staging_deletes_only_expired_staging_objects(self):
        old = datetime.now(timezone.utc) - timedelta(days=2)
        fresh = datetime.now(timezone.utc)

        class FakeS3:
            def __init__(self):
                self.list_requests = []
                self.deleted = []

            def list_objects_v2(self, **kwargs):
                self.list_requests.append(kwargs)
                return {
                    "IsTruncated": False,
                    "Contents": [
                        {"Key": "team/repo/staging/push-old/objects/x", "LastModified": old},
                        {"Key": "team/repo/staging/push-new/objects/x", "LastModified": fresh},
                    ],
                }

            def delete_objects(self, **kwargs):
                self.deleted.extend(kwargs["Delete"]["Objects"])

        provider = AwsProvider.__new__(AwsProvider)
        provider._dry_run = False
        provider._s3 = FakeS3()

        deleted = provider.cleanup_staging(
            repo_url="crab://my-bucket/team/repo",
            older_than_seconds=86400,
        )

        assert deleted == 1
        assert provider._s3.list_requests == [
            {"Bucket": "my-bucket", "Prefix": "team/repo/staging/"}
        ]
        assert provider._s3.deleted == [
            {"Key": "team/repo/staging/push-old/objects/x"}
        ]


class TestS3ProviderIntegration:
    """Tests for S3-compatible static credential provider wiring."""

    def test_registry_returns_s3_provider(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_S3_ACCESS_KEY_ID", "crab")
        monkeypatch.setenv("CRAB_AUTH_S3_SECRET_ACCESS_KEY", "crab")
        _providers.clear()

        provider = get_provider("s3")

        assert isinstance(provider, S3Provider)

    @pytest.mark.asyncio
    async def test_generate_returns_static_credentials_without_session_token(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_S3_ACCESS_KEY_ID", "crab")
        monkeypatch.setenv("CRAB_AUTH_S3_SECRET_ACCESS_KEY", "crab")
        monkeypatch.setenv("CRAB_AUTH_S3_REGION", "us-west-2")
        monkeypatch.delenv("CRAB_AUTH_S3_SESSION_TOKEN", raising=False)
        monkeypatch.setenv("AWS_SESSION_TOKEN", "ambient-token")

        result = await S3Provider().generate(
            identity="alice@example.com",
            repo_url="crab://my-bucket/team/repo",
            operation="clone",
            permissions=["read"],
        )

        assert result.credentials == {
            "access_key_id": "crab",
            "secret_access_key": "crab",
            "region": "us-west-2",
        }

    @pytest.mark.asyncio
    async def test_generate_accepts_explicit_s3_session_token(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_S3_ACCESS_KEY_ID", "crab")
        monkeypatch.setenv("CRAB_AUTH_S3_SECRET_ACCESS_KEY", "crab")
        monkeypatch.setenv("CRAB_AUTH_S3_SESSION_TOKEN", "explicit-token")

        result = await S3Provider().generate(
            identity="alice@example.com",
            repo_url="s3://my-bucket/team/repo",
            operation="clone",
            permissions=["read"],
        )

        assert result.credentials["session_token"] == "explicit-token"

    @pytest.mark.asyncio
    async def test_generate_requires_static_credentials(self, monkeypatch):
        monkeypatch.delenv("CRAB_AUTH_S3_ACCESS_KEY_ID", raising=False)
        monkeypatch.delenv("AWS_ACCESS_KEY_ID", raising=False)
        monkeypatch.setenv("CRAB_AUTH_S3_SECRET_ACCESS_KEY", "crab")

        with pytest.raises(ValueError, match="ACCESS_KEY_ID"):
            await S3Provider().generate(
                identity="alice@example.com",
                repo_url="crab://my-bucket/team/repo",
                operation="clone",
                permissions=["read"],
            )

    @pytest.mark.asyncio
    async def test_push_read_write_permissions_fail_closed(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_S3_ACCESS_KEY_ID", "crab")
        monkeypatch.setenv("CRAB_AUTH_S3_SECRET_ACCESS_KEY", "crab")

        with pytest.raises(ValueError, match="canonical write"):
            await S3Provider().generate(
                identity="alice@example.com",
                repo_url="crab://my-bucket/team/repo",
                operation="push",
                permissions=["read", "write"],
            )

    @pytest.mark.asyncio
    async def test_immutable_write_upload_prefix_must_match_repo(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_S3_ACCESS_KEY_ID", "crab")
        monkeypatch.setenv("CRAB_AUTH_S3_SECRET_ACCESS_KEY", "crab")

        with pytest.raises(ValueError, match="repo staging"):
            await S3Provider().generate(
                identity="alice@example.com",
                repo_url="crab://my-bucket/team/repo",
                operation="push",
                permissions=["immutable-write"],
                upload_prefix="other/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )


class TestAzureProviderScoping:
    """Test Azure prefix-scope safety behavior."""

    def test_parse_repo_url(self):
        container, prefix = _parse_azure_repo_url("crab://container/team/repo")
        assert container == "container"
        assert prefix == "team/repo"

    def test_directory_permission_string_read_only(self):
        assert _sas_permission_string(["read"]) == "rl"

    def test_directory_permission_string_read_write(self):
        assert _sas_permission_string(["read", "write"]) == "racwdl"

    def test_directory_permission_string_immutable_write_is_staging_write_only(self):
        assert _sas_permission_string(["immutable-write"]) == "acw"

    def test_directory_depth_matches_azure_sdd(self):
        assert _directory_depth("") == 0
        assert _directory_depth("team") == 1
        assert _directory_depth("/team/repo/") == 2

    @pytest.mark.asyncio
    async def test_container_root_repo_fails_closed(self):
        provider = AzureProvider()
        with pytest.raises(ValueError, match="repo prefix"):
            await provider.generate(
                identity="alice@example.com",
                repo_url="crab://container",
                operation="fetch",
                permissions=["read"],
            )

    @pytest.mark.asyncio
    async def test_prefix_generates_directory_scoped_sas(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_AZURE_STORAGE_ACCOUNT", "acct")

        calls = {}

        class DefaultAzureCredential:
            pass

        class BlobServiceClient:
            def __init__(self, account_url, credential):
                calls["account_url"] = account_url
                calls["credential"] = credential

            def get_user_delegation_key(self, key_start_time, key_expiry_time):
                calls["delegation_window"] = (key_start_time, key_expiry_time)
                return object()

        class ContainerSasPermissions:
            def __init__(self, **kwargs):
                calls["container_permissions"] = kwargs

        class UserDelegationKey:
            pass

        def generate_blob_sas(**kwargs):
            calls["blob_sas"] = kwargs
            return "sig"

        def generate_container_sas(**kwargs):
            calls["container_sas"] = kwargs
            return "container-sig"

        azure_mod = types.ModuleType("azure")
        identity_mod = types.ModuleType("azure.identity")
        storage_mod = types.ModuleType("azure.storage")
        blob_mod = types.ModuleType("azure.storage.blob")

        identity_mod.DefaultAzureCredential = DefaultAzureCredential
        blob_mod.BlobServiceClient = BlobServiceClient
        blob_mod.ContainerSasPermissions = ContainerSasPermissions
        blob_mod.UserDelegationKey = UserDelegationKey
        blob_mod.generate_blob_sas = generate_blob_sas
        blob_mod.generate_container_sas = generate_container_sas
        azure_mod.identity = identity_mod
        azure_mod.storage = storage_mod
        storage_mod.blob = blob_mod

        monkeypatch.setitem(sys.modules, "azure", azure_mod)
        monkeypatch.setitem(sys.modules, "azure.identity", identity_mod)
        monkeypatch.setitem(sys.modules, "azure.storage", storage_mod)
        monkeypatch.setitem(sys.modules, "azure.storage.blob", blob_mod)

        provider = AzureProvider()
        result = await provider.generate(
            identity="alice@example.com",
            repo_url="crab://container/team/repo",
            operation="gc",
            permissions=["read", "write"],
        )

        assert result.credentials["sas_token"] == "sig"
        assert result.credentials["storage_account"] == "acct"
        assert calls["account_url"] == "https://acct.blob.core.windows.net"
        assert "container_sas" not in calls
        assert calls["blob_sas"]["blob_name"] == "team/repo"
        assert calls["blob_sas"]["permission"] == "racwdl"
        assert calls["blob_sas"]["is_directory"] is True
        assert calls["blob_sas"]["sdd"] == "2"

    @pytest.mark.asyncio
    async def test_push_read_write_permissions_fail_closed(self):
        provider = AzureProvider()
        with pytest.raises(ValueError, match="canonical write"):
            await provider.generate(
                identity="alice@example.com",
                repo_url="crab://container/team/repo",
                operation="push",
                permissions=["read", "write"],
            )

    @pytest.mark.asyncio
    async def test_immutable_write_rejects_read_sas_tokens(self):
        provider = AzureProvider()
        with pytest.raises(ValueError, match="read permission"):
            await provider.generate(
                identity="alice@example.com",
                repo_url="crab://container/team/repo",
                operation="push",
                permissions=["read", "immutable-write"],
                upload_prefix="team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )

    @pytest.mark.asyncio
    async def test_immutable_write_only_generates_no_read_sas_tokens(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_AZURE_STORAGE_ACCOUNT", "acct")

        calls = []

        class DefaultAzureCredential:
            pass

        class BlobServiceClient:
            def __init__(self, account_url, credential):
                pass

            def get_user_delegation_key(self, key_start_time, key_expiry_time):
                return object()

        class UserDelegationKey:
            pass

        def generate_blob_sas(**kwargs):
            calls.append(kwargs)
            return f"sig-{len(calls)}"

        azure_mod = types.ModuleType("azure")
        identity_mod = types.ModuleType("azure.identity")
        storage_mod = types.ModuleType("azure.storage")
        blob_mod = types.ModuleType("azure.storage.blob")

        identity_mod.DefaultAzureCredential = DefaultAzureCredential
        blob_mod.BlobServiceClient = BlobServiceClient
        blob_mod.UserDelegationKey = UserDelegationKey
        blob_mod.generate_blob_sas = generate_blob_sas
        azure_mod.identity = identity_mod
        azure_mod.storage = storage_mod
        storage_mod.blob = blob_mod

        monkeypatch.setitem(sys.modules, "azure", azure_mod)
        monkeypatch.setitem(sys.modules, "azure.identity", identity_mod)
        monkeypatch.setitem(sys.modules, "azure.storage", storage_mod)
        monkeypatch.setitem(sys.modules, "azure.storage.blob", blob_mod)

        provider = AzureProvider()
        result = await provider.generate(
            identity="alice@example.com",
            repo_url="crab://container/team/repo",
            operation="push",
            permissions=["immutable-write"],
            upload_prefix="team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )

        assert "read_sas_tokens" not in result.credentials
        assert result.credentials["storage_account"] == "acct"
        assert result.credentials["write_sas_token"] == "sig-1"
        assert result.credentials["write_prefix"] == (
            "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )
        assert len(calls) == 1
        assert calls[0]["blob_name"] == "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        assert calls[0]["permission"] == "acw"

    @pytest.mark.asyncio
    async def test_immutable_write_upload_prefix_must_be_repo_staging_prefix(
        self, monkeypatch
    ):
        provider = AzureProvider()
        with pytest.raises(ValueError, match="repo staging"):
            await provider.generate(
                identity="alice@example.com",
                repo_url="crab://container/team/repo",
                operation="push",
                permissions=["immutable-write"],
                upload_prefix="team/other/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )

    def test_cleanup_staging_deletes_only_expired_blobs(self, monkeypatch):
        monkeypatch.setenv("CRAB_AUTH_AZURE_STORAGE_ACCOUNT", "acct")
        now = datetime.now(timezone.utc)
        calls = {}

        class DefaultAzureCredential:
            pass

        class Blob:
            def __init__(self, name, last_modified):
                self.name = name
                self.last_modified = last_modified

        class ContainerClient:
            def list_blobs(self, *, name_starts_with):
                calls["name_starts_with"] = name_starts_with
                return [
                    Blob("team/repo/staging/old/object", now - timedelta(hours=2)),
                    Blob("team/repo/staging/new/object", now),
                ]

            def delete_blob(self, name):
                calls.setdefault("deleted", []).append(name)

        class BlobServiceClient:
            def __init__(self, account_url, credential):
                calls["account_url"] = account_url
                calls["credential"] = credential

            def get_container_client(self, container):
                calls["container"] = container
                return ContainerClient()

        azure_mod = types.ModuleType("azure")
        identity_mod = types.ModuleType("azure.identity")
        storage_mod = types.ModuleType("azure.storage")
        blob_mod = types.ModuleType("azure.storage.blob")

        identity_mod.DefaultAzureCredential = DefaultAzureCredential
        blob_mod.BlobServiceClient = BlobServiceClient
        azure_mod.identity = identity_mod
        azure_mod.storage = storage_mod
        storage_mod.blob = blob_mod

        monkeypatch.setitem(sys.modules, "azure", azure_mod)
        monkeypatch.setitem(sys.modules, "azure.identity", identity_mod)
        monkeypatch.setitem(sys.modules, "azure.storage", storage_mod)
        monkeypatch.setitem(sys.modules, "azure.storage.blob", blob_mod)

        provider = AzureProvider()
        deleted = provider.cleanup_staging(
            repo_url="crab://container/team/repo",
            older_than_seconds=3600,
        )

        assert deleted == 1
        assert calls["account_url"] == "https://acct.blob.core.windows.net"
        assert calls["container"] == "container"
        assert calls["name_starts_with"] == "team/repo/staging/"
        assert calls["deleted"] == ["team/repo/staging/old/object"]


class TestGcpProviderScoping:
    """Test GCP external-scope safety behavior."""

    def test_gcp_prefix_expression_scopes_to_objects(self):
        expression = _gcs_prefix_expression("bucket", "team/repo")
        assert (
            "resource.name.startsWith('projects/_/buckets/bucket/objects/team/repo/')"
            in expression
        )
        assert "storage.googleapis.com/objectListPrefix" in expression
        assert ".startsWith('team/repo/')" in expression

    def test_gcp_prefix_expression_rejects_expression_injection_chars(self):
        with pytest.raises(ValueError, match="unsupported characters"):
            _gcs_prefix_expression("bucket", "team/repo' || true || '")

    def test_gcp_acl_view_read_expression_excludes_global_prefix(self):
        view_prefix = "team/repo/acl-views/v1/" + "a" * 64 + "/7-deadbeef"
        expression = _gcs_prefix_expression("bucket", view_prefix)

        assert f"objects/{view_prefix}/" in expression
        assert "objects/.crab/xorbs/" not in expression
        assert "objects/team/repo/packs/" not in expression

    def test_gcp_immutable_write_rejects_read_boundary(self):
        class Downscoped:
            class AccessBoundaryRule:
                def __init__(self, **kwargs):
                    pass

        with pytest.raises(ValueError, match="read permission"):
            _build_access_boundary_rules(
                Downscoped,
                bucket="bucket",
                repo_prefix="team/repo",
                upload_prefix="team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                permissions=["read", "immutable-write"],
            )

    def test_gcp_immutable_write_only_access_boundary_has_no_read_rule(self):
        class Downscoped:
            class AccessBoundaryRule:
                def __init__(
                    self,
                    *,
                    available_resource,
                    available_permissions,
                    availability_condition,
                ):
                    self.available_resource = available_resource
                    self.available_permissions = available_permissions
                    self.availability_condition = availability_condition

        rules = _build_access_boundary_rules(
            Downscoped,
            bucket="bucket",
            repo_prefix="team/repo",
            upload_prefix="team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            permissions=["immutable-write"],
        )

        assert len(rules) == 1
        assert rules[0].available_permissions == ["inRole:roles/storage.objectCreator"]
        assert "team/repo/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/" in rules[0].availability_condition[
            "expression"
        ]
        assert "objects/team/repo/manifest" not in rules[0].availability_condition["expression"]

    def test_gcp_access_boundary_rejects_upload_prefix_outside_repo_staging(self):
        class Downscoped:
            class AccessBoundaryRule:
                def __init__(self, **kwargs):
                    pass

        with pytest.raises(ValueError, match="repo staging"):
            _build_access_boundary_rules(
                Downscoped,
                bucket="bucket",
                repo_prefix="team/repo",
                upload_prefix="team/other/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                permissions=["immutable-write"],
            )

    @pytest.mark.asyncio
    async def test_gcp_immutable_write_requires_upload_prefix(self):
        provider = GcpProvider()
        with pytest.raises(ValueError, match="upload_prefix"):
            await provider.generate(
                identity="alice@example.com",
                repo_url="crab://bucket/team/repo",
                operation="push",
                permissions=["immutable-write"],
            )

    @pytest.mark.asyncio
    async def test_gcp_push_read_write_permissions_fail_closed(self):
        provider = GcpProvider()
        with pytest.raises(ValueError, match="canonical write"):
            await provider.generate(
                identity="alice@example.com",
                repo_url="crab://bucket/team/repo",
                operation="push",
                permissions=["read", "write"],
            )

    @pytest.mark.asyncio
    async def test_gcp_immutable_write_upload_prefix_must_be_repo_staging_prefix(self):
        provider = GcpProvider()
        with pytest.raises(ValueError, match="repo staging"):
            await provider.generate(
                identity="alice@example.com",
                repo_url="crab://bucket/team/repo",
                operation="push",
                permissions=["immutable-write"],
                upload_prefix="team/other/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )

    def test_cleanup_staging_deletes_only_expired_objects(self, monkeypatch):
        now = datetime.now(timezone.utc)
        calls = []

        class Response:
            def __init__(self, payload=None, status_code=200):
                self._payload = payload or {}
                self.status_code = status_code

            def json(self):
                return self._payload

            def raise_for_status(self):
                if self.status_code >= 400:
                    raise RuntimeError(f"status {self.status_code}")

        class AuthorizedSession:
            def __init__(self, credentials):
                calls.append(("session", credentials))

            def get(self, url, params):
                calls.append(("get", url, params))
                return Response({
                    "items": [
                        {
                            "name": "team/repo/staging/old/object",
                            "updated": (now - timedelta(hours=2))
                            .isoformat()
                            .replace("+00:00", "Z"),
                        },
                        {
                            "name": "team/repo/staging/new/object",
                            "updated": now.isoformat().replace("+00:00", "Z"),
                        },
                    ],
                })

            def delete(self, url):
                calls.append(("delete", url))
                return Response(status_code=204)

        google_mod = types.ModuleType("google")
        auth_mod = types.ModuleType("google.auth")
        transport_mod = types.ModuleType("google.auth.transport")
        requests_mod = types.ModuleType("google.auth.transport.requests")

        auth_mod.default = lambda scopes: ("creds", "project")
        requests_mod.AuthorizedSession = AuthorizedSession
        google_mod.auth = auth_mod
        auth_mod.transport = transport_mod
        transport_mod.requests = requests_mod

        monkeypatch.setitem(sys.modules, "google", google_mod)
        monkeypatch.setitem(sys.modules, "google.auth", auth_mod)
        monkeypatch.setitem(sys.modules, "google.auth.transport", transport_mod)
        monkeypatch.setitem(
            sys.modules,
            "google.auth.transport.requests",
            requests_mod,
        )

        provider = GcpProvider()
        deleted = provider.cleanup_staging(
            repo_url="crab://bucket/team/repo",
            older_than_seconds=3600,
        )

        assert deleted == 1
        get_call = next(call for call in calls if call[0] == "get")
        assert get_call[2]["prefix"] == "team/repo/staging/"
        delete_call = next(call for call in calls if call[0] == "delete")
        assert delete_call[1].endswith("/team%2Frepo%2Fstaging%2Fold%2Fobject")
