//! `crab install` / `crab uninstall` — manage crab git drivers.
//!
//! `install` registers the `filter.crab` and `diff.crab` drivers in git
//! config so that `.gitattributes` entries with `filter=crab diff=crab`
//! are processed.
//! Unlike `crab init` (which also creates the `.crab/` directory and
//! remote config), `install` only touches git driver configuration.
//!
//! Supports `--local` (default when inside a repo), `--global`, and
//! `--system` scopes.

use std::path::Path;
use std::process::Command;

use crate::core::error::{CrabError, Result};
use crate::core::output::OutputMode;
use crate::core::style::CliStyle;

/// Scope for git driver installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallScope {
    /// Repository-local (`--local`).
    Local,
    /// User-global (`--global`).
    Global,
    /// System-wide (`--system`).
    System,
}

impl InstallScope {
    fn git_flag(self) -> &'static str {
        match self {
            Self::Local => "--local",
            Self::Global => "--global",
            Self::System => "--system",
        }
    }
}

/// Arguments for the `crab install` command.
pub struct InstallArgs {
    /// Scope for the git driver config.
    pub scope: InstallScope,
    /// Overwrite existing filter config even if already set.
    pub force: bool,
    /// Skip the smudge filter (for faster clones that defer hydration).
    pub skip_smudge: bool,
    /// Install git aliases (ship, crab-status, crab-hydrate).
    pub aliases: bool,
    /// Skip shell completion installation (for CI/headless environments).
    pub no_completions: bool,
}

/// Git config keys that define the `filter.crab` driver.
const FILTER_KEYS: &[(&str, &str)] = &[
    ("filter.crab.process", "{bin} filter-process"),
    ("filter.crab.clean", "{bin} filter-process"),
    ("filter.crab.smudge", "{bin} filter-process"),
    ("filter.crab.required", "true"),
];

/// Git config keys that define the `diff.crab` driver.
const DIFF_KEYS: &[(&str, &str)] = &[("diff.crab.command", "{bin} diff-driver")];

/// Install the crab git drivers in git config.
pub fn run_install(args: &InstallArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_install_in(&cwd, args)
}

/// Install the git drivers, optionally scoped to `root`.
pub fn run_install_in(root: &Path, args: &InstallArgs) -> Result<()> {
    let bin = crab_binary_path();
    let flag = args.scope.git_flag();

    // Check if already installed (unless --force). Older installs only
    // registered filter.crab.*, so repair missing diff.crab.command
    // without requiring --force and without overwriting existing keys.
    let repair_only = !args.force
        && args.scope == InstallScope::Local
        && git_config_value(root, flag, "filter.crab.process")?.is_some();
    if !args.force && args.scope == InstallScope::Local {
        let filter = git_config_value(root, flag, "filter.crab.process")?;
        let diff = git_config_value(root, flag, "diff.crab.command")?;
        if filter.is_some() && diff.is_some() {
            eprintln!("crab git drivers already installed (use --force to overwrite)");
            return Ok(());
        }
    }

    let mut changed = false;
    for &(key, value_template) in FILTER_KEYS {
        if repair_only && git_config_value(root, flag, key)?.is_some() {
            continue;
        }

        // When --skip-smudge is set, skip the smudge filter so clones
        // don't automatically hydrate files.
        if args.skip_smudge && key == "filter.crab.smudge" {
            // Set smudge to a no-op (cat passes through unchanged).
            set_git_config(root, flag, key, "cat")?;
            changed = true;
            continue;
        }

        let value = value_template.replace("{bin}", &bin);
        set_git_config(root, flag, key, &value)?;
        changed = true;
    }

    for &(key, value_template) in DIFF_KEYS {
        if repair_only && git_config_value(root, flag, key)?.is_some() {
            continue;
        }

        let value = value_template.replace("{bin}", &bin);
        set_git_config(root, flag, key, &value)?;
        changed = true;
    }

    if repair_only && !changed {
        eprintln!("crab git drivers already installed (use --force to overwrite)");
        return Ok(());
    }

    let scope_name = match args.scope {
        InstallScope::Local => "local",
        InstallScope::Global => "global",
        InstallScope::System => "system",
    };

    // Install git hooks for automatic staging cleanup (local scope only).
    if args.scope == InstallScope::Local {
        install_hooks(root, &bin)?;
    }

    // Global scope enhancements: symlink check, credential discovery,
    // shell completions, git aliases, and success summary.
    if args.scope == InstallScope::Global {
        // Verify git-remote-crab symlink is in the same directory as crab.
        verify_remote_helper_symlink();

        // Install shell completions unless --no-completions was passed.
        if !args.no_completions {
            run_install_completions();
        }

        // Install git aliases if --aliases was passed.
        if args.aliases {
            install_git_aliases(root)?;
        }

        eprintln!(
            "{}",
            CliStyle::resolve(OutputMode::Text)
                .ok("Global setup complete. Any repo with .crab.toml will work automatically.")
        );
    }

    eprintln!("crab git drivers installed ({scope_name})");
    Ok(())
}

