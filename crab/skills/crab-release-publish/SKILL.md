---
name: crab-release-publish
description: Bump, build, publish, and verify Crab CLI releases and the Homebrew formula. Use for release artifacts, release notes, tags, cross-platform builds, GitHub release publication, and Homebrew updates.
---

# Crab release publishing

Treat a release as one traceable contract from version to checksums to
installable package. Build and verify before publishing; publish only after
the user has authorized the live operation.

## Artifact contract

The release contains:

- `crab-darwin-aarch64.tar.gz`
- `crab-darwin-x86_64.tar.gz`
- `crab-linux-aarch64.tar.gz`
- `crab-linux-x86_64.tar.gz`
- `crab-windows-aarch64.zip`
- `crab-windows-x86_64.zip`
- `SHA256SUMS.txt`

Darwin archives include the CLI and the supported FUSE helper. Linux archives
include the CLI. Windows archives include `crab.exe`. Never hand-copy binaries
or use `cargo install` as a release substitute.

## Release loop

1. Confirm the requested bump or explicit version, target platforms, artifact
   source, release destination, Homebrew update, and required evidence gates.
2. Check the checkout state and isolate unrelated changes. Confirm toolchain,
   cross targets, Docker or hosted-build availability, signing requirements,
   GitHub authentication, and archive utilities.
3. Bump the version through the canonical project target (`make bump-patch` or
   the explicit-version equivalent). Review version and lockfile changes.
4. Generate notes from the source history. Include tag, source commit, grouped
   changes, artifact list, and checksum reference.
5. Build into a clean volume-backed distribution directory. Keep Cargo output
   in a checkout-specific directory under `/Volumes/Workspace/crabbuild-target`
   when that volume is available; never silently fall back to a full local
   disk.
6. Verify the exact artifact set, archive contents, checksums, version output,
   and a smoke execution for each locally available binary.
7. Ask for confirmation immediately before live publication unless the user’s
   request already explicitly authorizes it. Upload all artifacts and
   `SHA256SUMS.txt` to the release destination, then update release notes.
8. Update Homebrew only after the matching tag has every artifact and checksum.
   Use the canonical Homebrew target and verify the rendered version and all
   archive digests from the remote release.

## Common targets

Use the project’s existing release targets rather than reconstructing them:

```text
make bump-patch
make release-macos-full
make release-macos-publish
make release-publish-dist
make release-ci
make release-list
make homebrew
make homebrew-local
make homebrew-dry
```

Use a hosted matrix when local Docker, cross targets, or platform SDKs are
missing. For a Homebrew-only request, do not rebuild or republish artifacts;
verify the release first and update only the formula.

## Verification

Read the published release back and confirm all seven assets, tag/version,
checksums, notes, and source commit. Read the Homebrew formula back from the
tap and compare the four supported archive digests. If Homebrew is installed,
run fetch/info or an equivalent resolution check. Report any unavailable audit
or platform test instead of implying it passed.

Never print tokens, cloud credentials, signing keys, or private evidence.
