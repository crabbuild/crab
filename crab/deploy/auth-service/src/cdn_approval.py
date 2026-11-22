"""Load an operator-pinned private CDN attestation and verify native placement."""

from __future__ import annotations

import hashlib
import json
import os
import re
from contextlib import closing
from pathlib import Path
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field

_Digest = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
_MAX_RECEIPT_BYTES = 8192
_PLACE_ERROR = "CDN approval conflicts with the selected native placement"
_ARM = "https://management.azure.com"
_ARM_ID = r"/subscriptions/[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}/resourcegroups/(?!\.{1,2}/)[a-z0-9._()-]{1,90}"


class _Record(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True, strict=True)


class _Binding(_Record):
    deployment: str
    native_resource: str
    native_location: str
    native_origin: str
    repo_prefix: str
    view_suffix: str
    base_url: str


class _Object(_Record):
    key_sha256: _Digest
    body_sha256: _Digest
    bytes: int = Field(gt=0, le=1024 * 1024)


class _Candidate(_Record):
    schema_id: Literal["crab.cdn-private-binding-candidate/v1"] = Field(alias="schema")
    binding: _Binding
    edge: Literal["cloudfront", "cloud-cdn", "front-door"]
    configuration_sha256: _Digest
    deployment_policy_sha256: _Digest
    object: _Object


def _unique_fields(fields: list[tuple[str, object]]) -> dict[str, object]:
    decoded = dict(fields)
    if len(decoded) != len(fields):
        raise ValueError("CDN approval repeats JSON fields")
    return decoded


class CdnApproval(BaseModel):
    """Operator configuration pinning the exact reviewed candidate bytes."""

    model_config = ConfigDict(extra="forbid", frozen=True)

    path: Path = Field(repr=False)
    sha256: _Digest

    def verify(
        self,
        *,
        provider: str,
        origin: str,
        repo_prefix: str,
        view_suffix: str,
        base_url: str,
        credentials: dict[str, object],
    ) -> None:
        with self.path.open("rb") as file:
            raw = file.read(_MAX_RECEIPT_BYTES + 1)
        if len(raw) > _MAX_RECEIPT_BYTES or hashlib.sha256(raw).hexdigest() != self.sha256:
            raise ValueError("CDN approval digest mismatch")
        try:
            candidate = _Candidate.model_validate(json.loads(raw, object_pairs_hook=_unique_fields))
        except (ValueError, RecursionError):
            raise ValueError("Invalid CDN approval receipt") from None
        binding = candidate.binding
        if (
            candidate.edge != provider.replace("_", "-")
            or (binding.native_origin, binding.repo_prefix, binding.view_suffix, binding.base_url)
            != (origin, repo_prefix, view_suffix, base_url)
        ):
            raise ValueError("CDN approval conflicts with the requested viewer route")
        try:
            if provider == "cloudfront":
                matches = _aws(binding, credentials)
            elif provider == "cloud_cdn":
                matches = _gcp(binding)
            else:
                matches = _azure(binding, credentials)
        except Exception:
            # Provider exceptions may contain credential-bearing URLs or headers.
            raise ValueError("Could not verify the approved CDN native placement") from None
        if not matches:
            raise ValueError(_PLACE_ERROR)


def _aws(binding: _Binding, credentials: dict[str, object]) -> bool:
    from boto3 import client
    from botocore.config import Config

    bucket = binding.native_origin.removeprefix("s3://")
    owners = re.fullmatch(rf"accounts/([0-9]{{12}})/buckets/{re.escape(bucket)}", binding.native_resource)
    region = binding.native_location
    if (
        os.environ.get("CRAB_AUTH_DRY_RUN", "").lower() in {"1", "true", "yes"}
        or owners is None
        or not re.fullmatch(r"(?!cn-)[a-z]{2}-[a-z]+-[1-9][0-9]*", region)
        or region != credentials.get("region")
        or not re.fullmatch(rf"arn:aws:cloudfront::{owners[1]}:distribution/[A-Za-z0-9_-]{{1,64}}", binding.deployment)
    ):
        return False
    # Use the service identity for metadata, outside the reader's scoped session.
    config = Config(connect_timeout=5, read_timeout=5, retries={"total_max_attempts": 1})
    with closing(client("s3", region_name=region, endpoint_url=f"https://s3.{region}.amazonaws.com", config=config)) as s3:
        location = s3.get_bucket_location(Bucket=bucket, ExpectedBucketOwner=owners[1])["LocationConstraint"]
    return {None: "us-east-1", "EU": "eu-west-1"}.get(location, location) == region


def _gcp(binding: _Binding) -> bool:
    import google.auth
    from google.auth.transport.requests import AuthorizedSession

    bucket = binding.native_origin.removeprefix("gs://")
    projects = re.fullmatch(rf"projects/([0-9]+)/buckets/{re.escape(bucket)}", binding.native_resource)
    if (
        projects is None
        or not re.fullmatch(r"[A-Z0-9-]{1,40}", binding.native_location)
        or not re.fullmatch(r"projects/[a-z][a-z0-9-]{4,28}[a-z0-9]/global/backendBuckets/[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?", binding.deployment)
    ):
        return False
    identity, _ = google.auth.default(scopes=["https://www.googleapis.com/auth/cloud-platform"])
    with AuthorizedSession(identity, refresh_timeout=5) as session:
        response = session.get(f"https://storage.googleapis.com/storage/v1/b/{bucket}",
                               params={"fields": "name,projectNumber,location"}, allow_redirects=False, timeout=5)
        response.raise_for_status()
        reported = response.json() if response.status_code == 200 else {}
    return (reported.get("name"), str(reported.get("projectNumber")), reported.get("location")) == (
        bucket, projects[1], binding.native_location,
    )


def _azure(binding: _Binding, credentials: dict[str, object]) -> bool:
    from azure.identity import DefaultAzureCredential
    from requests import Session

    endpoint, container = binding.native_origin.rsplit("/", 1)
    account = endpoint.removeprefix("https://").removesuffix(".blob.core.windows.net")
    scope = re.fullmatch(
        rf"(?P<account>{_ARM_ID}/providers/microsoft\.storage/storageaccounts/{re.escape(account)})"
        rf"/blobservices/default/containers/{re.escape(container)}", binding.native_resource,
    )
    if (
        scope is None
        or account != credentials.get("storage_account")
        or binding.native_resource.split("/")[2] != os.environ.get("CRAB_AUTH_AZURE_SUBSCRIPTION_ID", "").lower()
        or not re.fullmatch(r"[a-z0-9]{1,40}", binding.native_location)
        or not re.fullmatch(
            rf"{_ARM_ID}/providers/microsoft\.cdn/profiles/[a-z0-9-]+/afdendpoints/[a-z0-9-]+/routes/[a-z0-9-]+",
            binding.deployment,
        )
    ):
        return False
    with DefaultAzureCredential() as identity, Session() as session:
        token = identity.get_token(f"{_ARM}/.default").token
        session.headers["Authorization"] = f"Bearer {token}"

        def read(resource: str) -> dict:
            response = session.get(f"{_ARM}{resource}", params={"api-version": "2025-01-01"},
                                   allow_redirects=False, timeout=5)
            response.raise_for_status()
            return response.json() if response.status_code == 200 else {}

        account_record = read(scope["account"])
        properties = account_record.get("properties", {})
        return (
            account_record.get("id", "").lower() == scope["account"]
            and properties.get("primaryLocation") == binding.native_location
            and properties.get("primaryEndpoints", {}).get("blob", "").rstrip("/") == endpoint
            and read(binding.native_resource).get("id", "").lower() == binding.native_resource
        )
