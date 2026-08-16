use std::fmt::Display;

use clap::{Args, Subcommand, ValueEnum};
use crab_auth::managed::{
    EntityTag, IdempotencyKey, IssuedServiceToken, ManagedApiError, OrganizationRole,
};
use crab_auth::token_cache::expand_token_cache_path;
use crab_auth_store::{ManagedControlPlane, ManagedRepositoryError, ManagedRepositoryResolver};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};

#[derive(Debug, Args)]
pub struct OrganizationArgs {
    /// Managed service authority; defaults to the active login profile.
    #[arg(long, global = true, value_name = "AUTHORITY")]
    pub service: Option<String>,
    /// Emit structured JSON output.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: OrganizationCommand,
}

#[derive(Debug, Subcommand)]
pub enum OrganizationCommand {
    /// Create an organization and make the current principal its owner.
    Create { organization: String },
    /// List organizations visible to the current principal.
    List(ManagedPageArgs),
    /// Show one organization and its revision.
    Info { organization: String },
    /// Rename an organization.
    Rename {
        organization: String,
        new_slug: String,
        #[arg(long)]
        revision: u64,
    },
    /// Soft-delete an organization.
    Delete {
        organization: String,
        #[arg(long)]
        revision: u64,
    },
}

#[derive(Debug, Args)]
pub struct RepositoryArgs {
    /// Managed service authority; defaults to the active login profile.
    #[arg(long, global = true, value_name = "AUTHORITY")]
    pub service: Option<String>,
    /// Emit structured JSON output.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: RepositoryCommand,
}

#[derive(Debug, Subcommand)]
pub enum RepositoryCommand {
    /// Create a logical repository as ORG/REPO.
    Create { repository: String },
    /// List repositories in an organization.
    List {
        organization: String,
        #[command(flatten)]
        page: ManagedPageArgs,
    },
    /// Show one logical repository and its revision.
    Info { repository: String },
    /// Rename a logical repository.
    Rename {
        repository: String,
        new_slug: String,
        #[arg(long)]
        revision: u64,
    },
    /// Archive a logical repository.
    Archive {
        repository: String,
        #[arg(long)]
        revision: u64,
    },
    /// Soft-delete a logical repository.
    Delete {
        repository: String,
        #[arg(long)]
        revision: u64,
    },
    /// Restore a soft-deleted logical repository.
    Restore {
        repository: String,
        #[arg(long)]
        revision: u64,
    },
}

#[derive(Debug, Args)]
pub struct MemberArgs {
    /// Managed service authority; defaults to the active login profile.
    #[arg(long, global = true, value_name = "AUTHORITY")]
    pub service: Option<String>,
    /// Emit structured JSON output.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: MemberCommand,
}

#[derive(Debug, Subcommand)]
pub enum MemberCommand {
    /// List organization members.
    List {
        organization: String,
        #[command(flatten)]
        page: ManagedPageArgs,
    },
    /// Add an organization member.
    Add {
        organization: String,
        principal_id: Uuid,
        #[arg(long, value_enum)]
        role: CliOrganizationRole,
    },
    /// Change an organization member's role.
    Update {
        organization: String,
        principal_id: Uuid,
        #[arg(long, value_enum)]
        role: CliOrganizationRole,
        #[arg(long)]
        revision: u64,
    },
    /// Remove an organization member.
    Remove {
        organization: String,
        principal_id: Uuid,
        #[arg(long)]
        revision: u64,
    },
}

#[derive(Debug, Args)]
pub struct ServiceAccountArgs {
    /// Managed service authority; defaults to the active login profile.
    #[arg(long, global = true, value_name = "AUTHORITY")]
    pub service: Option<String>,
    /// Emit structured JSON output. Token secrets appear only in create-token and rotate output.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: ServiceAccountCommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceAccountCommand {
    /// List service accounts in an organization.
    List { organization: String },
    /// Create an OIDC workload identity.
    CreateWorkload {
        organization: String,
        name: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        issuer: String,
        #[arg(long)]
        subject: String,
    },
    /// Create an opaque token identity and print its secret once.
    CreateToken {
        organization: String,
        name: String,
        #[arg(long)]
        role: String,
        #[arg(long, default_value_t = 2_592_000)]
        expires_in_seconds: u64,
    },
    /// Rotate an opaque token and print its replacement secret once.
    Rotate {
        organization: String,
        account_id: Uuid,
        #[arg(long)]
        revision: u64,
        #[arg(long, default_value_t = 2_592_000)]
        expires_in_seconds: u64,
        #[arg(long, default_value_t = 0)]
        overlap_seconds: u64,
    },
    /// Revoke a service account.
    Revoke {
        organization: String,
        account_id: Uuid,
        #[arg(long)]
        revision: u64,
    },
}

