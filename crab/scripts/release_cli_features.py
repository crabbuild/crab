"""Cargo feature contract for the main executable in CLI release archives."""

from __future__ import annotations

from pathlib import Path


UNIX_CLI_FLAGS = (
    "simd-accel,tier,replication-s3-control-plane,replication-gcs-control-plane,"
    "replication-azure-control-plane,coordinator-dynamodb,coordinator-spanner,"
    "coordinator-cosmosdb,watch,nfs,gix-pathmatch"
)
WINDOWS_CLI_FLAGS = "simd-accel,tier,watch,nfs,gix-pathmatch"


def packaged_cli_features(target: str) -> str:
    """Resolve the packaged CLI flags into the sorted feature list Cargo embeds."""
    import tomllib

    manifest = Path(__file__).resolve().parents[1] / "Cargo.toml"
    features = tomllib.loads(manifest.read_text(encoding="utf-8"))["features"]
    flags = WINDOWS_CLI_FLAGS if target.endswith("-pc-windows-msvc") else UNIX_CLI_FLAGS
    pending = flags.split(",")
    enabled: set[str] = set()
    while pending:
        name = pending.pop()
        if name in enabled:
            continue
        enabled.add(name)
        # Only same-package aliases contribute to the CLI's CARGO_CFG_FEATURE.
        pending.extend(feature for feature in features[name] if feature in features)
    return ",".join(sorted(enabled))