/// Remove the crab git drivers from git config.
pub fn run_uninstall(scope: InstallScope) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_uninstall_in(&cwd, scope)
}

/// Remove the git driver config at the given scope.
pub fn run_uninstall_in(root: &Path, scope: InstallScope) -> Result<()> {
    let flag = scope.git_flag();

    for &(key, _) in FILTER_KEYS.iter().chain(DIFF_KEYS.iter()) {
        // SHELLOUT: `git config --unset` one-shot write. Same
        // Keep-table rationale as `set_git_config`: gix-config's
        // mutate/remove API is more awkward than the shellout.
        // --unset may fail if the key doesn't exist; that's fine.
        let _ = Command::new("git")
            .args(["config", flag, "--unset", key])
            .current_dir(root)
            .output();
    }

    let scope_name = match scope {
        InstallScope::Local => "local",
        InstallScope::Global => "global",
        InstallScope::System => "system",
    };

    // Remove hooks when uninstalling local scope.
    if scope == InstallScope::Local {
        uninstall_hooks(root);
    }

    // Global scope: remove driver sections, aliases, and completions.
    if scope == InstallScope::Global {
        // Remove sections entirely (handles "section not found" gracefully).
        for section in ["filter.crab", "diff.crab"] {
            let _ = Command::new("git")
                .args(["config", "--global", "--remove-section", section])
                .current_dir(root)
                .output();
        }

        // Remove git aliases if present.
        uninstall_git_aliases(root);

        // Remove shell completion files.
        uninstall_completions();

        eprintln!(
            "{}",
            CliStyle::resolve(OutputMode::Text).ok("Global crab configuration removed.")
        );
    }

    eprintln!("crab git drivers removed ({scope_name})");
    Ok(())
}

