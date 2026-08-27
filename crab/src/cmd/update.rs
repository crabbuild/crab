//! `crab update` — update the CLI from the public release repository.

use std::fs;
use std::io::{self, Cursor, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::Builder;

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};

const RELEASE_API_URL: &str = "https://api.github.com/repos/crabbuild/crab-oss/releases/latest";
const CHECKSUMS_ASSET: &str = "SHA256SUMS.txt";
const UPDATE_SCHEMA: &str = "update";
const UPDATE_SCHEMA_VERSION: &str = "1.0";
const FUSE_MOUNT_NAME: &str = "crab-fuse-mount";
const NFS_MOUNT_NAME: &str = "crab-nfs-mount";

/// Arguments for `crab update`.
pub struct UpdateArgs {
    pub check: bool,
    pub yes: bool,
    pub force: bool,
    pub mode: OutputMode,
}

/// Payload emitted by `crab update --json`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UpdatePayload {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub release_tag: Option<String>,
    pub status: UpdateStatus,
    pub asset: Option<String>,
    pub message: String,
}

/// High-level update outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    NoRelease,
    UpToDate,
    UpdateAvailable,
    PackageManagerManaged,
    Updated,
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    html_url: Option<String>,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// Run `crab update`.
pub async fn run_update(args: UpdateArgs) -> Result<()> {
    let current_version = current_version()?;
    let current_version_text = env!("CRAB_BUILD_VERSION").to_owned();
    let client = reqwest::Client::builder()
        .user_agent(format!("crab/{}", env!("CRAB_BUILD_VERSION")))
        .build()
        .map_err(|e| internal(format!("failed to build HTTP client: {e}")))?;

    let Some(release) = fetch_latest_release(&client).await? else {
        let payload = UpdatePayload {
            current_version: current_version_text,
            latest_version: None,
            release_tag: None,
            status: UpdateStatus::NoRelease,
            asset: None,
            message: "No crab release is available from crabbuild/crab-oss.".to_owned(),
        };
        emit_payload(&payload, args.mode);
        return Ok(());
    };

    let latest_version = parse_release_version(&release.tag_name)?;
    let latest_version_text = latest_version.to_string();
    let ordering = latest_version.cmp(&current_version);
    if ordering == std::cmp::Ordering::Less
        || (ordering == std::cmp::Ordering::Equal && !args.force)
    {
        let message = if ordering == std::cmp::Ordering::Less {
            format!(
                "crab {current_version} is newer than the latest published release {latest_version_text}."
            )
        } else {
            "crab is already up to date.".to_owned()
        };
        let payload = UpdatePayload {
            current_version: current_version_text,
            latest_version: Some(latest_version_text),
            release_tag: Some(release.tag_name),
            status: UpdateStatus::UpToDate,
            asset: None,
            message,
        };
        emit_payload(&payload, args.mode);
        return Ok(());
    }

    let asset_name = asset_name_for(std::env::consts::OS, std::env::consts::ARCH)?;
    let asset = find_asset(&release, asset_name)?;
    let selected_asset_name = asset.name.clone();
    let selected_asset_url = asset.browser_download_url.clone();
    let available_payload = UpdatePayload {
        current_version: current_version_text.clone(),
        latest_version: Some(latest_version_text.clone()),
        release_tag: Some(release.tag_name.clone()),
        status: UpdateStatus::UpdateAvailable,
        asset: Some(selected_asset_name.clone()),
        message: format!("crab {latest_version_text} is available."),
    };

    if args.check {
        emit_payload(&available_payload, args.mode);
        return Ok(());
    }

    let current_exe = std::env::current_exe().map_err(CrabError::Io)?;
    if let Some(prefix) = detect_homebrew_install(&current_exe) {
        let payload = UpdatePayload {
            status: UpdateStatus::PackageManagerManaged,
            message: format!(
                "crab appears to be managed by Homebrew at {}; run `brew upgrade crab`.",
                prefix.display()
            ),
            ..available_payload
        };
        emit_payload(&payload, args.mode);
        return Ok(());
    }

    require_confirmation(&args, &current_version_text, &latest_version_text)?;

    let checksums = find_asset(&release, CHECKSUMS_ASSET)?;
    let archive_asset = ReleaseAsset {
        name: selected_asset_name.clone(),
        browser_download_url: selected_asset_url,
    };
    let archive_bytes = download_asset(&client, &archive_asset).await?;
    let checksums_text = download_text_asset(&client, checksums).await?;
    verify_checksum(&checksums_text, &selected_asset_name, &archive_bytes)?;

    let install_dir = current_exe.parent().ok_or_else(|| {
        internal(format!(
            "could not determine install directory for {}",
            current_exe.display()
        ))
    })?;
    let temp_dir = Builder::new()
        .prefix(".crab-update-")
        .tempdir_in(install_dir)
        .map_err(CrabError::Io)?;
    let extracted = extract_update_binaries(&archive_bytes, temp_dir.path())?;
    require_mount_helpers_for_asset(&selected_asset_name, &extracted)?;
    make_executable(&extracted.crab)?;
    if let Some(fuse_mount) = &extracted.fuse_mount {
        make_executable(fuse_mount)?;
    }
    if let Some(nfs_mount) = &extracted.nfs_mount {
        make_executable(nfs_mount)?;
    }
    verify_candidate_binary(&extracted.crab)?;
    replace_binary(&current_exe, &extracted.crab)?;
    if let Some(fuse_mount) = &extracted.fuse_mount {
        install_or_replace_binary(&install_dir.join(FUSE_MOUNT_NAME), fuse_mount)?;
    }
    if let Some(nfs_mount) = &extracted.nfs_mount {
        install_or_replace_binary(&install_dir.join(NFS_MOUNT_NAME), nfs_mount)?;
    }
    ensure_remote_helper_symlink(&current_exe)?;

    let payload = UpdatePayload {
        current_version: current_version_text,
        latest_version: Some(latest_version_text.clone()),
        release_tag: Some(release.tag_name.clone()),
        status: UpdateStatus::Updated,
        asset: Some(selected_asset_name),
        message: format!("Updated crab to {latest_version_text}."),
    };
    emit_payload(&payload, args.mode);
    Ok(())
}

