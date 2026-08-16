//! `crab migrate` — rewrite history to move files into or out of crab tracking.
//!
//! Subcommands:
//! - `migrate import` — convert large files in history to crab pointers.
//! - `migrate export` — convert crab pointers back to full files.
//! - `migrate info`   — show which file patterns would benefit from migration.
//! - `migrate from-dvc` — convert a DVC pipeline to crab format.
//!
//! This is the crab equivalent of `git lfs migrate`. It rewrites git
//! history using `git filter-repo` (or a built-in tree walker) to replace
//! large blobs with crab pointer files, or vice versa.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::error::{CrabError, Result};
use crab_workflow::{MigrationReport, convert_dvc_to_crab};

/// Arguments for `crab migrate info`.
pub struct MigrateInfoArgs {
    /// Only consider files above this size threshold (bytes).
    pub above: u64,
    /// Limit output to the top N file extensions.
    pub top: usize,
}

/// Arguments for `crab migrate import`.
pub struct MigrateImportArgs {
    /// Glob patterns for files to convert to crab pointers.
    pub include: Vec<String>,
    /// Glob patterns to exclude from migration.
    pub exclude: Vec<String>,
    /// Size threshold — only migrate files above this size.
    pub above: u64,
    /// Report what would be migrated without rewriting.
    pub dry_run: bool,
    /// Rewrite all branches, not just the current one.
    pub everything: bool,
}

/// Arguments for `crab migrate export`.
pub struct MigrateExportArgs {
    /// Glob patterns for files to convert back from pointers.
    pub include: Vec<String>,
    /// Report what would be exported without rewriting.
    pub dry_run: bool,
}

/// Show migration statistics: which file types are large and would
/// benefit from crab tracking.
pub fn run_migrate_info(args: &MigrateInfoArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_migrate_info_in(&cwd, args)
}

/// Analyze the repository at `root` for migration candidates.
pub fn run_migrate_info_in(root: &Path, args: &MigrateInfoArgs) -> Result<()> {
    // Use `git rev-list --objects --all` + `git cat-file --batch-check`
    // to enumerate all blobs and their sizes.
    let output = Command::new("git")
        .args(["rev-list", "--objects", "--all"])
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "git rev-list failed".into(),
            origin: root.display().to_string(),
        });
    }

    let rev_list = String::from_utf8_lossy(&output.stdout);

    // Collect (extension, total_size, count) tuples.
    let mut ext_stats: std::collections::HashMap<String, (u64, u64)> =
        std::collections::HashMap::new();

    for line in rev_list.lines() {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() < 2 {
            continue;
        }

        let oid = parts[0];
        let path = parts[1];

        // Get the blob size via cat-file.
        let cat_output = Command::new("git")
            .args(["cat-file", "-s", oid])
            .current_dir(root)
            .output()?;

        if !cat_output.status.success() {
            continue;
        }

        let size_str = String::from_utf8_lossy(&cat_output.stdout);
        let size: u64 = match size_str.trim().parse() {
            Ok(s) => s,
            Err(_) => continue,
        };

        if size < args.above {
            continue;
        }

        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("<no ext>")
            .to_owned();

        let entry = ext_stats.entry(ext).or_insert((0, 0));
        entry.0 += size;
        entry.1 += 1;
    }

    // Sort by total size descending.
    let mut sorted: Vec<(String, u64, u64)> = ext_stats
        .into_iter()
        .map(|(ext, (size, count))| (ext, size, count))
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    if sorted.is_empty() {
        eprintln!("no files above {} bytes found in history", args.above);
        return Ok(());
    }

    println!("{:<15} {:>12} {:>8}", "Extension", "Total Size", "Count");
    println!("{}", "-".repeat(37));

    for (i, (ext, size, count)) in sorted.iter().enumerate() {
        if i >= args.top {
            break;
        }
        println!("*.{ext:<13} {:>12} {:>8}", format_bytes(*size), count);
    }

    Ok(())
}

