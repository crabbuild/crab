//! Manifest-backed self-upgrade for official Crab releases.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use flate2::read::GzDecoder;
use futures_util::StreamExt;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::core::error::{CrabError, Result};

const RELEASE_MANIFEST_URL: &str =
    "https://github.com/crabbuild/crab/releases/latest/download/crab-release.json";
const RELEASE_DOWNLOAD_BASE_URL: &str = "https://github.com/crabbuild/crab/releases/download";
const RELEASE_MANIFEST_SCHEMA: &str = "crab.release/1";
const MANIFEST_LIMIT: usize = 64 * 1024;
const ARCHIVE_LIMIT: u64 = 512 * 1024 * 1024;
const BINARY_LIMIT: u64 = 512 * 1024 * 1024;
const MAX_RELEASE_ARTIFACTS: usize = 32;
const MAX_TARGET_LENGTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveFormat {
    TarGz,
    Zip,
}

#[derive(Clone, Copy, Debug)]
struct PublishedTarget {
    triple: &'static str,
    archive: &'static str,
    format: ArchiveFormat,
}

const PUBLISHED_TARGETS: [PublishedTarget; 6] = [
    PublishedTarget {
        triple: "aarch64-apple-darwin",
        archive: "crab-darwin-aarch64.tar.gz",
        format: ArchiveFormat::TarGz,
    },
    PublishedTarget {
        triple: "aarch64-pc-windows-msvc",
        archive: "crab-windows-aarch64.zip",
        format: ArchiveFormat::Zip,
    },
    PublishedTarget {
        triple: "aarch64-unknown-linux-gnu",
        archive: "crab-linux-aarch64.tar.gz",
        format: ArchiveFormat::TarGz,
    },
    PublishedTarget {
        triple: "x86_64-apple-darwin",
        archive: "crab-darwin-x86_64.tar.gz",
        format: ArchiveFormat::TarGz,
    },
    PublishedTarget {
        triple: "x86_64-pc-windows-msvc",
        archive: "crab-windows-x86_64.zip",
        format: ArchiveFormat::Zip,
    },
    PublishedTarget {
        triple: "x86_64-unknown-linux-gnu",
        archive: "crab-linux-x86_64.tar.gz",
        format: ArchiveFormat::TarGz,
    },
];

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    schema: String,
    version: String,
    tag: String,
    artifacts: Vec<ReleaseArtifact>,
}

#[derive(Debug, Deserialize)]
struct ReleaseArtifact {
    target: String,
    archive: String,
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VersionDecision {
    Upgrade,
    Current,
    Newer,
}

#[derive(Debug)]
struct ReleasePlan {
    version: Version,
    tag: String,
    archive: String,
    sha256: String,
    bytes: u64,
    target: &'static PublishedTarget,
}

#[derive(Debug)]
struct UpgradeCheck {
    current: Version,
    decision: VersionDecision,
    plan: ReleasePlan,
    client: reqwest::Client,
}

struct StagedBinaries {
    crab: PathBuf,
    companions: Vec<(&'static str, PathBuf)>,
}

struct CompanionReplacement {
    destination: PathBuf,
    backup: Option<PathBuf>,
}

/// Upgrade the running Crab executable to the latest stable release.
pub async fn run_upgrade() -> Result<()> {
    let check = check_latest().await?;
    match check.decision {
        VersionDecision::Current => {
            println!("Crab {} is already the latest version.", check.current);
        }
        VersionDecision::Newer => {
            println!(
                "Crab {} is newer than the latest release ({}); no downgrade was performed.",
                check.current, check.plan.version
            );
        }
        VersionDecision::Upgrade => {
            let current = check.current.clone();
            let latest = check.plan.version.clone();
            install(&check).await?;
            println!("Upgraded Crab from {current} to {latest}.");
        }
    }
    Ok(())
}

async fn check_latest() -> Result<UpgradeCheck> {
    let target = current_target()?;
    let current = parse_version(env!("CRAB_BUILD_VERSION"), "installed Crab version")?;
    let client = http_client()?;
    let bytes = fetch_bounded(&client, RELEASE_MANIFEST_URL, MANIFEST_LIMIT).await?;
    let plan = parse_release_manifest(&bytes, target)?;
    let decision = version_decision(&current, &plan.version);
    Ok(UpgradeCheck {
        current,
        decision,
        plan,
        client,
    })
}

async fn install(check: &UpgradeCheck) -> Result<()> {
    let temporary = tempfile::tempdir().map_err(CrabError::Io)?;
    let archive_path = temporary.path().join(&check.plan.archive);
    let archive_url = format!(
        "{RELEASE_DOWNLOAD_BASE_URL}/{}/{}",
        check.plan.tag, check.plan.archive
    );
    let (downloaded, digest) =
        download_bounded(&check.client, &archive_url, &archive_path, ARCHIVE_LIMIT).await?;
    if downloaded != check.plan.bytes {
        return Err(internal(format!(
            "release archive size mismatch for {}: expected {} bytes, downloaded {downloaded}",
            check.plan.archive, check.plan.bytes
        )));
    }
    verify_digest(&digest, &check.plan.sha256)?;

    let staged_dir = temporary.path().join("staged");
    fs::create_dir(&staged_dir).map_err(CrabError::Io)?;
    let staged = extract_release(&archive_path, check.plan.target, &staged_dir)?;
    validate_executable(&staged.crab, &check.plan.version)?;
    replace_installation(&staged)
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("crab/{}", env!("CRAB_BUILD_VERSION")))
        .timeout(Duration::from_secs(5 * 60))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|error| internal(format!("could not create release client: {error}")))
}

