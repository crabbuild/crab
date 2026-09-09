#!/usr/bin/env python3
"""Generate the exact SDK qualification inputs with bounded memory."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import tempfile


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="existing external qualification directory")
    args = parser.parse_args()
    output = args.output.resolve(strict=True)
    if not output.is_dir():
        parser.error("output must be a directory")
    root = Path(__file__).resolve().parents[2]
    manifest = json.loads(
        (root / "crab/docs/architecture/sdk-qualification-fixtures.json").read_text()
    )
    if manifest["schema"] != "crab.sdk-qualification-fixtures" or manifest["version"] != 1:
        parser.error("unsupported qualification fixture manifest")
    for kind, specification in manifest["files"].items():
        destination = output / kind
        if destination.exists():
            parser.error(f"refusing to replace {destination}")
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=output, prefix=f".{kind}-", delete=False) as handle:
                temporary = Path(handle.name)
                digest = hashlib.sha256()
                remaining = specification["bytes"]
                index = 0
                while remaining:
                    count = min(manifest["block_bytes"], remaining)
                    seed = f"crab-sdk-fixture-v1/{kind}/{index}".encode("ascii")
                    block = hashlib.shake_256(seed).digest(count)
                    handle.write(block)
                    digest.update(block)
                    remaining -= count
                    index += 1
            actual = digest.hexdigest()
            if actual != specification["sha256"]:
                raise ValueError(f"{kind}: digest {actual} differs from qualification manifest")
            # Linking publishes without replacing an input created concurrently.
            destination.hardlink_to(temporary)
            print(f"{kind} bytes={specification['bytes']} sha256={actual}")
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
