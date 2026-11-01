#!/usr/bin/env python3
"""Qualify mixed ordinary Git, native Xet and LFS history on an isolated remote.

Use the LFS qualification driver's credential chain, command receipts and bounded
payload generation. The caller supplies a dedicated local RustFS bucket.
"""

from __future__ import annotations

import hashlib
import json
import sys

from run_lfs_qualification import Qualification, hash_file, parse_args, redact_text, write_deterministic


MIB = 1024 * 1024


class MixedHistory(Qualification):
    def snapshot(self) -> None:
        paths = ["code.txt", "large.raw", "model.xet", "renamed.xet",
                 "asset.bin", "renamed.bin", "merged.txt"]
        files = {name: {"sha256": hash_file(self.repo / name), "size": (self.repo / name).stat().st_size}
                 for name in paths if (self.repo / name).is_file()}
        revision = self.run_git(["rev-parse", "HEAD"], self.repo, "mixed revision").strip()
        self.report.setdefault("history", []).append({"revision": revision, "files": files})
        self.save()

    def create_history(self) -> None:
        size = self.args.min_size
        self.report["workload"].update({"profile": "mixed-history", "versions": 3,
                                       "object_count": 3, "commit_count": 5, "path_count": 1,
                                       "ordinary_git_blob_bytes": size, "xet_bytes": size,
                                       "lfs_bytes": size, "partial_edit_bytes": MIB})
        self.run_crab(["track", "*.xet"], self.repo, "track only Xet extension")
        for index, name in enumerate(("large.raw", "model.xet", "asset.bin")):
            write_deterministic(self.repo / name, size, self.args.seed, index)
        for version in range(3):
            native = "model.xet" if version == 0 else "renamed.xet"
            lfs = "asset.bin" if version == 0 else "renamed.bin"
            if version == 1:
                for old, new in (("model.xet", native), ("asset.bin", lfs)):
                    self.run_git(["mv", old, new], self.repo, f"rename {old}")
            if version:
                for name in (native, lfs, "large.raw"):
                    with (self.repo / name).open("r+b") as stream:
                        stream.seek(size // 2)
                        stream.write(hashlib.shake_256(f"{name}:{version}".encode()).digest(MIB))
            if version == 2:
                self.run_git(["rm", "-f", "large.raw"], self.repo, "delete ordinary large blob")
            (self.repo / "code.txt").write_text(f"mixed version {version}\n")
            self.run_crab(["add", native], self.repo, f"stage Xet version {version}")
            self.run_git(["add", "-A"], self.repo, f"stage mixed version {version}")
            self.run_git(["commit", "-m", f"mixed version {version}"], self.repo, f"commit mixed {version}")
            self.report["objects"].append({"index": version, "oid": hash_file(self.repo / lfs), "size": size})
            self.snapshot()
            if version % 2:
                self.run_git(["push", "origin", "main"], self.repo, "ordinary Git mixed push")
            else:
                self.run_crab(["push", "origin", "main"], self.repo, "native mixed push")
        self.run_git(["checkout", "-b", "feature"], self.repo, "create merge branch")
        (self.repo / "merged.txt").write_text("independent feature change\n")
        self.run_git(["add", "merged.txt"], self.repo, "stage feature")
        self.run_git(["commit", "-m", "feature change"], self.repo, "commit feature")
        self.run_git(["push", "origin", "feature"], self.repo, "publish feature")
        self.run_git(["checkout", "main"], self.repo, "return to main")
        self.run_git(["merge", "--no-ff", "feature", "-m", "merge mixed history"], self.repo, "merge feature")
        self.snapshot()
        self.run_git(["push", "origin", "main"], self.repo, "publish merge")

    def verify_source(self) -> None:
        local = self.run_git(["rev-parse", "HEAD"], self.repo, "local final ref").strip()
        refs = self.run_git(["ls-remote", "origin", "refs/heads/main"], self.repo, "remote final ref")
        self.check("final-ref-equality", bool(refs.split()) and refs.split()[0] == local)
        self.run_git(["fsck", "--strict", "--full"], self.repo, "source strict Git fsck")

    def verify_clone(self) -> None:
        clone = self.evidence_dir / "clone"
        self.env["CRAB_CACHE_DIR"] = str(self.evidence_dir / "cold-clone-cache")
        self.run_crab(["clone", "--lazy", self.args.remote_url, str(clone)], self.evidence_dir, "cold mixed clone")
        self.run_lfs(["fetch", "--all", "origin"], clone, "fetch complete LFS history")
        for index, entry in enumerate(self.report["history"]):
            self.env["CRAB_CACHE_DIR"] = str(self.evidence_dir / f"history-cache-{index}")
            self.run_git(["checkout", "--force", "--detach", entry["revision"]], clone, f"checkout history {index}")
            self.run_lfs(["checkout"], clone, f"checkout historical LFS {index}")
            self.run_crab(["hydrate", "--all"], clone, f"hydrate historical Xet {index}")
            for name, expected in entry["files"].items():
                path = clone / name
                self.check(f"history-{index}-{name}-byte-identity",
                           path.is_file() and path.stat().st_size == expected["size"]
                           and hash_file(path) == expected["sha256"], {"revision": entry["revision"]})
            self.check(f"history-{index}-rename-and-delete",
                       all((clone / name).exists() == (name in entry["files"])
                           for name in ("large.raw", "model.xet", "renamed.xet", "asset.bin", "renamed.bin")))
        self.run_git(["fsck", "--strict", "--full"], clone, "cold strict Git fsck")
        data = json.loads(self.run_crab(["fsck", "--json"], clone, "mixed Crab fsck")).get("data", {})
        self.check("mixed-fsck-clean-without-repair", data.get("passed") is True
                   and all(data.get(key) == 0 for key in ("errors", "repaired", "repair_failures")))


def main() -> int:
    args = parse_args(sys.argv[1:])
    if args.min_size != args.max_size or args.min_size < 2 * MIB:
        raise ValueError("mixed history requires equal min/max sizes of at least 2 MiB")
    qualification = MixedHistory(args)
    qualification.report.update(schema="crab.mixed-history-qualification", profile="mixed-history")
    try:
        qualification.run_all()
    except Exception as error:
        qualification.report.update(status="failed", error=redact_text(str(error), qualification.secrets))
        qualification.save()
        raise
    print(f"passed: {qualification.evidence_dir / 'report.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
