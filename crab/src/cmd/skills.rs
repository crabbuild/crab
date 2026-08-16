//! Install the self-contained skill catalog shipped with Crab.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::Serialize;
use tempfile::NamedTempFile;

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};

const SKILLS_SCHEMA_VERSION: &str = "1.0";

struct SkillAsset {
    name: &'static str,
    content: &'static str,
}

static SKILL_ASSETS: &[SkillAsset] = &[
    SkillAsset {
        name: "crab-cli-core",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-cli-core/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-cli-verification",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-cli-verification/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-diagnostics-recovery",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-diagnostics-recovery/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-git-sync",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-git-sync/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-large-files",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-large-files/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-lfs",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-lfs/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-managed-operations",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-managed-operations/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-mount",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-mount/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-release-publish",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-release-publish/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-repository-lifecycle",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-repository-lifecycle/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-storage-ops",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-storage-ops/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-tier-replication",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-tier-replication/SKILL.md"
        )),
    },
    SkillAsset {
        name: "crab-workflow",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-workflow/SKILL.md"
        )),
    },
];

/// Skill-management subcommands.
#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    /// List the Crab skills embedded in this binary.
    List {
        /// Emit a structured JSON envelope.
        #[arg(long)]
        json: bool,
    },
    /// Install one embedded skill into an agent provider's skill home.
    Install(InstallArgs),
}

/// Arguments for `crab skills install`.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Agent provider: codex, claude, or gemini.
    #[arg(value_name = "PROVIDER")]
    pub provider: String,
    /// Directory name to create under the provider's skills directory.
    #[arg(value_name = "NAME")]
    pub name: String,
    /// Bundled Crab skill ID to install.
    #[arg(long, value_name = "NAME")]
    pub skill: String,
    /// Override the provider's discovered skills directory.
    #[arg(long, value_name = "PATH")]
    pub root: Option<PathBuf>,
    /// Replace the existing `SKILL.md` in the destination directory.
    #[arg(long, short)]
    pub force: bool,
    /// Emit a structured JSON envelope.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct SkillListPayload {
    skills: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct SkillInstallPayload {
    provider: String,
    name: String,
    skill: String,
    path: String,
}

impl SkillsCommand {
    /// Resolve the output mode for this subcommand.
    pub fn output_mode(&self) -> OutputMode {
        match self {
            Self::List { json } | Self::Install(InstallArgs { json, .. }) => {
                OutputMode::from_flags(*json, false)
            }
        }
    }
}

/// Execute a `crab skills` command.
pub fn run(command: SkillsCommand) -> Result<()> {
    match command {
        SkillsCommand::List { json } => run_list(OutputMode::from_flags(json, false)),
        SkillsCommand::Install(args) => run_install(&args),
    }
}

fn run_list(mode: OutputMode) -> Result<()> {
    let mut skills: Vec<String> = SKILL_ASSETS
        .iter()
        .map(|asset| asset.name.to_owned())
        .collect();
    skills.sort_unstable();

    if mode == OutputMode::Json {
        emit_json(
            "skills.list",
            SKILLS_SCHEMA_VERSION,
            SkillListPayload { skills },
        );
    } else {
        for skill in skills {
            println!("{skill}");
        }
    }
    Ok(())
}

fn run_install(args: &InstallArgs) -> Result<()> {
    let (provider, provider_root) = resolve_provider(&args.provider, args.root.as_deref())?;
    let asset = find_skill(&args.skill)?;
    validate_name(&args.name, "name")?;

    let destination = provider_root.join(&args.name);
    install_skill(&destination, asset.content, args.force)?;

    let payload = SkillInstallPayload {
        provider,
        name: args.name.clone(),
        skill: asset.name.to_owned(),
        path: destination.display().to_string(),
    };

    if args.json {
        emit_json("skills.install", SKILLS_SCHEMA_VERSION, payload);
    } else {
        println!(
            "installed {} as {} for {} at {}",
            payload.skill, payload.name, payload.provider, payload.path
        );
    }
    Ok(())
}

fn find_skill(name: &str) -> Result<&'static SkillAsset> {
    SKILL_ASSETS
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| config_error("skill", format!("unknown bundled skill '{name}'")))
}

fn validate_name(name: &str, field: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && Path::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|component| component == name)
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        });
    if valid {
        Ok(())
    } else {
        Err(config_error(
            field,
            "must be one path component containing only letters, digits, '.', '-' or '_'"
                .to_owned(),
        ))
    }
}

