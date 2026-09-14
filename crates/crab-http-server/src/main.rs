use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use crab_http_server::catalog::CatalogStore;
use crab_http_server::{RepositoryAccess, RepositoryMember};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Serve and administer Crab repositories")]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the public application and private management listeners.
    Serve,
    /// Check the management listener's readiness endpoint.
    Healthcheck,
    /// Validate required storage reads, lists, writes, coordination, and deletes.
    StorageProbe,
    /// Create, adopt, or list cataloged repositories.
    Repository {
        #[command(subcommand)]
        command: RepositoryCommand,
    },
    /// Inspect or administer the embedded Cell runtime.
    Cells {
        #[command(subcommand)]
        command: CellsCommand,
    },
}

#[derive(Subcommand)]
enum CellsCommand {
    /// Inspect or administer compiled Cell releases.
    Release {
        #[command(subcommand)]
        command: CellReleaseCommand,
    },
}

#[derive(Subcommand)]
enum CellReleaseCommand {
    /// Print the exact canonical descriptor compiled into this binary.
    Inspect {
        #[arg(long, required = true)]
        json: bool,
    },
    /// Upload the compiled descriptor and conditionally select it for rollout.
    Prepare {
        #[arg(long)]
        expected_revision: u64,
        #[arg(long)]
        image: String,
    },
    /// Verify every cataloged Cell and publish the prepared release as current.
    Activate {
        #[arg(long)]
        expected_revision: u64,
        #[arg(long, value_enum)]
        strategy: ActivationStrategy,
    },
    /// Print the canonical durable release selection.
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
enum ActivationStrategy {
    Compatible,
}

#[derive(Subcommand)]
enum RepositoryCommand {
    /// Initialize a repository and publish it to the catalog.
    Create(CreateRepository),
    /// Publish an existing canonical Crab repository to the catalog.
    Adopt(AdoptRepository),
    /// Replace a cataloged repository's membership.
    SetMembers(SetRepositoryMembers),
    /// Print the durable repository catalog as JSON.
    List,
}

#[derive(Args)]
struct RepositoryIdentity {
    #[arg(long)]
    owner: String,
    #[arg(long)]
    name: String,
    #[arg(long, help = "Path below storage.url, normally OWNER/NAME")]
    prefix: String,
    #[arg(long, default_value = "")]
    description: String,
    #[arg(long, help = "TOML file containing a members array, or - for stdin")]
    members_file: Option<PathBuf>,
}

#[derive(Args)]
struct CreateRepository {
    #[command(flatten)]
    identity: RepositoryIdentity,
    #[arg(long, default_value = "main")]
    default_branch: String,
}

#[derive(Args)]
struct AdoptRepository {
    #[command(flatten)]
    identity: RepositoryIdentity,
}

#[derive(Args)]
struct SetRepositoryMembers {
    #[arg(long)]
    owner: String,
    #[arg(long)]
    name: String,
    #[arg(long, help = "TOML file containing a members array, or - for stdin")]
    members_file: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MembersFile {
    members: Vec<RepositoryMember>,
}

impl RepositoryIdentity {
    fn members(&self, authenticated: bool) -> crab_http_server::Result<Vec<RepositoryMember>> {
        let members = match &self.members_file {
            None => Vec::new(),
            Some(path) => members_from_path(path)?,
        };
        validate_members(members, authenticated)
    }
}

fn members_from_path(path: &Path) -> crab_http_server::Result<Vec<RepositoryMember>> {
    if path == Path::new("-") {
        return read_members(std::io::stdin().lock());
    }
    read_members(std::fs::File::open(path)?)
}

fn validate_members(
    members: Vec<RepositoryMember>,
    authenticated: bool,
) -> crab_http_server::Result<Vec<RepositoryMember>> {
    if authenticated
        && !members
            .iter()
            .any(|member| member.access == RepositoryAccess::Admin)
    {
        return Err(crab_http_server::Error::Config(
            "authenticated repositories require at least one admin member",
        ));
    }
    Ok(members)
}

fn read_members(mut reader: impl Read) -> crab_http_server::Result<Vec<RepositoryMember>> {
    let mut source = String::new();
    reader.read_to_string(&mut source)?;
    Ok(toml::from_str::<MembersFile>(&source)?.members)
}

#[tokio::main]
async fn main() -> crab_http_server::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|source| crab_http_server::Error::Logging { source })?;
    let arguments = Arguments::parse();
    let config = crab_http_server::Config::read(&arguments.config)?;
    match arguments.command.unwrap_or(Command::Serve) {
        Command::Serve => crab_http_server::serve(config).await,
        Command::Healthcheck => healthcheck(&config).await,
        Command::StorageProbe => crab_http_server::probe_storage(&config).await,
        Command::Repository { command } => repository(&config, command).await,
        Command::Cells { command } => cells(&config, command).await,
    }
}