async fn fetch_latest_release(client: &reqwest::Client) -> Result<Option<ReleaseResponse>> {
    let response = client
        .get(RELEASE_API_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| internal(format!("failed to query GitHub releases: {e}")))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(internal(format!(
            "GitHub release query failed with HTTP {}",
            response.status()
        )));
    }

    response
        .json::<ReleaseResponse>()
        .await
        .map(Some)
        .map_err(|e| internal(format!("failed to parse GitHub release response: {e}")))
}

async fn download_asset(client: &reqwest::Client, asset: &ReleaseAsset) -> Result<Vec<u8>> {
    let response = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| internal(format!("failed to download {}: {e}", asset.name)))?;
    if !response.status().is_success() {
        return Err(internal(format!(
            "download for {} failed with HTTP {}",
            asset.name,
            response.status()
        )));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| internal(format!("failed to read {} response body: {e}", asset.name)))
}

async fn download_text_asset(client: &reqwest::Client, asset: &ReleaseAsset) -> Result<String> {
    let bytes = download_asset(client, asset).await?;
    String::from_utf8(bytes)
        .map_err(|e| internal(format!("{} is not valid UTF-8: {e}", asset.name)))
}

fn emit_payload(payload: &UpdatePayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json(UPDATE_SCHEMA, UPDATE_SCHEMA_VERSION, payload);
    } else {
        println!("{}", payload.message);
        if let Some(tag) = &payload.release_tag {
            println!("release: {tag}");
        }
        if let Some(asset) = &payload.asset {
            println!("asset: {asset}");
        }
    }
}

fn current_version() -> Result<Version> {
    parse_version(env!("CRAB_BUILD_VERSION"))
}

fn parse_release_version(tag: &str) -> Result<Version> {
    parse_version(tag)
}