/// Rewrite history to convert matching files to crab pointers.
pub fn run_migrate_import(args: &MigrateImportArgs) -> Result<()> {
    if args.include.is_empty() {
        return Err(CrabError::Configuration {
            key: "at least one --include pattern is required".into(),
            origin: "crab migrate import".into(),
        });
    }

    if args.dry_run {
        eprintln!("migrate import (dry run):");
        eprintln!("  include: {:?}", args.include);
        eprintln!("  exclude: {:?}", args.exclude);
        eprintln!("  above: {} bytes", args.above);
        eprintln!("  everything: {}", args.everything);
        eprintln!("  (no changes will be made)");
        return Ok(());
    }

    // Check that git-filter-repo is available.
    let check = Command::new("git")
        .args(["filter-repo", "--version"])
        .output();

    match check {
        Ok(o) if o.status.success() => {
            tracing::info!("git-filter-repo available, proceeding with history rewrite");
        }
        _ => {
            eprintln!(
                "error: git-filter-repo is required for history rewriting.\n\
                 Install it with: pip install git-filter-repo\n\
                 Or see: https://github.com/newren/git-filter-repo"
            );
            return Err(CrabError::Configuration {
                key: "git-filter-repo not found".into(),
                origin: "PATH".into(),
            });
        }
    }

    eprintln!(
        "crab migrate import: history rewriting is a destructive operation.\n\
         Back up your repository before proceeding.\n\
         Patterns: {:?}",
        args.include,
    );

    // The actual rewrite would use git-filter-repo with a blob callback
    // that replaces matching blobs with crab pointer content.
    // This is a placeholder for the full implementation.
    eprintln!("migrate import: full rewrite engine not yet wired");

    Ok(())
}

/// Rewrite history to convert crab pointers back to full files.
pub fn run_migrate_export(args: &MigrateExportArgs) -> Result<()> {
    if args.dry_run {
        eprintln!("migrate export (dry run):");
        eprintln!("  include: {:?}", args.include);
        eprintln!("  (no changes will be made)");
        return Ok(());
    }

    eprintln!("migrate export: full rewrite engine not yet wired");
    Ok(())
}

/// Convert a DVC pipeline (`dvc.yaml`) to `crab.yaml`.
///
/// Locates `dvc.yaml` in the given directory (or cwd), parses it,
/// converts each stage to crab format, and either writes
/// `crab.yaml` or prints to stdout.
pub fn run_migrate_from_dvc(
    dir: Option<&Path>,
    to_stdout: bool,
    output: Option<&Path>,
) -> Result<()> {
    let dvc_path = locate_dvc_yaml(dir)?;
    let dvc_content = std::fs::read_to_string(&dvc_path).map_err(CrabError::Io)?;

    let (yaml_content, mut report) = convert_dvc_to_crab(&dvc_content)?;

    if to_stdout {
        print!("{yaml_content}");
    } else {
        let out_path = match output {
            Some(p) => p.to_path_buf(),
            None => dvc_path.with_file_name("crab.yaml"),
        };
        write_crab_yaml(&yaml_content, &out_path)?;
        report.output_path = Some(out_path);
    }

    print_migration_report(&report);
    Ok(())
}

fn locate_dvc_yaml(dir: Option<&Path>) -> Result<PathBuf> {
    let base = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().map_err(CrabError::Io)?,
    };
    let candidate = base.join("dvc.yaml");
    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(CrabError::Configuration {
            key: "dvc.yaml not found".into(),
            origin: base.display().to_string(),
        })
    }
}

fn write_crab_yaml(content: &str, output_path: &Path) -> Result<()> {
    std::fs::write(output_path, content).map_err(CrabError::Io)
}

fn print_migration_report(report: &MigrationReport) {
    println!("Migration Report");
    println!("{}", "=".repeat(50));
    println!("Stages converted: {}", report.stages_converted);

    if let Some(path) = &report.output_path {
        println!("Output written to: {}", path.display());
    }

    if report.warnings.is_empty() {
        println!("Warnings: none");
    } else {
        println!("Warnings ({}):", report.warnings.len());
        for warning in &report.warnings {
            println!("  [{}] {}", warning.stage, warning.message);
        }
    }
    println!("{}", "=".repeat(50));
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn migrate_import_requires_include_patterns() {
        let args = MigrateImportArgs {
            include: vec![],
            exclude: vec![],
            above: 0,
            dry_run: false,
            everything: false,
        };
        let result = run_migrate_import(&args);
        assert!(result.is_err());
    }

    #[test]
    fn migrate_import_dry_run_succeeds() {
        let args = MigrateImportArgs {
            include: vec!["*.bin".into()],
            exclude: vec![],
            above: 1024,
            dry_run: true,
            everything: false,
        };
        let result = run_migrate_import(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn migrate_export_dry_run_succeeds() {
        let args = MigrateExportArgs {
            include: vec!["*.bin".into()],
            dry_run: true,
        };
        let result = run_migrate_export(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn migrate_from_dvc_writes_parseable_crab_yaml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("dvc.yaml"),
            r#"
stages:
  train:
    cmd: python train.py
    outs:
      - model.pkl
"#,
        )
        .unwrap();

        let output = tmp.path().join("crab.yaml");
        run_migrate_from_dvc(Some(tmp.path()), false, Some(&output)).unwrap();

        let yaml = std::fs::read_to_string(output).unwrap();
        let workflow = crab_workflow::parse_yaml(&yaml).unwrap();
        assert!(
            workflow
                .stages
                .contains_key(&crab_workflow::StageName::parse("train").unwrap())
        );
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
    }
}
