//! `crab lfs install` / `crab lfs uninstall` — configure git to use
//! crab for LFS filters and transfers.
//!
//! `install` sets the `lfs.customtransfer.crab.path`,
//! `lfs.customtransfer.crab.args`, `lfs.customtransfer.crab.concurrent`,
//! `lfs.customtransfer.crab.direction`, and repository-scoped
//! `lfs.standalonetransferagent`
//! git config keys, registers Crab's standalone `filter.lfs`
//! clean/smudge commands, and writes a pre-push hook that delegates
//! to `crab lfs pre-push`.
//!
//! `uninstall` removes those config entries.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::core::error::{CrabError, Result};

/// Git LFS transfer-agent keys set by `crab lfs install`.
const LFS_TRANSFER_CONFIG_KEYS: &[(&str, &str)] = &[
    ("lfs.customtransfer.crab.path", "{bin}"),
    ("lfs.customtransfer.crab.args", "lfs-transfer-agent"),
    ("lfs.customtransfer.crab.concurrent", "true"),
    ("lfs.customtransfer.crab.direction", "both"),
    ("lfs.standalonetransferagent", "crab"),
];

/// Git filter keys removed by `crab lfs uninstall`.
const LFS_FILTER_CONFIG_KEYS: &[&str] = &[
    "filter.lfs.clean",
    "filter.lfs.smudge",
    "filter.lfs.process",
    "filter.lfs.required",
];

/// LFS block placed first in Crab-managed pre-push hooks.
const PRE_PUSH_HOOK_BLOCK: &str = "\
# Crab LFS: publish objects before refs
command -v crab >/dev/null 2>&1 || { echo >&2 \"crab not found\"; exit 2; }
crab lfs pre-push \"$@\" || exit $?
";

/// Hook emitted by Crab releases before managed blocks had a marker.
const LEGACY_PRE_PUSH_HOOK: &str = "\
#!/bin/sh
command -v crab >/dev/null 2>&1 || { echo >&2 \"crab not found\"; exit 0; }
crab lfs pre-push \"$@\"
";

#[derive(Debug, Clone, Copy, Default)]
pub struct LfsInstallOptions {
    pub local: bool,
    pub worktree: bool,
    pub system: bool,
    pub force: bool,
    pub manual: bool,
    pub skip_smudge: bool,
    pub skip_repo: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LfsUninstallOptions {
    pub local: bool,
    pub worktree: bool,
    pub system: bool,
    pub skip_repo: bool,
}

/// Install crab as the LFS filter driver and transfer agent.
///
/// Sets `lfs.customtransfer.crab.path`, `lfs.customtransfer.crab.args`,
/// `lfs.customtransfer.crab.concurrent`, `lfs.customtransfer.crab.direction`,
/// `lfs.standalonetransferagent`, and `filter.lfs.*` in git config,
/// and writes a pre-push hook to Git's configured hooks directory.
///
/// When `local` is true, config is written to `.git/config` only.
/// When `skip_smudge` is true, additionally sets `filter.lfs.smudge` to
/// skip mode so LFS pointers are not expanded on checkout.
pub fn run_lfs_install(options: LfsInstallOptions) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_lfs_install_in(&cwd, options)
}

/// Testable inner implementation that accepts an explicit root directory.
pub fn run_lfs_install_in(root: &Path, options: LfsInstallOptions) -> Result<()> {
    validate_install_options(options)?;
    let bin = crab_binary_path();
    if options.manual {
        print_manual_instructions(&bin, options.skip_smudge);
        return Ok(());
    }

    let flag = config_scope_flag(options);

    for &(key, value_template) in LFS_TRANSFER_CONFIG_KEYS {
        let value = value_template.replace("{bin}", &bin);

        // Idempotent: skip if the key already has the correct value.
        if config_matches(root, flag, key, &value) {
            tracing::debug!(key, value, "LFS config already set, skipping");
            continue;
        }

        set_git_config(root, flag, key, &value)?;
    }

    for (key, value) in lfs_filter_config(&bin, options.skip_smudge) {
        if config_matches(root, flag, key, &value) {
            tracing::debug!(key, value, "LFS filter config already set, skipping");
            continue;
        }
        set_git_config(root, flag, key, &value)?;
    }

    maybe_install_pre_push_hook(root, options)?;

    eprintln!("crab LFS transfer agent installed");
    Ok(())
}

fn validate_install_options(options: LfsInstallOptions) -> Result<()> {
    validate_scope_selection(
        "crab lfs install",
        options.local,
        options.worktree,
        options.system,
    )
}

fn config_scope_flag(options: LfsInstallOptions) -> &'static str {
    if options.local {
        "--local"
    } else if options.worktree {
        "--worktree"
    } else if options.system {
        "--system"
    } else {
        // Crab must not intercept unrelated repositories through a global
        // standalone-agent setting. Users who intentionally want a system
        // install must opt into --system explicitly.
        "--local"
    }
}

fn print_manual_instructions(bin: &str, skip_smudge: bool) {
    println!("Run the following commands to install LFS configuration:");
    println!();
    println!("  git config --local lfs.customtransfer.crab.path \"{bin}\"");
    println!("  git config --local lfs.customtransfer.crab.args lfs-transfer-agent");
    println!("  git config --local lfs.customtransfer.crab.concurrent true");
    println!("  git config --local lfs.customtransfer.crab.direction both");
    println!("  git config --local lfs.standalonetransferagent crab");
    println!("  git config --local filter.lfs.clean \"{bin} lfs clean -- %f\"");
    if skip_smudge {
        println!("  git config --local filter.lfs.smudge \"{bin} lfs smudge --skip -- %f\"");
        println!("  git config --local filter.lfs.process \"{bin} lfs filter-process --skip\"");
    } else {
        println!("  git config --local filter.lfs.smudge \"{bin} lfs smudge -- %f\"");
        println!("  git config --local filter.lfs.process \"{bin} lfs filter-process\"");
    }
    println!("  git config --local filter.lfs.required true");
    println!();
    println!("Install the following as pre-push in the directory printed by:");
    println!("  git rev-parse --git-path hooks");
    println!();
    for line in pre_push_hook_content().lines() {
        println!("  {line}");
    }
}

/// Remove crab LFS filter and transfer configuration.
pub fn run_lfs_uninstall(options: LfsUninstallOptions) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_lfs_uninstall_in(&cwd, options)
}

