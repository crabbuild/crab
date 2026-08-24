//! `crab lfs merge-driver` — Git LFS merge driver endpoint.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

use sha2::{Digest, Sha256};

use crate::core::error::{CrabError, Result};
use crab_git::lfs_pointer::{LfsPointer, hex_encode};
use crab_git::pointer_detect::{PointerKind, classify};

use super::store_setup::{LfsRemoteContext, resolve_lfs_remote_for_operation_sync};

const DEFAULT_MERGE_PROGRAM: &str = "git merge-file --stdout --marker-size=%L %A %O %B >%D";

#[derive(Debug, Clone)]
pub struct LfsMergeDriverOptions {
    pub ancestor: Option<String>,
    pub current: Option<String>,
    pub other: Option<String>,
    pub marker_size: usize,
    pub output: Option<String>,
    pub program: Option<String>,
}

impl Default for LfsMergeDriverOptions {
    fn default() -> Self {
        Self {
            ancestor: None,
            current: None,
            other: None,
            marker_size: 12,
            output: None,
            program: None,
        }
    }
}

pub fn run_lfs_merge_driver(options: LfsMergeDriverOptions) -> Result<ExitCode> {
    let repo_root = std::env::current_dir()?;
    let read_remote = resolve_lfs_remote_for_operation_sync("smudge").ok();
    let write_remote = resolve_lfs_remote_for_operation_sync("lfs").ok();
    run_lfs_merge_driver_in(
        &repo_root,
        options,
        read_remote.as_ref(),
        write_remote.as_ref(),
    )
}

fn run_lfs_merge_driver_in(
    repo_root: &Path,
    options: LfsMergeDriverOptions,
    read_remote: Option<&LfsRemoteContext>,
    write_remote: Option<&LfsRemoteContext>,
) -> Result<ExitCode> {
    let paths = merge_driver_paths(&options)?;
    let temp_dir = tempfile::tempdir()?;
    let ancestor_temp = temp_dir.path().join("merge-driver-O");
    let current_temp = temp_dir.path().join("merge-driver-A");
    let other_temp = temp_dir.path().join("merge-driver-B");
    let destination_temp = temp_dir.path().join("merge-driver-D");

    materialize_merge_input(
        repo_root,
        &paths.ancestor,
        &ancestor_temp,
        read_remote,
        write_remote,
    )?;
    materialize_merge_input(
        repo_root,
        &paths.current,
        &current_temp,
        read_remote,
        write_remote,
    )?;
    materialize_merge_input(
        repo_root,
        &paths.other,
        &other_temp,
        read_remote,
        write_remote,
    )?;
    fs::File::create(&destination_temp)?;

    let replacements = merge_program_replacements(
        &ancestor_temp,
        &current_temp,
        &other_temp,
        &destination_temp,
        options.marker_size,
    );
    let program = options.program.as_deref().unwrap_or(DEFAULT_MERGE_PROGRAM);
    let formatted = format_percent_sequences(program, &replacements);
    let status = run_merge_program(repo_root, &formatted)?;

    let merged = fs::read(&destination_temp).map_err(CrabError::Io)?;
    let pointer = clean_merged_content(repo_root, &merged, write_remote)?;
    fs::write(&paths.output, pointer).map_err(CrabError::Io)?;

    Ok(exit_code_from_status(status))
}

#[derive(Debug)]
struct MergeDriverPaths {
    ancestor: PathBuf,
    current: PathBuf,
    other: PathBuf,
    output: PathBuf,
}

fn merge_driver_paths(options: &LfsMergeDriverOptions) -> Result<MergeDriverPaths> {
    let Some(ancestor) = options.ancestor.as_ref().filter(|path| !path.is_empty()) else {
        return missing_required_options();
    };
    let Some(current) = options.current.as_ref().filter(|path| !path.is_empty()) else {
        return missing_required_options();
    };
    let Some(other) = options.other.as_ref().filter(|path| !path.is_empty()) else {
        return missing_required_options();
    };
    let Some(output) = options.output.as_ref().filter(|path| !path.is_empty()) else {
        return missing_required_options();
    };

    Ok(MergeDriverPaths {
        ancestor: PathBuf::from(ancestor),
        current: PathBuf::from(current),
        other: PathBuf::from(other),
        output: PathBuf::from(output),
    })
}

fn missing_required_options<T>() -> Result<T> {
    Err(CrabError::Configuration {
        key: "merge-driver".to_owned(),
        origin: "the --ancestor, --current, --other, and --output options are mandatory".to_owned(),
    })
}

fn materialize_merge_input(
    repo_root: &Path,
    source: &Path,
    destination: &Path,
    read_remote: Option<&LfsRemoteContext>,
    write_remote: Option<&LfsRemoteContext>,
) -> Result<()> {
    let input = fs::read(source).map_err(CrabError::Io)?;
    match classify(&input) {
        PointerKind::Lfs(pointer) => {
            let content = resolve_lfs_content(repo_root, &pointer, read_remote, write_remote)?;
            fs::write(destination, content).map_err(CrabError::Io)?;
        }
        PointerKind::Crab(_) | PointerKind::NotAPointer => {
            fs::write(destination, input).map_err(CrabError::Io)?;
        }
    }
    Ok(())
}

