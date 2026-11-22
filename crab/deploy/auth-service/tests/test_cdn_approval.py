"""Viewer issuance requires both the pinned binding and the selected cloud placement."""

from types import SimpleNamespace
from unittest.mock import MagicMock

import boto3
import google.auth
import pytest
from botocore.stub import Stubber

from src.cdn_approval import CdnApproval


def export(provider, origin):
    return {"provider": provider, "origin": origin, "repo_prefix": "org/models",
            "base_url": "https://downloads.example.com"}


def verify(approval, entry, credentials=None):
    CdnApproval(**approval).verify(**entry, view_suffix="", credentials=credentials or {})


@pytest.mark.parametrize("field, value", [
    ("native_origin", "s3://other-bucket"),
    ("base_url", "https://otro.example.com"),
    ("repo_prefix", "org/other"),
    ("view_suffix", "/acl-views/v2/other"),
])
def test_approval_cannot_be_reused_for_a_different_route(approve_cdn, monkeypatch, field, value):
    entry = export("cloudfront", "s3://test-bucket")
    aws = MagicMock()
    monkeypatch.setattr("src.cdn_approval._aws", aws)
    with pytest.raises(ValueError, match="conflicts with the requested viewer route"):
        verify(approve_cdn(entry, **{field: value}), entry)
    aws.assert_not_called()


def test_s3_approval_checks_expected_owner_and_native_region(approve_cdn, monkeypatch):
    entry = export("cloudfront", "s3://test-bucket")
    approval = approve_cdn(entry)
    monkeypatch.delenv("CRAB_AUTH_DRY_RUN", raising=False)
    s3 = boto3.client("s3", region_name="us-west-2", aws_access_key_id="test", aws_secret_access_key="test")
    client = MagicMock(return_value=s3)
    monkeypatch.setattr(boto3, "client", client)
    with pytest.raises(ValueError, match="conflicts with the selected native placement"):
        verify(approval, entry, {"region": "us-east-1"})
    client.assert_not_called()
    params = {"Bucket": "test-bucket", "ExpectedBucketOwner": "123456789012"}
    with Stubber(s3) as stub:
        stub.add_response("get_bucket_location", {"LocationConstraint": "us-west-2"}, params)
        stub.add_client_error("get_bucket_location", "AccessDenied", http_status_code=403, expected_params=params)
        verify(approval, entry, {"region": "us-west-2"})
        with pytest.raises(ValueError, match="Could not verify the approved CDN native placement"):
            verify(approval, entry, {"region": "us-west-2"})
        stub.assert_no_pending_responses()
    assert client.call_args.kwargs["endpoint_url"] == "https://s3.us-west-2.amazonaws.com"


def test_aws_dry_run_cannot_mint_live_viewer_credentials(approve_cdn, monkeypatch):
    entry = export("cloudfront", "s3://test-bucket")
    monkeypatch.setenv("CRAB_AUTH_DRY_RUN", "true")
    client = MagicMock()
    monkeypatch.setattr(boto3, "client", client)
    with pytest.raises(ValueError, match="conflicts with the selected native placement"):
        verify(approve_cdn(entry), entry, {"region": "us-west-2"})
    client.assert_not_called()


@pytest.mark.parametrize("project, location, status", [
    ("123456789", "US", 200), ("999999999", "US", 200), ("123456789", "ASIA", 200), ("123456789", "US", 302),
])
def test_gcs_approval_checks_live_project_and_location(approve_cdn, monkeypatch, project, location, status):
    entry = export("cloud_cdn", "gs://test-bucket")
    approval = approve_cdn(entry)
    monkeypatch.setattr(google.auth, "default", lambda **kwargs: (object(), "auth-project"))
    authorized = MagicMock()
    monkeypatch.setattr("google.auth.transport.requests.AuthorizedSession", lambda *args, **kwargs: authorized)
    session = authorized.__enter__.return_value
    response = session.get.return_value
    response.status_code = status
    response.json.return_value = {"name": "test-bucket", "projectNumber": project, "location": location}
    if (project, location, status) == ("123456789", "US", 200):
        verify(approval, entry)
    else:
        with pytest.raises(ValueError, match="conflicts with the selected native placement"):
            verify(approval, entry)
    session.get.assert_called_once_with("https://storage.googleapis.com/storage/v1/b/test-bucket",
                                       params={"fields": "name,projectNumber,location"}, allow_redirects=False, timeout=5)


@pytest.mark.parametrize("change", ["valid", "subscription", "account", "container", "location"])
def test_blob_approval_checks_subscription_account_location_and_container(approve_cdn, monkeypatch, change):
    entry = export("front_door", "https://account.blob.core.windows.net/test-bucket")
    approval = approve_cdn(entry)
    subscription = "00000000-0000-0000-0000-000000000001"
    monkeypatch.setenv("CRAB_AUTH_AZURE_SUBSCRIPTION_ID", subscription if change != "subscription" else subscription[:-1] + "2")
    account = f"/subscriptions/{subscription}/resourcegroups/test/providers/microsoft.storage/storageaccounts/account"
    container = f"{account}/blobservices/default/containers/test-bucket"
    azure = MagicMock()
    monkeypatch.setattr("azure.identity.DefaultAzureCredential", lambda: azure)
    azure.__enter__.return_value.get_token.return_value = SimpleNamespace(token="test-token")
    http = MagicMock()
    monkeypatch.setattr("requests.Session", lambda: http)
    session = http.__enter__.return_value
    account_response, container_response = MagicMock(status_code=200), MagicMock(status_code=200)
    account_response.json.return_value = {"id": account, "properties": {
        "primaryLocation": "westus2" if change != "location" else "eastus",
        "primaryEndpoints": {"blob": "https://account.blob.core.windows.net/"},
    }}
    container_response.json.return_value = {"id": container if change != "container" else container + "-other"}
    session.get.side_effect = [account_response, container_response]
    credentials = {"storage_account": "account" if change != "account" else "otheraccount"}
    if change == "valid":
        verify(approval, entry, credentials)
        assert [call.args[0] for call in session.get.call_args_list] == [
            f"https://management.azure.com{account}", f"https://management.azure.com{container}",
        ]
    else:
        with pytest.raises(ValueError, match="conflicts with the selected native placement"):
            verify(approval, entry, credentials)
        if change in {"subscription", "account"}:
            session.get.assert_not_called()
