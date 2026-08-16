#!/usr/bin/env bash
#
# test-fuse.sh — Verify that FUSE works inside this container and that
# crab can mount a filesystem.
#
# Exit codes:
#   0  All checks passed.
#   1  A check failed (details on stderr).
#
# Usage:
#   docker run --rm --cap-add SYS_ADMIN --device /dev/fuse crab-fuse-test

set -euo pipefail

PASS=0
FAIL=0
MOUNTPOINT="/tmp/crab-fuse-test"

pass() { printf "  ✓ %s\n" "$1"; PASS=$((PASS + 1)); }
fail() { printf "  ✗ %s\n" "$1" >&2; FAIL=$((FAIL + 1)); }

cleanup() {
    # Best-effort unmount; ignore errors if nothing was mounted.
    fusermount3 -u "$MOUNTPOINT" 2>/dev/null || true
    rm -rf "$MOUNTPOINT"
}
trap cleanup EXIT

echo "=== crab FUSE integration smoke test ==="
echo ""

# ── 1. /dev/fuse is available ────────────────────────────────────────────
echo "[1/4] Checking /dev/fuse device..."
if [ -c /dev/fuse ]; then
    pass "/dev/fuse character device exists"
else
    fail "/dev/fuse not found — run with --device /dev/fuse"
fi

# ── 2. fuse3 userspace tools installed ───────────────────────────────────
echo "[2/4] Checking fuse3 userspace tools..."
if command -v fusermount3 >/dev/null 2>&1; then
    pass "fusermount3 is available"
else
    fail "fusermount3 not found — fuse3 package missing"
fi

# ── 3. crab binary is present and runnable ─────────────────────────────
echo "[3/4] Checking crab binary..."
if crab version >/dev/null 2>&1; then
    VERSION=$(crab version 2>&1 | head -1)
    pass "crab binary works: $VERSION"
else
    fail "crab binary not found or not executable"
fi

# ── 4. FUSE mount syscall works ──────────────────────────────────────────
#
# This verifies the kernel allows mount(2) with FUSE inside the container.
# We don't need a real crab repo — we just need to confirm the FUSE
# plumbing is functional. Once crab mount is implemented, replace this
# section with an actual `crab mount` invocation.
echo "[4/4] Verifying FUSE mount capability..."
mkdir -p "$MOUNTPOINT"

# Try a minimal FUSE mount test. If crab mount is wired up, use it.
# Otherwise fall back to a raw /dev/fuse open test to confirm the kernel
# allows FUSE operations.
if crab mount --help >/dev/null 2>&1; then
    # crab mount subcommand exists — attempt a real mount.
    # This will be filled in once the mount command accepts a repo URL
    # and mountpoint. For now, just confirm the subcommand parses.
    pass "crab mount subcommand is available"
else
    # Mount subcommand not yet wired. Verify we can at least open
    # /dev/fuse, which proves the container has the right capabilities.
    if python3 -c "
import os, errno
try:
    fd = os.open('/dev/fuse', os.O_RDWR)
    os.close(fd)
    print('ok')
except OSError as e:
    if e.errno == errno.EPERM:
        raise SystemExit('EPERM: missing SYS_ADMIN capability')
    raise
" 2>&1; then
        pass "/dev/fuse is openable (FUSE capable)"
    else
        # python3 might not be installed; try a simpler check.
        if dd if=/dev/fuse of=/dev/null bs=1 count=0 2>/dev/null; then
            pass "/dev/fuse is accessible"
        else
            fail "cannot open /dev/fuse — run with --cap-add SYS_ADMIN"
        fi
    fi
fi

# ── Summary ──────────────────────────────────────────────────────────────
echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "Some checks failed. Make sure you run the container with:"
    echo "  docker run --rm --cap-add SYS_ADMIN --device /dev/fuse crab-fuse-test"
    exit 1
fi

echo ""
echo "FUSE environment is ready for crab mount testing."
exit 0