/// Testable inner implementation.
pub fn run_lfs_uninstall_in(root: &Path, options: LfsUninstallOptions) -> Result<()> {
    validate_uninstall_options(options)?;
    let flag = uninstall_config_scope_flag(options);

    for &(key, _) in LFS_TRANSFER_CONFIG_KEYS {
        unset_git_config(root, flag, key);
    }

    for key in LFS_FILTER_CONFIG_KEYS {
        unset_git_config(root, flag, key);
    }

    maybe_uninstall_pre_push_hook(root, options)?;

    eprintln!("crab LFS transfer agent configuration removed");
    Ok(())
}

fn validate_uninstall_options(options: LfsUninstallOptions) -> Result<()> {
    validate_scope_selection(
        "crab lfs uninstall",
        options.local,
        options.worktree,
        options.system,
    )
}

fn validate_scope_selection(
    command: &str,
    local: bool,
    worktree: bool,
    system: bool,
) -> Result<()> {
    let selected_scopes = [local, worktree, system]
        .into_iter()
        .filter(|selected| *selected)
        .count();
    if selected_scopes > 1 {
        return Err(CrabError::Configuration {
            key: command.to_owned(),
            origin: "choose only one of --local, --worktree, or --system".to_owned(),
        });
    }
    Ok(())
}

fn uninstall_config_scope_flag(options: LfsUninstallOptions) -> &'static str {
    if options.local {
        "--local"
    } else if options.worktree {
        "--worktree"
    } else if options.system {
        "--system"
    } else {
        "--local"
    }
}

fn unset_git_config(root: &Path, flag: &str, key: &str) {
    // Missing keys are expected during uninstall and should not make the
    // command fail.
    let _ = Command::new("git")
        .args(["config", flag, "--unset", key])
        .current_dir(root)
        .output();
}