fn parse_version(raw: &str) -> Result<Version> {
    Version::parse(raw.trim_start_matches('v')).map_err(|e| CrabError::Configuration {
        key: format!("version {raw:?}"),
        origin: format!("failed to parse semantic version: {e}"),
    })
}

fn asset_name_for(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("crab-darwin-aarch64.tar.gz"),
        ("macos", "x86_64") => Ok("crab-darwin-x86_64.tar.gz"),
        ("linux", "aarch64") => Ok("crab-linux-aarch64.tar.gz"),
        ("linux", "x86_64") => Ok("crab-linux-x86_64.tar.gz"),
        _ => Err(CrabError::Configuration {
            key: "platform".to_owned(),
            origin: format!("crab update does not support {os}/{arch} yet"),
        }),
    }
}

fn find_asset<'a>(release: &'a ReleaseResponse, name: &str) -> Result<&'a ReleaseAsset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| {
            let url = release.html_url.as_deref().unwrap_or(RELEASE_API_URL);
            CrabError::Configuration {
                key: name.to_owned(),
                origin: format!(
                    "release {} does not include required asset {name}; see {url}",
                    release.tag_name
                ),
            }
        })
}

fn parse_checksum(checksums: &str, asset_name: &str) -> Result<String> {
    for line in checksums.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if name == asset_name {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    Err(CrabError::Configuration {
        key: asset_name.to_owned(),
        origin: format!("{CHECKSUMS_ASSET} does not contain a checksum for {asset_name}"),
    })
}

fn verify_checksum(checksums: &str, asset_name: &str, bytes: &[u8]) -> Result<()> {
    let expected = parse_checksum(checksums, asset_name)?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if expected == actual {
        return Ok(());
    }
    Err(CrabError::CorruptObject {
        path: asset_name.to_owned(),
        reason: format!("SHA256 mismatch: expected {expected}, got {actual}"),
    })
}

fn require_confirmation(args: &UpdateArgs, current: &str, latest: &str) -> Result<()> {
    if args.yes {
        return Ok(());
    }
    if args.mode == OutputMode::Json || !io::stdin().is_terminal() {
        return Err(CrabError::Configuration {
            key: "--yes".to_owned(),
            origin: "updating non-interactively requires --yes".to_owned(),
        });
    }

    print!("Update crab from {current} to {latest}? [y/N] ");
    io::stdout().flush().map_err(CrabError::Io)?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).map_err(CrabError::Io)?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "confirmation".to_owned(),
        origin: "update cancelled".to_owned(),
    })
}

