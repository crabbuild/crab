//! Git LFS extension configuration and filter execution.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::core::error::{CrabError, Result};
use crab_git::lfs_pointer::{LfsExtension as PointerExtension, LfsPointer, hex_encode};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LfsExtension {
    pub(crate) name: String,
    pub(crate) clean: String,
    pub(crate) smudge: String,
    pub(crate) priority: i32,
}

pub(crate) struct StagedCleanExtensionOutput {
    pub(crate) staged: crate::lfs::cache::StagedObject,
    pub(crate) pointer_extensions: Vec<PointerExtension>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PipeExtensionResult {
    name: String,
    oid_in: [u8; 32],
    oid_out: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PipeExtensionOutput {
    content: Vec<u8>,
    results: Vec<PipeExtensionResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtensionAction {
    Clean,
    Smudge,
}

impl ExtensionAction {
    fn command<'a>(self, ext: &'a LfsExtension) -> &'a str {
        match self {
            Self::Clean => &ext.clean,
            Self::Smudge => &ext.smudge,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Smudge => "smudge",
        }
    }
}

pub(crate) fn configured_extensions() -> Result<BTreeMap<String, LfsExtension>> {
    let output = Command::new("git")
        .args(["config", "--null", "--list"])
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "failed to read git config".to_owned(),
            origin: e.to_string(),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CrabError::Configuration {
            key: "failed to read git config".to_owned(),
            origin: stderr,
        });
    }

    Ok(parse_extensions(&output.stdout))
}

pub(crate) fn configured_extensions_sorted() -> Result<Vec<LfsExtension>> {
    sorted_extensions(configured_extensions()?)
}

pub(crate) fn parse_extensions(raw: &[u8]) -> BTreeMap<String, LfsExtension> {
    let mut extensions = BTreeMap::new();

    for entry in raw.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some((key, value)) = split_config_entry(entry) else {
            continue;
        };
        let Some((name, field)) = parse_extension_key(&key) else {
            continue;
        };

        let ext = extensions
            .entry(name.to_owned())
            .or_insert_with(|| LfsExtension {
                name: name.to_owned(),
                ..LfsExtension::default()
            });

        match field {
            "clean" => ext.clean = value,
            "smudge" => ext.smudge = value,
            "priority" => {
                if let Ok(priority) = value.parse::<i32>() {
                    ext.priority = priority;
                }
            }
            _ => {}
        }
    }

    extensions
}

pub(crate) fn parse_extension_key(key: &str) -> Option<(&str, &str)> {
    let suffix = key.strip_prefix("lfs.extension.")?;
    let (name, field) = suffix.rsplit_once('.')?;
    if name.is_empty() {
        return None;
    }
    Some((name, field))
}

pub(crate) fn sorted_extensions(
    extensions: BTreeMap<String, LfsExtension>,
) -> Result<Vec<LfsExtension>> {
    let mut seen_priorities = HashSet::new();
    let mut values: Vec<_> = extensions.into_values().collect();
    for ext in &values {
        if !seen_priorities.insert(ext.priority) {
            return Err(CrabError::Configuration {
                key: "lfs.extension".to_owned(),
                origin: format!("duplicate priority {}", ext.priority),
            });
        }
    }
    values.sort_by_key(|ext| ext.priority);
    Ok(values)
}

pub(crate) fn missing(name: &str) -> LfsExtension {
    LfsExtension {
        name: name.to_owned(),
        ..LfsExtension::default()
    }
}

pub(crate) fn smudge_content(
    pointer: &LfsPointer,
    content: Vec<u8>,
    file_name: &str,
) -> Result<Vec<u8>> {
    if pointer.extensions.is_empty() {
        return Ok(content);
    }

    let configured = configured_extensions()?;
    let extensions = smudge_extensions_for_pointer(pointer, &configured)?;
    let response = pipe_extensions(ExtensionAction::Smudge, &content, file_name, &extensions)?;
    verify_smudge_extension_results(pointer, &response)?;
    Ok(response.content)
}

