"""Canonical inventory and byte verification for isolated public exports."""

from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path
import re
from tempfile import TemporaryDirectory
import time


SCHEMA = "crab.cdn-export.v1"
PUBLICATION_SCHEMA = "crab.cdn-export-publication.v1"
PUBLICATION_BINDING_FIELDS = {
    "source_origin", "source_placement", "export_origin", "export_placement", "repository_prefix",
    "base_url", "deployment", "inventory_sha256",
}
MAX_INVENTORY_BYTES = 8 * 1024 * 1024
CONTENT_TYPE = "application/octet-stream"
CACHE_CONTROL = "public, max-age=86400"
SEGMENT = re.compile(r"[A-Za-z0-9._-]+")
SHA256 = re.compile(r"[a-f0-9]{64}")
S3_REGION = re.compile(r"(?!cn-|us-gov-|us-iso|eu-iso|eusc-)[a-z]{2}(?:-[a-z]+)+-[0-9]+")
AZURE_CONTAINER = re.compile(
    r"/subscriptions/[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}/resourcegroups/"
    r"[a-z0-9_.()-]{1,90}/providers/microsoft\.storage/storageaccounts/([a-z0-9]{3,24})"
    r"/blobservices/default/containers/([a-z0-9][a-z0-9-]{1,61}[a-z0-9])"
)
HTTPS_HOST = re.compile(r"https://[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+")
GIT_KEY = re.compile(
    r"v2/(?:capsules|checkpoints|catalogs|pack-layers|history(?:/source-roots)?)/"
    r"([a-f0-9]{2})/([a-f0-9]{64})"
)
LFS_KEY = re.compile(r"lfs/objects/([a-f0-9]{2})/([a-f0-9]{2})/([a-f0-9]{64})")


class ExportError(Exception):
    pass


@dataclass(frozen=True)
class Object:
    path: str
    size: int
    sha256: str


def exact_fields(data, fields):
    if not isinstance(data, dict) or data.keys() != set(fields.split()):
        raise ExportError("unexpected or missing inventory fields")


def unique_fields(pairs):
    data = dict(pairs)
    if len(data) != len(pairs):
        raise ExportError("duplicate inventory field")
    return data


def read_document(path):
    with path.open("rb") as inventory:
        raw = inventory.read(MAX_INVENTORY_BYTES + 1)
    if len(raw) > MAX_INVENTORY_BYTES:
        raise ExportError("public export record exceeds the supported size")
    return raw, json.loads(raw, object_pairs_hook=unique_fields)


def native_placement(origin, placement):
    exact_fields(placement, "resource location")
    resource, location = placement["resource"], placement["location"]
    if not all(isinstance(value, str) for value in (origin, resource, location)):
        raise ExportError("invalid native bucket placement")
    if origin.startswith("s3://"):
        valid = (S3_REGION.fullmatch(location) and
                 re.fullmatch(r"accounts/[0-9]{12}/buckets/" + re.escape(origin[5:]), resource))
    elif origin.startswith("gs://"):
        valid = (re.fullmatch(r"[A-Z0-9-]{1,40}", location) and
                 re.fullmatch(r"projects/[1-9][0-9]*/buckets/" + re.escape(origin[5:]), resource))
    else:
        arm = AZURE_CONTAINER.fullmatch(resource)
        valid = (arm and re.fullmatch(r"[a-z0-9]{1,40}", location)
                 and origin == f"https://{arm[1]}.blob.core.windows.net/{arm[2]}")
    if not valid:
        raise ExportError("native placement does not match its origin")
    return placement


def parse_inventory(data, max_object):
    exact_fields(data, "schema source_origin source_placement repository_prefix objects")
    prefix = data["repository_prefix"]
    if (data["schema"] != SCHEMA or not isinstance(data["source_origin"], str)
            or not data["source_origin"] or not isinstance(prefix, str) or len(prefix) > 255
            or any(not SEGMENT.fullmatch(part) or part in (".", "..")
                   for part in prefix.split("/"))):
        raise ExportError("invalid inventory schema or repository prefix")
    entries = data["objects"]
    if not isinstance(entries, list) or not entries:
        raise ExportError("inventory must contain approved objects")
    objects = {}
    for entry in entries:
        exact_fields(entry, "path size sha256")
        obj = Object(**entry)
        if (not isinstance(obj.path, str) or not isinstance(obj.sha256, str)
                or not SHA256.fullmatch(obj.sha256) or type(obj.size) is not int
                or not 0 <= obj.size <= max_object):
            raise ExportError("invalid path, SHA-256 or size in inventory")
        git = GIT_KEY.fullmatch(obj.path)
        lfs = LFS_KEY.fullmatch(obj.path)
        if not ((git and git[2].startswith(git[1]))
                or (lfs and lfs[3] == obj.sha256 and lfs[3].startswith(lfs[1] + lfs[2]))):
            raise ExportError("inventory path is outside the approved immutable key grammar")
        if obj.path in objects:
            raise ExportError("duplicate inventory object path")
        objects[obj.path] = obj
    source = data["source_origin"]
    return source, native_placement(source, data["source_placement"]), prefix, tuple(objects.values())


