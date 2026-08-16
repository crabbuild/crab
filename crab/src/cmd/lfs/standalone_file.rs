//! `crab lfs standalone-file` — Git LFS standalone transfer adapter endpoint.

use std::process::ExitCode;

use crate::core::error::Result;

/// Run `crab lfs standalone-file`.
///
/// The upstream Git LFS command is an internal transfer adapter for
/// `file://` remotes. Crab uses the same JSON-line custom transfer
/// protocol, but routes objects through the configured Crab LFS object store.
pub fn run_lfs_standalone_file(args: &[String]) -> Result<ExitCode> {
    if !standalone_file_args_valid(args) {
        eprintln!("crab lfs standalone-file does not accept arguments");
        return Ok(ExitCode::FAILURE);
    }

    super::block_on_runtime(super::transfer_agent::run_lfs_transfer_agent())?;
    Ok(ExitCode::SUCCESS)
}

fn standalone_file_args_valid(args: &[String]) -> bool {
    args.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_file_accepts_no_args() {
        assert!(standalone_file_args_valid(&[]));
    }

    #[test]
    fn standalone_file_rejects_args() {
        assert!(!standalone_file_args_valid(&["extra".to_owned()]));
    }
}