fn git_config_value(root: &Path, flag: &str, key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", flag, key])
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

/// Marker line embedded in hooks so we can identify crab-managed hooks.
const HOOK_MARKER: &str = "# crab-managed hook — do not edit above this line";

/// Git hooks that crab installs for automatic staging cleanup.
///
/// `post-checkout` fires after `git checkout`, `git switch`, `git clone`.
/// `post-merge` fires after `git merge` (including `git pull`).
const HOOK_NAMES: &[&str] = &["post-checkout", "post-merge"];

/// Install post-checkout and post-merge hooks that run `crab reset --sync`.
///
/// If a hook file already exists and wasn't created by crab, the crab
/// invocation is appended (preserving the user's existing hook). If the
/// hook was created by crab, it's overwritten.
fn install_hooks(root: &Path, bin: &str) -> Result<()> {
    let hooks_dir = resolve_hooks_dir(root)?;
    std::fs::create_dir_all(&hooks_dir)?;

    for &hook_name in HOOK_NAMES {
        let hook_path = hooks_dir.join(hook_name);
        let crab_line = format!("{bin} reset --sync 2>/dev/null || true");

        if hook_path.exists() {
            let existing = std::fs::read_to_string(&hook_path)?;

            if existing.contains(HOOK_MARKER) {
                // We own this hook — overwrite it.
                write_hook(&hook_path, bin, &crab_line)?;
            } else if existing.contains(&crab_line) {
                // Already has our line, skip.
                tracing::debug!(hook = hook_name, "hook already contains crab line");
            } else {
                // User's hook exists — append our line.
                let mut content = existing;
                if !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push('\n');
                content.push_str(HOOK_MARKER);
                content.push('\n');
                content.push_str(&crab_line);
                content.push('\n');
                std::fs::write(&hook_path, content)?;
                make_executable(&hook_path)?;
                tracing::debug!(hook = hook_name, "appended crab line to existing hook");
            }
        } else {
            write_hook(&hook_path, bin, &crab_line)?;
            tracing::debug!(hook = hook_name, "created hook");
        }
    }

    eprintln!("crab hooks installed (post-checkout, post-merge)");
    Ok(())
}

/// Write a fresh crab-managed hook file.
fn write_hook(path: &Path, _bin: &str, crab_line: &str) -> Result<()> {
    let content = format!("#!/bin/sh\n{HOOK_MARKER}\n{crab_line}\n");
    std::fs::write(path, content)?;
    make_executable(path)?;
    Ok(())
}

/// Remove crab lines from hooks, or delete the hook if we own it entirely.
fn uninstall_hooks(root: &Path) {
    let Ok(hooks_dir) = resolve_hooks_dir(root) else {
        return;
    };

    for &hook_name in HOOK_NAMES {
        let hook_path = hooks_dir.join(hook_name);
        if !hook_path.exists() {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&hook_path) else {
            continue;
        };

        if content.contains(HOOK_MARKER) {
            // Check if we own the entire hook (shebang + marker + our line).
            let lines: Vec<&str> = content.lines().collect();
            let non_empty: Vec<&&str> = lines.iter().filter(|l| !l.is_empty()).collect();
            if non_empty.len() <= 3 {
                // We own it — delete.
                let _ = std::fs::remove_file(&hook_path);
                tracing::debug!(hook = hook_name, "removed crab hook");
            } else {
                // User has other content — remove only our lines.
                let cleaned: String = content
                    .lines()
                    .filter(|line| {
                        !line.contains(HOOK_MARKER) && !line.contains("crab reset --sync")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = std::fs::write(&hook_path, format!("{cleaned}\n"));
                tracing::debug!(hook = hook_name, "removed crab lines from hook");
            }
        }
    }
}

/// Resolve the hooks directory, respecting `core.hooksPath`.
pub(crate) fn resolve_hooks_dir(root: &Path) -> Result<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "hooks directory".to_owned(),
            origin: format!("git rev-parse --git-path hooks failed: {stderr}"),
        });
    }

    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if resolved.is_empty() {
        return Err(CrabError::Configuration {
            key: "hooks directory".to_owned(),
            origin: "git rev-parse --git-path hooks returned an empty path".to_owned(),
        });
    }

    let path = Path::new(&resolved);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