fn detect_homebrew_install(current_exe: &Path) -> Option<PathBuf> {
    let current = current_exe.to_string_lossy();
    for marker in [
        "/opt/homebrew/Cellar/crab/",
        "/usr/local/Cellar/crab/",
        "/Homebrew/Cellar/crab/",
    ] {
        if let Some(pos) = current.find(marker) {
            let end = pos + marker.len() - "/Cellar/crab/".len();
            return Some(PathBuf::from(&current[..end]));
        }
    }

    let output = Command::new("brew")
        .args(["--prefix", "crab"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let prefix = String::from_utf8(output.stdout).ok()?;
    let prefix_path = PathBuf::from(prefix.trim());
    if !prefix_path.as_os_str().is_empty() && current_exe.starts_with(&prefix_path) {
        return Some(prefix_path);
    }
    None
}

struct ExtractedUpdate {
    crab: PathBuf,
    fuse_mount: Option<PathBuf>,
    nfs_mount: Option<PathBuf>,
}

fn extract_update_binaries(archive_bytes: &[u8], dest_dir: &Path) -> Result<ExtractedUpdate> {
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = Archive::new(decoder);
    let mut crab = None;
    let mut fuse_mount = None;
    let mut nfs_mount = None;
    for entry in archive.entries().map_err(CrabError::Io)? {
        let mut entry = entry.map_err(CrabError::Io)?;
        let name = entry
            .path()
            .map_err(CrabError::Io)?
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        match name.as_deref() {
            Some("crab") => {
                let dest = dest_dir.join("crab");
                entry.unpack(&dest).map_err(CrabError::Io)?;
                crab = Some(dest);
            }
            Some(FUSE_MOUNT_NAME) => {
                let dest = dest_dir.join(FUSE_MOUNT_NAME);
                entry.unpack(&dest).map_err(CrabError::Io)?;
                fuse_mount = Some(dest);
            }
            Some(NFS_MOUNT_NAME) => {
                let dest = dest_dir.join(NFS_MOUNT_NAME);
                entry.unpack(&dest).map_err(CrabError::Io)?;
                nfs_mount = Some(dest);
            }
            _ => {}
        }
    }
    let Some(crab) = crab else {
        return Err(CrabError::Configuration {
            key: "archive".to_owned(),
            origin: "release archive did not contain a crab binary".to_owned(),
        });
    };
    Ok(ExtractedUpdate {
        crab,
        fuse_mount,
        nfs_mount,
    })
}

fn require_mount_helpers_for_asset(asset_name: &str, extracted: &ExtractedUpdate) -> Result<()> {
    if !asset_name.starts_with("crab-windows-") && extracted.fuse_mount.is_none() {
        return Err(CrabError::Configuration {
            key: "archive".to_owned(),
            origin: format!("{asset_name} did not contain {FUSE_MOUNT_NAME}"),
        });
    }
    if extracted.nfs_mount.is_none() {
        return Err(CrabError::Configuration {
            key: "archive".to_owned(),
            origin: format!("{asset_name} did not contain {NFS_MOUNT_NAME}"),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).map_err(CrabError::Io)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions).map_err(CrabError::Io)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn verify_candidate_binary(path: &Path) -> Result<()> {
    let output = Command::new(path).arg("version").output().map_err(|e| {
        internal(format!(
            "failed to run candidate binary {}: {e}",
            path.display()
        ))
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(internal(format!(
        "candidate binary failed sanity check with status {}: {}",
        output.status,
        stderr.trim()
    )))
}

fn replace_binary(current_exe: &Path, candidate: &Path) -> Result<()> {
    let backup = current_exe.with_extension("crab-update-backup");
    if backup.exists() {
        fs::remove_file(&backup).map_err(CrabError::Io)?;
    }
    fs::rename(current_exe, &backup).map_err(|e| {
        CrabError::Io(io::Error::new(
            e.kind(),
            format!(
                "failed to move {} to {}: {e}",
                current_exe.display(),
                backup.display()
            ),
        ))
    })?;

    if let Err(install_err) = fs::rename(candidate, current_exe) {
        let rollback = fs::rename(&backup, current_exe);
        return match rollback {
            Ok(()) => Err(CrabError::Io(io::Error::new(
                install_err.kind(),
                format!(
                    "failed to install {} to {}; restored previous binary: {install_err}",
                    candidate.display(),
                    current_exe.display()
                ),
            ))),
            Err(rollback_err) => Err(CrabError::Io(io::Error::new(
                install_err.kind(),
                format!(
                    "failed to install {} to {}, and rollback from {} failed: {install_err}; rollback error: {rollback_err}",
                    candidate.display(),
                    current_exe.display(),
                    backup.display()
                ),
            ))),
        };
    }

    fs::remove_file(&backup).map_err(CrabError::Io)?;
    Ok(())
}

fn install_or_replace_binary(destination: &Path, candidate: &Path) -> Result<()> {
    if destination.exists() {
        return replace_binary(destination, candidate);
    }
    fs::rename(candidate, destination).map_err(|e| {
        CrabError::Io(io::Error::new(
            e.kind(),
            format!(
                "failed to install {} to {}: {e}",
                candidate.display(),
                destination.display()
            ),
        ))
    })
}

#[cfg(unix)]
fn ensure_remote_helper_symlink(current_exe: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let Some(dir) = current_exe.parent() else {
        return Ok(());
    };
    let helper = dir.join("git-remote-crab");
    if helper.exists() || helper.symlink_metadata().is_ok() {
        fs::remove_file(&helper).map_err(CrabError::Io)?;
    }
    symlink(current_exe, helper).map_err(CrabError::Io)
}

#[cfg(not(unix))]
fn ensure_remote_helper_symlink(_current_exe: &Path) -> Result<()> {
    Ok(())
}

fn internal(message: String) -> CrabError {
    CrabError::Internal(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let encoder = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            for (name, contents) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                archive
                    .append_data(&mut header, *name, *contents)
                    .expect("test archive entry should append");
            }
            archive
                .into_inner()
                .expect("test archive should finish")
                .finish()
                .expect("test gzip should finish");
        }
        bytes
    }

    #[cfg(unix)]
    fn release_archive_with_nfs_symlink() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let encoder = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            for (name, contents) in [
                ("crab", b"crab-bin".as_slice()),
                (FUSE_MOUNT_NAME, b"fuse-mount-bin".as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                archive
                    .append_data(&mut header, name, contents)
                    .expect("test archive entry should append");
            }

            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header
                .set_path(NFS_MOUNT_NAME)
                .expect("symlink path should be valid");
            header
                .set_link_name("crab")
                .expect("symlink target should be valid");
            header.set_cksum();
            archive
                .append(&header, io::empty())
                .expect("test archive symlink should append");
            archive
                .into_inner()
                .expect("test archive should finish")
                .finish()
                .expect("test gzip should finish");
        }
        bytes
    }

    fn release_with_assets(names: &[&str]) -> ReleaseResponse {
        ReleaseResponse {
            tag_name: "v1.2.3".to_owned(),
            html_url: Some("https://github.com/crabbuild/crab-oss/releases/tag/v1.2.3".to_owned()),
            assets: names
                .iter()
                .map(|name| ReleaseAsset {
                    name: (*name).to_owned(),
                    browser_download_url: format!("https://example.test/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn parses_version_with_optional_v_prefix() {
        assert_eq!(
            parse_version("v1.2.3").map(|v| v.to_string()).ok(),
            Some("1.2.3".to_owned())
        );
        assert_eq!(
            parse_version("1.2.3").map(|v| v.to_string()).ok(),
            Some("1.2.3".to_owned())
        );
    }

    #[test]
    fn maps_supported_platform_assets() {
        assert_eq!(
            asset_name_for("macos", "aarch64").ok(),
            Some("crab-darwin-aarch64.tar.gz")
        );
        assert_eq!(
            asset_name_for("macos", "x86_64").ok(),
            Some("crab-darwin-x86_64.tar.gz")
        );
        assert_eq!(
            asset_name_for("linux", "aarch64").ok(),
            Some("crab-linux-aarch64.tar.gz")
        );
        assert_eq!(
            asset_name_for("linux", "x86_64").ok(),
            Some("crab-linux-x86_64.tar.gz")
        );
        assert!(asset_name_for("windows", "x86_64").is_err());
    }

    #[test]
    fn finds_release_asset_by_exact_name() {
        let release = release_with_assets(&["crab-linux-x86_64.tar.gz", CHECKSUMS_ASSET]);
        let asset = find_asset(&release, "crab-linux-x86_64.tar.gz");
        assert_eq!(
            asset.map(|a| a.name.as_str()).ok(),
            Some("crab-linux-x86_64.tar.gz")
        );
        assert!(find_asset(&release, "crab-darwin-aarch64.tar.gz").is_err());
    }

    #[test]
    fn parses_and_verifies_checksum() {
        let bytes = b"release-bytes";
        let hash = format!("{:x}", Sha256::digest(bytes));
        let sums = format!("{hash}  crab-linux-x86_64.tar.gz\n");

        assert_eq!(
            parse_checksum(&sums, "crab-linux-x86_64.tar.gz").ok(),
            Some(hash)
        );
        assert!(verify_checksum(&sums, "crab-linux-x86_64.tar.gz", bytes).is_ok());
        assert!(verify_checksum(&sums, "crab-linux-x86_64.tar.gz", b"other").is_err());
    }

    #[test]
    fn extracts_fuse_mount_when_archive_contains_it() {
        let archive = release_archive(&[
            ("crab", b"crab-bin".as_slice()),
            (FUSE_MOUNT_NAME, b"fuse-mount-bin".as_slice()),
            (NFS_MOUNT_NAME, b"nfs-mount-bin".as_slice()),
        ]);
        let dir = tempfile::tempdir().expect("tempdir should be created");

        let extracted =
            extract_update_binaries(&archive, dir.path()).expect("archive should extract");

        assert_eq!(
            fs::read(&extracted.crab).expect("crab binary should exist"),
            b"crab-bin"
        );
        let fuse_mount = extracted
            .fuse_mount
            .expect("FUSE mount binary should be extracted");
        assert_eq!(
            fs::read(fuse_mount).expect("FUSE mount binary should exist"),
            b"fuse-mount-bin"
        );
        let nfs_mount = extracted
            .nfs_mount
            .expect("NFS mount binary should be extracted");
        assert_eq!(
            fs::read(nfs_mount).expect("NFS mount binary should exist"),
            b"nfs-mount-bin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extracts_nfs_mount_when_archive_contains_symlink() {
        let archive = release_archive_with_nfs_symlink();
        let dir = tempfile::tempdir().expect("tempdir should be created");

        let extracted =
            extract_update_binaries(&archive, dir.path()).expect("archive should extract");

        let nfs_mount = extracted
            .nfs_mount
            .expect("NFS mount helper should be extracted");
        assert_eq!(
            fs::read_link(&nfs_mount).expect("NFS mount helper should be a symlink"),
            PathBuf::from("crab")
        );
        assert_eq!(
            fs::read(&nfs_mount).expect("NFS mount helper should resolve to crab"),
            b"crab-bin"
        );
    }

    #[test]
    fn update_requires_expected_mount_helpers() {
        let extracted = ExtractedUpdate {
            crab: PathBuf::from("crab"),
            fuse_mount: None,
            nfs_mount: Some(PathBuf::from("crab-nfs-mount")),
        };
        assert!(require_mount_helpers_for_asset("crab-darwin-aarch64.tar.gz", &extracted).is_err());
        assert!(require_mount_helpers_for_asset("crab-linux-aarch64.tar.gz", &extracted).is_err());
        assert!(require_mount_helpers_for_asset("crab-windows-x86_64.zip", &extracted).is_ok());

        let extracted = ExtractedUpdate {
            crab: PathBuf::from("crab"),
            fuse_mount: Some(PathBuf::from("crab-fuse-mount")),
            nfs_mount: None,
        };
        assert!(require_mount_helpers_for_asset("crab-darwin-aarch64.tar.gz", &extracted).is_err());
        assert!(require_mount_helpers_for_asset("crab-linux-aarch64.tar.gz", &extracted).is_err());
        assert!(require_mount_helpers_for_asset("crab-windows-x86_64.zip", &extracted).is_err());
    }

    #[test]
    fn detects_homebrew_cellar_paths() {
        let arm = Path::new("/opt/homebrew/Cellar/crab/1.2.3/bin/crab");
        let intel = Path::new("/usr/local/Cellar/crab/1.2.3/bin/crab");
        assert_eq!(
            detect_homebrew_install(arm).map(|p| p.display().to_string()),
            Some("/opt/homebrew".to_owned())
        );
        assert_eq!(
            detect_homebrew_install(intel).map(|p| p.display().to_string()),
            Some("/usr/local".to_owned())
        );
        assert!(detect_homebrew_install(Path::new("/usr/local/bin/crab")).is_none());
    }

    #[test]
    fn update_payload_serializes_status() {
        let payload = UpdatePayload {
            current_version: "1.0.1".to_owned(),
            latest_version: Some("1.0.2".to_owned()),
            release_tag: Some("v1.0.2".to_owned()),
            status: UpdateStatus::UpdateAvailable,
            asset: Some("crab-linux-x86_64.tar.gz".to_owned()),
            message: "crab 1.0.2 is available.".to_owned(),
        };
        let json = serde_json::to_value(payload);
        let Ok(json) = json else {
            panic!("payload should serialize");
        };
        assert_eq!(json["status"], "update_available");
        assert_eq!(json["latest_version"], "1.0.2");
    }
}