/// Runs clean extensions as file-to-file transforms so clean never
/// materializes a full LFS object in memory.
pub(crate) fn clean_staged_with_extensions(
    mut staged: crate::lfs::cache::StagedObject,
    lfs_dir: &Path,
    file_name: &str,
    extensions: &[LfsExtension],
) -> Result<StagedCleanExtensionOutput> {
    let mut pointer_extensions = Vec::new();
    for ext in extensions {
        let oid_in = *staged.oid();
        let output = run_extension_file(
            ExtensionAction::Clean,
            ext,
            file_name,
            staged.path(),
            lfs_dir,
        )?;
        let next = crate::lfs::cache::StagedObject::from_temp(output)?;
        if oid_in != *next.oid() {
            let priority = u8::try_from(pointer_extensions.len()).map_err(|source| {
                CrabError::Configuration {
                    key: "lfs.extension".to_owned(),
                    origin: format!("too many pointer extensions: {source}"),
                }
            })?;
            pointer_extensions.push(PointerExtension {
                name: ext.name.clone(),
                priority,
                oid: oid_in,
                oid_type: "sha256".to_owned(),
            });
        }
        staged = next;
    }

    Ok(StagedCleanExtensionOutput {
        staged,
        pointer_extensions,
    })
}

fn smudge_extensions_for_pointer(
    pointer: &LfsPointer,
    configured: &BTreeMap<String, LfsExtension>,
) -> Result<Vec<LfsExtension>> {
    let mut extensions = BTreeMap::new();
    for pointer_ext in &pointer.extensions {
        if pointer_ext.oid_type != "sha256" {
            return Err(CrabError::Configuration {
                key: format!("lfs extension {}", pointer_ext.name),
                origin: format!("unsupported extension OID type {}", pointer_ext.oid_type),
            });
        }
        let mut ext =
            configured
                .get(&pointer_ext.name)
                .cloned()
                .ok_or_else(|| CrabError::Configuration {
                    key: format!("lfs.extension.{}", pointer_ext.name),
                    origin: "extension is not configured".to_owned(),
                })?;
        ext.priority = i32::from(pointer_ext.priority);
        extensions.insert(ext.name.clone(), ext);
    }

    let mut sorted = sorted_extensions(extensions)?;
    sorted.reverse();
    Ok(sorted)
}

fn verify_smudge_extension_results(
    pointer: &LfsPointer,
    response: &PipeExtensionOutput,
) -> Result<()> {
    let Some(first) = response.results.first() else {
        return Ok(());
    };

    if first.oid_in != pointer.oid {
        return Err(CrabError::Configuration {
            key: "lfs extension smudge".to_owned(),
            origin: format!(
                "actual OID {} during smudge does not match expected {}",
                hex_encode(&first.oid_in),
                hex_encode(&pointer.oid),
            ),
        });
    }

    let actual_by_name: HashMap<_, _> = response
        .results
        .iter()
        .map(|result| (result.name.as_str(), result))
        .collect();
    for expected in &pointer.extensions {
        let actual =
            actual_by_name
                .get(expected.name.as_str())
                .ok_or_else(|| CrabError::Configuration {
                    key: format!("lfs.extension.{}", expected.name),
                    origin: "extension did not run during smudge".to_owned(),
                })?;
        if actual.oid_out != expected.oid {
            return Err(CrabError::Configuration {
                key: format!("lfs.extension.{}", expected.name),
                origin: format!(
                    "actual OID {} does not match expected {}",
                    hex_encode(&actual.oid_out),
                    hex_encode(&expected.oid),
                ),
            });
        }
    }

    Ok(())
}

fn pipe_extensions(
    action: ExtensionAction,
    content: &[u8],
    file_name: &str,
    extensions: &[LfsExtension],
) -> Result<PipeExtensionOutput> {
    let mut current = content.to_vec();
    let mut results = Vec::new();

    for ext in extensions {
        let oid_in = sha256(&current);
        let output = run_extension_command(action, ext, file_name, current)?;
        let oid_out = sha256(&output);
        results.push(PipeExtensionResult {
            name: ext.name.clone(),
            oid_in,
            oid_out,
        });
        current = output;
    }

    Ok(PipeExtensionOutput {
        content: current,
        results,
    })
}