/// Set the executable bit on a file (Unix only).
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn set_git_config(root: &Path, flag: &str, key: &str, value: &str) -> Result<()> {
    // SHELLOUT: `git config --local/--global key value` writes the
    // filter-driver config. gix-config's write API is materially
    // more awkward than the shellout for one-shot setup writes —
    // see `requirements.md` Per-Site Decision Matrix Keep table.
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

/// Resolve the path to the crab binary.
fn crab_binary_path() -> String {
    crate::cmd::init::crab_binary_path()
}

// ---------------------------------------------------------------------------
// Global install helpers
// ---------------------------------------------------------------------------

/// Verify that `git-remote-crab` resolves to the same directory as `crab`.
/// Prints fix instructions if there's a mismatch.
fn verify_remote_helper_symlink() {
    let crab_which = Command::new("which")
        .arg("crab")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    let helper_which = Command::new("which")
        .arg("git-remote-crab")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    match (crab_which, helper_which) {
        (Some(crab_path), Some(helper_path)) => {
            let crab_dir = Path::new(&crab_path).parent();
            let helper_dir = Path::new(&helper_path).parent();
            if crab_dir != helper_dir {
                let style = CliStyle::resolve(OutputMode::Text);
                eprintln!(
                    "{}\n  Fix: ln -sf {crab_path} {}/git-remote-crab",
                    style.warn(&format!(
                        "git-remote-crab is at {helper_path} but crab is at {crab_path}."
                    )),
                    crab_dir
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
            }
        }
        (Some(crab_path), None) => {
            let crab_dir = Path::new(&crab_path)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let style = CliStyle::resolve(OutputMode::Text);
            eprintln!(
                "{}\n  Fix: ln -sf {crab_path} {crab_dir}/git-remote-crab",
                style.warn("git-remote-crab not found on PATH.")
            );
        }
        _ => {
            tracing::debug!("could not locate crab binary via `which`");
        }
    }
}

/// Install git aliases for common crab commands.
fn install_git_aliases(root: &Path) -> Result<()> {
    let aliases = [
        ("alias.ship", "!crab ship"),
        ("alias.crab-status", "!crab status"),
        ("alias.crab-hydrate", "!crab hydrate"),
    ];

    for &(key, value) in &aliases {
        let output = Command::new("git")
            .args(["config", "--global", key, value])
            .current_dir(root)
            .output()?;

        if !output.status.success() {
            tracing::warn!(key, "failed to set git alias");
        }
    }

    eprintln!("Git aliases installed: git ship, git crab-status, git crab-hydrate");
    Ok(())
}

/// Remove git aliases if present (ignores errors if not set).
fn uninstall_git_aliases(root: &Path) {
    let aliases = ["alias.ship", "alias.crab-status", "alias.crab-hydrate"];

    for key in &aliases {
        let _ = Command::new("git")
            .args(["config", "--global", "--unset", key])
            .current_dir(root)
            .output();
    }
}

// ---------------------------------------------------------------------------
// Shell completions
// ---------------------------------------------------------------------------

/// Standard completion file paths for each shell.
fn completion_paths() -> Vec<(String, std::path::PathBuf)> {
    let Some(home) = dirs_path() else {
        return vec![];
    };

    vec![
        (
            "bash".to_owned(),
            home.join(".local/share/bash-completion/completions/crab"),
        ),
        ("zsh".to_owned(), home.join(".zfunc/_crab")),
        (
            "fish".to_owned(),
            home.join(".config/fish/completions/crab.fish"),
        ),
    ]
}

/// Resolve the user's home directory.
fn dirs_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Generate and install shell completion scripts to standard locations.
pub fn run_install_completions() {
    let paths = completion_paths();
    if paths.is_empty() {
        tracing::debug!("could not determine HOME, skipping completions");
        return;
    }

    for (shell, path) in &paths {
        let script = match shell.as_str() {
            "bash" => generate_bash_completion(),
            "zsh" => generate_zsh_completion(),
            "fish" => generate_fish_completion(),
            _ => continue,
        };

        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::debug!(shell, error = %e, "failed to create completion dir");
            continue;
        }

        match std::fs::write(path, script) {
            Ok(()) => tracing::debug!(shell, path = %path.display(), "wrote completion script"),
            Err(e) => tracing::debug!(shell, error = %e, "failed to write completion script"),
        }
    }

    eprintln!("Shell completions installed (bash, zsh, fish)");
}

/// Remove shell completion files if they exist.
fn uninstall_completions() {
    let paths = completion_paths();
    for (_shell, path) in &paths {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Mirror-mode hooks (Task 19)
// ---------------------------------------------------------------------------

/// Marker comment for mirror-mode hook lines. Used for idempotent
/// append detection.
const MIRROR_HOOK_MARKER: &str = "# Crab mirror:";

pub(crate) const MIRROR_PRE_PUSH_HOOK: &str = "#!/bin/sh\n# Crab mirror: push xorbs before refs go to origin\ncrab add . --skip-git-add 2>/dev/null\ncrab push --remote crab --quiet 2>/dev/null\n";

/// Hook definitions for mirror mode: (hook_name, content_lines).
const MIRROR_HOOKS: &[(&str, &str)] = &[
    ("pre-push", MIRROR_PRE_PUSH_HOOK),
    (
        "post-checkout",
        "#!/bin/sh\n# Crab mirror: hydrate pointer files after checkout\ncrab hydrate . --quiet 2>/dev/null || true\n",
    ),
    (
        "post-merge",
        "#!/bin/sh\n# Crab mirror: hydrate after merge/pull\ncrab hydrate . --quiet 2>/dev/null || true\n",
    ),
];

/// Install mirror-mode git hooks (pre-push, post-checkout, post-merge).
///
/// If a hook file already exists, checks whether crab mirror lines are
/// already present (idempotent). If not present, appends the crab lines
/// after existing content. If the hook doesn't exist, creates it fresh.
pub fn install_mirror_hooks(root: &Path) -> Result<()> {
    let hooks_dir = resolve_hooks_dir(root)?;
    std::fs::create_dir_all(&hooks_dir)?;

    for &(hook_name, full_content) in MIRROR_HOOKS {
        let hook_path = hooks_dir.join(hook_name);

        // Extract just the crab-specific lines (everything after the shebang).
        let crab_lines: String = full_content
            .lines()
            .filter(|line| line.contains(MIRROR_HOOK_MARKER) || line.starts_with("crab "))
            .fold(String::new(), |mut lines, line| {
                lines.push_str(line);
                lines.push('\n');
                lines
            });

        if hook_path.exists() {
            let existing = std::fs::read_to_string(&hook_path)?;

            // Already has mirror lines — skip (idempotent).
            if existing.contains(MIRROR_HOOK_MARKER) {
                tracing::debug!(
                    hook = hook_name,
                    "mirror hook lines already present, skipping"
                );
                continue;
            }

            // Append crab mirror lines to existing hook.
            let mut content = existing;
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push('\n');
            content.push_str(&crab_lines);
            std::fs::write(&hook_path, content)?;
            make_executable(&hook_path)?;
            tracing::debug!(
                hook = hook_name,
                "appended mirror hook lines to existing hook"
            );
        } else {
            // Create fresh hook.
            std::fs::write(&hook_path, full_content)?;
            make_executable(&hook_path)?;
            tracing::debug!(hook = hook_name, "created mirror hook");
        }
    }

    eprintln!("Mirror hooks installed (pre-push, post-checkout, post-merge)");
    Ok(())
}

fn generate_bash_completion() -> &'static str {
    r#"# crab bash completion
# Auto-generated by `crab install --global`

_crab_completions() {
    local cur prev subcmds
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    subcmds="configure init add reset clone mirror doctor du track untrack stat gc fsck compact repack optimize tier metadb cache staging errors version config status hydrate diff dehydrate env ls-files fetch prune logs install uninstall lock unlock locks migrate push ship import run exp workflow params metrics lfs mount unmount daemon login logout auth filter-process"

    if [[ ${COMP_CWORD} -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "${subcmds}" -- "${cur}") )
        return 0
    fi
}

complete -F _crab_completions crab
"#
}

fn generate_zsh_completion() -> &'static str {
    r"#compdef crab
# crab zsh completion
# Auto-generated by `crab install --global`

_crab() {
    local -a subcmds
    subcmds=(
        'configure:Guided cloud and repository setup'
        'init:Initialize a new crab repository'
        'add:Stage files for crab'
        'reset:Unstage files'
        'clone:Clone a crab repository'
        'mirror:Mirror a Git remote into a Crab remote'
        'doctor:Run health check'
        'du:Show disk usage'
        'track:Track file patterns'
        'untrack:Stop tracking patterns'
        'status:Report hydration state'
        'hydrate:Materialize pointer files'
        'dehydrate:Replace files with pointers'
        'ship:Add + commit + push'
        'push:Native concurrent push'
        'optimize:Optimize storage, caches, indexes, and replicas'
        'install:Install git drivers'
        'uninstall:Remove git drivers'
    )

    _arguments -C \
        '1:subcommand:->subcmd' \
        '*::arg:->args'

    case $state in
        subcmd)
            _describe 'subcommand' subcmds
            ;;
    esac
}

compdef _crab crab
"
}