async fn cells(
    config: &crab_http_server::Config,
    command: CellsCommand,
) -> crab_http_server::Result<()> {
    let bytes = match command {
        CellsCommand::Release {
            command: CellReleaseCommand::Inspect { json: true },
        } => crab_http_server::cell_release_descriptor()?,
        CellsCommand::Release {
            command: CellReleaseCommand::Inspect { json: false },
        } => return Err(crab_http_server::Error::Config("--json is required")),
        CellsCommand::Release {
            command:
                CellReleaseCommand::Prepare {
                    expected_revision,
                    image,
                },
        } => crab_http_server::prepare_cell_release(config, expected_revision, &image).await?,
        CellsCommand::Release {
            command:
                CellReleaseCommand::Activate {
                    expected_revision,
                    strategy: ActivationStrategy::Compatible,
                },
        } => crab_http_server::activate_cell_release(config, expected_revision).await?,
        CellsCommand::Release {
            command: CellReleaseCommand::Status,
        } => crab_http_server::cell_release_status(config).await?,
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

async fn healthcheck(config: &crab_http_server::Config) -> crab_http_server::Result<()> {
    let mut address = config.management_listen;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    let url = format!("http://{address}/readyz");
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|source| crab_http_server::Error::Healthcheck { source })?
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|source| crab_http_server::Error::Healthcheck { source })?;
    Ok(())
}

async fn repository(
    config: &crab_http_server::Config,
    command: RepositoryCommand,
) -> crab_http_server::Result<()> {
    match command {
        RepositoryCommand::Create(arguments) => {
            let identity = arguments.identity;
            let members = identity.members(config.auth.is_some())?;
            let catalog = CatalogStore::from_config(config)?;
            let record = catalog
                .create_repository(
                    identity.owner,
                    identity.name,
                    identity.prefix,
                    arguments.default_branch,
                    identity.description,
                    members,
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
        }
        RepositoryCommand::Adopt(arguments) => {
            let identity = arguments.identity;
            let members = identity.members(config.auth.is_some())?;
            let catalog = CatalogStore::from_config(config)?;
            let record = catalog
                .adopt_repository(
                    identity.owner,
                    identity.name,
                    identity.prefix,
                    identity.description,
                    members,
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
        }
        RepositoryCommand::SetMembers(arguments) => {
            let members = validate_members(
                members_from_path(&arguments.members_file)?,
                config.auth.is_some(),
            )?;
            let catalog = CatalogStore::from_config(config)?;
            let record = catalog
                .set_members(&arguments.owner, &arguments.name, members)
                .await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
        }
        RepositoryCommand::List => {
            let catalog = CatalogStore::from_config(config)?;
            let (document, _) = catalog.load().await?;
            println!("{}", serde_json::to_string_pretty(&document)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_line_contract_is_valid() {
        Arguments::command().debug_assert();
    }

    #[test]
    fn release_inspect_requires_the_json_contract() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "release",
            "inspect",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            arguments.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Inspect { json: true }
                }
            })
        ));

        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "cells",
                "release",
                "inspect",
            ])
            .is_err()
        );
    }

    #[test]
    fn release_prepare_activate_and_status_match_the_administration_contract() {
        let prepare = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "release",
            "prepare",
            "--expected-revision",
            "7",
            "--image",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ])
        .unwrap();
        assert!(matches!(
            prepare.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Prepare {
                        expected_revision: 7,
                        ..
                    }
                }
            })
        ));

        let activate = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "release",
            "activate",
            "--expected-revision",
            "8",
            "--strategy",
            "compatible",
        ])
        .unwrap();
        assert!(matches!(
            activate.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Activate {
                        expected_revision: 8,
                        strategy: ActivationStrategy::Compatible,
                    }
                }
            })
        ));

        let status = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "release",
            "status",
        ])
        .unwrap();
        assert!(matches!(
            status.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Status
                }
            })
        ));
    }

    #[test]
    fn members_file_accepts_stdin_and_preserves_admin_identity() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "repository",
            "create",
            "--owner",
            "team",
            "--name",
            "project",
            "--prefix",
            "team/project",
            "--members-file",
            "-",
        ])
        .unwrap();
        let Command::Repository {
            command: RepositoryCommand::Create(create),
        } = arguments.command.unwrap()
        else {
            panic!("repository create command was not parsed");
        };
        assert_eq!(
            create.identity.members_file.as_deref(),
            Some(Path::new("-"))
        );

        let members = read_members(
            b"members = [{ subject = 'alice-sub', name = 'Alice', access = 'admin' }]".as_slice(),
        )
        .unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].subject, "alice-sub");
        assert_eq!(members[0].access, RepositoryAccess::Admin);
    }

    #[test]
    fn set_members_command_requires_an_explicit_members_file() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "repository",
            "set-members",
            "--owner",
            "team",
            "--name",
            "project",
            "--members-file",
            "-",
        ])
        .unwrap();
        let Command::Repository {
            command: RepositoryCommand::SetMembers(update),
        } = arguments.command.unwrap()
        else {
            panic!("repository set-members command was not parsed");
        };

        assert_eq!(update.members_file, Path::new("-"));
    }

    #[test]
    fn authenticated_repository_requires_an_admin_member() {
        let identity = RepositoryIdentity {
            owner: "team".into(),
            name: "project".into(),
            prefix: "team/project".into(),
            description: String::new(),
            members_file: None,
        };

        assert!(matches!(
            identity.members(true),
            Err(crab_http_server::Error::Config(
                "authenticated repositories require at least one admin member"
            ))
        ));
        assert!(identity.members(false).unwrap().is_empty());
    }
}