#[derive(Debug, Clone, Args)]
pub struct ManagedPageArgs {
    /// Opaque cursor returned by the preceding page.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Maximum entities returned in this page.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=100))]
    pub limit: u16,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliOrganizationRole {
    Owner,
    Admin,
    Writer,
    Reader,
    Billing,
}

impl From<CliOrganizationRole> for OrganizationRole {
    fn from(value: CliOrganizationRole) -> Self {
        match value {
            CliOrganizationRole::Owner => Self::Owner,
            CliOrganizationRole::Admin => Self::Admin,
            CliOrganizationRole::Writer => Self::Writer,
            CliOrganizationRole::Reader => Self::Reader,
            CliOrganizationRole::Billing => Self::Billing,
        }
    }
}

impl OrganizationArgs {
    #[must_use]
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

impl RepositoryArgs {
    #[must_use]
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

impl MemberArgs {
    #[must_use]
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

impl ServiceAccountArgs {
    #[must_use]
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

pub async fn run_organization(
    args: OrganizationArgs,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    let mode = args.output_mode();
    let connection = connect(config, args.service.as_deref(), cancel).await?;
    let client = &connection.client;
    match args.command {
        OrganizationCommand::Create { organization } => {
            let created = client
                .create_organization(&organization, &idempotency_key()?)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.organization", &created.value, || {
                format!(
                    "Created organization {} (revision {})",
                    created.value.slug, created.value.revision
                )
            });
        }
        OrganizationCommand::List(page) => {
            let cursor = page_cursor(page.cursor)?;
            let listed = client
                .list_organizations(cursor.as_ref(), page.limit)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.organization.list", &listed, || {
                listed
                    .organizations
                    .iter()
                    .map(|organization| {
                        format!(
                            "{}\t{:?}\t{}",
                            organization.slug, organization.state, organization.revision
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        OrganizationCommand::Info { organization } => {
            let found = client
                .organization(&organization)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.organization", &found.value, || {
                format!(
                    "{}\t{:?}\trevision {}",
                    found.value.slug, found.value.state, found.value.revision
                )
            });
        }
        OrganizationCommand::Rename {
            organization,
            new_slug,
            revision,
        } => {
            let updated = client
                .update_organization(
                    &organization,
                    &new_slug,
                    &revision_etag(revision)?,
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.organization", &updated.value, || {
                format!(
                    "Renamed organization to {} (revision {})",
                    updated.value.slug, updated.value.revision
                )
            });
        }
        OrganizationCommand::Delete {
            organization,
            revision,
        } => {
            client
                .delete_organization(
                    &organization,
                    &revision_etag(revision)?,
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            let deleted = MutationResult::new("organization.delete", organization);
            emit(mode, "managed.mutation", &deleted, || {
                "Organization deleted".to_owned()
            });
        }
    }
    Ok(())
}

pub async fn run_repository(
    args: RepositoryArgs,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    let mode = args.output_mode();
    let connection = connect(config, args.service.as_deref(), cancel).await?;
    let client = &connection.client;
    match args.command {
        RepositoryCommand::Create { repository } => {
            let (organization, repository) = repository_name(&repository)?;
            let created = client
                .create_repository(organization, repository, &idempotency_key()?)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.repository", &created.value, || {
                format!("Created {}", created.value.canonical_url)
            });
        }
        RepositoryCommand::List { organization, page } => {
            let cursor = page_cursor(page.cursor)?;
            let listed = client
                .list_repositories(&organization, cursor.as_ref(), page.limit)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.repository.list", &listed, || {
                listed
                    .repositories
                    .iter()
                    .map(|repository| {
                        format!(
                            "{}\t{:?}\t{}",
                            repository.canonical_url, repository.state, repository.revision
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        RepositoryCommand::Info { repository } => {
            let (organization, repository) = repository_name(&repository)?;
            let found = client
                .resolve_repository(organization, repository)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.repository", &found.value, || {
                format!(
                    "{}\t{:?}\trevision {}",
                    found.value.canonical_url, found.value.state, found.value.revision
                )
            });
        }
        RepositoryCommand::Rename {
            repository,
            new_slug,
            revision,
        } => {
            let (organization, repository) = repository_name(&repository)?;
            let updated = client
                .rename_repository(
                    organization,
                    repository,
                    &new_slug,
                    &revision_etag(revision)?,
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.repository", &updated.value, || {
                format!("Renamed repository to {}", updated.value.canonical_url)
            });
        }
        RepositoryCommand::Archive {
            repository,
            revision,
        } => {
            let (organization, repository) = repository_name(&repository)?;
            let updated = client
                .archive_repository(
                    organization,
                    repository,
                    &revision_etag(revision)?,
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.repository", &updated.value, || {
                format!("Archived {}", updated.value.canonical_url)
            });
        }
        RepositoryCommand::Delete {
            repository,
            revision,
        } => {
            let (organization, repository) = repository_name(&repository)?;
            let updated = client
                .delete_repository(
                    organization,
                    repository,
                    &revision_etag(revision)?,
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.repository", &updated.value, || {
                format!("Deleted {}", updated.value.canonical_url)
            });
        }
        RepositoryCommand::Restore {
            repository,
            revision,
        } => {
            let (organization, repository) = repository_name(&repository)?;
            let updated = client
                .restore_repository(
                    organization,
                    repository,
                    &revision_etag(revision)?,
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.repository", &updated.value, || {
                format!("Restored {}", updated.value.canonical_url)
            });
        }
    }
    Ok(())
}

pub async fn run_member(
    args: MemberArgs,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    let mode = args.output_mode();
    let connection = connect(config, args.service.as_deref(), cancel).await?;
    let client = &connection.client;
    match args.command {
        MemberCommand::List { organization, page } => {
            let cursor = page_cursor(page.cursor)?;
            let listed = client
                .list_organization_members(&organization, cursor.as_ref(), page.limit)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.member.list", &listed, || {
                listed
                    .members
                    .iter()
                    .map(|member| {
                        format!(
                            "{}\t{:?}\t{}",
                            member.principal_id, member.role, member.revision
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        MemberCommand::Add {
            organization,
            principal_id,
            role,
        } => {
            let added = client
                .add_organization_member(
                    &organization,
                    principal_id,
                    role.into(),
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.member", &added.value, || {
                format!("Added member {}", added.value.principal_id)
            });
        }
        MemberCommand::Update {
            organization,
            principal_id,
            role,
            revision,
        } => {
            let updated = client
                .update_organization_member(
                    &organization,
                    principal_id,
                    role.into(),
                    &revision_etag(revision)?,
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.member", &updated.value, || {
                format!("Updated member {}", updated.value.principal_id)
            });
        }
        MemberCommand::Remove {
            organization,
            principal_id,
            revision,
        } => {
            client
                .remove_organization_member(
                    &organization,
                    principal_id,
                    &revision_etag(revision)?,
                    &idempotency_key()?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            let removed = MutationResult::new("member.remove", principal_id);
            emit(mode, "managed.mutation", &removed, || {
                "Member removed".to_owned()
            });
        }
    }
    Ok(())
}

pub async fn run_service_account(
    args: ServiceAccountArgs,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    let mode = args.output_mode();
    let connection = connect(config, args.service.as_deref(), cancel).await?;
    let client = &connection.client;
    match args.command {
        ServiceAccountCommand::List { organization } => {
            let listed = client
                .list_service_accounts(&organization)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.service_account.list", &listed, || {
                listed
                    .accounts
                    .iter()
                    .map(|account| {
                        format!(
                            "{}\t{}\t{}\t{}",
                            account.id, account.name, account.kind, account.revision
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        ServiceAccountCommand::CreateWorkload {
            organization,
            name,
            role,
            issuer,
            subject,
        } => {
            let created = client
                .create_workload_service_account(&organization, &name, &role, &issuer, &subject)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit(mode, "managed.service_account", &created.value, || {
                format!("Created workload service account {}", created.value.id)
            });
        }
        ServiceAccountCommand::CreateToken {
            organization,
            name,
            role,
            expires_in_seconds,
        } => {
            let issued = client
                .create_opaque_service_account(&organization, &name, &role, expires_in_seconds)
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit_issued(mode, &issued.value, "Created opaque service account");
        }
        ServiceAccountCommand::Rotate {
            organization,
            account_id,
            revision,
            expires_in_seconds,
            overlap_seconds,
        } => {
            let issued = client
                .rotate_service_account_token(
                    &organization,
                    account_id,
                    expires_in_seconds,
                    overlap_seconds,
                    &revision_etag(revision)?,
                )
                .await
                .map_err(|error| api_error(&connection, error))?;
            emit_issued(mode, &issued.value, "Rotated service-account token");
        }
        ServiceAccountCommand::Revoke {
            organization,
            account_id,
            revision,
        } => {
            client
                .revoke_service_account(&organization, account_id, &revision_etag(revision)?)
                .await
                .map_err(|error| api_error(&connection, error))?;
            let revoked = MutationResult::new("service_account.revoke", account_id);
            emit(mode, "managed.mutation", &revoked, || {
                "Service account revoked".to_owned()
            });
        }
    }
    Ok(())
}

async fn connect(
    config: &Config,
    service: Option<&str>,
    cancel: &CancellationToken,
) -> Result<ManagedControlPlane> {
    let token_cache = expand_token_cache_path(&config.auth.token_cache_path);
    ManagedRepositoryResolver::new(token_cache)
        .connect(service, cancel)
        .await
        .map_err(CrabError::from)
}

fn api_error(connection: &ManagedControlPlane, source: ManagedApiError) -> CrabError {
    ManagedRepositoryError::Api {
        canonical_url: format!("crab://{}", connection.authority),
        source,
    }
    .into()
}

fn idempotency_key() -> Result<IdempotencyKey> {
    IdempotencyKey::new(Uuid::now_v7().to_string()).map_err(CrabError::from)
}

fn revision_etag(revision: u64) -> Result<EntityTag> {
    EntityTag::new(format!("\"revision-{revision}\"")).map_err(CrabError::from)
}

fn page_cursor(value: Option<String>) -> Result<Option<crab_auth::managed::PageCursor>> {
    value
        .map(crab_auth::managed::PageCursor::new)
        .transpose()
        .map_err(CrabError::from)
}

fn repository_name(value: &str) -> Result<(&str, &str)> {
    let Some((organization, repository)) = value.split_once('/') else {
        return Err(CrabError::Configuration {
            key: "managed repository must use ORG/REPO form".to_owned(),
            origin: value.to_owned(),
        });
    };
    if organization.is_empty() || repository.is_empty() || repository.contains('/') {
        return Err(CrabError::Configuration {
            key: "managed repository must contain exactly one non-empty ORG/REPO pair".to_owned(),
            origin: value.to_owned(),
        });
    }
    Ok((organization, repository))
}

fn emit<T, F>(mode: OutputMode, schema: &'static str, value: &T, text: F)
where
    T: Serialize,
    F: FnOnce() -> String,
{
    match mode {
        OutputMode::Text => println!("{}", text()),
        OutputMode::Json => emit_json(schema, "1.0", value),
        OutputMode::Jsonl => unreachable!("managed administration commands do not accept JSONL"),
    }
}

fn emit_issued(mode: OutputMode, issued: &IssuedServiceToken, message: &str) {
    emit(mode, "managed.service_account.credential", issued, || {
        format!(
            "{message} {}\nToken: {}\nStore this token now; it will not be shown again.",
            issued.account.id,
            issued.token.expose_secret()
        )
    });
}

#[derive(Serialize)]
struct MutationResult {
    schema_version: u16,
    action: &'static str,
    resource: String,
}

impl MutationResult {
    fn new(action: &'static str, resource: impl Display) -> Self {
        Self {
            schema_version: 1,
            action,
            resource: resource.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_name_requires_exact_logical_pair() {
        assert_eq!(repository_name("acme/models").unwrap(), ("acme", "models"));
        assert!(repository_name("crab://crab.build/acme/models").is_err());
        assert!(repository_name("acme/nested/models").is_err());
    }

    #[test]
    fn revision_etag_is_strong_and_exact() {
        assert_eq!(revision_etag(42).unwrap().as_str(), "\"revision-42\"");
    }
}