fn generate_fish_completion() -> &'static str {
    r#"# crab fish completion
# Auto-generated by `crab install --global`

set -l crab_subcmds configure init add reset clone mirror doctor du track untrack status hydrate dehydrate ship push optimize install uninstall

complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "configure" -d "Guided cloud and repository setup"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "init" -d "Initialize a new crab repository"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "add" -d "Stage files for crab"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "reset" -d "Unstage files"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "clone" -d "Clone a crab repository"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "mirror" -d "Mirror a Git remote into a Crab remote"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "doctor" -d "Run health check"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "du" -d "Show disk usage"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "track" -d "Track file patterns"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "untrack" -d "Stop tracking patterns"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "status" -d "Report hydration state"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "hydrate" -d "Materialize pointer files"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "dehydrate" -d "Replace files with pointers"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "ship" -d "Add + commit + push"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "push" -d "Native concurrent push"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "optimize" -d "Optimize storage, caches, indexes, and replicas"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "install" -d "Install git drivers"
complete -c crab -n "not __fish_seen_subcommand_from $crab_subcmds" -a "uninstall" -d "Remove git drivers"
"#
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

    #[test]
    fn install_sets_filter_config() {
        let dir = temp_git_repo();
        let args = InstallArgs {
            scope: InstallScope::Local,
            force: true,
            skip_smudge: false,
            aliases: false,
            no_completions: true,
        };

        run_install_in(dir.path(), &args).unwrap();

        let output = Command::new("git")
            .args(["config", "--local", "filter.crab.required"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let val = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert_eq!(val, "true");

        let diff_command = git_config_value(dir.path(), "--local", "diff.crab.command")
            .unwrap()
            .expect("diff driver command");
        assert!(
            diff_command.contains("diff-driver"),
            "diff.crab.command should run diff-driver, got: {diff_command}",
        );
    }

    #[test]
    fn uninstall_removes_filter_config() {
        let dir = temp_git_repo();
        let args = InstallArgs {
            scope: InstallScope::Local,
            force: true,
            skip_smudge: false,
            aliases: false,
            no_completions: true,
        };

        run_install_in(dir.path(), &args).unwrap();
        run_uninstall_in(dir.path(), InstallScope::Local).unwrap();

        let output = Command::new("git")
            .args(["config", "--local", "filter.crab.process"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        // Should fail (key removed) or be empty.
        assert!(
            !output.status.success() || output.stdout.is_empty(),
            "filter config should be removed after uninstall",
        );

        let output = Command::new("git")
            .args(["config", "--local", "diff.crab.command"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            !output.status.success() || output.stdout.is_empty(),
            "diff config should be removed after uninstall",
        );
    }

    #[test]
    fn install_repairs_missing_diff_driver_without_force() {
        let dir = temp_git_repo();
        Command::new("git")
            .args([
                "config",
                "--local",
                "filter.crab.process",
                "custom filter-process",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let args = InstallArgs {
            scope: InstallScope::Local,
            force: false,
            skip_smudge: false,
            aliases: false,
            no_completions: true,
        };

        run_install_in(dir.path(), &args).unwrap();

        let filter_process = git_config_value(dir.path(), "--local", "filter.crab.process")
            .unwrap()
            .expect("filter process");
        assert_eq!(filter_process, "custom filter-process");

        let diff_command = git_config_value(dir.path(), "--local", "diff.crab.command")
            .unwrap()
            .expect("diff driver command");
        assert!(
            diff_command.contains("diff-driver"),
            "diff.crab.command should be repaired, got: {diff_command}",
        );
    }

    #[test]
    fn install_skip_smudge_sets_cat() {
        let dir = temp_git_repo();
        let args = InstallArgs {
            scope: InstallScope::Local,
            force: true,
            skip_smudge: true,
            aliases: false,
            no_completions: true,
        };

        run_install_in(dir.path(), &args).unwrap();

        let output = Command::new("git")
            .args(["config", "--local", "filter.crab.smudge"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let val = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert_eq!(val, "cat");
    }
}
