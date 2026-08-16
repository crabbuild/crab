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

struct ProviderSpec {
    name: &'static str,
    home_env: Option<&'static str>,
    env_dir: Option<&'static str>,
    default_dir: Option<&'static str>,
}

impl ProviderSpec {
    const fn global(name: &'static str, default_dir: &'static str) -> Self {
        Self {
            name,
            home_env: None,
            env_dir: None,
            default_dir: Some(default_dir),
        }
    }

    const fn env(
        name: &'static str,
        home_env: &'static str,
        env_dir: &'static str,
        default_dir: &'static str,
    ) -> Self {
        Self {
            name,
            home_env: Some(home_env),
            env_dir: Some(env_dir),
            default_dir: Some(default_dir),
        }
    }

    const fn project_only(name: &'static str) -> Self {
        Self {
            name,
            home_env: None,
            env_dir: None,
            default_dir: None,
        }
    }
}

static PROVIDER_SPECS: &[ProviderSpec] = &[
    ProviderSpec::global("aider-desk", ".aider-desk/skills"),
    ProviderSpec::global("adal", ".adal/skills"),
    ProviderSpec::global("amp", ".config/agents/skills"),
    ProviderSpec::global("antigravity", ".gemini/antigravity/skills"),
    ProviderSpec::global("antigravity-cli", ".gemini/antigravity-cli/skills"),
    ProviderSpec::global("astrbot", ".astrbot/data/skills"),
    ProviderSpec::global("autohand-code", ".autohand/skills"),
    ProviderSpec::global("augment", ".augment/skills"),
    ProviderSpec::global("bob", ".bob/skills"),
    ProviderSpec::env("claude", "CLAUDE_HOME", "skills", ".claude/skills"),
    ProviderSpec::env("claude-code", "CLAUDE_HOME", "skills", ".claude/skills"),
    ProviderSpec::global("cline", ".cline/skills"),
    ProviderSpec::global("codearts-agent", ".codeartsdoer/skills"),
    ProviderSpec::global("codebuddy", ".codebuddy/skills"),
    ProviderSpec::global("codemaker", ".codemaker/skills"),
    ProviderSpec::global("codestudio", ".codestudio/skills"),
    ProviderSpec::env("codex", "CODEX_HOME", "skills", ".codex/skills"),
    ProviderSpec::global("command-code", ".commandcode/skills"),
    ProviderSpec::global("continue", ".continue/skills"),
    ProviderSpec::global("copilot", ".copilot/skills"),
    ProviderSpec::global("crush", ".config/crush/skills"),
    ProviderSpec::global("cortex", ".snowflake/cortex/skills"),
    ProviderSpec::global("cursor", ".cursor/skills"),
    ProviderSpec::global("deepagents", ".deepagents/agent/skills"),
    ProviderSpec::global("dexto", ".agents/skills"),
    ProviderSpec::global("devin", ".config/devin/skills"),
    ProviderSpec::global("droid", ".factory/skills"),
    ProviderSpec::project_only("eve"),
    ProviderSpec::global("firebender", ".firebender/skills"),
    ProviderSpec::global("forgecode", ".forge/skills"),
    ProviderSpec::env("gemini", "GEMINI_HOME", "skills", ".gemini/skills"),
    ProviderSpec::env("gemini-cli", "GEMINI_HOME", "skills", ".gemini/skills"),
    ProviderSpec::global("github-copilot", ".copilot/skills"),
    ProviderSpec::global("goose", ".config/goose/skills"),
    ProviderSpec::global("grok", ".grok/skills"),
    ProviderSpec::global("hermes-agent", ".hermes/skills"),
    ProviderSpec::global("iflow-cli", ".iflow/skills"),
    ProviderSpec::global("inference-sh", ".inferencesh/skills"),
    ProviderSpec::global("jazz", ".jazz/skills"),
    ProviderSpec::global("junie", ".junie/skills"),
    ProviderSpec::global("kilo", ".kilocode/skills"),
    ProviderSpec::global("kimchi", ".config/kimchi/harness/skills"),
    ProviderSpec::global("kimi", ".agents/skills"),
    ProviderSpec::global("kimi-code-cli", ".agents/skills"),
    ProviderSpec::global("kiro", ".kiro/skills"),
    ProviderSpec::global("kiro-cli", ".kiro/skills"),
    ProviderSpec::global("kode", ".kode/skills"),
    ProviderSpec::global("lingma", ".lingma/skills"),
    ProviderSpec::global("loaf", ".agents/skills"),
    ProviderSpec::global("mcpjam", ".mcpjam/skills"),
    ProviderSpec::global("minimax-code", ".minimax/skills"),
    ProviderSpec::global("mistral-vibe", ".vibe/skills"),
    ProviderSpec::global("moxby", ".moxby/skills"),
    ProviderSpec::global("mux", ".mux/skills"),
    ProviderSpec::global("neovate", ".neovate/skills"),
    ProviderSpec::global("ona", ".ona/skills"),
    ProviderSpec::global("openclaw", ".openclaw/skills"),
    ProviderSpec::global("opencode", ".config/opencode/skills"),
    ProviderSpec::global("openhands", ".openhands/skills"),
    ProviderSpec::global("pi", ".pi/agent/skills"),
    ProviderSpec::global("pochi", ".pochi/skills"),
    ProviderSpec::project_only("promptscript"),
    ProviderSpec::global("qoder", ".qoder/skills"),
    ProviderSpec::global("qoder-cn", ".qoder-cn/skills"),
    ProviderSpec::global("qwen", ".qwen/skills"),
    ProviderSpec::global("qwen-code", ".qwen/skills"),
    ProviderSpec::global("reasonix", ".reasonix/skills"),
    ProviderSpec::global("replit", ".config/agents/skills"),
    ProviderSpec::global("roo", ".roo/skills"),
    ProviderSpec::global("roo-code", ".roo/skills"),
    ProviderSpec::global("rovodev", ".rovodev/skills"),
    ProviderSpec::global("tabnine-cli", ".tabnine/agent/skills"),
    ProviderSpec::global("terramind", ".terramind/skills"),
    ProviderSpec::global("tinycloud", ".tinycloud/skills"),
    ProviderSpec::global("trae", ".trae/skills"),
    ProviderSpec::global("trae-cn", ".trae-cn/skills"),
    ProviderSpec::global("universal", ".config/agents/skills"),
    ProviderSpec::global("warp", ".agents/skills"),
    ProviderSpec::global("windsurf", ".codeium/windsurf/skills"),
    ProviderSpec::global("zcode", ".zcode/skills"),
    ProviderSpec::global("zed", ".agents/skills"),
    ProviderSpec::global("zencoder", ".zencoder/skills"),
    ProviderSpec::global("zenflow", ".zencoder/skills"),
];

