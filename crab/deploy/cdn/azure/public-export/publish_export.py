#!/usr/bin/env python3
"""Publish an approved Crab inventory to the isolated Front Door export container."""

import argparse
import base64
from contextlib import closing
from email.utils import formatdate
import hashlib
from http.client import HTTPSConnection, HTTPException
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote, urlencode, urlsplit
from uuid import uuid4
import xml.etree.ElementTree as ET

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from public_inventory import (
    CACHE_CONTROL, CONTENT_TYPE, ExportError, native_placement, publication_receipt, read_inventory,
    spool_part, validate_public_origins, verify_body, viewer_url,
    write_publication,
)


BLOB_ORIGIN = re.compile(
    r"https://([a-z0-9]{3,24})\.blob\.core\.windows\.net/([a-z0-9][a-z0-9-]{1,61}[a-z0-9])"
)
MAX_BLOCKS = 50_000
MAX_OBJECT = MAX_BLOCKS * 4_000 * 1024 ** 2
DEFAULT_BLOCK_BYTES = 64 * 1024 ** 2
SINGLE_PUT_BYTES = 8 * 1024 ** 2
STORAGE_AUDIENCE = "https://storage.azure.com/"


class AzError(ExportError):
    def __init__(self, operation):
        super().__init__(f"az {' '.join(operation)} failed")


def account_container(origin):
    match = BLOB_ORIGIN.fullmatch(origin) if isinstance(origin, str) else None
    if not match or "--" in match[2]:
        raise ExportError("expected a native Azure Blob origin")
    return match[1], match[2]


def az(configuration, arguments):
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith("AZURE_STORAGE_")}
    environment["AZURE_CONFIG_DIR"] = str(configuration)
    result = subprocess.run(
        ["az", *arguments, "--output", "json", "--only-show-errors"],
        env=environment, check=False, capture_output=True, text=True,
    )
    if result.returncode:
        raise AzError(arguments[:3])
    return json.loads(result.stdout) if result.stdout.strip() else None


def resource(options, resource_id, api_version):
    return az(
        options.operator_configuration,
        ["resource", "show", "--ids", resource_id, "--api-version", api_version],
    )


def blob_placement(origin, account, container, account_id, container_id):
    properties = account["properties"]
    if (account["id"].lower() != account_id.lower() or container["id"].lower() != container_id.lower()
            or properties["primaryEndpoints"]["blob"].rstrip("/") != origin.rsplit("/", 1)[0]):
        raise ExportError("Azure ARM resources differ from the approved Blob origin")
    return native_placement(origin, {"resource": container["id"].lower(), "location": properties.get("primaryLocation")})


def find_export(options, source, source_placement, prefix, digest):
    deployment = az(options.operator_configuration, [
        "deployment", "group", "show", "--resource-group", options.resource_group,
        "--name", options.deployment, "--subscription", options.subscription,
    ])["properties"]
    if deployment["provisioningState"] != "Succeeded":
        raise ExportError("export deployment must have succeeded")
    values = {key: entry["value"] for key, entry in deployment["outputs"].items()}
    export = values["exportOrigin"]
    account, container = account_container(export)
    if (values["liveSourceOrigin"] != source
            or values["sourceRepositoryPrefix"] != prefix
            or values["inventorySha256"] != digest
            or values["publicDeliveryEnabled"] is not False):
        raise ExportError("deployment must approve this inventory with public delivery disabled")
    validate_public_origins(source, export)

    account_id = (
        f"/subscriptions/{options.subscription}/resourceGroups/{options.resource_group}/"
        f"providers/Microsoft.Storage/storageAccounts/{account}"
    )
    container_id = f"{account_id}/blobServices/default/containers/{container}"
    if values["exportContainerId"].lower() != container_id.lower():
        raise ExportError("deployment does not own the selected export container")
    storage = resource(options, account_id, "2025-01-01")
    properties = storage["properties"]
    tags = storage["tags"]
    if (storage["id"].lower() != account_id.lower()
            or tags.get("CrabPublicExport") != "true"
            or tags.get("CrabSourceOrigin") != source
            or tags.get("CrabApprovedInventorySha256") != digest
            or properties.get("allowBlobPublicAccess") is not False
            or properties.get("allowSharedKeyAccess") is not False
            or properties.get("isHnsEnabled") is not True):
        raise ExportError("native export account is not staged with the approved inventory")
    bucket = resource(options, container_id, "2025-01-01")
    if (bucket["id"].lower() != container_id.lower()
            or bucket["properties"].get("publicAccess", "None") != "None"):
        raise ExportError("native export container is not private")
    export_placement = blob_placement(export, storage, bucket, account_id, container_id)
    blob_properties = resource(options, f"{account_id}/blobServices/default", "2025-01-01")["properties"]
    if (blob_properties.get("isVersioningEnabled", False) is not False
            or any(blob_properties[property_name].get("enabled") is not False
                   for property_name in ("deleteRetentionPolicy", "containerDeleteRetentionPolicy"))):
        raise ExportError("native export must keep versioning and soft delete disabled")

    edge_prefix = (
        f"/subscriptions/{options.subscription}/resourceGroups/{options.resource_group}/"
        "providers/Microsoft.Cdn/profiles/"
    ).lower()
    origin_id = values["edgeOriginId"]
    route_id = values["edgeRouteId"]
    if not origin_id.lower().startswith(edge_prefix) or not route_id.lower().startswith(edge_prefix):
        raise ExportError("Front Door resources are outside the export deployment")
    origin = resource(options, origin_id, "2025-06-01")
    route = resource(options, route_id, "2025-06-01")
    route_properties = route["properties"]
    endpoint_id, separator, _ = route_id.rpartition("/routes/")
    if not separator or "/afdEndpoints/" not in endpoint_id:
        raise ExportError("invalid Front Door endpoint or route identity")
    endpoint = resource(options, endpoint_id, "2025-06-01")
    base_url = viewer_url("https://" + endpoint["properties"]["hostName"])
    expected_host = f"{account}.blob.core.windows.net"
    if (route["id"].lower() != route_id.lower() or endpoint["id"].lower() != endpoint_id.lower()
            or base_url != values["baseUrl"] or not base_url.endswith(".azurefd.net")
            or origin["properties"]["hostName"] != expected_host
            or origin["properties"]["originHostHeader"] != expected_host
            or route_properties.get("originPath") != f"/{container}"
            or route_properties["originGroup"]["id"].lower() != origin["id"].rsplit("/origins/", 1)[0].lower()
            or route_properties.get("enabledState") != "Disabled"):
        raise ExportError("Front Door route does not serve the staged export container")
    source_container_id = source_placement["resource"]
    source_account_id = source_container_id.split("/blobservices/", 1)[0]
    observed_source = blob_placement(source, resource(options, source_account_id, "2025-01-01"),
                                     resource(options, source_container_id, "2025-01-01"),
                                     source_account_id, source_container_id)
    if observed_source != source_placement:
        raise ExportError("source Blob container differs from the approved placement")
    return export, export_placement, route["id"], base_url