/// Install the pre-push hook in `.git/hooks/pre-push`.
///
/// Creates the hooks directory if it doesn't exist. Crab's mirror hook is
/// composed automatically; an arbitrary existing hook requires a manual merge
/// or `--force` so LFS publication cannot be silently bypassed.
fn maybe_install_pre_push_hook(root: &Path, options: LfsInstallOptions) -> Result<()> {
    if options.skip_repo {
        return Ok(());
    }
    match super::hooks_dir_from(root) {
        Ok(hooks_dir) => install_pre_push_hook_at(&hooks_dir, options.force),
        Err(e) if !options.local && !options.worktree => {
            tracing::debug!(error = %e, "not in a repository, skipping LFS pre-push hook");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

pub(super) fn install_pre_push_hook_at(hooks_dir: &Path, force: bool) -> Result<()> {
    fs::create_dir_all(hooks_dir).map_err(|e| CrabError::Configuration {
        key: format!("failed to create hooks directory: {e}"),
        origin: hooks_dir.display().to_string(),
    })?;

    let hook_path = hooks_dir.join("pre-push");
    let content = if hook_path.exists() && !force {
        let existing = fs::read_to_string(&hook_path).map_err(|e| CrabError::Configuration {
            key: "pre-push hook".to_owned(),
            origin: format!("failed to read {}: {e}", hook_path.display()),
        })?;

        if existing == crate::cmd::install::MIRROR_PRE_PUSH_HOOK {
            make_pre_push_hook_executable(&hook_path)?;
            tracing::debug!(path = %hook_path.display(), "mirror pre-push hook already owns LFS publication");
            return Ok(());
        }
        if existing
            == format!(
                "#!/bin/sh\n{}",
                crate::cmd::install::OBSOLETE_MIRROR_PRE_PUSH_BODY
            )
        {
            crate::cmd::install::MIRROR_PRE_PUSH_HOOK.to_owned()
        } else if let Some(composed) = with_mirror_hook(&existing) {
            // Only a known mirror remainder is combined. A standalone LFS
            // install must not enable mirror mode merely because it owns stdin.
            if existing.contains("# Crab mirror:") {
                composed
            } else {
                pre_push_hook_content()
            }
        } else if managed_hook_remainder(&existing).is_some()
            && !existing.contains("# Crab mirror:")
        {
            make_pre_push_hook_executable(&hook_path)?;
            return Ok(());
        } else if let Some(remainder) =
            legacy_hook_remainder(&existing).filter(|rest| !rest.contains("# Crab mirror:"))
        {
            let mut upgraded = pre_push_hook_content();
            upgraded.push_str(remainder);
            upgraded
        } else {
            return Err(CrabError::Configuration {
                key: "pre-push hook".to_owned(),
                origin: format!(
                    "{} is not managed by Crab; merge `crab lfs pre-push \"$@\" || exit $?` manually, or use --force to overwrite it",
                    hook_path.display(),
                ),
            });
        }
    } else {
        pre_push_hook_content()
    };

    fs::write(&hook_path, content).map_err(|e| CrabError::Configuration {
        key: format!("failed to write pre-push hook: {e}"),
        origin: hook_path.display().to_string(),
    })?;

    make_pre_push_hook_executable(&hook_path)?;

    tracing::debug!(path = %hook_path.display(), "installed pre-push hook");
    Ok(())
}

fn maybe_uninstall_pre_push_hook(root: &Path, options: LfsUninstallOptions) -> Result<()> {
    if options.skip_repo {
        return Ok(());
    }

    let hooks_dir = match super::hooks_dir_from(root) {
        Ok(hooks_dir) => hooks_dir,
        Err(e) if !options.local && !options.worktree => {
            tracing::debug!(error = %e, "not in a repository, skipping LFS pre-push hook removal");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    uninstall_pre_push_hook_at(&hooks_dir)
}

fn uninstall_pre_push_hook_at(hooks_dir: &Path) -> Result<()> {
    let hook_path = hooks_dir.join("pre-push");
    let Ok(content) = fs::read_to_string(&hook_path) else {
        return Ok(());
    };

    let managed_remainder =
        managed_hook_remainder(&content).or_else(|| legacy_hook_remainder(&content));
    if let Some(remainder) = managed_remainder {
        if remainder.is_empty() {
            fs::remove_file(&hook_path).map_err(|e| CrabError::Configuration {
                key: format!("failed to remove pre-push hook: {e}"),
                origin: hook_path.display().to_string(),
            })?;
        } else {
            let remainder = remainder.strip_prefix('\n').unwrap_or(remainder);
            let preserved = format!("#!/bin/sh\n{remainder}");
            fs::write(&hook_path, preserved).map_err(|e| CrabError::Configuration {
                key: format!("failed to update pre-push hook: {e}"),
                origin: hook_path.display().to_string(),
            })?;
        }
        tracing::debug!(path = %hook_path.display(), "removed LFS pre-push hook block");
    } else {
        tracing::debug!(path = %hook_path.display(), "pre-push hook is not crab-managed, leaving in place");
    }

    Ok(())
}

pub(super) fn pre_push_hook_content() -> String {
    let mut content = String::from("#!/bin/sh\n");
    content.push_str(PRE_PUSH_HOOK_BLOCK);
    content
}

fn managed_hook_remainder(content: &str) -> Option<&str> {
    content
        .strip_prefix("#!/bin/sh\n")?
        .strip_prefix(PRE_PUSH_HOOK_BLOCK)
}

fn legacy_hook_remainder(content: &str) -> Option<&str> {
    content.strip_prefix(LEGACY_PRE_PUSH_HOOK)
}

pub(crate) fn with_mirror_hook(existing: &str) -> Option<String> {
    use crate::cmd::install::{MIRROR_PRE_PUSH_HOOK, OBSOLETE_MIRROR_PRE_PUSH_BODY};
    let remainder = managed_hook_remainder(existing).or_else(|| legacy_hook_remainder(existing))?;
    let remainder = remainder.trim();
    let current_mirror = MIRROR_PRE_PUSH_HOOK.strip_prefix("#!/bin/sh\n")?;
    if remainder.is_empty()
        || remainder == current_mirror.trim()
        || remainder == OBSOLETE_MIRROR_PRE_PUSH_BODY.trim()
    {
        return Some(MIRROR_PRE_PUSH_HOOK.to_owned());
    }
    None
}

fn make_pre_push_hook_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(path, perms).map_err(|e| CrabError::Configuration {
            key: format!("failed to set hook permissions: {e}"),
            origin: path.display().to_string(),
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Check whether a git config key already has the expected value.
fn config_matches(root: &Path, flag: &str, key: &str, expected: &str) -> bool {
    #[cfg(feature = "gix-config")]
    {
        // Only local/global reads go through the resolver; the
        // resolver materializes both layers, so `flag` is a no-op
        // as long as the key winds up set at the expected
        // precedence. The shellout behavior this replaces also
        // matched "config key returned this value at `flag` scope",
        // and the resolver's precedence rules reproduce that.
        let _ = flag;
        let git_dir = match crate::core::config_resolver::discover_git_dir_from(root) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let resolver = match crate::core::config_resolver::GixConfigResolver::open(&git_dir) {
            Ok(r) => r,
            Err(_) => return false,
        };
        resolver.string(key).as_deref() == Some(expected)
    }

    #[cfg(not(feature = "gix-config"))]
    {
        let output = Command::new("git")
            .args(["config", flag, key])
            .current_dir(root)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let current = String::from_utf8_lossy(&o.stdout);
                current.trim() == expected
            }
            _ => false,
        }
    }
}

/// Set a single git config key.
fn set_git_config(root: &Path, flag: &str, key: &str, value: &str) -> Result<()> {
    // SHELLOUT: `git config --local/--global key value` one-shot
    // write. Keep-table rationale: gix-config's write API is
    // materially more awkward than the shellout for setup-time
    // writes. See `requirements.md` Per-Site Decision Matrix.
    let output = Command::new("git")
        .args(["config", flag, key, value])
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: key.to_owned(),
            origin: format!("git config {flag} failed: {stderr}"),
        });
    }

    tracing::debug!(key, value, flag, "set git config");
    Ok(())
}

fn lfs_filter_config(bin: &str, skip_smudge: bool) -> Vec<(&'static str, String)> {
    let (smudge, process) = if skip_smudge {
        (
            format!("{bin} lfs smudge --skip -- %f"),
            format!("{bin} lfs filter-process --skip"),
        )
    } else {
        (
            format!("{bin} lfs smudge -- %f"),
            format!("{bin} lfs filter-process"),
        )
    };
    vec![
        ("filter.lfs.clean", format!("{bin} lfs clean -- %f")),
        ("filter.lfs.smudge", smudge),
        ("filter.lfs.process", process),
        ("filter.lfs.required", "true".to_owned()),
    ]
}

/// Resolve the path to the crab binary.
fn crab_binary_path() -> String {
    crate::cmd::init::crab_binary_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempGitRepo {
        _git_env: crate::test::git_repo::CleanGitEnvGuard,
        dir: tempfile::TempDir,
    }

    impl TempGitRepo {
        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    fn temp_git_repo() -> TempGitRepo {
        let git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        TempGitRepo {
            _git_env: git_env,
            dir,
        }
    }

    fn get_config(root: &Path, flag: &str, key: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["config", flag, key])
            .current_dir(root)
            .output()
            .unwrap();
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            None
        }
    }

    fn local_options(skip_smudge: bool) -> LfsInstallOptions {
        LfsInstallOptions {
            local: true,
            skip_smudge,
            ..LfsInstallOptions::default()
        }
    }

    #[test]
    fn install_sets_lfs_transfer_agent_config() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let agent = get_config(dir.path(), "--local", "lfs.standalonetransferagent");
        assert_eq!(agent.as_deref(), Some("crab"));

        let path = get_config(dir.path(), "--local", "lfs.customtransfer.crab.path");
        assert!(path.is_some(), "transfer agent path should be set");

        let args = get_config(dir.path(), "--local", "lfs.customtransfer.crab.args");
        assert_eq!(args.as_deref(), Some("lfs-transfer-agent"));

        let concurrent = get_config(dir.path(), "--local", "lfs.customtransfer.crab.concurrent");
        assert_eq!(concurrent.as_deref(), Some("true"));

        let direction = get_config(dir.path(), "--local", "lfs.customtransfer.crab.direction");
        assert_eq!(direction.as_deref(), Some("both"));
    }

    #[test]
    fn install_sets_lfs_filter_config() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let clean = get_config(dir.path(), "--local", "filter.lfs.clean");
        assert!(
            clean
                .as_deref()
                .is_some_and(|v| v.ends_with(" lfs clean -- %f")),
            "clean filter should invoke crab lfs clean, got: {clean:?}"
        );

        let smudge = get_config(dir.path(), "--local", "filter.lfs.smudge");
        assert!(
            smudge
                .as_deref()
                .is_some_and(|v| v.ends_with(" lfs smudge -- %f")),
            "smudge filter should invoke crab lfs smudge, got: {smudge:?}"
        );

        let process = get_config(dir.path(), "--local", "filter.lfs.process");
        assert!(
            process
                .as_deref()
                .is_some_and(|v| v.ends_with(" lfs filter-process")),
            "process filter should invoke crab lfs filter-process, got: {process:?}"
        );

        let required = get_config(dir.path(), "--local", "filter.lfs.required");
        assert_eq!(required.as_deref(), Some("true"));
    }

    #[test]
    fn install_creates_pre_push_hook() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        assert!(hook.exists(), "pre-push hook should be created");

        let content = fs::read_to_string(&hook).unwrap();
        assert!(content.contains("crab lfs pre-push"));
    }

    #[test]
    fn install_uses_configured_hooks_path() {
        let dir = temp_git_repo();
        let status = Command::new("git")
            .args(["config", "core.hooksPath", ".custom-hooks"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        assert!(dir.path().join(".custom-hooks/pre-push").exists());
        assert!(!dir.path().join(".git/hooks/pre-push").exists());
    }

    #[test]
    fn install_is_idempotent() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();
        // Second install should succeed without error.
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let agent = get_config(dir.path(), "--local", "lfs.standalonetransferagent");
        assert_eq!(agent.as_deref(), Some("crab"));
    }

    #[test]
    fn install_skip_smudge_sets_filter() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(true)).unwrap();

        let smudge = get_config(dir.path(), "--local", "filter.lfs.smudge");
        assert!(smudge.is_some(), "smudge filter should be set");
        let val = smudge.unwrap();
        assert!(
            val.ends_with(" lfs smudge --skip -- %f"),
            "smudge should contain --skip, got: {val}"
        );

        let process = get_config(dir.path(), "--local", "filter.lfs.process");
        assert!(
            process
                .as_deref()
                .is_some_and(|v| v.ends_with(" lfs filter-process --skip")),
            "process should contain --skip, got: {process:?}"
        );
    }

    #[test]
    fn uninstall_local_removes_local_config_and_crab_hook() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(true)).unwrap();
        run_lfs_uninstall_in(
            dir.path(),
            LfsUninstallOptions {
                local: true,
                ..LfsUninstallOptions::default()
            },
        )
        .unwrap();

        let agent = get_config(dir.path(), "--local", "lfs.standalonetransferagent");
        assert!(
            agent.is_none(),
            "transfer agent config should be removed after uninstall"
        );

        let path = get_config(dir.path(), "--local", "lfs.customtransfer.crab.path");
        assert!(
            path.is_none(),
            "transfer agent path should be removed after uninstall"
        );

        let args = get_config(dir.path(), "--local", "lfs.customtransfer.crab.args");
        assert!(
            args.is_none(),
            "transfer agent args should be removed after uninstall"
        );

        let smudge = get_config(dir.path(), "--local", "filter.lfs.smudge");
        assert!(
            smudge.is_none(),
            "smudge filter should be removed after uninstall"
        );

        let clean = get_config(dir.path(), "--local", "filter.lfs.clean");
        assert!(
            clean.is_none(),
            "clean filter should be removed after uninstall"
        );

        let process = get_config(dir.path(), "--local", "filter.lfs.process");
        assert!(
            process.is_none(),
            "process filter should be removed after uninstall"
        );

        let required = get_config(dir.path(), "--local", "filter.lfs.required");
        assert!(
            required.is_none(),
            "required filter flag should be removed after uninstall"
        );

        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        assert!(
            !hook.exists(),
            "crab-managed pre-push hook should be removed"
        );
    }

    #[test]
    fn install_default_uses_local_config_scope() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), LfsInstallOptions::default()).unwrap();

        let agent = get_config(dir.path(), "--local", "lfs.standalonetransferagent");
        assert_eq!(agent.as_deref(), Some("crab"));
    }

    #[test]
    fn uninstall_skip_repo_keeps_crab_hook() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        run_lfs_uninstall_in(
            dir.path(),
            LfsUninstallOptions {
                local: true,
                skip_repo: true,
                ..LfsUninstallOptions::default()
            },
        )
        .unwrap();

        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        assert!(
            hook.exists(),
            "skip-repo should leave pre-push hook in place"
        );
    }

    #[test]
    fn uninstall_keeps_custom_pre_push_hook() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        fs::write(&hook, "#!/bin/sh\necho custom\n").unwrap();

        run_lfs_uninstall_in(
            dir.path(),
            LfsUninstallOptions {
                local: true,
                ..LfsUninstallOptions::default()
            },
        )
        .unwrap();

        let content = fs::read_to_string(&hook).unwrap();
        assert_eq!(content, "#!/bin/sh\necho custom\n");
    }

    #[test]
    fn pre_push_hook_is_executable() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let hook = dir.path().join(".git").join("hooks").join("pre-push");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::metadata(&hook).unwrap().permissions();
            assert!(
                perms.mode() & 0o111 != 0,
                "pre-push hook should be executable"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn idempotent_install_repairs_pre_push_hook_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o644)).unwrap();

        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let perms = fs::metadata(&hook).unwrap().permissions();
        assert_ne!(perms.mode() & 0o111, 0);
    }

    #[test]
    fn install_skip_repo_leaves_hook_absent() {
        let dir = temp_git_repo();
        run_lfs_install_in(
            dir.path(),
            LfsInstallOptions {
                skip_repo: true,
                ..local_options(false)
            },
        )
        .unwrap();

        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        assert!(!hook.exists(), "pre-push hook should not be created");
    }

    #[test]
    fn install_force_overwrites_existing_hook() {
        let dir = temp_git_repo();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        fs::write(&hook, "#!/bin/sh\necho custom\n").unwrap();

        run_lfs_install_in(
            dir.path(),
            LfsInstallOptions {
                force: true,
                ..local_options(false)
            },
        )
        .unwrap();

        let content = fs::read_to_string(&hook).unwrap();
        assert!(content.contains("crab lfs pre-push"));
    }

    #[test]
    fn install_upgrades_legacy_hook_to_fail_closed() {
        let dir = temp_git_repo();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        fs::write(&hook, LEGACY_PRE_PUSH_HOOK).unwrap();

        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let content = fs::read_to_string(&hook).unwrap();
        assert!(content.contains("exit 2"));
        assert!(content.contains("|| exit $?"));
    }

    #[test]
    fn install_upgrades_legacy_hook_composed_with_mirror() {
        let dir = temp_git_repo();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        let mirror_body = crate::cmd::install::MIRROR_PRE_PUSH_HOOK
            .strip_prefix("#!/bin/sh\n")
            .unwrap();
        let legacy_composed = format!("{LEGACY_PRE_PUSH_HOOK}\n{mirror_body}");
        fs::write(&hook, legacy_composed).unwrap();

        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let content = fs::read_to_string(&hook).unwrap();
        assert_eq!(content, crate::cmd::install::MIRROR_PRE_PUSH_HOOK);
    }

    #[test]
    fn install_rejects_unmanaged_pre_push_hook() {
        let dir = temp_git_repo();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        let custom = "#!/bin/sh\necho custom\n";
        fs::write(&hook, custom).unwrap();

        let result = run_lfs_install_in(dir.path(), local_options(false));

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&hook).unwrap(), custom);
    }

    #[test]
    fn install_preserves_mirror_publication_owner() {
        let dir = temp_git_repo();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        let mirror = crate::cmd::install::MIRROR_PRE_PUSH_HOOK;
        fs::write(&hook, mirror).unwrap();

        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let content = fs::read_to_string(&hook).unwrap();
        assert_eq!(content, mirror);
    }

    #[test]
    fn mirror_install_after_lfs_uses_the_same_batch_owner() {
        let dir = temp_git_repo();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();
        crate::cmd::install::install_mirror_hooks(dir.path()).unwrap();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();
        crate::cmd::install::install_mirror_hooks(dir.path()).unwrap();
        let content = fs::read_to_string(dir.path().join(".git/hooks/pre-push")).unwrap();
        assert_eq!(content, crate::cmd::install::MIRROR_PRE_PUSH_HOOK);
    }

    #[cfg(unix)]
    #[test]
    fn composed_hook_invokes_one_batch_owner_and_propagates_failure() {
        let dir = temp_git_repo();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        fs::write(&hook, crate::cmd::install::MIRROR_PRE_PUSH_HOOK).unwrap();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        let bin_dir = tempfile::tempdir().unwrap();
        let crab = bin_dir.path().join("crab");
        fs::write(
            &crab,
            "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = mirror-pre-push ] || exit 99\nexit 23\n",
        )
        .unwrap();

        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&crab, fs::Permissions::from_mode(0o755)).unwrap();
        let status = Command::new(&hook)
            .env("PATH", bin_dir.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(23));
    }

    #[test]
    fn uninstall_preserves_composed_mirror_hook() {
        let dir = temp_git_repo();
        let hook = dir.path().join(".git").join("hooks").join("pre-push");
        fs::write(&hook, crate::cmd::install::MIRROR_PRE_PUSH_HOOK).unwrap();
        run_lfs_install_in(dir.path(), local_options(false)).unwrap();

        run_lfs_uninstall_in(
            dir.path(),
            LfsUninstallOptions {
                local: true,
                ..LfsUninstallOptions::default()
            },
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(&hook).unwrap(),
            crate::cmd::install::MIRROR_PRE_PUSH_HOOK,
        );
    }
}
