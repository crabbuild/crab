#!/usr/bin/env python3
"""Publish an approved Crab inventory to the isolated Cloud CDN export bucket."""

import argparse
import json
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


GCS_ORIGIN = re.compile(r"gs://([a-z0-9][a-z0-9._-]{1,220}[a-z0-9])")
MAX_OBJECT = 5 * 1024 ** 4


def bucket(origin):
    match = GCS_ORIGIN.fullmatch(origin) if isinstance(origin, str) else None
    if not match:
        raise ExportError("expected a native gs:// bucket")
    return match.group(1)


def gcloud(configuration, arguments):
    command = [
        "gcloud", f"--configuration={configuration}", "--quiet", "--verbosity=error", "--format=json",
    ]
    result = subprocess.run(
        [*command, *arguments], capture_output=True, text=True, check=False
    )
    if result.returncode:
        raise ExportError(f"gcloud {' '.join(arguments[:3])} failed")
    return json.loads(result.stdout)


def bucket_placement(origin, metadata):
    name = bucket(origin)
    if metadata.get("name") != name:
        raise ExportError("observed GCS bucket differs from its native origin")
    return native_placement(origin, {"resource": f"projects/{metadata.get('projectNumber')}/buckets/{name}",
                                     "location": metadata.get("location")})


def find_export(options, source, source_placement, prefix, digest):
    outputs = json.loads(options.deployment.read_text())
    values = {key: output["value"] for key, output in outputs.items()}
    export = values["export_origin"]
    export_bucket = bucket(export)
    if (values["source_origin"] != source or export == source
            or values["repository_prefix"] != prefix
            or values["approved_inventory_sha256"] != digest
            or values["public_delivery_enabled"] is not False):
        raise ExportError("deployment must approve this inventory with public delivery disabled")

    meta = gcloud(
        options.operator_configuration, ["storage", "buckets", "describe", export, "--raw"],
    )
    labels = meta["labels"]
    iam = meta["iamConfiguration"]
    if (meta["name"] != export_bucket or labels.get("crab_public_export") != "true"
            or labels.get("crab_inventory_0") != digest[:32]
            or labels.get("crab_inventory_1") != digest[32:]
            or iam.get("publicAccessPrevention") != "enforced"
            or iam["uniformBucketLevelAccess"]["enabled"] is not True
            or meta.get("versioning", {}).get("enabled", False) is not False
            or str(meta["softDeletePolicy"]["retentionDurationSeconds"]) != "0"):
        raise ExportError("native export bucket is not staged with the approved inventory")

    backend = values["backend_bucket"]
    project = values["project_id"]
    name = r"[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?"
    if not re.fullmatch(name, backend) or not re.fullmatch(name, project):
        raise ExportError("invalid Google Cloud export backend or project")
    identity = gcloud(options.operator_configuration, ["projects", "describe", project])
    export_placement = bucket_placement(export, meta)
    if (identity.get("projectId") != project
            or export_placement["resource"] != f"projects/{identity.get('projectNumber')}/buckets/{export_bucket}"):
        raise ExportError("export bucket is outside the approved Cloud CDN project")
    settings = gcloud(
        options.operator_configuration,
        ["compute", "backend-buckets", "describe", backend, "--project", project],
    )
    if settings.get("name") != backend or settings.get("bucketName") != export_bucket or settings.get("enableCdn") is not True:
        raise ExportError("Cloud CDN backend does not serve the staged export bucket")
    base_url = viewer_url(values["base_url"])
    hostname = base_url.removeprefix("https://")
    policy_link = settings["edgeSecurityPolicy"]
    policy_name = re.fullmatch(rf"(?:https://www\.googleapis\.com/compute/v1/)?projects/{project}/global/securityPolicies/({name})",
                               policy_link)
    if not policy_name:
        raise ExportError("Cloud CDN backend lacks its staged viewer policy")
    policy = gcloud(options.operator_configuration,
                    ["compute", "security-policies", "describe", policy_name[1], "--project", project])
    host_guard = f"!has(request.headers['host']) || request.headers['host'] != '{hostname}'"
    if policy.get("type") != "CLOUD_ARMOR_EDGE" or not any(
        rule.get("action") == "deny(403)" and rule.get("preview", False) is False
        and rule.get("match", {}).get("expr", {}).get("expression") == host_guard
        for rule in policy["rules"]
    ):
        raise ExportError("Cloud CDN viewer policy does not match the staged hostname")
    source_meta = gcloud(options.operator_configuration, ["storage", "buckets", "describe", source, "--raw"])
    if bucket_placement(source, source_meta) != source_placement:
        raise ExportError("source bucket differs from the approved GCS placement")
    return export, export_placement, f"projects/{project}/global/backendBuckets/{backend}", project, base_url


