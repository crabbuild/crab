//! `crab lfs pointer` — generate, validate, and inspect LFS pointers.
//!
//! Provides a standalone command for debugging pointer issues:
//! - `--file <path>`: generate the LFS pointer for a local file
//! - `--pointer <path>`: compare a generated pointer with another pointer file
//! - `--stdin`: read a pointer from stdin and display parsed fields
//! - `--check [--strict|--no-strict]`: validate a pointer from stdin

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::core::error::{CrabError, Result};
use crab_git::lfs_pointer::{LfsPointer, hex_encode};

/// Run `crab lfs pointer` with the given flags.
///
/// Exactly one of `file`, `stdin`, or `check` must be specified.
/// `strict` is only meaningful when combined with `check`.
pub fn run_lfs_pointer(
    file: Option<&str>,
    pointer: Option<&str>,
    stdin: bool,
    check: bool,
    strict: bool,
    no_strict: bool,
) -> Result<std::process::ExitCode> {
    let strict = effective_strict(check, strict, no_strict)?;

    if check {
        if pointer.is_some() {
            return Err(CrabError::Configuration {
                key: "pointer".to_owned(),
                origin: "--pointer is only valid when comparing with --file".to_owned(),
            });
        }
        let input = if let Some(path) = file {
            if stdin {
                return Err(CrabError::Configuration {
                    key: "pointer".to_owned(),
                    origin: "--check accepts either --file or --stdin, not both".to_owned(),
                });
            }
            read_file(Path::new(path))?
        } else {
            read_stdin()?
        };
        return pointer_check_bytes(&input, strict);
    }

    if let Some(path) = file {
        let generated = pointer_bytes_for_file(Path::new(path))?;
        if let Some(pointer_path) = pointer {
            let expected = read_file(Path::new(pointer_path))?;
            return pointer_compare(&generated, &expected);
        }
        if stdin {
            let expected = read_stdin()?;
            return pointer_compare(&generated, &expected);
        }
        write_generated_pointer(Path::new(path), &generated)?;
        return Ok(std::process::ExitCode::SUCCESS);
    }

    if pointer.is_some() {
        return Err(CrabError::Configuration {
            key: "pointer".to_owned(),
            origin: "--pointer requires --file".to_owned(),
        });
    }

    if stdin {
        return pointer_stdin();
    }

    Err(CrabError::Configuration {
        key: "pointer".to_owned(),
        origin: "specify one of --file, --stdin, or --check".to_owned(),
    })
}

fn effective_strict(check: bool, strict: bool, no_strict: bool) -> Result<bool> {
    if strict && no_strict {
        return Err(CrabError::Configuration {
            key: "pointer".to_owned(),
            origin: "Cannot combine --strict with --no-strict".to_owned(),
        });
    }
    if strict && !check {
        return Err(CrabError::Configuration {
            key: "pointer".to_owned(),
            origin: "--strict requires --check".to_owned(),
        });
    }
    if no_strict && !check {
        return Err(CrabError::Configuration {
            key: "pointer".to_owned(),
            origin: "--no-strict requires --check".to_owned(),
        });
    }

    Ok(check && strict)
}

/// Generate and display the LFS pointer for a local file.
fn pointer_bytes_for_file(path: &Path) -> Result<Vec<u8>> {
    let content = read_file(path)?;

    let oid: [u8; 32] = Sha256::digest(&content).into();
    let size = content.len() as u64;

    let pointer = LfsPointer {
        oid,
        size,
        extensions: Vec::new(),
    };

    Ok(pointer.serialize())
}

fn write_generated_pointer(path: &Path, pointer: &[u8]) -> Result<()> {
    let output = String::from_utf8_lossy(pointer);
    print!("{output}");

    let pointer = LfsPointer::parse(pointer)?;
    eprintln!("Git LFS pointer for {}", path.display());
    eprintln!();
    eprintln!("version https://git-lfs.github.com/spec/v1");
    eprintln!("oid sha256:{}", hex_encode(&pointer.oid));
    eprintln!("size {}", pointer.size);
    Ok(())
}

/// Read a pointer from stdin and display its parsed fields.
fn pointer_stdin() -> Result<std::process::ExitCode> {
    let buf = read_stdin()?;

    let pointer = LfsPointer::parse(&buf)?;

    println!("version https://git-lfs.github.com/spec/v1");
    println!("oid sha256:{}", hex_encode(&pointer.oid));
    println!("size {}", pointer.size);

    for ext in &pointer.extensions {
        println!(
            "ext-{}-{} {}:{}",
            ext.priority,
            ext.name,
            ext.oid_type,
            hex_encode(&ext.oid),
        );
    }

    Ok(std::process::ExitCode::SUCCESS)
}

fn pointer_compare(generated: &[u8], expected: &[u8]) -> Result<std::process::ExitCode> {
    if generated == expected {
        return Ok(std::process::ExitCode::SUCCESS);
    }
    eprintln!("pointer does not match");
    Ok(std::process::ExitCode::from(1))
}

/// Validate a pointer. Exits 0 if valid, 1 if invalid, 2 if noncanonical.
///
/// When `strict` is true, additionally rejects non-canonical pointers.
fn pointer_check_bytes(buf: &[u8], strict: bool) -> Result<std::process::ExitCode> {
    match LfsPointer::parse(buf) {
        Ok(_) if strict && !LfsPointer::is_canonical(buf) => {
            eprintln!("pointer is valid but not canonical");
            Ok(std::process::ExitCode::from(2))
        }
        Ok(_) => Ok(std::process::ExitCode::SUCCESS),
        Err(e) => {
            eprintln!("invalid pointer: {e}");
            Ok(std::process::ExitCode::from(1))
        }
    }
}

fn read_stdin() -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| CrabError::Configuration {
            key: "stdin".to_owned(),
            origin: format!("failed to read stdin: {e}"),
        })?;
    Ok(buf)
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("failed to read file: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_compare_succeeds_for_identical_bytes() {
        let pointer = LfsPointer {
            oid: [7; 32],
            size: 9,
            extensions: Vec::new(),
        }
        .serialize();

        assert_eq!(
            pointer_compare(&pointer, &pointer).unwrap(),
            std::process::ExitCode::SUCCESS
        );
    }

    #[test]
    fn pointer_compare_fails_for_different_bytes() {
        let generated = LfsPointer {
            oid: [7; 32],
            size: 9,
            extensions: Vec::new(),
        }
        .serialize();
        let expected = LfsPointer {
            oid: [8; 32],
            size: 9,
            extensions: Vec::new(),
        }
        .serialize();

        assert_eq!(
            pointer_compare(&generated, &expected).unwrap(),
            std::process::ExitCode::from(1)
        );
    }

    #[test]
    fn effective_strict_rejects_conflicting_flags() {
        assert!(effective_strict(true, true, true).is_err());
    }

    #[test]
    fn effective_strict_accepts_no_strict_check_mode() {
        assert!(!effective_strict(true, false, true).unwrap());
    }

    #[test]
    fn pointer_check_strict_rejects_noncanonical_pointer() {
        let mut pointer = LfsPointer {
            oid: [7; 32],
            size: 9,
            extensions: Vec::new(),
        }
        .serialize();
        pointer.pop();

        assert_eq!(
            pointer_check_bytes(&pointer, true).unwrap(),
            std::process::ExitCode::from(2)
        );
        assert_eq!(
            pointer_check_bytes(&pointer, false).unwrap(),
            std::process::ExitCode::SUCCESS
        );
    }
}