static SKILL_ASSETS: &[SkillAsset] = &[
    SkillAsset {
        name: "crab-cli-core",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/crab-cli-core/SKILL.md"
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
    /// Agent Skills provider ID, such as codex, claude-code, cursor, or opencode.
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
    let spec = PROVIDER_SPECS
        .iter()
        .find(|candidate| candidate.name == normalized)
        .ok_or_else(|| {
            config_error(
                "provider",
                format!("unsupported provider '{provider}'; see the supported Agent Skills providers in the Crab documentation"),
            )
        })?;

    let skills_root = match root {
        Some(path) => path.to_owned(),
        None => default_provider_root(spec)?,
    };
    Ok((spec.name.to_owned(), skills_root))
}

fn default_provider_root(spec: &ProviderSpec) -> Result<PathBuf> {
    if let Some(env_var) = spec.home_env {
        if let Some(path) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
            let Some(env_dir) = spec.env_dir else {
                return Err(config_error(
                    "provider",
                    format!(
                        "provider '{}' has an invalid environment configuration",
                        spec.name
                    ),
                ));
            };
            return Ok(PathBuf::from(path).join(env_dir));
        }
    }

    let Some(default_dir) = spec.default_dir else {
        return Err(config_error(
            "provider",
            format!(
                "provider '{}' has no global skills directory; pass --root PATH for a project",
                spec.name
            ),
        ));
    };

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
    fn local_verification_skill_is_not_packaged() {
        assert!(find_skill("crab-cli-verification").is_err());
    }

    #[test]
    fn provider_registry_covers_major_agent_hosts() {
        for provider in [
            "codex",
            "claude-code",
            "gemini-cli",
            "cursor",
            "windsurf",
            "cline",
            "roo",
            "github-copilot",
            "opencode",
            "openhands",
            "goose",
            "continue",
            "amp",
            "replit",
            "antigravity",
            "kiro-cli",
            "qwen-code",
            "kimi-code-cli",
            "junie",
            "droid",
            "trae",
            "qoder",
            "pi",
            "crush",
            "zed",
        ] {
            assert!(
                resolve_provider(provider, Some(Path::new("skills"))).is_ok(),
                "missing provider: {provider}"
            );
        }
        assert!(PROVIDER_SPECS.len() >= 70);
    }

    #[test]
    fn provider_names_are_case_insensitive_and_project_roots_are_supported() -> Result<()> {
        let root = Path::new("project/.agents/skills");
        let (provider, resolved) = resolve_provider("OpenCode", Some(root))?;
        assert_eq!(provider, "opencode");
        assert_eq!(resolved, root);

        assert!(resolve_provider("eve", None).is_err());
        let (_, resolved) = resolve_provider("eve", Some(root))?;
        assert_eq!(resolved, root);
        Ok(())
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
