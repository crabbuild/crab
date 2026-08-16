---
name: crab-release-publish
description: Publish Crab CLI releases to crabbuild/crab-release and keep the Homebrew tap/formula updated. Use this whenever the user asks to bump a Crab version, build OS release artifacts, generate a changelog or release notes, publish/upload artifacts to crab-release, update Homebrew/homebrew-tap/Formula/crab.rb after a release, run release-macos-publish, release-publish-dist, release-ci, make homebrew, or says "bump patch and release Crab".
compatibility: CrabBuild repository on macOS with Git, Rust, gh CLI, and the repo release scripts.
metadata:
  author: CrabBuild
  version: "1.1"
---

# Crab Release Publish

Use this skill to run the Crab CLI release workflow repeatably and carefully.
The release publishes artifacts to `crabbuild/crab-release`, so treat the final
publish as a live operation that needs explicit confirmation after dry-run proof.

## First Moves

1. Work from the CrabBuild repo root.
2. Inspect the current release contract before acting:
   - `crab/Makefile`
   - `crab/scripts/release.sh`
   - `crab/scripts/update-homebrew.sh`
   - `.github/workflows/release.yml`
   - `crab/scripts/bump-version.sh`
3. Check worktree state with `git status --short`. Do not overwrite unrelated user changes.
   If `crab/Cargo.toml`, `Cargo.lock`, or release scripts already have user changes,
   read them before deciding how to proceed.
4. Confirm the requested release shape:
   - Patch/minor/major/explicit version bump.
   - Local macOS publish, or publish existing release dist artifacts.
   - Release artifacts only, Homebrew-only for an already-published tag, or full release plus Homebrew.
   - Whether enterprise evidence gates are required.

If the user says "bump patch and build the OS releases, then publish artifacts
to crab-release", default to patch bump plus the macOS local full-matrix publish
path unless prerequisites are missing. Do not ask for a second confirmation for Homebrew
when the user already confirmed the release publish; the tap update is part of
making the release installable.

## Release Contract

The CLI release artifact set is exactly:

- `crab-darwin-aarch64.tar.gz`
- `crab-darwin-x86_64.tar.gz`
- `crab-linux-aarch64.tar.gz`
- `crab-linux-x86_64.tar.gz`
- `crab-windows-aarch64.zip`
- `crab-windows-x86_64.zip`
- `SHA256SUMS.txt`

macOS archives contain `crab` and `crab-fuse-mount`.
Linux archives contain `crab`.
Windows archives contain `crab.exe`.

Use existing repo commands instead of hand-built artifacts:

```bash
cd crab
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

Never use `cargo install` for Crab release preparation.
Never manually copy binaries into release artifacts.
Never print GitHub tokens, AWS credentials, or evidence bundle secrets.
Never update the Homebrew tap before the matching `crabbuild/crab-release` tag
has all release assets and `SHA256SUMS.txt`; formula URLs must point at
published artifacts.

## macOS Build Directory

On this machine, use the large volume-backed build tree:

```bash
export CARGO_TARGET_DIR=/Volumes/Workspace/CrabBuild/target
export CRAB_RELEASE_DIST_DIR=/Volumes/Workspace/CrabBuild/target/crab-release-dist
export DIST_DIR="$CRAB_RELEASE_DIST_DIR"
export RELEASE_DIST_DIR="$CRAB_RELEASE_DIST_DIR"
```

Do not use the repo-local `target/` directory under the current CrabBuild
checkout for release builds; it does not have enough space. Put release assets
under `/Volumes/Workspace/CrabBuild/target`, then publish assets from that
volume-backed dist directory.

Keep artifacts in the clean `crab-release-dist` subdirectory rather than using
`/Volumes/Workspace/CrabBuild/target` itself as `DIST_DIR`, because
`crab/scripts/release.sh` removes `DIST_DIR` before rebuilding artifacts.

## Prerequisite Check

Before a local macOS release, check:

```bash
test -d /Volumes/Workspace/CrabBuild || { echo "missing mounted workspace: /Volumes/Workspace/CrabBuild"; exit 2; }
mkdir -p /Volumes/Workspace/CrabBuild/target
uname -s
gh auth status
gh repo view crabbuild/crab-release
rustc -vV
rustup target list --installed
cargo xwin --version
docker info
command -v zip
command -v shasum || command -v sha256sum
```

Local full-matrix macOS release needs:

- macOS host.
- `gh` authenticated with access to `crabbuild/crab-release`.
- Docker running for Linux artifacts.
- `cargo-xwin`, LLVM tools, `zip`, and installed Windows Rust targets.
- Installed macOS Rust targets for both Darwin arches.
- macFUSE/pkg-config support for Darwin FUSE builds.

If prerequisites are missing, report the exact missing dependency and choose one:

- Install the missing local dependency if the user asked to continue locally.
- Dispatch hosted release workflow with `make release-ci` if local full matrix is not practical.
- Build a non-publishing smoke with `make release-build` only when the user asks for proof, not for release.

## Version Bump

For patch release:

```bash
cd crab
make bump-patch
```

For explicit version:

```bash
cd crab
make bump-set VERSION=1.2.3
```

After bumping:

1. Read the new version from `crab/Cargo.toml`.
2. Let Cargo update `Cargo.lock` through the normal build/test command if needed.
3. Review version-related diffs before release.
4. Do not stage or commit unrelated user changes.

## Changelog / Release Notes

Generate release notes before publishing, then ensure `crabbuild/crab-release`
uses those notes after artifacts are created/uploaded. The release repo contains
artifacts, while the source history lives in `crabbuild/crab`, so do not rely on
`--generate-notes` against `crabbuild/crab-release` for meaningful changelog text.

Suggested notes flow:

```bash
git fetch --tags origin
tag="v$(sed -n 's/^version = "\(.*\)"/\1/p' crab/Cargo.toml | head -1)"
previous_tag="$(gh release list --repo crabbuild/crab-release --limit 20 --json tagName --jq '.[].tagName' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1 || true)"
notes_file="$(mktemp)"
.codex/skills/crab-release-publish/scripts/generate-release-notes.sh "$tag" "$previous_tag" "$notes_file"
```

If GitHub generated notes fail, use a local fallback:

```bash
git fetch --tags origin
git log --no-merges --format='- %s (%h)' "${previous_tag}..HEAD"
```

Release notes should include:

- Release title and version tag.
- Human-readable changelog grouped from commits or GitHub generated notes.
- Source repo link for the tag.
- Source commit SHA.
- Artifact list.
- Checksums or a note that `SHA256SUMS.txt` is attached.

After a successful publish, update the `crab-release` release notes:

```bash
gh release edit "$tag" --repo crabbuild/crab-release --notes-file "$notes_file"
```

## Homebrew Tap Automation

The Homebrew tap is `crabbuild/homebrew-tap`. The formula is rendered and pushed
by `crab/scripts/update-homebrew.sh`; do not edit `Formula/crab.rb` by hand.

Use this after a release is already published, or immediately after either local
or existing-dist publishing succeeds:

```bash
cd crab
./scripts/update-homebrew.sh "$tag"
```

Use a dry run before pushing when the user asks to preview, when diagnosing a
formula problem, or when release/checksum state looks surprising:

```bash
cd crab
./scripts/update-homebrew.sh --dry-run "$tag"
```

Use local checksums only when the release dist was already verified and those
exact files are being or have just been published:

```bash
cd crab
DIST_DIR="$CRAB_RELEASE_DIST_DIR" ./scripts/update-homebrew.sh --local "$tag"
```

Homebrew formula proof should show:

- `gh release view "$tag" --repo crabbuild/crab-release` lists all six archives and `SHA256SUMS.txt`.
- The rendered formula version matches the tag without the leading `v`.
- The four Homebrew archive checksums match the release asset digests or `SHA256SUMS.txt`.
- `gh api repos/crabbuild/homebrew-tap/contents/Formula/crab.rb -H 'Accept: application/vnd.github.raw'` reads back the pushed formula.
- If Homebrew is installed, `brew update`, `brew fetch --formula crabbuild/tap/crab --force`, and `brew info --formula crabbuild/tap/crab` resolve the new version.

If `brew audit` is feasible, run it. If local Xcode Command Line Tools or
Homebrew state blocks audit, report that blocker and keep the GitHub readback
plus `brew fetch`/`brew info` proof.

For a Homebrew-only request such as "v1.0.4 is already published, update brew",
do not rebuild artifacts. Verify the release exists, render/push the formula
with `./scripts/update-homebrew.sh "$tag"`, then verify the tap readback.

## Local macOS Publish Path

Use this when the user wants to release from their Mac and local prerequisites are present.

1. Run prerequisite checks.
2. Bump the version.
3. Generate release notes.
4. Run a dry full-matrix build:

   ```bash
   cd crab
   export CARGO_TARGET_DIR=/Volumes/Workspace/CrabBuild/target
   export CRAB_RELEASE_DIST_DIR=/Volumes/Workspace/CrabBuild/target/crab-release-dist
   export DIST_DIR="$CRAB_RELEASE_DIST_DIR"
   export RELEASE_DIST_DIR="$CRAB_RELEASE_DIST_DIR"
   make release-macos-full
   ```

5. Verify the volume-backed release dist contains exactly the release artifact set and checksums:

   ```bash
   cd crab
   find "$CRAB_RELEASE_DIST_DIR" -maxdepth 1 -type f | sort
   (cd "$CRAB_RELEASE_DIST_DIR" && shasum -a 256 -c SHA256SUMS.txt)
   make release-archive-contents-check
   ```

6. Show the user:
   - Version and tag.
   - Artifact list.
   - Evidence gate status.
   - Changelog preview.
   - Any worktree changes that will be included.
7. Ask for explicit confirmation before publishing.
8. Publish:

   ```bash
   cd crab
   export CARGO_TARGET_DIR=/Volumes/Workspace/CrabBuild/target
   export CRAB_RELEASE_DIST_DIR=/Volumes/Workspace/CrabBuild/target/crab-release-dist
   export DIST_DIR="$CRAB_RELEASE_DIST_DIR"
   export RELEASE_DIST_DIR="$CRAB_RELEASE_DIST_DIR"
   make release-macos-publish
   ```

9. Update notes on `crabbuild/crab-release` with the generated notes file.
10. Update the Homebrew tap:

    ```bash
    cd crab
    ./scripts/update-homebrew.sh "$tag"
    ```

11. Verify:

    ```bash
    gh release view "$tag" --repo crabbuild/crab-release
    gh release download "$tag" --repo crabbuild/crab-release --pattern SHA256SUMS.txt --dir /tmp/crab-release-check
    gh api repos/crabbuild/homebrew-tap/contents/Formula/crab.rb -H 'Accept: application/vnd.github.raw'
    brew update
    brew fetch --formula crabbuild/tap/crab --force
    brew info --formula crabbuild/tap/crab
    ```

## Publish Existing Dist

Use this only when the volume-backed release dist was already built for the same version and you have verified it.

```bash
cd crab
export CRAB_RELEASE_DIST_DIR=/Volumes/Workspace/CrabBuild/target/crab-release-dist
export RELEASE_DIST_DIR="$CRAB_RELEASE_DIST_DIR"
(cd "$CRAB_RELEASE_DIST_DIR" && shasum -a 256 -c SHA256SUMS.txt)
make release-publish-dist
```

Then update the release notes with the generated notes file, update Homebrew
with `./scripts/update-homebrew.sh "$tag"`, and verify the tap readback.

## Evidence Gates

Default release-grade local publish expects retained evidence unless the user
explicitly authorizes a bypass for non-release smoke work.

Important variables:

- `REPLICA_RELEASE_EVIDENCE_DIR`
- `REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID`
- `CACHE_SERVICE_RELEASE_EVIDENCE_DIR`
- `CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID`
- `RELEASE_BYPASS_EVIDENCE=1` only for explicitly approved non-release smoke or emergency cases.

If evidence is missing for a release-grade publish, stop and explain the missing
run or directory. Do not silently bypass evidence.

## Final Report

For a completed release, report:

- Version/tag.
- Release URL on `crabbuild/crab-release`.
- Source commit.
- Artifact names.
- Changelog/release notes status.
- Homebrew tap commit or readback status, plus install/upgrade commands.
- Proof run: build command, checksum verification, release view/download check, Homebrew formula readback/fetch.
- Any skipped proof and why.

For a blocked release, report the first actionable blocker and the safest next command.