fn resolve_provider(provider: &str, root: Option<&Path>) -> Result<(String, PathBuf)> {
    let normalized = provider.to_ascii_lowercase();
    let (name, env_var, default_dir) = match normalized.as_str() {
        "codex" => ("codex", "CODEX_HOME", ".codex"),
        "claude" => ("claude", "CLAUDE_HOME", ".claude"),
        "gemini" => ("gemini", "GEMINI_HOME", ".gemini"),
        _ => {
            return Err(config_error(
                "provider",
                format!("unsupported provider '{provider}'; expected codex, claude, or gemini"),
            ));
        }
    };

    let skills_root = match root {
        Some(path) => path.to_owned(),
        None => provider_home(env_var, default_dir)?.join("skills"),
    };
    Ok((name.to_owned(), skills_root))
}

fn provider_home(env_var: &str, default_dir: &str) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| {
            config_error(
                "home",
                "could not determine the user home directory".to_owned(),
            )
        })?;
    Ok(home.join(default_dir))
}

fn install_skill(destination: &Path, content: &str, force: bool) -> Result<()> {
    if destination.exists() {
        if !destination.is_dir() {
            return Err(config_error(
                "destination",
                format!("{} exists and is not a directory", destination.display()),
            ));
        }
        if !force {
            return Err(config_error(
                "destination",
                format!(
                    "{} already exists; pass --force to replace SKILL.md",
                    destination.display()
                ),
            ));
        }
    } else {
        std::fs::create_dir_all(destination)?;
    }

    let skill_path = destination.join("SKILL.md");
    let mut temporary = NamedTempFile::new_in(destination).map_err(CrabError::Io)?;
    temporary
        .write_all(content.as_bytes())
        .map_err(CrabError::Io)?;
    temporary.as_file().sync_all().map_err(CrabError::Io)?;

    temporary
        .persist(&skill_path)
        .map_err(|error| CrabError::Io(error.error))?;
    Ok(())
}

fn config_error(key: &str, detail: String) -> CrabError {
    CrabError::Configuration {
        key: format!("{key}: {detail}"),
        origin: "skills".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_contains_all_crab_skill_domains() {
        for name in [
            "crab-cli-core",
            "crab-cli-verification",
            "crab-diagnostics-recovery",
            "crab-git-sync",
            "crab-large-files",
            "crab-lfs",
            "crab-managed-operations",
            "crab-mount",
            "crab-release-publish",
            "crab-repository-lifecycle",
            "crab-storage-ops",
            "crab-tier-replication",
            "crab-workflow",
        ] {
            assert!(find_skill(name).is_ok(), "missing catalog entry: {name}");
        }
    }

    #[test]
    fn install_writes_a_self_contained_skill() -> Result<()> {
        let root = tempfile::tempdir().map_err(CrabError::Io)?;
        let destination = root.path().join("skill");
        let asset = find_skill("crab-large-files")?;

        install_skill(&destination, asset.content, false)?;

        let installed = std::fs::read_to_string(destination.join("SKILL.md"))?;
        assert_eq!(installed, asset.content);
        Ok(())
    }

    #[test]
    fn install_requires_force_for_existing_skill_directory() -> Result<()> {
        let root = tempfile::tempdir().map_err(CrabError::Io)?;
        let destination = root.path().join("skill");
        std::fs::create_dir_all(&destination)?;
        std::fs::write(destination.join("SKILL.md"), "old")?;

        let error =
            install_skill(&destination, "new", false).expect_err("existing skill must fail");
        assert!(error.to_string().contains("--force"));
        Ok(())
    }

    #[test]
    fn force_replaces_skill_without_removing_other_files() -> Result<()> {
        let root = tempfile::tempdir().map_err(CrabError::Io)?;
        let destination = root.path().join("skill");
        std::fs::create_dir_all(&destination)?;
        std::fs::write(destination.join("SKILL.md"), "old")?;
        std::fs::write(destination.join("local-notes.txt"), "keep")?;

        install_skill(&destination, "new", true)?;

        assert_eq!(
            std::fs::read_to_string(destination.join("SKILL.md"))?,
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("local-notes.txt"))?,
            "keep"
        );
        Ok(())
    }

    #[test]
    fn install_rejects_path_traversal_names() {
        assert!(validate_name("../skill", "name").is_err());
        assert!(validate_name("nested/skill", "name").is_err());
        assert!(validate_name("skill-name", "name").is_ok());
    }
}
