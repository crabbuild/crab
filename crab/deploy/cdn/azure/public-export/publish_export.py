#!/usr/bin/env python3
"""Publish an approved Crab inventory to the isolated Front Door export container."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from public_inventory import (
    CACHE_CONTROL, CONTENT_TYPE, ExportError, native_placement, publication_receipt, read_inventory, verify_body, viewer_url,
    write_publication,
)


BLOB_ORIGIN = re.compile(
    r"https://([a-z0-9]{3,24})\.blob\.core\.windows\.net/([a-z0-9][a-z0-9-]{1,61}[a-z0-9])"
)
MAX_OBJECT = 50_000 * 4 * 1024 ** 2


class AzError(ExportError):
    def __init__(self, operation, stderr):
        self.already_exists = bool(re.search(r"\b(?:BlobAlreadyExists|ConditionNotMet)\b", stderr))
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
        raise AzError(arguments[:3], result.stderr)
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
    if (values["liveSourceOrigin"] != source or account == account_container(source)[0]
            or values["sourceRepositoryPrefix"] != prefix
            or values["inventorySha256"] != digest
            or values["publicDeliveryEnabled"] is not False):
        raise ExportError("deployment must approve this inventory with public delivery disabled")

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
    with tempfile.TemporaryDirectory(
        prefix="crab-cdn-export-", dir=options.scratch_directory
    ) as scratch:
        for obj in objects:
            path = Path(scratch).resolve() / "payload"
            key = f"{prefix}/{obj.path}"
            download_verified(options.source_configuration, object_arguments(source, key), obj, path)
            destination = object_arguments(export, key)
            try:
                az(options.writer_configuration, [
                    "storage", "blob", "upload", *destination, "--no-progress", "--file", str(path),
                    "--type", "block", "--overwrite", "false", "--if-none-match", "*",
                    "--content-type", CONTENT_TYPE, "--content-cache-control", CACHE_CONTROL,
                ])
            except AzError as error:
                if not error.already_exists:
                    raise
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