fn run_extension_command(
    action: ExtensionAction,
    ext: &LfsExtension,
    file_name: &str,
    input: Vec<u8>,
) -> Result<Vec<u8>> {
    let (program, args) = split_extension_command(action.command(ext), file_name)?;
    let mut child = Command::new(&program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CrabError::Configuration {
            key: format!("lfs.extension.{}.{}", ext.name, action.as_str()),
            origin: format!("failed to start {program}: {source}"),
        })?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CrabError::Internal("extension stdin unavailable".to_owned()))?;
    let writer = std::thread::spawn(move || stdin.write_all(&input).map(|_| ()));
    let output = child
        .wait_with_output()
        .map_err(|source| CrabError::Configuration {
            key: format!("lfs.extension.{}.{}", ext.name, action.as_str()),
            origin: format!("failed to wait for {program}: {source}"),
        })?;
    writer
        .join()
        .map_err(|_| CrabError::Internal("extension stdin writer panicked".to_owned()))?
        .map_err(|source| CrabError::Configuration {
            key: format!("lfs.extension.{}.{}", ext.name, action.as_str()),
            origin: format!("failed to write extension input: {source}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CrabError::Configuration {
            key: format!("lfs.extension.{}.{}", ext.name, action.as_str()),
            origin: format!("extension '{}' failed with: {}", ext.name, stderr),
        });
    }

    Ok(output.stdout)
}

fn run_extension_file(
    action: ExtensionAction,
    ext: &LfsExtension,
    file_name: &str,
    input: &Path,
    lfs_dir: &Path,
) -> Result<tempfile::NamedTempFile> {
    let (program, args) = split_extension_command(action.command(ext), file_name)?;
    let temp_dir = lfs_dir.join("tmp");
    std::fs::create_dir_all(&temp_dir).map_err(CrabError::Io)?;
    let output = tempfile::Builder::new()
        .prefix("crab-lfs-ext-")
        .tempfile_in(temp_dir)
        .map_err(CrabError::Io)?;
    let stdin = File::open(input).map_err(CrabError::Io)?;
    let stdout = output.reopen().map_err(CrabError::Io)?;
    let result = Command::new(&program)
        .args(args)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| CrabError::Configuration {
            key: format!("lfs.extension.{}.{}", ext.name, action.as_str()),
            origin: format!("failed to run {program}: {source}"),
        })?;
    if !result.status.success() {
        return Err(CrabError::Configuration {
            key: format!("lfs.extension.{}.{}", ext.name, action.as_str()),
            origin: format!(
                "extension '{}' failed with: {}",
                ext.name,
                String::from_utf8_lossy(&result.stderr).trim()
            ),
        });
    }
    Ok(output)
}

fn split_extension_command(command: &str, file_name: &str) -> Result<(String, Vec<String>)> {
    let mut pieces = command.split(' ');
    let program = pieces.next().unwrap_or_default().trim().to_owned();
    if program.is_empty() {
        return Err(CrabError::Configuration {
            key: "lfs.extension".to_owned(),
            origin: "extension command is empty".to_owned(),
        });
    }
    let args = pieces.map(|arg| arg.replace("%f", file_name)).collect();
    Ok((program, args))
}

fn split_config_entry(entry: &[u8]) -> Option<(String, String)> {
    let split_at = entry.iter().position(|b| *b == b'\n')?;
    let key = String::from_utf8_lossy(&entry[..split_at]).to_string();
    let value = String::from_utf8_lossy(&entry[split_at + 1..]).to_string();
    Some((key, value))
}

