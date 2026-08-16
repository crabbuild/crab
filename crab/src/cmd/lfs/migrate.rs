//! `crab lfs migrate` — history rewriting for LFS migration.
//!
//! Dispatches `import`, `export`, and `info` subcommands to the
//! corresponding functions in [`crate::lfs::migrate`].

use crate::core::error::Result;

/// Options for `crab lfs migrate import`.
#[derive(Debug, Clone)]
pub struct LfsMigrateImportOptions {
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub above: Option<String>,
    pub fixup: bool,
    pub no_rewrite: bool,
    pub no_rewrite_files: Vec<String>,
    pub message: Option<String>,
    pub object_map: Option<String>,
    pub refs: LfsMigrateRefSelection,
    pub yes: bool,
    pub verbose: bool,
    pub from_crab: bool,
}

/// Options for `crab lfs migrate export`.
#[derive(Debug, Clone)]
pub struct LfsMigrateExportOptions {
    pub include: String,
    pub exclude: Option<String>,
    pub object_map: Option<String>,
    pub remote: Option<String>,
    pub refs: LfsMigrateRefSelection,
    pub yes: bool,
    pub verbose: bool,
    pub to_crab: bool,
}

/// Options for `crab lfs migrate info`.
#[derive(Debug, Clone)]
pub struct LfsMigrateInfoOptions {
    pub above: Option<String>,
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub top: Option<usize>,
    pub unit: Option<String>,
    pub pointers: Option<String>,
    pub fixup: bool,
    pub refs: LfsMigrateRefSelection,
}

/// Shared Git LFS-compatible ref selection flags for `crab lfs migrate`.
#[derive(Debug, Clone, Default)]
pub struct LfsMigrateRefSelection {
    pub everything: bool,
    pub include_refs: Vec<String>,
    pub exclude_refs: Vec<String>,
    pub branches: Vec<String>,
    pub skip_fetch: bool,
}

/// Run `crab lfs migrate import`.
pub fn run_migrate_import(options: LfsMigrateImportOptions) -> Result<()> {
    crate::lfs::migrate::migrate_import_with_options(crate::lfs::migrate::MigrateImportOptions {
        include: options.include.as_deref(),
        exclude: options.exclude.as_deref(),
        above: options.above.as_deref(),
        fixup: options.fixup,
        no_rewrite: options.no_rewrite,
        no_rewrite_files: options.no_rewrite_files,
        message: options.message.as_deref(),
        object_map: options.object_map.as_deref(),
        refs: core_ref_selection(options.refs),
        yes: options.yes,
        verbose: options.verbose,
        from_crab: options.from_crab,
    })
}

/// Run `crab lfs migrate export`.
pub fn run_migrate_export(options: LfsMigrateExportOptions) -> Result<()> {
    crate::lfs::migrate::migrate_export_with_options(crate::lfs::migrate::MigrateExportOptions {
        include: &options.include,
        exclude: options.exclude.as_deref(),
        object_map: options.object_map.as_deref(),
        remote: options.remote.as_deref(),
        refs: core_ref_selection(options.refs),
        yes: options.yes,
        verbose: options.verbose,
        to_crab: options.to_crab,
    })
}

/// Run `crab lfs migrate info`.
pub fn run_migrate_info(options: LfsMigrateInfoOptions) -> Result<()> {
    let refs = core_ref_selection(options.refs);
    let pointer_mode = pointer_mode_for_info(options.fixup, options.pointers.as_deref())?;
    crate::lfs::migrate::migrate_info_with_options(crate::lfs::migrate::MigrateInfoOptions {
        above: options.above.as_deref(),
        include: options.include.as_deref(),
        exclude: options.exclude.as_deref(),
        top: options.top,
        unit: options.unit.as_deref(),
        pointer_mode,
        fixup: options.fixup,
        refs: &refs,
    })
}

fn core_ref_selection(
    selection: LfsMigrateRefSelection,
) -> crate::lfs::migrate::MigrateRefSelection {
    crate::lfs::migrate::MigrateRefSelection {
        everything: selection.everything,
        include_refs: selection.include_refs,
        exclude_refs: selection.exclude_refs,
        branches: selection.branches,
        skip_fetch: selection.skip_fetch,
    }
}

fn pointer_mode_from_cli(
    value: Option<&str>,
) -> Result<crate::lfs::migrate::MigrateInfoPointerMode> {
    match value {
        None => Ok(crate::lfs::migrate::MigrateInfoPointerMode::Follow),
        Some("only") => Ok(crate::lfs::migrate::MigrateInfoPointerMode::PointersOnly),
        Some("follow") => Ok(crate::lfs::migrate::MigrateInfoPointerMode::Follow),
        Some("no-follow") => Ok(crate::lfs::migrate::MigrateInfoPointerMode::NoFollow),
        Some("ignore") => Ok(crate::lfs::migrate::MigrateInfoPointerMode::Ignore),
        Some(other) => Err(crate::core::error::CrabError::Configuration {
            key: "--pointers".to_owned(),
            origin: format!("expected follow, no-follow, ignore, or only; got {other}"),
        }),
    }
}

fn pointer_mode_for_info(
    fixup: bool,
    value: Option<&str>,
) -> Result<crate::lfs::migrate::MigrateInfoPointerMode> {
    let mode = pointer_mode_from_cli(value)?;
    if fixup {
        if value.is_some() && mode != crate::lfs::migrate::MigrateInfoPointerMode::Ignore {
            return Err(crate::core::error::CrabError::Configuration {
                key: "--fixup".to_owned(),
                origin: "--fixup is only compatible with --pointers=ignore".to_owned(),
            });
        }
        return Ok(crate::lfs::migrate::MigrateInfoPointerMode::Ignore);
    }
    Ok(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_mode_from_cli_preserves_legacy_bare_pointers() {
        let mode = pointer_mode_from_cli(Some("only")).unwrap();

        assert_eq!(
            mode,
            crate::lfs::migrate::MigrateInfoPointerMode::PointersOnly
        );
    }

    #[test]
    fn pointer_mode_from_cli_defaults_to_follow() {
        let mode = pointer_mode_from_cli(None).unwrap();

        assert_eq!(mode, crate::lfs::migrate::MigrateInfoPointerMode::Follow);
    }

    #[test]
    fn info_fixup_implies_ignore_pointer_mode() {
        let mode = pointer_mode_for_info(true, None).unwrap();

        assert_eq!(mode, crate::lfs::migrate::MigrateInfoPointerMode::Ignore);
    }

    #[test]
    fn info_fixup_rejects_non_ignore_pointer_mode() {
        let result = pointer_mode_for_info(true, Some("follow"));

        assert!(result.is_err());
    }
}
