#!/usr/bin/env python3
"""Verify and import one CI-qualified server image for a Compose fleet."""

import argparse
import hashlib
import json
import re
import subprocess
import tarfile
from datetime import datetime, timezone
from pathlib import Path

from qualify import command, image_provenance


def digest(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def artifact_metadata(artifact: Path) -> dict:
    source = (artifact / "source-revision").read_text().strip()
    platform = (artifact / "platform").read_text().strip()
    ci_id = (artifact / "image-id").read_text().strip()
    checksum = (artifact / "image.sha256").read_text().split()
    if not re.fullmatch(r"[0-9a-f]{40}", source):
        raise ValueError("CI artifact has an invalid source revision")
    if platform not in ("linux/amd64", "linux/arm64"):
        raise ValueError("CI artifact must contain one qualified Linux platform")
    if (len(checksum) != 2 or checksum[1] != "image.tar.gz"
            or not re.fullmatch(r"[0-9a-f]{64}", checksum[0])):
        raise ValueError("CI artifact checksum must name image.tar.gz")
    archive_path = artifact / "image.tar.gz"
    with archive_path.open("rb") as source_file:
        hasher = hashlib.sha256()
        for chunk in iter(lambda: source_file.read(1024 * 1024), b""):
            hasher.update(chunk)
    if hasher.hexdigest() != checksum[0]:
        raise ValueError("CI image archive checksum mismatch")

    # The current workflow's Docker save emits an OCI layout. Only accept its
    # single-manifest image contract; a platform index requires separate proof.
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        if len({member.name for member in members}) != len(members):
            raise ValueError("CI image archive has duplicate member names")

        def read(name):
            member = archive.getmember(name)
            if not member.isfile() or member.size > 1024 * 1024:
                raise ValueError(f"invalid or oversized image metadata: {name}")
            with archive.extractfile(member) as stream:
                return stream.read()

        def blob(descriptor):
            identity = descriptor["digest"]
            if not re.fullmatch(r"sha256:[0-9a-f]{64}", identity):
                raise ValueError("invalid image descriptor digest")
            data = read("blobs/sha256/" + identity.removeprefix("sha256:"))
            if len(data) != descriptor["size"] or digest(data) != identity:
                raise ValueError("image descriptor does not match its blob")
            return json.loads(data)

        if json.loads(read("oci-layout")) != {"imageLayoutVersion": "1.0.0"}:
            raise ValueError("CI image archive must use OCI image layout 1.0.0")
        index = json.loads(read("index.json"))
        if index["schemaVersion"] != 2 or len(index["manifests"]) != 1:
            raise ValueError("CI image archive must contain one image manifest")
        descriptor = index["manifests"][0]
        if descriptor["mediaType"] != "application/vnd.oci.image.manifest.v1+json":
            raise ValueError("CI image archive must contain a direct OCI image manifest")
        manifest = blob(descriptor)
        if manifest["schemaVersion"] != 2:
            raise ValueError("invalid image manifest schema")
        config = blob(manifest["config"])
        manifest_id = descriptor["digest"]
        config_id = manifest["config"]["digest"]
        if ci_id not in (manifest_id, config_id):
            raise ValueError("CI image ID is neither the verified manifest nor its config")
        if (config["config"].get("Labels") or {}).get("org.opencontainers.image.revision") != source:
            raise ValueError("image config source revision differs from CI source receipt")
        if f"{config['os']}/{config['architecture']}" != platform:
            raise ValueError("image config platform differs from CI platform receipt")
        # Docker's two stores consume different archive entry points. Verify
        # they select the same config and ordered layers before invoking load.
        docker = json.loads(read("manifest.json"))
        layers = ["blobs/sha256/" + layer["digest"].removeprefix("sha256:") for layer in manifest["layers"]]
        if (len(docker) != 1 or docker[0]["Config"] != "blobs/sha256/" + config_id[7:]
                or docker[0]["Layers"] != layers):
            raise ValueError("Docker and OCI archive manifests select different images")
    return {
        "source": source, "platform": platform, "archive_sha256": checksum[0],
        "ci_image_id": ci_id, "manifest_digest": manifest_id, "config_digest": config_id,
    }


def import_image(artifact: Path, project: str, output: Path) -> dict:
    if not re.fullmatch(r"crab-cell-issue-[a-z0-9-]+", project):
        raise ValueError("project must be named crab-cell-issue-*")
    if output.exists():
        raise ValueError(f"import receipt already exists: {output}")
    receipt = artifact_metadata(artifact)
    command("docker", "image", "load", "--input", str(artifact / "image.tar.gz"))
    # CI recorded the config ID while Colima exposed the manifest ID. Accept
    # only those two content identities from this verified archive, never a tag.
    known = {receipt["config_digest"], receipt["manifest_digest"]}
    installed = set(command("docker", "image", "ls", "--quiet", "--no-trunc").splitlines())
    candidates = known & installed
    if len(candidates) != 1:
        raise ValueError("Docker did not expose exactly one verified image identity")
    image = image_provenance(candidates.pop())
    if image["image"] not in known:
        raise ValueError("Docker inspect returned an unverified image identity")
    if image["source"] != receipt["source"] or image["platform"] != receipt["platform"]:
        raise ValueError("imported image differs from the verified source or platform")
    tag = f"{project}:local"
    command("docker", "image", "tag", image["image"], tag)
    receipt.update(imported_at=datetime.now(timezone.utc).isoformat(), image=image["image"], tag=tag)
    with output.open("x") as destination:
        destination.write(json.dumps(receipt, indent=2) + "\n")
    return receipt


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--output", type=Path, help="receipt path; defaults to artifact/import-receipt.json")
    args = parser.parse_args()
    artifact = args.artifact.expanduser().resolve()
    output = args.output.expanduser().resolve() if args.output else artifact / "import-receipt.json"
    try:
        import_image(artifact, args.project, output)
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Image import failed: {error}\n")
    print(output)


if __name__ == "__main__":
    main()