def object_arguments(origin, key):
    account, container = account_container(origin)
    return ["--auth-mode", "login", "--account-name", account,
            "--container-name", container, "--name", key]


def download_verified(configuration, location, obj, path):
    blob = az(configuration, ["storage", "blob", "show", *location])
    meta = blob["properties"]
    if meta.get("contentLength") != obj.size or not meta.get("etag"):
        raise ExportError(f"inventory size or native ETag mismatch for {obj.path}")
    az(configuration, [
        "storage", "blob", "download", *location, "--no-progress",
        "--file", str(path), "--if-match", meta["etag"],
    ])
    verify_body(obj, path)
    return blob


class BlobWriter:
    def __init__(self, configuration, origin):
        _, container = account_container(origin)
        self.configuration = configuration
        self.container = container
        self.connection = HTTPSConnection(urlsplit(origin).netloc, timeout=120, blocksize=128 * 1024)
        self.bearer = ""
        self.expires_on = 0

    def close(self):
        self.connection.close()

    def request(self, method, target, body=None, headers=None):
        now = time.time()
        if self.expires_on <= now + 60:
            response = az(self.configuration, ["account", "get-access-token", "--resource", STORAGE_AUDIENCE])
            if not isinstance(response, dict):
                raise ExportError("Azure CLI did not return a Storage access token")
            token, expiry = response.get("accessToken"), response.get("expires_on")
            if (not isinstance(token, str) or not re.fullmatch(r"[A-Za-z0-9._~+/=-]{1,16384}", token)
                    or response.get("tokenType") != "Bearer" or type(expiry) is not int or expiry <= now):
                raise ExportError("Azure CLI did not return a valid Storage bearer token and expiry")
            self.bearer, self.expires_on = token, expiry
        request_headers = {
            "Authorization": f"Bearer {self.bearer}", "x-ms-version": "2023-11-03", "x-ms-date": formatdate(usegmt=True),
        }
        try:
            self.connection.request(method, target, body=body, headers=(headers or {}) | request_headers)
            with self.connection.getresponse() as response:
                if len(response.read(65537)) > 65536:
                    raise ExportError("Azure Blob write response exceeds the supported size")
                status, code = response.status, response.getheader("x-ms-error-code", "")
        except (OSError, HTTPException) as error:
            raise ExportError(f"Azure Blob {method} failed ({type(error).__name__})") from error
        return status, code

    def create(self, key, obj, path):
        target = quote(f"/{self.container}/{key}", safe="/")
        if obj.size <= SINGLE_PUT_BYTES:
            with path.open("rb") as source:
                data = source.read(obj.size + 1)
            if len(data) != obj.size or hashlib.sha256(data).hexdigest() != obj.sha256:
                raise ExportError(f"source spool differs from the approved object for {obj.path}")
            checksum = base64.b64encode(hashlib.md5(data, usedforsecurity=False).digest()).decode("ascii")
            result = self.request("PUT", target, data, {
                "If-None-Match": "*", "x-ms-blob-type": "BlockBlob", "Content-Type": CONTENT_TYPE,
                "x-ms-blob-cache-control": CACHE_CONTROL, "Content-MD5": checksum,
            })
        else:
            result = self.upload_blocks(target, obj, path)
        if result not in ((201, ""), (409, "BlobAlreadyExists"), (412, "ConditionNotMet")):
            self.reject("create", result)

    @staticmethod
    def reject(operation, result):
        status, code = result
        safe_code = code if re.fullmatch(r"[A-Za-z0-9]{1,80}", code) else "unclassified"
        raise ExportError(f"Azure Blob {operation} failed ({status} {safe_code})")

    def upload_blocks(self, target, obj, path):
        existing = self.request("HEAD", target)
        if existing == (200, ""):
            return 409, "BlobAlreadyExists"
        if existing != (404, "BlobNotFound"):
            self.reject("properties", existing)
        block_bytes = max(DEFAULT_BLOCK_BYTES, (obj.size + MAX_BLOCKS - 1) // MAX_BLOCKS)
        block_list = ET.Element("BlockList")
        attempt = uuid4().hex
        body_digest = hashlib.sha256()
        block_file = path.with_name("block")
        with path.open("rb") as source:
            for index, offset in enumerate(range(0, obj.size, block_bytes), 1):
                size = min(block_bytes, obj.size - offset)
                block_digest = hashlib.md5(usedforsecurity=False)
                spool_part(source, block_file, size, body_digest, block_digest)
                # Equal-length IDs isolate staging attempts on the same key.
                block_id = base64.b64encode(f"{attempt}{index:05d}".encode("ascii")).decode("ascii")
                query = urlencode({"comp": "block", "blockid": block_id})
                with block_file.open("rb") as block:
                    result = self.request("PUT", f"{target}?{query}", block, {
                        "Content-Length": str(size),
                        "Content-MD5": base64.b64encode(block_digest.digest()).decode("ascii"),
                    })
                if result != (201, ""):
                    self.reject("stage block", result)
                ET.SubElement(block_list, "Uncommitted").text = block_id
            if source.read(1) or body_digest.hexdigest() != obj.sha256:
                raise ExportError(f"source spool differs from the approved object for {obj.path}")
        block_file.unlink()
        return self.request("PUT", f"{target}?comp=blocklist", ET.tostring(block_list), {
            "If-None-Match": "*", "Content-Type": "application/xml",
            "x-ms-blob-content-type": CONTENT_TYPE, "x-ms-blob-cache-control": CACHE_CONTROL,
        })


def publish(options):
    source, source_placement, prefix, digest, objects = read_inventory(options.inventory, account_container, MAX_OBJECT)
    if len(prefix) > 64:
        raise ExportError("Front Door fixture requires a repository prefix of at most 64 characters")
    for configuration in (
        options.operator_configuration, options.source_configuration, options.writer_configuration,
    ):
        if az(configuration, ["cloud", "show"]).get("name") != "AzureCloud":
            raise ExportError("export requires Azure public-cloud configurations")
    staged = find_export(options, source, source_placement, prefix, digest)
    export, export_placement, route, base_url = staged
    with (tempfile.TemporaryDirectory(prefix="crab-cdn-export-", dir=options.scratch_directory) as scratch,
          closing(BlobWriter(options.writer_configuration, export)) as writer):
        for obj in objects:
            path = Path(scratch).resolve() / "payload"
            key = f"{prefix}/{obj.path}"
            download_verified(options.source_configuration, object_arguments(source, key), obj, path)
            destination = object_arguments(export, key)
            writer.create(key, obj, path)
            path.unlink()
            blob = download_verified(options.writer_configuration, destination, obj, path)
            meta = blob["properties"]
            settings = meta["contentSettings"]
            if (blob.get("metadata") or meta.get("blobType") != "BlockBlob"
                    or settings.get("contentType") != CONTENT_TYPE
                    or settings.get("cacheControl") != CACHE_CONTROL
                    or any(settings.get(field) for field in (
                        "contentEncoding", "contentDisposition", "contentLanguage",
                    ))):
                raise ExportError(f"export representation differs for {obj.path}")
            path.unlink()
    if find_export(options, source, source_placement, prefix, digest) != staged:
        raise ExportError("export target changed during publication")
    return publication_receipt(source, export, source_placement, export_placement,
                               prefix, base_url, route, digest, objects)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inventory", type=Path)
    parser.add_argument("--deployment", required=True)
    parser.add_argument("--subscription", required=True, help="Subscription UUID")
    parser.add_argument("--resource-group", required=True)
    parser.add_argument("--operator-configuration", type=Path, required=True)
    parser.add_argument("--source-configuration", type=Path, required=True)
    parser.add_argument("--writer-configuration", type=Path, required=True)
    parser.add_argument("--scratch-directory", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    options = parser.parse_args()
    try:
        print(json.dumps(write_publication(options.receipt, lambda: publish(options))))
    except (ExportError, ValueError, OSError, KeyError, TypeError) as error:
        print(f"Export stopped: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
