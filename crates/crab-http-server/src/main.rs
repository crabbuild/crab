use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use crab_http_server::RepositoryMember;
use crab_http_server::catalog::CatalogStore;
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
    /// Validate catalog reads and conditional coordination writes.
    StorageProbe,
    /// Create, adopt, or list cataloged repositories.
    Repository {
        #[command(subcommand)]
        command: RepositoryCommand,
    },
}

#[derive(Subcommand)]
enum RepositoryCommand {
    /// Initialize a repository and publish it to the catalog.
    Create(CreateRepository),
    /// Publish an existing canonical Crab repository to the catalog.
    Adopt(AdoptRepository),
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
    #[arg(long, help = "TOML file containing a members array")]
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MembersFile {
    members: Vec<RepositoryMember>,
}

impl RepositoryIdentity {
    fn members(&self) -> crab_http_server::Result<Vec<RepositoryMember>> {
        let Some(path) = &self.members_file else {
            return Ok(Vec::new());
        };
        let source = std::fs::read_to_string(path)?;
        Ok(toml::from_str::<MembersFile>(&source)?.members)
    }
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
    }
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
    let catalog = CatalogStore::from_config(config)?;
    match command {
        RepositoryCommand::Create(arguments) => {
            let identity = arguments.identity;
            let members = identity.members()?;
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
            let members = identity.members()?;
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
        RepositoryCommand::List => {
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
}