def read_inventory(path, validate_origin, max_object):
    raw, data = read_document(path)
    source, placement, prefix, objects = parse_inventory(data, max_object)
    validate_origin(source)
    return source, placement, prefix, hashlib.sha256(raw).hexdigest(), objects


def viewer_url(value):
    if not isinstance(value, str) or len(value) > 261 or not HTTPS_HOST.fullmatch(value):
        raise ExportError("public export requires a canonical HTTPS viewer hostname")
    return value


def validate_public_origins(source, export):
    source_scheme, _, source_address = source.partition("://")
    export_scheme, _, export_address = export.partition("://")
    if source_scheme == "https":
        source_address = source_address.split("/", 1)[0]
        export_address = export_address.split("/", 1)[0]
    if (source_scheme not in ("s3", "gs", "https") or source_scheme != export_scheme
            or source_address == export_address):
        raise ExportError("public CDN requires a separate export bucket or Azure storage account in the source cloud")


def publication_receipt(source, export, source_placement, export_placement, prefix, base_url,
                        deployment, inventory_sha256, objects):
    source_placement = native_placement(source, source_placement)
    export_placement = native_placement(export, export_placement)
    validate_public_origins(source, export)
    return {
        "schema": PUBLICATION_SCHEMA,
        "source_origin": source,
        "source_placement": source_placement,
        "export_origin": export,
        "export_placement": export_placement,
        "repository_prefix": prefix,
        "base_url": base_url,
        "deployment": deployment,
        "inventory_sha256": inventory_sha256,
        "objects": [asdict(obj) for obj in sorted(objects, key=lambda obj: obj.path)],
    }


def read_publication(directory, approved_sha256):
    raw, receipt = read_document(directory / "publication.json")
    if hashlib.sha256(raw).hexdigest() != approved_sha256:
        raise ExportError("publication receipt differs from the preregistered SHA-256")
    exact_fields(receipt, "schema objects sealed_at_unix_ms " + " ".join(PUBLICATION_BINDING_FIELDS))
    viewer_url(receipt["base_url"])
    if any(not isinstance(receipt[key], str) or not receipt[key]
           for key in PUBLICATION_BINDING_FIELDS - {"source_placement", "export_placement"}):
        raise ExportError("invalid public export binding")
    sealed_at = receipt["sealed_at_unix_ms"]
    if type(sealed_at) is not int or sealed_at <= 0:
        raise ExportError("public export receipt needs its seal time")
    inventory_raw, inventory = read_document(directory / "inventory.json")
    source, source_placement, prefix, objects = parse_inventory(inventory, 2**64 - 1)
    expected = publication_receipt(source, receipt["export_origin"], source_placement, receipt["export_placement"],
                                   prefix, receipt["base_url"], receipt["deployment"],
                                   hashlib.sha256(inventory_raw).hexdigest(), objects)
    # Canonical JSON comparison also distinguishes booleans from integer sizes.
    if json.dumps(receipt, sort_keys=True) != json.dumps(expected | {"sealed_at_unix_ms": sealed_at}, sort_keys=True):
        raise ExportError("publication receipt does not match the exact approved inventory")
    return {name: receipt[name] for name in PUBLICATION_BINDING_FIELDS}, objects, sealed_at


def write_publication(path, publish):
    if path.exists() or path.is_symlink():
        raise ExportError("publication receipt path must be new")
    # Expose the complete receipt at a new path, even if publishers race.
    with TemporaryDirectory(prefix=".crab-export-publication-", dir=path.parent) as scratch:
        receipt = publish() | {"sealed_at_unix_ms": time.time_ns() // 1_000_000}
        raw = (json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n").encode()
        if len(raw) > MAX_INVENTORY_BYTES:
            raise ExportError("publication receipt exceeds the supported size")
        staged = Path(scratch) / "receipt.json"
        with staged.open("xb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.link(staged, path)
    return {
        "publication_sha256": hashlib.sha256(raw).hexdigest(),
        "inventory_sha256": receipt["inventory_sha256"],
        "objects": len(receipt["objects"]),
    }


def verify_body(obj, path):
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    if path.stat().st_size != obj.size or digest != obj.sha256:
        raise ExportError(f"inventory digest mismatch for {obj.path}")


def spool_part(source, path, size, body_digest, part_digest):
    with path.open("wb") as output:
        while size:
            block = source.read(min(size, 1024 * 1024))
            if not block:
                raise ExportError("source spool ended before the approved size")
            output.write(block)
            part_digest.update(block)
            body_digest.update(block)
            size -= len(block)
