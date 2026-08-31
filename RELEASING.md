# Releasing Crab

Crab releases are built and published by `.github/workflows/release.yml`. The
workflow is the only release publisher. A stable annotated tag reachable from
`main` starts the pipeline; local release tooling only builds artifacts.

## Release contract

- Stable tags use `vMAJOR.MINOR.PATCH` and are annotated.
- The CLI, auth-server, and cache-server manifests, their `Cargo.lock` entries,
  the tag, and the dated `CHANGELOG.md` section carry the same version.
- The release workflow builds six native archives: ARM64 and x86_64 for macOS,
  Linux, and Windows. It executes the packaged CLI, validates archive layouts,
  generates `SHA256SUMS.txt`, and records build-provenance attestations.
- Self-contained RustFS, native workflow, NFS feature, partial-clone, rollback,
  and Git compatibility gates must pass before publication.
- Retained enterprise, native NFS, and live AWS/cross-platform evidence are
  opt-in gates. Enable them with the corresponding workflow-dispatch input or
  repository variable when a release claims those environments.
- GitHub releases and their assets are immutable after publication. A failed
  run may replace its incomplete draft, but it cannot replace a published
  release.

## Prepare a release

Create a release branch from current `main`, then run:

```bash
cd crab
make bump-set VERSION=1.2.3
```

Move the `CHANGELOG.md` entries from `Unreleased` into a dated `1.2.3` section.
Validate the metadata and release contracts:

```bash
cd crab
make release-metadata-check
make release-archive-contents-check
```

Run the normal Rust, architecture, and web checks for the touched surfaces.
Merge the release change through the usual pull-request process.

## Publish

From an up-to-date, clean `main`, create and push the annotated tag:

```bash
git pull --rebase origin main
git tag -a v1.2.3 -m "Release Crab v1.2.3"
git push origin v1.2.3
```

Do not create the GitHub release manually. The tag starts the release workflow,
which publishes to `crabbuild/crab` and updates Homebrew when
`TAP_GITHUB_TOKEN` is configured.

For a failed run, fix the cause without moving or recreating the tag, merge the
fix to `main`, and publish a new version. If the failure was transient and the
tagged source needs no change, rerun the failed workflow from GitHub Actions or
dispatch it with `make release` while checked out at the tagged version.

## Verify

Download the release into a new directory, verify every checksum, and execute
the platform binary. GitHub provenance can be checked with:

```bash
gh release verify v1.2.3 --repo crabbuild/crab
gh attestation verify crab-linux-x86_64.tar.gz --repo crabbuild/crab
```

Confirm that the release is marked latest and immutable, the public installers
resolve the new tag, and `crab update --check` reports the new version from an
older installation.