async fn fetch_bounded(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| internal(format!("could not download Crab release manifest: {error}")))?;
    if !response.status().is_success() {
        return Err(internal(format!(
            "could not download Crab release manifest: HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(internal(format!(
            "Crab release manifest exceeded {limit} bytes"
        )));
    }

    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| internal(format!("could not read Crab release manifest: {error}")))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(internal(format!(
                "Crab release manifest exceeded {limit} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn download_bounded(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    limit: u64,
) -> Result<(u64, String)> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .map_err(|error| internal(format!("could not download release archive: {error}")))?;
    if !response.status().is_success() {
        return Err(internal(format!(
            "could not download release archive: HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(internal(format!("release archive exceeded {limit} bytes")));
    }

    let mut output = File::create(destination).map_err(CrabError::Io)?;
    let mut digest = Sha256::new();
    let mut written = 0_u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| internal(format!("could not read release archive: {error}")))?;
        written = written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| internal("release archive size overflowed".to_owned()))?;
        if written > limit {
            return Err(internal(format!("release archive exceeded {limit} bytes")));
        }
        output.write_all(&chunk).map_err(CrabError::Io)?;
        digest.update(&chunk);
    }
    output.sync_all().map_err(CrabError::Io)?;
    let digest: [u8; 32] = digest.finalize().into();
    Ok((written, crab_git::lfs_pointer::hex_encode(&digest)))
}

fn parse_release_manifest(bytes: &[u8], target: &'static PublishedTarget) -> Result<ReleasePlan> {
    let manifest: ReleaseManifest = serde_json::from_slice(bytes)
        .map_err(|error| internal(format!("invalid Crab release manifest: {error}")))?;
    release_plan(manifest, target)
}

fn release_plan(
    manifest: ReleaseManifest,
    target: &'static PublishedTarget,
) -> Result<ReleasePlan> {
    if manifest.schema != RELEASE_MANIFEST_SCHEMA {
        return Err(internal(format!(
            "unsupported Crab release manifest schema '{}'",
            manifest.schema
        )));
    }
    let version = parse_version(&manifest.version, "Crab release version")?;
    if !version.pre.is_empty() {
        return Err(internal(format!(
            "Crab release manifest version '{}' is a prerelease",
            manifest.version
        )));
    }
    let expected_tag = format!("v{version}");
    if manifest.tag != expected_tag {
        return Err(internal(format!(
            "Crab release manifest tag '{}' does not match version {version}",
            manifest.tag
        )));
    }
    if manifest.artifacts.is_empty() || manifest.artifacts.len() > MAX_RELEASE_ARTIFACTS {
        return Err(internal(format!(
            "Crab release manifest must contain between 1 and {MAX_RELEASE_ARTIFACTS} artifacts"
        )));
    }

    let mut seen = BTreeSet::new();
    let mut selected = None;
    for artifact in manifest.artifacts {
        validate_artifact(&artifact)?;
        if !seen.insert(artifact.target.clone()) {
            return Err(internal(format!(
                "Crab release manifest contains duplicate target '{}'",
                artifact.target
            )));
        }
        if let Some(published) = published_target(&artifact.target)
            && artifact.archive != published.archive
        {
            return Err(internal(format!(
                "Crab release manifest archive '{}' does not match target '{}'",
                artifact.archive, artifact.target
            )));
        }
        if artifact.target == target.triple {
            selected = Some(ReleasePlan {
                version: version.clone(),
                tag: expected_tag.clone(),
                archive: artifact.archive,
                sha256: artifact.sha256,
                bytes: artifact.bytes,
                target,
            });
        }
    }
    selected.ok_or_else(|| {
        internal(format!(
            "Crab release manifest is missing target '{}'",
            target.triple
        ))
    })
}

fn validate_artifact(artifact: &ReleaseArtifact) -> Result<()> {
    if artifact.target.is_empty()
        || artifact.target.len() > MAX_TARGET_LENGTH
        || !artifact.target.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(internal(format!(
            "Crab release manifest contains invalid target '{}'",
            artifact.target
        )));
    }
    if Path::new(&artifact.archive)
        .file_name()
        .and_then(|name| name.to_str())
        != Some(artifact.archive.as_str())
        || !artifact.archive.starts_with("crab-")
        || !(artifact.archive.ends_with(".tar.gz") || artifact.archive.ends_with(".zip"))
    {
        return Err(internal(format!(
            "Crab release manifest contains invalid archive '{}'",
            artifact.archive
        )));
    }
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(internal(format!(
            "Crab release manifest contains an invalid SHA-256 digest for '{}'",
            artifact.target
        )));
    }
    if artifact.bytes == 0 || artifact.bytes > ARCHIVE_LIMIT {
        return Err(internal(format!(
            "Crab release manifest contains an invalid archive size for '{}'",
            artifact.target
        )));
    }
    Ok(())
}

