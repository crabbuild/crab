//! `crab lfs ext` — display configured Git LFS extensions.

use std::process::ExitCode;

use crate::core::error::Result;
use crate::lfs::extension::{LfsExtension, configured_extensions, missing, sorted_extensions};

/// Run `crab lfs ext` or `crab lfs ext list [name...]`.
pub fn run_lfs_ext(names: &[String]) -> Result<ExitCode> {
    let extensions = configured_extensions()?;
    let selected = if names.is_empty() {
        sorted_extensions(extensions)?
    } else {
        names
            .iter()
            .map(|name| {
                extensions
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| missing(name))
            })
            .collect()
    };

    for ext in selected {
        print_extension(&ext);
    }

    Ok(ExitCode::SUCCESS)
}

fn print_extension(ext: &LfsExtension) {
    println!("Extension: {}", ext.name);
    println!("    clean = {}", ext.clean);
    println!("    smudge = {}", ext.smudge);
    println!("    priority = {}", ext.priority);
}
