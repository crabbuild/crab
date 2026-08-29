//! Guided first-repository setup for Crab.

use std::io::IsTerminal;
use std::path::Path;

use console::Term;
use tokio_util::sync::CancellationToken;

use crate::cmd::setup::SetupArgs;
use crate::core::config::{GcListProfile, StorageProvider};
use crate::core::error::{CrabError, Result};
use crate::core::output::OutputMode;
use crate::core::project_config::ProjectConfig;
use crate::core::style::CliStyle;

/// Inputs for the guided repository configuration flow.
pub struct ConfigureArgs {
    pub remote: Option<String>,
    pub storage_provider: Option<StorageProvider>,
    pub gc_list_profile: Option<GcListProfile>,
    pub track: Vec<String>,
    pub no_auto_track: bool,
    pub dry_run: bool,
}

struct ConfigurePlan {
    remote: String,
    storage_provider: Option<StorageProvider>,
    gc_list_profile: Option<GcListProfile>,
}

/// Configure cloud storage, Git integration, and large-file tracking.
pub async fn run_configure(args: ConfigureArgs, cancel: &CancellationToken) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_configure_at(&cwd, args, cancel).await
}

/// Configure a repository rooted at an explicit path.
pub async fn run_configure_at(
    root: &Path,
    args: ConfigureArgs,
    cancel: &CancellationToken,
) -> Result<()> {
    let plan = resolve_plan(root, &args)?;
    let style = CliStyle::resolve(OutputMode::Text);

    if args.dry_run {
        eprintln!("{}", style.bold("Crab setup plan"));
        eprintln!("  Remote       {}", plan.remote);
        eprintln!(
            "  Provider     {}",
            plan.storage_provider
                .as_ref()
                .map_or("infer from remote", StorageProvider::label)
        );
        eprintln!(
            "  Large files  {}",
            tracking_summary(&args.track, args.no_auto_track)
        );
        eprintln!(
            "  GC listing   {}",
            plan.gc_list_profile.map_or(
                "adaptive (preserves an existing choice)",
                GcListProfile::as_str
            )
        );
        eprintln!("\nRun again without --dry-run to apply this plan.");
        return Ok(());
    }

    eprintln!("{}", style.bold("Configure Crab"));
    eprintln!("  Remote   {}", plan.remote);
    eprintln!(
        "  Provider {}",
        plan.storage_provider
            .as_ref()
            .map_or("infer from remote", StorageProvider::label)
    );
    eprintln!(
        "  GC listing {}\n",
        plan.gc_list_profile.map_or(
            "adaptive (preserves an existing choice)",
            GcListProfile::as_str
        )
    );

    crate::cmd::init::run_init_for_configure(
        &plan.remote,
        root,
        cancel,
        plan.storage_provider,
        plan.gc_list_profile,
    )
    .await?;
    crate::cmd::init::initialize_remote_repository(&plan.remote, root, cancel).await?;

    crate::cmd::setup::run_setup_at(
        root,
        &SetupArgs {
            no_auto_track: args.no_auto_track,
            track: args.track,
            include: Vec::new(),
            exclude: Vec::new(),
            dry_run: false,
            force: false,
            mode: OutputMode::Text,
        },
        cancel,
    )
    .await?;

    Ok(())
}

fn resolve_plan(root: &Path, args: &ConfigureArgs) -> Result<ConfigurePlan> {
    if let Some(remote) = args.remote.as_ref() {
        return configure_plan(
            remote.clone(),
            args.storage_provider.clone(),
            args.gc_list_profile,
        );
    }

    if let Some(config) = ProjectConfig::discover(root) {
        let storage_provider = args.storage_provider.clone().or_else(|| {
            config
                .auth
                .as_ref()
                .and_then(|auth| auth.storage_provider.clone())
        });
        return configure_plan(config.remote.url, storage_provider, args.gc_list_profile);
    }

    if !std::io::stdin().is_terminal() {
        return Err(CrabError::Configuration {
            key: "remote".to_owned(),
            origin: "No remote provided. Usage: crab configure <REMOTE> --provider <s3|gcs|azure>"
                .to_owned(),
        });
    }

    let provider = match args.storage_provider.clone() {
        Some(provider) => provider,
        None => prompt_provider()?,
    };
    let remote = prompt_remote(&provider)?;

    Ok(ConfigurePlan {
        remote,
        storage_provider: Some(provider),
        gc_list_profile: args.gc_list_profile,
    })
}

fn configure_plan(
    remote: String,
    storage_provider: Option<StorageProvider>,
    gc_list_profile: Option<GcListProfile>,
) -> Result<ConfigurePlan> {
    if !crate::cmd::init::is_valid_init_url(&remote) {
        return Err(CrabError::Configuration {
            key: "remote".to_owned(),
            origin: format!("Invalid remote URL: {remote}"),
        });
    }
    Ok(ConfigurePlan {
        remote,
        storage_provider,
        gc_list_profile,
    })
}