fn parse_version(raw: &str, label: &str) -> Result<Version> {
    Version::parse(raw).map_err(|error| internal(format!("invalid {label}: {error}")))
}

fn version_decision(current: &Version, latest: &Version) -> VersionDecision {
    match current.cmp(latest) {
        std::cmp::Ordering::Less => VersionDecision::Upgrade,
        std::cmp::Ordering::Equal => VersionDecision::Current,
        std::cmp::Ordering::Greater => VersionDecision::Newer,
    }
}

fn published_target(target: &str) -> Option<&'static PublishedTarget> {
    PUBLISHED_TARGETS
        .iter()
        .find(|candidate| candidate.triple == target)
}

fn current_target() -> Result<&'static PublishedTarget> {
    let target = env!("CRAB_BUILD_TARGET");
    published_target(target).ok_or_else(|| {
        internal(format!(
            "unsupported upgrade target: {target}; supported targets are {}",
            PUBLISHED_TARGETS
                .iter()
                .map(|target| target.triple)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

fn verify_digest(actual: &str, expected: &str) -> Result<()> {
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(internal(
        "release archive failed SHA-256 verification".to_owned(),
    ))
}

fn extract_release(
    archive_path: &Path,
    target: &PublishedTarget,
    destination: &Path,
) -> Result<StagedBinaries> {
    match target.format {
        ArchiveFormat::TarGz => extract_tar_gz(archive_path, destination),
        ArchiveFormat::Zip => extract_zip(archive_path, destination),
    }
}

fn extract_tar_gz(archive_path: &Path, destination: &Path) -> Result<StagedBinaries> {
    let file = File::open(archive_path).map_err(CrabError::Io)?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    let mut crab = None;
    let mut fuse_mount = None;
    let mut nfs_link = false;
    for entry in archive
        .entries()
        .map_err(|error| internal(format!("invalid release archive: {error}")))?
    {
        let mut entry =
            entry.map_err(|error| internal(format!("invalid release archive: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| internal(format!("invalid release archive path: {error}")))?
            .into_owned();
        validate_archive_path(&path)?;
        match path.to_str() {
            Some("crab") if entry.header().entry_type().is_file() && crab.is_none() => {
                let staged = destination.join("crab");
                extract_entry(&mut entry, &staged)?;
                crab = Some(staged);
            }
            Some("crab-fuse-mount")
                if entry.header().entry_type().is_file() && fuse_mount.is_none() =>
            {
                let staged = destination.join("crab-fuse-mount");
                extract_entry(&mut entry, &staged)?;
                fuse_mount = Some(staged);
            }
            Some("crab-nfs-mount") if entry.header().entry_type().is_symlink() && !nfs_link => {
                let target = entry
                    .link_name()
                    .map_err(|error| internal(format!("invalid release symlink: {error}")))?;
                if target.as_deref() != Some(Path::new("crab")) {
                    return Err(internal(
                        "release archive crab-nfs-mount must link to crab".to_owned(),
                    ));
                }
                nfs_link = true;
            }
            _ => {
                return Err(internal(format!(
                    "release archive contains unexpected entry {}",
                    path.display()
                )));
            }
        }
    }
    let crab = crab.ok_or_else(|| internal("release archive is missing crab".to_owned()))?;
    let fuse_mount = fuse_mount
        .ok_or_else(|| internal("release archive is missing crab-fuse-mount".to_owned()))?;
    if !nfs_link {
        return Err(internal(
            "release archive is missing crab-nfs-mount".to_owned(),
        ));
    }
    Ok(StagedBinaries {
        crab,
        companions: vec![("crab-fuse-mount", fuse_mount)],
    })
}

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<StagedBinaries> {
    let file = File::open(archive_path).map_err(CrabError::Io)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| internal(format!("invalid release archive: {error}")))?;
    let mut crab = None;
    let mut nfs_mount = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| internal(format!("invalid release archive: {error}")))?;
        let path = entry
            .enclosed_name()
            .ok_or_else(|| internal(format!("unsafe release archive path: {}", entry.name())))?
            .to_path_buf();
        validate_archive_path(&path)?;
        match path.to_str() {
            Some("crab.exe") if entry.is_file() && crab.is_none() => {
                let staged = destination.join("crab.exe");
                extract_reader(&mut entry, &staged)?;
                crab = Some(staged);
            }
            Some("crab-nfs-mount.exe") if entry.is_file() && nfs_mount.is_none() => {
                let staged = destination.join("crab-nfs-mount.exe");
                extract_reader(&mut entry, &staged)?;
                nfs_mount = Some(staged);
            }
            _ => {
                return Err(internal(format!(
                    "release archive contains unexpected entry {}",
                    path.display()
                )));
            }
        }
    }
    let crab = crab.ok_or_else(|| internal("release archive is missing crab.exe".to_owned()))?;
    let nfs_mount = nfs_mount
        .ok_or_else(|| internal("release archive is missing crab-nfs-mount.exe".to_owned()))?;
    Ok(StagedBinaries {
        crab,
        companions: vec![("crab-nfs-mount.exe", nfs_mount)],
    })
}

fn validate_archive_path(path: &Path) -> Result<()> {
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) || path.components().count() != 1
    {
        return Err(internal(format!(
            "release archive contains unsafe path {}",
            path.display()
        )));
    }
    Ok(())
}

fn extract_entry<R: Read>(entry: &mut tar::Entry<'_, R>, destination: &Path) -> Result<()> {
    extract_reader(entry, destination)
}

fn extract_reader(reader: &mut impl Read, destination: &Path) -> Result<()> {
    let mut output = File::create(destination).map_err(CrabError::Io)?;
    let written =
        io::copy(&mut reader.take(BINARY_LIMIT + 1), &mut output).map_err(CrabError::Io)?;
    if written > BINARY_LIMIT {
        return Err(internal(
            "release executable exceeded the size limit".to_owned(),
        ));
    }
    output.sync_all().map_err(CrabError::Io)?;
    make_executable(destination)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).map_err(CrabError::Io)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(CrabError::Io)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn validate_executable(path: &Path, expected: &Version) -> Result<()> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|error| internal(format!("could not run staged Crab executable: {error}")))?;
    if !output.status.success() {
        return Err(internal(
            "staged Crab executable failed its version check".to_owned(),
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| internal("staged Crab version output is not UTF-8".to_owned()))?;
    let expected_output = format!("crab {expected}");
    if stdout.trim() != expected_output {
        return Err(internal(format!(
            "staged Crab executable reported '{}', expected '{expected_output}'",
            stdout.trim()
        )));
    }
    Ok(())
}

fn replace_installation(staged: &StagedBinaries) -> Result<()> {
    let current = std::env::current_exe().map_err(CrabError::Io)?;
    let install_dir = current.parent().ok_or_else(|| {
        internal(format!(
            "could not determine install directory for {}",
            current.display()
        ))
    })?;
    let mut replacements = Vec::new();
    for (name, candidate) in &staged.companions {
        match replace_companion(&install_dir.join(name), candidate) {
            Ok(replacement) => replacements.push(replacement),
            Err(error) => {
                rollback_companions(&replacements);
                return Err(error);
            }
        }
    }

    if let Err(error) = self_replace::self_replace(&staged.crab) {
        rollback_companions(&replacements);
        return Err(internal(format!(
            "could not replace {}: {error}. Check that the executable is writable by the current user",
            current.display()
        )));
    }
    commit_companions(&replacements);
    Ok(())
}

fn replace_companion(destination: &Path, candidate: &Path) -> Result<CompanionReplacement> {
    let install_dir = destination
        .parent()
        .ok_or_else(|| internal("companion executable has no install directory".to_owned()))?;
    let mut staged = tempfile::Builder::new()
        .prefix(".crab-upgrade-")
        .tempfile_in(install_dir)
        .map_err(CrabError::Io)?;
    let mut source = File::open(candidate).map_err(CrabError::Io)?;
    io::copy(&mut source, staged.as_file_mut()).map_err(CrabError::Io)?;
    staged.as_file().sync_all().map_err(CrabError::Io)?;
    make_executable(staged.path())?;

    let backup = if destination.symlink_metadata().is_ok() {
        let backup_slot = tempfile::Builder::new()
            .prefix(".crab-upgrade-backup-")
            .tempfile_in(install_dir)
            .map_err(CrabError::Io)?;
        let backup_path = backup_slot.path().to_path_buf();
        drop(backup_slot);
        fs::rename(destination, &backup_path).map_err(CrabError::Io)?;
        Some(backup_path)
    } else {
        None
    };

    if let Err(error) = staged.persist(destination) {
        if let Some(path) = &backup {
            let _ = fs::rename(path, destination);
        }
        return Err(CrabError::Io(error.error));
    }
    Ok(CompanionReplacement {
        destination: destination.to_path_buf(),
        backup,
    })
}

fn rollback_companions(replacements: &[CompanionReplacement]) {
    for replacement in replacements.iter().rev() {
        let _ = fs::remove_file(&replacement.destination);
        if let Some(backup) = &replacement.backup {
            let _ = fs::rename(backup, &replacement.destination);
        }
    }
}

fn commit_companions(replacements: &[CompanionReplacement]) {
    for replacement in replacements {
        if let Some(backup) = &replacement.backup
            && let Err(error) = fs::remove_file(backup)
        {
            tracing::warn!(path = %backup.display(), %error, "could not remove upgrade backup");
        }
    }
}

fn internal(message: String) -> CrabError {
    CrabError::Internal(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn downloaded_archive_digest_preserves_sha256_hex_encoding() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(connection.read_u8().await.unwrap());
                assert!(request.len() < 8192);
            }
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc")
                .await
                .unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("archive");
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let result = download_bounded(
            &client,
            &format!("http://{address}/archive"),
            &destination,
            3,
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(
            result,
            (
                3,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_owned()
            )
        );
    }

    fn manifest(version: &str) -> ReleaseManifest {
        ReleaseManifest {
            schema: RELEASE_MANIFEST_SCHEMA.to_owned(),
            version: version.to_owned(),
            tag: format!("v{version}"),
            artifacts: PUBLISHED_TARGETS
                .iter()
                .map(|target| ReleaseArtifact {
                    target: target.triple.to_owned(),
                    archive: target.archive.to_owned(),
                    sha256: "a".repeat(64),
                    bytes: 42,
                })
                .collect(),
        }
    }

    #[test]
    fn version_policy_upgrades_only_to_a_newer_release() {
        let current = Version::new(1, 2, 3);
        assert_eq!(
            version_decision(&current, &Version::new(1, 2, 4)),
            VersionDecision::Upgrade
        );
        assert_eq!(
            version_decision(&current, &Version::new(1, 2, 3)),
            VersionDecision::Current
        );
        assert_eq!(
            version_decision(&current, &Version::new(1, 2, 2)),
            VersionDecision::Newer
        );
    }

    #[test]
    fn published_targets_map_to_release_archives() {
        let expected = [
            ("aarch64-apple-darwin", "crab-darwin-aarch64.tar.gz"),
            ("aarch64-pc-windows-msvc", "crab-windows-aarch64.zip"),
            ("aarch64-unknown-linux-gnu", "crab-linux-aarch64.tar.gz"),
            ("x86_64-apple-darwin", "crab-darwin-x86_64.tar.gz"),
            ("x86_64-pc-windows-msvc", "crab-windows-x86_64.zip"),
            ("x86_64-unknown-linux-gnu", "crab-linux-x86_64.tar.gz"),
        ];
        for (triple, archive) in expected {
            assert_eq!(
                published_target(triple).map(|target| target.archive),
                Some(archive)
            );
        }
        assert!(published_target("x86_64-unknown-linux-musl").is_none());
    }

    #[test]
    fn release_manifest_selects_the_exact_target() {
        let target = published_target("aarch64-apple-darwin").expect("published target");
        let plan = release_plan(manifest("1.2.3"), target).expect("valid manifest");
        assert_eq!(plan.version, Version::new(1, 2, 3));
        assert_eq!(plan.tag, "v1.2.3");
        assert_eq!(plan.archive, "crab-darwin-aarch64.tar.gz");
        assert_eq!(plan.sha256, "a".repeat(64));
        assert_eq!(plan.bytes, 42);
    }

    #[test]
    fn release_manifest_rejects_prerelease_tag_and_duplicate_target() {
        let target = published_target("aarch64-apple-darwin").expect("published target");
        assert!(release_plan(manifest("1.2.4-beta.1"), target).is_err());

        let mut mismatched = manifest("1.2.3");
        mismatched.tag = "v1.2.4".to_owned();
        assert!(release_plan(mismatched, target).is_err());

        let mut duplicate = manifest("1.2.3");
        duplicate.artifacts[5].target = duplicate.artifacts[0].target.clone();
        duplicate.artifacts[5].archive = duplicate.artifacts[0].archive.clone();
        assert!(release_plan(duplicate, target).is_err());
    }

    #[test]
    fn release_manifest_rejects_missing_or_mismatched_archive() {
        let target = published_target("aarch64-apple-darwin").expect("published target");
        let mut missing = manifest("1.2.3");
        missing
            .artifacts
            .retain(|artifact| artifact.target != target.triple);
        assert!(release_plan(missing, target).is_err());

        let mut mismatched = manifest("1.2.3");
        mismatched.artifacts[0].archive = "crab-linux-x86_64.tar.gz".to_owned();
        assert!(release_plan(mismatched, target).is_err());
    }

    #[test]
    fn checksum_requires_an_exact_digest() {
        assert!(verify_digest(&"a".repeat(64), &"A".repeat(64)).is_ok());
        assert!(verify_digest(&"a".repeat(64), &"b".repeat(64)).is_err());
    }

    #[test]
    fn archive_paths_are_single_relative_names() {
        assert!(validate_archive_path(Path::new("crab")).is_ok());
        assert!(validate_archive_path(Path::new("bin/crab")).is_err());
        assert!(validate_archive_path(Path::new("../crab")).is_err());
        assert!(validate_archive_path(Path::new("/crab")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn staged_executable_must_report_the_release_version() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let candidate = temporary.path().join("crab");
        fs::write(&candidate, "#!/bin/sh\necho 'crab 1.2.3'\n").expect("write candidate");
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))
            .expect("make candidate executable");
        assert!(validate_executable(&candidate, &Version::new(1, 2, 3)).is_ok());
        assert!(validate_executable(&candidate, &Version::new(1, 2, 4)).is_err());
    }
}