def storage_client(credentials_file, project):
    import google.auth
    from google.cloud import storage

    credentials, _ = google.auth.load_credentials_from_file(
        str(credentials_file), scopes=["https://www.googleapis.com/auth/devstorage.read_write"],
    )
    return storage.Client(
        project=project, credentials=credentials,
        client_options={"api_endpoint": "https://storage.googleapis.com"},
    )


def download_verified(blob, obj, path):
    blob.reload()
    if blob.size != obj.size or not blob.generation or not blob.metageneration:
        raise ExportError(f"inventory size or generation mismatch for {obj.path}")
    blob.download_to_filename(
        str(path), raw_download=True, if_generation_match=blob.generation,
        if_metageneration_match=blob.metageneration,
    )
    verify_body(obj, path)


def create_blob(blob, path):
    from google.api_core.exceptions import PreconditionFailed

    blob.cache_control = CACHE_CONTROL
    try:
        blob.upload_from_filename(str(path), content_type=CONTENT_TYPE, if_generation_match=0)
    except PreconditionFailed:
        # The existing generation must pass the same body and metadata checks.
        pass


def publish(options):
    source, source_placement, prefix, digest, objects = read_inventory(options.inventory, bucket, MAX_OBJECT)
    staged = find_export(options, source, source_placement, prefix, digest)
    export, export_placement, backend, project, base_url = staged
    reader = storage_client(options.source_adc, project).bucket(bucket(source))
    writer = storage_client(options.writer_adc, project).bucket(bucket(export))
    with tempfile.TemporaryDirectory(
        prefix="crab-cdn-export-", dir=options.scratch_directory
    ) as scratch:
        for obj in objects:
            path = Path(scratch).resolve() / "payload"
            key = f"{prefix}/{obj.path}"
            download_verified(reader.blob(key), obj, path)
            create_blob(writer.blob(key), path)
            path.unlink()
            destination = writer.blob(key)
            download_verified(destination, obj, path)
            if (destination.content_type != CONTENT_TYPE or destination.cache_control != CACHE_CONTROL
                    or any(getattr(destination, property_name, None) for property_name in (
                        "content_encoding", "content_disposition", "content_language",
                        "metadata", "contexts", "custom_time",
                    ))):
                raise ExportError(f"export metadata differs from the approved profile for {obj.path}")
            path.unlink()
    if find_export(options, source, source_placement, prefix, digest) != staged:
        raise ExportError("export target changed during publication")
    return publication_receipt(source, export, source_placement, export_placement,
                               prefix, base_url, backend, digest, objects)


def main():
    from google.api_core.exceptions import GoogleAPICallError
    from google.auth.exceptions import GoogleAuthError

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inventory", type=Path)
    parser.add_argument("--deployment", type=Path, required=True, help="Saved terraform output -json")
    parser.add_argument("--operator-configuration", required=True)
    parser.add_argument("--source-adc", type=Path, required=True)
    parser.add_argument("--writer-adc", type=Path, required=True)
    parser.add_argument("--scratch-directory", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    options = parser.parse_args()
    try:
        print(json.dumps(write_publication(options.receipt, lambda: publish(options))))
    except (GoogleAPICallError, GoogleAuthError) as error:
        print(f"Export stopped: Google Cloud request failed ({type(error).__name__})", file=sys.stderr)
        return 1
    except (ExportError, ValueError, OSError, KeyError, TypeError) as error:
        print(f"Export stopped: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
