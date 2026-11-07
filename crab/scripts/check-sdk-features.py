#!/usr/bin/env python3
"""Check SDK normal dependency boundaries across all Cargo target platforms."""

import json
from pathlib import Path
import subprocess


PROFILES = {
    "default": (),
    "remote": ("remote",),
    "remote-files": ("remote", "files"),
    "publish": ("publish",),
    "local": ("local",),
    "service": ("service",),
    "service-remote": ("service", "remote"),
    "service-files": ("service", "remote", "files"),
    "service-publish": ("service", "publish"),
    "local-service": ("local", "service"),
}
HYDRATION = { "crab-lfs", "crab-cache", "crab-cache-store", "xet-data", "xet-client"}


def violations(profile, features, packages):
    forbidden = {name for name in packages if name in {"crab", "crab-vfs"}
                 or name.startswith("crab-") and name.endswith("-server")}
    required = {"crab-sdk"}
    if profile == "default":
        forbidden |= packages & {"tokio", "tokio-util", "object_store"}
        forbidden |= {name for name in packages if name.startswith("crab-") and name != "crab-sdk"}
    else:
        required |= {"crab-remote-git", "crab-storage", "crab-read"}
        if "files" not in features and "publish" not in features and "local" not in features:
            forbidden |= packages & HYDRATION
        else:
            required |= {"crab-read", "crab-lfs"}
        if "publish" in features or "local" in features:
            required |= {"crab-remote", "crab-write", "crab-coordination"}
        if "local" in features:
            required |= {"crab-remote", "tokio"}
        if "service" in features:
            required |= {"crab-auth", "crab-auth-store"}
    return sorted(forbidden), sorted(required - packages)


def main():
    root = Path(__file__).resolve().parents[2]
    reports = {}
    for profile, features in PROFILES.items():
        command = ["cargo", "tree", "-p", "crab-sdk", "--locked",
                   "--target", "all", "--edges", "normal", "--prefix", "none", "--format", "{p}"]
        if features:
            command += ["--no-default-features", "--features", ",".join(features)]
        result = subprocess.run(command, cwd=root, check=True, capture_output=True, text=True)
        packages = {line.split()[0] for line in result.stdout.splitlines() if line.strip()}
        forbidden, missing = violations(profile, set(features), packages)
        reports[profile] = {"packages": len(packages), "forbidden": forbidden, "missing": missing}
    print(json.dumps(reports, indent=2))
    raise SystemExit(1 if any(item["forbidden"] or item["missing"] for item in reports.values()) else 0)


if __name__ == "__main__":
    main()