fn resolve_lfs_content(
    repo_root: &Path,
    pointer: &LfsPointer,
    read_remote: Option<&LfsRemoteContext>,
    write_remote: Option<&LfsRemoteContext>,
) -> Result<Vec<u8>> {
    let oid_hex = hex_encode(&pointer.oid);
    let local_lfs_dir = local_lfs_dir(repo_root, read_remote.or(write_remote))?;
    match crate::lfs::cache::read_pointer(&local_lfs_dir, pointer) {
        Ok(Some(content)) => return Ok(content),
        Ok(None) | Err(CrabError::LfsObjectCorrupt { .. }) => {}
        Err(error) => return Err(error),
    }

    let Some(remote) = read_remote else {
        return Err(CrabError::Configuration {
            key: oid_hex,
            origin: "LFS object is not in the local cache and no Crab LFS remote is configured"
                .to_owned(),
        });
    };

    let content = super::block_on_runtime(async {
        remote
            .store
            .verify(&pointer.oid)
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(CrabError::from)
    })?;
    crate::lfs::cache::install_bytes(&local_lfs_dir, &pointer.oid, pointer.size, &content)?;
    Ok(content)
}

fn clean_merged_content(
    repo_root: &Path,
    content: &[u8],
    write_remote: Option<&LfsRemoteContext>,
) -> Result<Vec<u8>> {
    if matches!(classify(content), PointerKind::Lfs(_)) {
        return Ok(content.to_vec());
    }

    let oid: [u8; 32] = Sha256::digest(content).into();
    let oid_hex = hex_encode(&oid);
    let local_lfs_dir = local_lfs_dir(repo_root, write_remote)?;
    let local_path =
        crate::lfs::cache::install_bytes(&local_lfs_dir, &oid, content.len() as u64, content)?;

    if let Some(remote) = write_remote {
        let upload_path = local_path.clone();
        if let Err(error) = super::block_on_runtime(async {
            remote
                .store
                .put_stream(&oid, &upload_path)
                .await
                .map_err(CrabError::from)
        }) {
            tracing::debug!(
                oid = %oid_hex,
                error = %error,
                "merge-driver: failed to upload merged LFS object",
            );
        }
    }

    let pointer = LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    Ok(pointer.serialize())
}

fn local_lfs_dir(repo_root: &Path, remote: Option<&LfsRemoteContext>) -> Result<PathBuf> {
    if let Some(remote) = remote {
        return Ok(if remote.local_lfs_dir.is_absolute() {
            remote.local_lfs_dir.clone()
        } else {
            repo_root.join(&remote.local_lfs_dir)
        });
    }

    let git_dir = crate::git::discover::discover_git_dir_from(repo_root)?;
    Ok(if git_dir.is_absolute() {
        git_dir.join("lfs")
    } else {
        repo_root.join(git_dir).join("lfs")
    })
}

fn merge_program_replacements(
    ancestor: &Path,
    current: &Path,
    other: &Path,
    destination: &Path,
    marker_size: usize,
) -> HashMap<String, String> {
    HashMap::from([
        ("A".to_owned(), current.display().to_string()),
        ("B".to_owned(), other.display().to_string()),
        ("O".to_owned(), ancestor.display().to_string()),
        ("D".to_owned(), destination.display().to_string()),
        ("L".to_owned(), marker_size.to_string()),
    ])
}

fn format_percent_sequences(pattern: &str, replacements: &HashMap<String, String>) -> String {
    let mut formatted = String::new();
    let mut percent = false;

    for ch in pattern.chars() {
        if !percent && ch == '%' {
            percent = true;
            continue;
        }
        if percent {
            percent = false;
            if ch == '%' {
                formatted.push('%');
            } else if let Some(value) = replacements.get(&ch.to_string()) {
                formatted.push_str(&shell_quote_single(value));
            }
            continue;
        }
        formatted.push(ch);
    }

    formatted
}

fn shell_quote_single(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'@' | b'/' | b'.' | b'-')
        })
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run_merge_program(repo_root: &Path, program: &str) -> Result<ExitStatus> {
    Command::new("sh")
        .args(["-c", program])
        .current_dir(repo_root)
        .status()
        .map_err(|e| CrabError::Configuration {
            key: "merge-driver".to_owned(),
            origin: format!("failed to run merge program {program:?}: {e}"),
        })
}

