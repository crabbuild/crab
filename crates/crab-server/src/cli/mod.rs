use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use crab_server::catalog::CatalogStore;
use crab_server::{RepositoryAccess, RepositoryMember};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

mod ecs;

#[derive(Parser)]
#[command(version, about = "Serve and administer Crab repositories")]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
    #[arg(long, global = true)]
    peer_advertise_host: Option<String>,
    #[arg(
        long,
        global = true,
        conflicts_with = "peer_advertise_host",
        help = "Read this task's awsvpc address from ECS metadata"
    )]
    peer_advertise_host_from_ecs_metadata: bool,
    #[arg(long, global = true)]
    cell_failure_zone: Option<String>,
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
    /// Initialize the shared native actor directory without changing repositories.
    InitializeDirectory,
    /// Print the resource-derived Cell admission envelope.
    Capacity {
        #[arg(long, required = true)]
        json: bool,
        #[arg(long, help = "Read the running server's startup envelope")]
        live: bool,
    },
    /// Print the running server's private Prometheus sample.
    Metrics,
    /// Print the durable control state for one repository Cell.
    Status {
        #[arg(long)]
        owner: String,
        #[arg(long)]
        name: String,
    },
    /// Build or resume the authoritative issue/PR search index.
    RebuildSearch {
        #[arg(long)]
        owner: String,
        #[arg(long)]
        name: String,
    },
    /// Inspect the compiled Cellar application version.
    Runtime,
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
    fn members(&self, authenticated: bool) -> crab_server::Result<Vec<RepositoryMember>> {
        let members = match &self.members_file {
            None => Vec::new(),
            Some(path) => members_from_path(path)?,
        };
        validate_members(members, authenticated)
    }
}

fn members_from_path(path: &Path) -> crab_server::Result<Vec<RepositoryMember>> {
    if path == Path::new("-") {
        return read_members(std::io::stdin().lock());
    }
    read_members(std::fs::File::open(path)?)
}

fn validate_members(
    members: Vec<RepositoryMember>,
    authenticated: bool,
) -> crab_server::Result<Vec<RepositoryMember>> {
    if authenticated
        && !members
            .iter()
            .any(|member| member.access == RepositoryAccess::Admin)
    {
        return Err(crab_server::Error::Config(
            "authenticated repositories require at least one admin member",
        ));
    }
    Ok(members)
}

fn read_members(mut reader: impl Read) -> crab_server::Result<Vec<RepositoryMember>> {
    let mut source = String::new();
    reader.read_to_string(&mut source)?;
    Ok(toml::from_str::<MembersFile>(&source)?.members)
}

pub(super) async fn run() -> crab_server::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|source| crab_server::Error::Logging { source })?;
    let arguments = Arguments::parse();
    let mut config = crab_server::Config::read(&arguments.config)?;
    if let Some(host) = arguments.peer_advertise_host.as_deref() {
        config.set_peer_advertise_host(host)?;
    } else if arguments.peer_advertise_host_from_ecs_metadata {
        config.set_peer_advertise_host(&ecs::peer_advertise_host().await?)?;
    }
    config.set_cell_failure_zone(arguments.cell_failure_zone.as_deref())?;
    match arguments.command.unwrap_or(Command::Serve) {
        Command::Serve => crab_server::serve(config).await,
        Command::Healthcheck => healthcheck(&config).await,
        Command::StorageProbe => crab_server::probe_storage(&config).await,
        Command::Repository { command } => repository(&config, command).await,
        Command::Cells { command } => cells(&config, command).await,
    }
}