fn prompt_provider() -> Result<StorageProvider> {
    let term = Term::stderr();
    eprintln!("Choose where Crab should store large-file data:");
    eprintln!("  1  Amazon S3 or S3-compatible storage");
    eprintln!("  2  Google Cloud Storage");
    eprintln!("  3  Azure Blob Storage");

    for attempt in 0..3 {
        eprint!("Provider [1]: ");
        let input = term.read_line().map_err(CrabError::Io)?;
        if let Some(provider) = parse_provider_choice(&input) {
            return Ok(provider);
        }
        if attempt < 2 {
            eprintln!("Choose 1, 2, or 3 (or enter s3, gcs, or azure).");
        }
    }

    Err(CrabError::Configuration {
        key: "storage provider".to_owned(),
        origin: "No valid provider selected after 3 attempts".to_owned(),
    })
}

fn prompt_remote(provider: &StorageProvider) -> Result<String> {
    let term = Term::stderr();
    eprintln!();
    eprintln!("Enter a bucket/container and repository path.");
    eprintln!("Example: team-data/models");

    for attempt in 0..3 {
        eprint!("Storage path: ");
        let input = term.read_line().map_err(CrabError::Io)?;
        if let Some(remote) = remote_from_input(provider, &input) {
            return Ok(remote);
        }
        if attempt < 2 {
            eprintln!("Include both a bucket/container and repository path.");
        }
    }

    Err(CrabError::Configuration {
        key: "remote".to_owned(),
        origin: "No valid storage path entered after 3 attempts".to_owned(),
    })
}

fn parse_provider_choice(input: &str) -> Option<StorageProvider> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "s3" | "aws" => Some(StorageProvider::S3),
        "2" | "gcs" | "gs" | "google" => Some(StorageProvider::Gcs),
        "3" | "azure" | "az" => Some(StorageProvider::Azure),
        _ => None,
    }
}

fn remote_from_input(provider: &StorageProvider, input: &str) -> Option<String> {
    let input = input.trim().trim_matches('/');
    if input.is_empty() {
        return None;
    }

    if input.contains("://") {
        return crate::cmd::init::is_valid_init_url(input).then(|| input.to_owned());
    }

    let (_, repo_path) = input.split_once('/')?;
    if repo_path.is_empty() {
        return None;
    }

    let scheme = match provider {
        StorageProvider::S3 | StorageProvider::Auto => "s3",
        StorageProvider::Gcs => "gs",
        StorageProvider::Azure => "azure",
    };
    let remote = format!("{scheme}://{input}");
    crate::cmd::init::is_valid_init_url(&remote).then_some(remote)
}

fn tracking_summary(patterns: &[String], no_auto_track: bool) -> String {
    if !patterns.is_empty() {
        return patterns.join(", ");
    }
    if no_auto_track {
        "filter only; do not scan".to_owned()
    } else {
        "scan and suggest patterns".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_prompt_accepts_numbers_names_and_default() {
        assert_eq!(parse_provider_choice(""), Some(StorageProvider::S3));
        assert_eq!(parse_provider_choice("2"), Some(StorageProvider::Gcs));
        assert_eq!(parse_provider_choice("AZURE"), Some(StorageProvider::Azure));
        assert_eq!(parse_provider_choice("local"), None);
    }

    #[test]
    fn storage_path_uses_provider_specific_scheme() {
        assert_eq!(
            remote_from_input(&StorageProvider::S3, "team/models").as_deref(),
            Some("s3://team/models")
        );
        assert_eq!(
            remote_from_input(&StorageProvider::Gcs, "team/models").as_deref(),
            Some("gs://team/models")
        );
        assert_eq!(
            remote_from_input(&StorageProvider::Azure, "team/models").as_deref(),
            Some("azure://team/models")
        );
    }

    #[test]
    fn storage_path_accepts_full_remote_and_rejects_bucket_only() {
        assert_eq!(
            remote_from_input(&StorageProvider::S3, "gs://team/models").as_deref(),
            Some("gs://team/models")
        );
        assert_eq!(remote_from_input(&StorageProvider::S3, "team"), None);
    }

    #[test]
    fn explicit_plan_rejects_invalid_remote() {
        let result = configure_plan("team/models".to_owned(), Some(StorageProvider::S3), None);
        assert!(matches!(result, Err(CrabError::Configuration { .. })));
    }

    #[tokio::test]
    async fn dry_run_does_not_create_repository_state() {
        let root = tempfile::tempdir().unwrap();
        let args = ConfigureArgs {
            remote: Some("s3://team-data/models".to_owned()),
            storage_provider: Some(StorageProvider::S3),
            gc_list_profile: None,
            track: vec!["*.safetensors".to_owned()],
            no_auto_track: false,
            dry_run: true,
        };

        run_configure_at(root.path(), args, &CancellationToken::new())
            .await
            .unwrap();

        assert!(!root.path().join(".git").exists());
        assert!(!root.path().join(".crab").exists());
        assert!(!root.path().join(".crab.toml").exists());
        assert!(!root.path().join(".gitattributes").exists());
    }
}
