"""Stock Git LFS large-object proof shared by native process transitions."""

import hashlib
import shutil
import subprocess
import urllib.request

ACTIVITY_TIMEOUT_SECONDS = 30 * 60


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            result.update(chunk)
    return result.hexdigest()


def prepare(cmd, root, origin, size, env):
    source = root / "large-file-source"
    source.mkdir()
    client_env = env.copy()
    client_env.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")

    def git(*arguments):
        cmd(["git", *arguments], cwd=source, env=client_env)

    git("init", "--initial-branch=main")
    git("config", "user.name", "Large file qualification")
    git("config", "user.email", "large-file@example.invalid")
    git("lfs", "install", "--local")
    # Publication can continue after the body is received. Match the server's
    # verification budget so a slow store does not trigger overlapping retries.
    git("config", "lfs.activitytimeout", str(ACTIVITY_TIMEOUT_SECONDS))
    git("lfs", "track", "*.bin")
    git("remote", "add", "origin", origin + "/git/team/large-files.git")
    path = source / "large.bin"
    hasher = hashlib.sha256()
    remaining = size
    index = 0
    with path.open("wb") as output:
        while remaining:
            chunk = hashlib.shake_256(index.to_bytes(8, "big")).digest(
                min(1024 * 1024, remaining)
            )
            output.write(chunk)
            hasher.update(chunk)
            remaining -= len(chunk)
            index += 1
    oid = hasher.hexdigest()
    git("add", ".gitattributes", "large.bin")
    git("commit", "-m", "Publish large LFS object")
    pointer = subprocess.check_output(
        ["git", "show", "HEAD:large.bin"], cwd=source, env=client_env, text=True
    )
    assert f"oid sha256:{oid}\nsize {size}\n" in pointer
    git("push", "origin", "main")
    return {"bytes": size, "sha256": oid, "activity_timeout_seconds": ACTIVITY_TIMEOUT_SECONDS}


def verify(cmd, root, origin, content, phase, env):
    client_env = env.copy()
    client_env.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
    destination = root / ("large-file-" + phase)
    cmd(
        [
            "git", "-c", "protocol.version=2",
            "-c", f"lfs.activitytimeout={ACTIVITY_TIMEOUT_SECONDS}",
            "-c", "filter.lfs.process=git-lfs filter-process",
            "-c", "filter.lfs.required=true",
            "clone", origin + "/git/team/large-files.git", destination,
        ],
        env=client_env,
    )
    path = destination / "large.bin"
    assert (path.stat().st_size, digest(path)) == (content["bytes"], content["sha256"])
    cmd(["git", "lfs", "fsck"], cwd=destination, env=client_env)
    # Discard only this phase's synthetic checkout after proving independent
    # hydration. Large proof runs must not retain another full payload per phase.
    shutil.rmtree(destination)


def verify_resume(root, origin, content):
    size, oid = content["bytes"], content["sha256"]
    url = origin + f"/git/team/large-files.git/info/lfs/objects/{oid}?size={size}"
    destination = root / "large-file-resumed.bin"
    split = min(1024 * 1024, size - 1)
    for start, end, mode in [(0, split - 1, "wb"), (split, size - 1, "ab")]:
        request = urllib.request.Request(url, headers={"Range": f"bytes={start}-{end}"})
        with urllib.request.urlopen(request, timeout=1800) as response, destination.open(mode) as output:
            assert response.status == 206
            assert response.headers["Content-Range"] == f"bytes {start}-{end}/{size}"
            while chunk := response.read(1024 * 1024):
                output.write(chunk)
    assert (destination.stat().st_size, digest(destination)) == (size, oid)
    destination.unlink()