async fn cells(config: &crab_server::Config, command: CellsCommand) -> crab_server::Result<()> {
    let bytes = match command {
        CellsCommand::InitializeDirectory => {
            management_request(config, reqwest::Method::POST, "/internal/directory", None)
                .await?
                .bytes()
                .await
                .map_err(|source| crab_server::Error::Healthcheck { source })?
                .to_vec()
        }
        CellsCommand::Capacity {
            json: true,
            live: true,
        } => live_capacity(config).await?,
        CellsCommand::Capacity {
            json: true,
            live: false,
        } => crab_server::cell_capacity(config)?,
        CellsCommand::Capacity { json: false, .. } => {
            return Err(crab_server::Error::Config("--json is required"));
        }
        CellsCommand::Metrics => management_body(config, "/metrics").await?,
        CellsCommand::Status { owner, name } => {
            let path = format!("/internal/repositories/{owner}/{name}/cell");
            management_body(config, &path).await?
        }
        CellsCommand::RebuildSearch { owner, name } => {
            rebuild_search(config, &owner, &name).await?
        }
        CellsCommand::Runtime => {
            serde_json::to_vec(&serde_json::json!({"runtime": "cellar", "version": "0.1.0"}))?
        }
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

async fn healthcheck(config: &crab_server::Config) -> crab_server::Result<()> {
    management_request(config, reqwest::Method::GET, "/readyz", None).await?;
    Ok(())
}

async fn live_capacity(config: &crab_server::Config) -> crab_server::Result<Vec<u8>> {
    management_body(config, "/capacity").await
}

async fn management_body(config: &crab_server::Config, path: &str) -> crab_server::Result<Vec<u8>> {
    Ok(management_request(config, reqwest::Method::GET, path, None)
        .await?
        .bytes()
        .await
        .map_err(|source| crab_server::Error::Healthcheck { source })?
        .to_vec())
}

async fn management_request(
    config: &crab_server::Config,
    method: reqwest::Method,
    path: &str,
    body: Option<&crab_server::RebuildSearchInput>,
) -> crab_server::Result<reqwest::Response> {
    let mut identity = std::fs::read(&config.cells.peer_certificate)?;
    identity.extend_from_slice(&std::fs::read(&config.cells.peer_private_key)?);
    let identity = reqwest::Identity::from_pem(&identity)
        .map_err(|source| crab_server::Error::Healthcheck { source })?;
    let authorities = reqwest::Certificate::from_pem_bundle(&std::fs::read(&config.cells.peer_ca)?)
        .map_err(|source| crab_server::Error::Healthcheck { source })?;
    let (url, resolution) = management_target(config, path)?;
    let mut client = reqwest::Client::builder()
        // PEM identities use Rustls; additive workspace features can select
        // Native TLS by default, which rejects this identity before connecting.
        .use_rustls_tls()
        .timeout(Duration::from_secs(10))
        .https_only(true)
        .identity(identity)
        .tls_built_in_root_certs(false);
    if let Some((name, address)) = resolution {
        client = client.resolve(&name, address);
    }
    for authority in authorities {
        client = client.add_root_certificate(authority);
    }
    let mut request = client
        .build()
        .map_err(|source| crab_server::Error::Healthcheck { source })?
        .request(method, url);
    if let Some(body) = body {
        request = request.json(body);
    }
    request
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|source| crab_server::Error::Healthcheck { source })
}

async fn rebuild_search(
    config: &crab_server::Config,
    owner: &str,
    name: &str,
) -> crab_server::Result<Vec<u8>> {
    let path = format!("/internal/repositories/{owner}/{name}/discussion-index");
    let initial = management_body(config, &path).await?;
    let status: crab_server::SearchIndexStatus = serde_json::from_slice(&initial)?;
    let begin = match status {
        crab_server::SearchIndexStatus::Missing => Some(None),
        crab_server::SearchIndexStatus::Ready { generation, .. } => Some(Some(generation)),
        crab_server::SearchIndexStatus::Building { .. } => None,
    };
    let mut status = if let Some(expected) = begin {
        let input = crab_server::RebuildSearchInput::Begin {
            expected,
            generation: uuid::Uuid::now_v7(),
        };
        serde_json::from_slice(
            &management_request(config, reqwest::Method::POST, &path, Some(&input))
                .await?
                .bytes()
                .await
                .map_err(|source| crab_server::Error::Healthcheck { source })?,
        )?
    } else {
        status
    };
    loop {
        match status {
            crab_server::SearchIndexStatus::Ready { .. } => {
                return Ok(serde_json::to_vec(&status)?);
            }
            crab_server::SearchIndexStatus::Missing => {
                return Err(crab_server::Error::Config(
                    "discussion index preparation did not start",
                ));
            }
            crab_server::SearchIndexStatus::Building {
                generation,
                ref progress,
            } => {
                tracing::info!(phase = ?progress.phase, processed = progress.processed, "preparing discussion index");
                let input = crab_server::RebuildSearchInput::Step {
                    generation,
                    version: progress.version,
                };
                status = serde_json::from_slice(
                    &management_request(config, reqwest::Method::POST, &path, Some(&input))
                        .await?
                        .bytes()
                        .await
                        .map_err(|source| crab_server::Error::Healthcheck { source })?,
                )?;
            }
        }
    }
}

fn management_target(
    config: &crab_server::Config,
    path: &str,
) -> crab_server::Result<(url::Url, Option<(String, std::net::SocketAddr)>)> {
    let mut url = config.cells.peer_advertise.clone();
    let tls_name = config
        .cells
        .peer_tls_server_name
        .as_deref()
        .or_else(|| url.host_str())
        .ok_or(crab_server::Error::Config(
            "Cell peer TLS server name is invalid",
        ))?
        .to_owned();
    url.set_host(Some(&tls_name))
        .map_err(|_| crab_server::Error::Config("Cell peer TLS server name is invalid"))?;
    url.set_path(path);
    let resolution = if tls_name.parse::<std::net::IpAddr>().is_err() {
        let listen_ip = config.management_listen.ip();
        let local_ip = if listen_ip.is_unspecified() {
            match listen_ip {
                std::net::IpAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                std::net::IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            }
        } else {
            listen_ip
        };
        Some((
            tls_name,
            std::net::SocketAddr::new(local_ip, config.management_listen.port()),
        ))
    } else {
        None
    };
    Ok((url, resolution))
}

async fn repository(
    config: &crab_server::Config,
    command: RepositoryCommand,
) -> crab_server::Result<()> {
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
            crab_server::initialize_repository_cell(config, record.id).await?;
            let (document, _) = catalog.load().await?;
            let ready = document
                .repositories
                .into_iter()
                .find(|candidate| candidate.id == record.id)
                .ok_or(crab_server::catalog::CatalogError::NotFound)?;
            println!("{}", serde_json::to_string_pretty(&ready)?);
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
            crab_server::initialize_repository_cell(config, record.id).await?;
            let (document, _) = catalog.load().await?;
            let ready = document
                .repositories
                .into_iter()
                .find(|candidate| candidate.id == record.id)
                .ok_or(crab_server::catalog::CatalogError::NotFound)?;
            println!("{}", serde_json::to_string_pretty(&ready)?);
        }
        RepositoryCommand::SetMembers(arguments) => {
            let members = validate_members(
                members_from_path(&arguments.members_file)?,
                config.auth.is_some(),
            )?;
            let catalog = CatalogStore::from_config(config)?;
            let record = catalog
                .set_members(
                    &arguments.owner,
                    &arguments.name,
                    members,
                    config.auth.is_some(),
                )
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
mod tests;