fn sha256(content: &[u8]) -> [u8; 32] {
    Sha256::digest(content).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extensions_reads_lfs_extension_config() {
        let raw = b"lfs.extension.foo.clean\nfoo-clean %f\0\
            lfs.extension.foo.smudge\nfoo-smudge %f\0\
            lfs.extension.foo.priority\n2\0\
            lfs.extension.bar.priority\n1\0\
            lfs.extension.bar.clean\nbar-clean\0";

        let extensions = parse_extensions(raw);

        assert_eq!(
            extensions.get("foo"),
            Some(&LfsExtension {
                name: "foo".to_owned(),
                clean: "foo-clean %f".to_owned(),
                smudge: "foo-smudge %f".to_owned(),
                priority: 2,
            })
        );
    }

    #[test]
    fn sorted_extensions_orders_by_priority() {
        let mut extensions = BTreeMap::new();
        extensions.insert(
            "z".to_owned(),
            LfsExtension {
                name: "z".to_owned(),
                priority: 3,
                ..LfsExtension::default()
            },
        );
        extensions.insert(
            "a".to_owned(),
            LfsExtension {
                name: "a".to_owned(),
                priority: 2,
                ..LfsExtension::default()
            },
        );
        extensions.insert(
            "b".to_owned(),
            LfsExtension {
                name: "b".to_owned(),
                priority: 1,
                ..LfsExtension::default()
            },
        );

        let names: Vec<String> = sorted_extensions(extensions)
            .unwrap()
            .into_iter()
            .map(|ext| ext.name)
            .collect();

        assert_eq!(names, vec!["b", "a", "z"]);
    }

    #[test]
    fn sorted_extensions_rejects_duplicate_priorities() {
        let mut extensions = BTreeMap::new();
        extensions.insert(
            "a".to_owned(),
            LfsExtension {
                name: "a".to_owned(),
                priority: 1,
                ..LfsExtension::default()
            },
        );
        extensions.insert(
            "b".to_owned(),
            LfsExtension {
                name: "b".to_owned(),
                priority: 1,
                ..LfsExtension::default()
            },
        );

        assert!(sorted_extensions(extensions).is_err());
    }

    #[test]
    fn parse_extension_key_ignores_non_extension_keys() {
        assert_eq!(
            parse_extension_key("lfs.extension.foo.clean"),
            Some(("foo", "clean"))
        );
        assert_eq!(parse_extension_key("lfs.foo.clean"), None);
        assert_eq!(parse_extension_key("lfs.extension..clean"), None);
    }

    #[cfg(unix)]
    #[test]
    fn clean_extensions_record_pointer_metadata() {
        let (_dir, script, log) = case_extension_script();
        let ext = LfsExtension {
            name: "caseinverter".to_owned(),
            clean: format!("{} clean -- %f", script.display()),
            smudge: format!("{} smudge -- %f", script.display()),
            priority: 0,
        };
        let content = b"abc\ndef";

        let lfs_dir = tempfile::tempdir().unwrap();
        let mut writer = crate::lfs::cache::ObjectWriter::new(lfs_dir.path()).unwrap();
        writer.write_all(content).unwrap();
        let cleaned = clean_staged_with_extensions(
            writer.finish().unwrap(),
            lfs_dir.path(),
            "dir1/abc.dat",
            &[ext],
        )
        .unwrap();

        assert_eq!(std::fs::read(cleaned.staged.path()).unwrap(), b"ABC\nDEF");
        assert_eq!(*cleaned.staged.oid(), sha256(b"ABC\nDEF"));
        assert_eq!(cleaned.staged.size(), 7);
        assert_eq!(
            cleaned.pointer_extensions,
            vec![PointerExtension {
                name: "caseinverter".to_owned(),
                priority: 0,
                oid: sha256(content),
                oid_type: "sha256".to_owned(),
            }]
        );
        let log = std::fs::read_to_string(log).unwrap();
        assert!(log.contains("clean: dir1/abc.dat"));
    }

    #[cfg(unix)]
    #[test]
    fn smudge_extensions_reverse_clean_transform() {
        let (_dir, script, log) = case_extension_script();
        let ext = LfsExtension {
            name: "caseinverter".to_owned(),
            clean: format!("{} clean -- %f", script.display()),
            smudge: format!("{} smudge -- %f", script.display()),
            priority: 0,
        };
        let original = b"abc\ndef";
        let stored = b"ABC\nDEF";
        let pointer = LfsPointer {
            oid: sha256(stored),
            size: stored.len() as u64,
            extensions: vec![PointerExtension {
                name: "caseinverter".to_owned(),
                priority: 0,
                oid: sha256(original),
                oid_type: "sha256".to_owned(),
            }],
        };
        let mut configured = BTreeMap::new();
        configured.insert(ext.name.clone(), ext);
        let smudge_exts = smudge_extensions_for_pointer(&pointer, &configured).unwrap();

        let response = pipe_extensions(
            ExtensionAction::Smudge,
            stored,
            "dir1/abc.dat",
            &smudge_exts,
        )
        .unwrap();
        verify_smudge_extension_results(&pointer, &response).unwrap();

        assert_eq!(response.content, original);
        let log = std::fs::read_to_string(log).unwrap();
        assert!(log.contains("smudge: dir1/abc.dat"));
    }

    #[cfg(unix)]
    fn case_extension_script() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("case-ext.sh");
        let log = dir.path().join("case-ext.log");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 action=\"$1\"\n\
                 shift\n\
                 shift\n\
                 path=\"$1\"\n\
                 printf '%s: %s\\n' \"$action\" \"$path\" >> '{}'\n\
                 if [ \"$action\" = clean ]; then\n\
                   tr '[:lower:]' '[:upper:]'\n\
                 else\n\
                   tr '[:upper:]' '[:lower:]'\n\
                 fi\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, script, log)
    }
}