fn exit_code_from_status(status: ExitStatus) -> ExitCode {
    if let Some(code) = status.code().and_then(|code| u8::try_from(code).ok()) {
        return ExitCode::from(code);
    }
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer_bytes(content: &[u8]) -> (Vec<u8>, String) {
        let oid: [u8; 32] = Sha256::digest(content).into();
        let pointer = LfsPointer {
            oid,
            size: content.len() as u64,
            extensions: Vec::new(),
        };
        (pointer.serialize(), hex_encode(&oid))
    }

    fn write_cached_object(repo_root: &Path, content: &[u8]) -> String {
        let (_pointer, oid_hex) = pointer_bytes(content);
        let local_path = repo_root
            .join(".git")
            .join("lfs")
            .join("objects")
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(&oid_hex);
        fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        fs::write(local_path, content).unwrap();
        oid_hex
    }

    fn read_cached_object(repo_root: &Path, pointer_bytes: &[u8]) -> Vec<u8> {
        let pointer = LfsPointer::parse(pointer_bytes).unwrap();
        let oid_hex = hex_encode(&pointer.oid);
        let local_path = repo_root
            .join(".git")
            .join("lfs")
            .join("objects")
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(&oid_hex);
        fs::read(local_path).unwrap()
    }

    #[test]
    fn missing_options_are_rejected() {
        let err = merge_driver_paths(&LfsMergeDriverOptions::default()).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn format_percent_sequences_matches_git_lfs_rules() {
        let replacements = HashMap::from([
            ("A".to_owned(), "current".to_owned()),
            ("B".to_owned(), "other file".to_owned()),
            ("P".to_owned(), "some ' output \" file".to_owned()),
        ]);

        assert_eq!(
            format_percent_sequences("merge-foo %A %%A", &replacements),
            "merge-foo current %A"
        );
        assert_eq!(
            format_percent_sequences("merge-foo >%B", &replacements),
            "merge-foo >'other file'"
        );
        assert_eq!(
            format_percent_sequences("merge-foo %P", &replacements),
            "merge-foo 'some '\\'' output \" file'"
        );
        assert_eq!(
            format_percent_sequences("merge-foo %Z done", &replacements),
            "merge-foo  done"
        );
    }

    #[test]
    fn clean_merged_content_writes_pointer_and_local_object() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();

        let pointer = clean_merged_content(repo.path(), b"merged\n", None).unwrap();

        assert_eq!(read_cached_object(repo.path(), &pointer), b"merged\n");
    }

    #[cfg(unix)]
    #[test]
    fn merge_driver_runs_custom_program_and_cleans_output() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        let ancestor = repo.path().join("ancestor.txt");
        let current = repo.path().join("current.txt");
        let other = repo.path().join("other.txt");
        let output = repo.path().join("output.txt");
        fs::write(&ancestor, b"base\n").unwrap();
        fs::write(&current, b"ours\n").unwrap();
        fs::write(&other, b"theirs\n").unwrap();

        let exit = run_lfs_merge_driver_in(
            repo.path(),
            LfsMergeDriverOptions {
                ancestor: Some(ancestor.display().to_string()),
                current: Some(current.display().to_string()),
                other: Some(other.display().to_string()),
                marker_size: 12,
                output: Some(output.display().to_string()),
                program: Some("cat %A %B >%D".to_owned()),
            },
            None,
            None,
        )
        .unwrap();

        assert_eq!(exit, ExitCode::SUCCESS);
        let output_pointer = fs::read(output).unwrap();
        assert_eq!(
            read_cached_object(repo.path(), &output_pointer),
            b"ours\ntheirs\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_driver_resolves_pointer_inputs_from_local_cache() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        let ancestor = repo.path().join("ancestor.txt");
        let current = repo.path().join("current.txt");
        let other = repo.path().join("other.txt");
        let output = repo.path().join("output.txt");
        fs::write(&ancestor, b"base\n").unwrap();
        let (current_pointer, _current_oid) = pointer_bytes(b"ours\n");
        write_cached_object(repo.path(), b"ours\n");
        fs::write(&current, current_pointer).unwrap();
        fs::write(&other, b"theirs\n").unwrap();

        run_lfs_merge_driver_in(
            repo.path(),
            LfsMergeDriverOptions {
                ancestor: Some(ancestor.display().to_string()),
                current: Some(current.display().to_string()),
                other: Some(other.display().to_string()),
                marker_size: 12,
                output: Some(output.display().to_string()),
                program: Some("cat %A >%D".to_owned()),
            },
            None,
            None,
        )
        .unwrap();

        let output_pointer = fs::read(output).unwrap();
        assert_eq!(read_cached_object(repo.path(), &output_pointer), b"ours\n");
    }

    #[cfg(unix)]
    #[test]
    fn merge_driver_returns_merge_program_exit_code_after_cleaning() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        let ancestor = repo.path().join("ancestor.txt");
        let current = repo.path().join("current.txt");
        let other = repo.path().join("other.txt");
        let output = repo.path().join("output.txt");
        fs::write(&ancestor, b"base\n").unwrap();
        fs::write(&current, b"ours\n").unwrap();
        fs::write(&other, b"theirs\n").unwrap();

        let exit = run_lfs_merge_driver_in(
            repo.path(),
            LfsMergeDriverOptions {
                ancestor: Some(ancestor.display().to_string()),
                current: Some(current.display().to_string()),
                other: Some(other.display().to_string()),
                marker_size: 12,
                output: Some(output.display().to_string()),
                program: Some("printf conflict >%D; exit 3".to_owned()),
            },
            None,
            None,
        )
        .unwrap();

        assert_eq!(exit, ExitCode::from(3));
        let output_pointer = fs::read(output).unwrap();
        assert_eq!(
            read_cached_object(repo.path(), &output_pointer),
            b"conflict"
        );
    }
}
