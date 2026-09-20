use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use crab_http_server::catalog::CatalogStore;
use crab_http_server::{RepositoryAccess, RepositoryMember};
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
    #[arg(long, global = true)]
    cell_failure_host: Option<String>,
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
    /// Report whether one exact node boot session is currently live.
    Node {
        #[arg(long)]
        session: String,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Inspect or administer compiled Cell releases.
    Release {
        #[command(subcommand)]
        command: CellReleaseCommand,
    },
    /// Create or verify immutable application backup pins.
    Backup {
        #[command(subcommand)]
        command: CellBackupCommand,
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
    /// Initialize an empty application or admit this binary's selected release.
    Bootstrap {
        #[arg(long)]
        image: String,
    },
    /// Verify every cataloged Cell and publish the prepared release as current.
    Activate {
        #[arg(long)]
        expected_revision: u64,
        #[arg(long, value_enum)]
        strategy: ActivationStrategy,
        #[arg(long, required_if_eq("strategy", "compatible"))]
        minimum_eligible_nodes: Option<usize>,
        /// Delete immutable objects older than this many hours during maintenance.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        retention_grace_hours: Option<u64>,
        /// Bound immutable-object deletions in this maintenance pass.
        #[arg(
            long,
            requires = "retention_grace_hours",
            value_parser = clap::value_parser!(u64).range(1..=100_000)
        )]
        retention_max_deletes: Option<u64>,
    },
    /// Print the canonical durable release selection.
    Status,
    /// List a bounded page of pending or failed Cell migrations.
    Migrations {
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum CellBackupCommand {
    /// Pin the current release, catalog, controls, and exact LTX roots.
    Create {
        #[arg(long)]
        pin: String,
    },
    /// Reopen one pin and verify every immutable dependency.
    Verify {
        #[arg(long)]
        pin: String,
    },
    /// Restore one pin into an isolated prefix in the configured bucket.
    Restore {
        #[arg(long)]
        pin: String,
        #[arg(long)]
        destination_prefix: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ActivationStrategy {
    Compatible,
    Maintenance,
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
    let mut config = crab_http_server::Config::read(&arguments.config)?;
    if let Some(host) = arguments.peer_advertise_host.as_deref() {
        config.set_peer_advertise_host(host)?;
    } else if arguments.peer_advertise_host_from_ecs_metadata {
        config.set_peer_advertise_host(&ecs::peer_advertise_host().await?)?;
    }
    config.set_cell_failure_domain(
        arguments.cell_failure_zone.as_deref(),
        arguments.cell_failure_host.as_deref(),
    )?;
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
        CellsCommand::Capacity {
            json: true,
            live: true,
        } => live_capacity(config).await?,
        CellsCommand::Capacity {
            json: true,
            live: false,
        } => crab_http_server::cell_capacity(config)?,
        CellsCommand::Capacity { json: false, .. } => {
            return Err(crab_http_server::Error::Config("--json is required"));
        }
        CellsCommand::Metrics => management_body(config, "/metrics").await?,
        CellsCommand::Status { owner, name } => {
            crab_http_server::repository_cell_status(config, &owner, &name).await?
        }
        CellsCommand::Node {
            session,
            json: true,
        } => crab_http_server::cell_node_status(config, &session).await?,
        CellsCommand::Node { json: false, .. } => {
            return Err(crab_http_server::Error::Config("--json is required"));
        }
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
            command: CellReleaseCommand::Bootstrap { image },
        } => crab_http_server::bootstrap_cell_release(config, &image).await?,
        CellsCommand::Release {
            command:
                CellReleaseCommand::Activate {
                    expected_revision,
                    strategy: ActivationStrategy::Compatible,
                    minimum_eligible_nodes,
                    retention_grace_hours,
                    retention_max_deletes,
                },
        } => {
            if retention_grace_hours.is_some() || retention_max_deletes.is_some() {
                return Err(crab_http_server::Error::Config(
                    "compatible activation does not accept retention options",
                ));
            }
            let Some(minimum_eligible_nodes) = minimum_eligible_nodes else {
                return Err(crab_http_server::Error::Config(
                    "compatible activation requires --minimum-eligible-nodes",
                ));
            };
            crab_http_server::activate_cell_release(
                config,
                expected_revision,
                minimum_eligible_nodes,
            )
            .await?
        }
        CellsCommand::Release {
            command:
                CellReleaseCommand::Activate {
                    expected_revision,
                    strategy: ActivationStrategy::Maintenance,
                    minimum_eligible_nodes,
                    retention_grace_hours,
                    retention_max_deletes,
                },
        } => {
            if minimum_eligible_nodes.is_some() {
                return Err(crab_http_server::Error::Config(
                    "maintenance activation does not accept --minimum-eligible-nodes",
                ));
            }
            let retention_grace = retention_grace_hours
                .map(|hours| {
                    hours.checked_mul(60 * 60).map(Duration::from_secs).ok_or(
                        crab_http_server::Error::Config("Cell retention grace is too large"),
                    )
                })
                .transpose()?;
            crab_http_server::enter_cell_maintenance(
                config,
                expected_revision,
                retention_grace,
                retention_max_deletes,
            )
            .await?
        }
        CellsCommand::Release {
            command: CellReleaseCommand::Status,
        } => crab_http_server::cell_release_status(config).await?,
        CellsCommand::Release {
            command: CellReleaseCommand::Migrations { after, limit },
        } => crab_http_server::cell_release_migrations(config, after.as_deref(), limit).await?,
        CellsCommand::Backup {
            command: CellBackupCommand::Create { pin },
        } => crab_http_server::create_cell_backup(config, &pin).await?,
        CellsCommand::Backup {
            command: CellBackupCommand::Verify { pin },
        } => crab_http_server::verify_cell_backup(config, &pin).await?,
        CellsCommand::Backup {
            command:
                CellBackupCommand::Restore {
                    pin,
                    destination_prefix,
                },
        } => crab_http_server::restore_cell_backup(config, &pin, &destination_prefix).await?,
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

async fn healthcheck(config: &crab_http_server::Config) -> crab_http_server::Result<()> {
    management_get(config, "/readyz").await?;
    Ok(())
}

async fn live_capacity(config: &crab_http_server::Config) -> crab_http_server::Result<Vec<u8>> {
    management_body(config, "/capacity").await
}

async fn management_body(
    config: &crab_http_server::Config,
    path: &'static str,
) -> crab_http_server::Result<Vec<u8>> {
    Ok(management_get(config, path)
        .await?
        .bytes()
        .await
        .map_err(|source| crab_http_server::Error::Healthcheck { source })?
        .to_vec())
}

async fn management_get(
    config: &crab_http_server::Config,
    path: &'static str,
) -> crab_http_server::Result<reqwest::Response> {
    let mut identity = std::fs::read(&config.cells.peer_certificate)?;
    identity.extend_from_slice(&std::fs::read(&config.cells.peer_private_key)?);
    let identity = reqwest::Identity::from_pem(&identity)
        .map_err(|source| crab_http_server::Error::Healthcheck { source })?;
    let authorities = reqwest::Certificate::from_pem_bundle(&std::fs::read(&config.cells.peer_ca)?)
        .map_err(|source| crab_http_server::Error::Healthcheck { source })?;
    let (url, resolution) = management_target(config, path)?;
    let mut client = reqwest::Client::builder()
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
    client
        .build()
        .map_err(|source| crab_http_server::Error::Healthcheck { source })?
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|source| crab_http_server::Error::Healthcheck { source })
}

fn management_target(
    config: &crab_http_server::Config,
    path: &'static str,
) -> crab_http_server::Result<(url::Url, Option<(String, std::net::SocketAddr)>)> {
    let mut url = config.cells.peer_advertise.clone();
    let tls_name = config
        .cells
        .peer_tls_server_name
        .as_deref()
        .or_else(|| url.host_str())
        .ok_or(crab_http_server::Error::Config(
            "Cell peer TLS server name is invalid",
        ))?
        .to_owned();
    url.set_host(Some(&tls_name))
        .map_err(|_| crab_http_server::Error::Config("Cell peer TLS server name is invalid"))?;
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
            crab_http_server::initialize_repository_cell(config, record.id).await?;
            let (document, _) = catalog.load().await?;
            let ready = document
                .repositories
                .into_iter()
                .find(|candidate| candidate.id == record.id)
                .ok_or(crab_http_server::catalog::CatalogError::NotFound)?;
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
            crab_http_server::initialize_repository_cell(config, record.id).await?;
            let (document, _) = catalog.load().await?;
            let ready = document
                .repositories
                .into_iter()
                .find(|candidate| candidate.id == record.id)
                .ok_or(crab_http_server::catalog::CatalogError::NotFound)?;
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
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_line_contract_is_valid() {
        Arguments::command().debug_assert();
    }

    #[test]
    fn peer_advertise_host_is_a_global_option() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "healthcheck",
            "--peer-advertise-host",
            "10.42.3.17",
        ])
        .unwrap();

        assert_eq!(arguments.peer_advertise_host.as_deref(), Some("10.42.3.17"));

        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "--peer-advertise-host-from-ecs-metadata",
        ])
        .unwrap();
        assert!(arguments.peer_advertise_host_from_ecs_metadata);

        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "--peer-advertise-host",
                "10.42.3.17",
                "--peer-advertise-host-from-ecs-metadata",
            ])
            .is_err()
        );
    }

    #[test]
    fn cell_failure_domain_is_a_global_option() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "serve",
            "--cell-failure-zone",
            "zone-a",
            "--cell-failure-host",
            "worker-17",
        ])
        .unwrap();

        assert_eq!(arguments.cell_failure_zone.as_deref(), Some("zone-a"));
        assert_eq!(arguments.cell_failure_host.as_deref(), Some("worker-17"));
    }

    #[test]
    fn healthcheck_uses_the_stable_tls_name_against_the_local_listener() {
        let config = crab_http_server::Config {
            listen: "127.0.0.1:8788".parse().unwrap(),
            management_listen: "0.0.0.0:8789".parse().unwrap(),
            storage: crab_http_server::StorageConfig {
                url: "s3://bucket/root".into(),
            },
            cells: crab_http_server::CellsConfig {
                data_dir: "/var/lib/crab/cells".into(),
                local_disk_limit_bytes: 32 * 1024 * 1024 * 1024,
                peer_advertise: "https://10.42.3.17:8789".parse().unwrap(),
                failure_zone: None,
                failure_host: None,
                peer_tls_server_name: Some("crab-http-server-peer".into()),
                peer_certificate: "/run/secrets/crab/peer/tls.crt".into(),
                peer_private_key: "/run/secrets/crab/peer/tls.key".into(),
                peer_ca: "/run/secrets/crab/peer/ca.crt".into(),
            },
            auth: None,
        };

        let (url, resolution) = management_target(&config, "/readyz").unwrap();

        assert_eq!(url.as_str(), "https://crab-http-server-peer:8789/readyz");
        assert_eq!(
            resolution,
            Some((
                "crab-http-server-peer".into(),
                "127.0.0.1:8789".parse().unwrap()
            ))
        );
        let (url, resolution) = management_target(&config, "/capacity").unwrap();
        assert_eq!(url.as_str(), "https://crab-http-server-peer:8789/capacity");
        assert_eq!(
            resolution,
            Some((
                "crab-http-server-peer".into(),
                "127.0.0.1:8789".parse().unwrap()
            ))
        );
        let (url, resolution) = management_target(&config, "/metrics").unwrap();
        assert_eq!(url.as_str(), "https://crab-http-server-peer:8789/metrics");
        assert_eq!(
            resolution,
            Some((
                "crab-http-server-peer".into(),
                "127.0.0.1:8789".parse().unwrap()
            ))
        );
    }

    #[test]
    fn cells_metrics_is_a_private_management_command() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "metrics",
        ])
        .unwrap();

        assert!(matches!(
            arguments.command,
            Some(Command::Cells {
                command: CellsCommand::Metrics
            })
        ));
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
    fn cell_capacity_requires_the_json_contract() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "capacity",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            arguments.command,
            Some(Command::Cells {
                command: CellsCommand::Capacity {
                    json: true,
                    live: false
                }
            })
        ));

        let live = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "capacity",
            "--json",
            "--live",
        ])
        .unwrap();
        assert!(matches!(
            live.command,
            Some(Command::Cells {
                command: CellsCommand::Capacity {
                    json: true,
                    live: true
                }
            })
        ));

        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "cells",
                "capacity",
            ])
            .is_err()
        );
    }

    #[test]
    fn cell_status_requires_one_repository_identity() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "status",
            "--owner",
            "team",
            "--name",
            "repository",
        ])
        .unwrap();
        assert!(matches!(
            arguments.command,
            Some(Command::Cells {
                command: CellsCommand::Status { owner, name }
            }) if owner == "team" && name == "repository"
        ));

        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "cells",
                "status",
                "--owner",
                "team",
            ])
            .is_err()
        );
    }

    #[test]
    fn cell_node_status_requires_an_exact_session_and_json() {
        let arguments = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "node",
            "--session",
            "11111111111111111111111111111111",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            arguments.command,
            Some(Command::Cells {
                command: CellsCommand::Node {
                    session,
                    json: true
                }
            }) if session == "11111111111111111111111111111111"
        ));

        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "cells",
                "node",
                "--session",
                "11111111111111111111111111111111",
            ])
            .is_err()
        );
    }

    #[test]
    fn cell_backup_commands_require_one_pin_and_restore_prefix() {
        let pin = "11111111111111111111111111111111";
        let create = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "backup",
            "create",
            "--pin",
            pin,
        ])
        .unwrap();
        assert!(matches!(
            create.command,
            Some(Command::Cells {
                command: CellsCommand::Backup {
                    command: CellBackupCommand::Create { pin: parsed }
                }
            }) if parsed == pin
        ));

        let verify = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "backup",
            "verify",
            "--pin",
            pin,
        ])
        .unwrap();
        assert!(matches!(
            verify.command,
            Some(Command::Cells {
                command: CellsCommand::Backup {
                    command: CellBackupCommand::Verify { pin: parsed }
                }
            }) if parsed == pin
        ));

        let restore = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "backup",
            "restore",
            "--pin",
            pin,
            "--destination-prefix",
            "qualification/restored-cells",
        ])
        .unwrap();
        assert!(matches!(
            restore.command,
            Some(Command::Cells {
                command: CellsCommand::Backup {
                    command: CellBackupCommand::Restore {
                        pin: parsed,
                        destination_prefix,
                    }
                }
            }) if parsed == pin && destination_prefix == "qualification/restored-cells"
        ));

        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "cells",
                "backup",
                "create",
            ])
            .is_err()
        );
    }

    #[test]
    fn release_bootstrap_prepare_activate_and_status_match_the_administration_contract() {
        let bootstrap = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "release",
            "bootstrap",
            "--image",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ])
        .unwrap();
        assert!(matches!(
            bootstrap.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Bootstrap { .. }
                }
            })
        ));

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
            "--minimum-eligible-nodes",
            "2",
        ])
        .unwrap();
        assert!(matches!(
            activate.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Activate {
                        expected_revision: 8,
                        strategy: ActivationStrategy::Compatible,
                        minimum_eligible_nodes: Some(2),
                        retention_grace_hours: None,
                        retention_max_deletes: None,
                    }
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
                "activate",
                "--expected-revision",
                "8",
                "--strategy",
                "compatible",
            ])
            .is_err()
        );

        let maintenance = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "release",
            "activate",
            "--expected-revision",
            "8",
            "--strategy",
            "maintenance",
        ])
        .unwrap();
        assert!(matches!(
            maintenance.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Activate {
                        expected_revision: 8,
                        strategy: ActivationStrategy::Maintenance,
                        minimum_eligible_nodes: None,
                        retention_grace_hours: None,
                        retention_max_deletes: None,
                    }
                }
            })
        ));

        let retention = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "release",
            "activate",
            "--expected-revision",
            "8",
            "--strategy",
            "maintenance",
            "--retention-grace-hours",
            "168",
            "--retention-max-deletes",
            "25000",
        ])
        .unwrap();
        assert!(matches!(
            retention.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Activate {
                        expected_revision: 8,
                        strategy: ActivationStrategy::Maintenance,
                        minimum_eligible_nodes: None,
                        retention_grace_hours: Some(168),
                        retention_max_deletes: Some(25_000),
                    }
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
                "activate",
                "--expected-revision",
                "8",
                "--strategy",
                "maintenance",
                "--retention-max-deletes",
                "25000",
            ])
            .is_err()
        );
        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "cells",
                "release",
                "activate",
                "--expected-revision",
                "8",
                "--strategy",
                "maintenance",
                "--retention-grace-hours",
                "0",
            ])
            .is_err()
        );
        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "cells",
                "release",
                "activate",
                "--expected-revision",
                "8",
                "--strategy",
                "maintenance",
                "--retention-grace-hours",
                "168",
                "--retention-max-deletes",
                "100001",
            ])
            .is_err()
        );

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

        let migrations = Arguments::try_parse_from([
            "crab-http-server",
            "--config",
            "server.toml",
            "cells",
            "release",
            "migrations",
            "--after",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--limit",
            "25",
        ])
        .unwrap();
        assert!(matches!(
            migrations.command,
            Some(Command::Cells {
                command: CellsCommand::Release {
                    command: CellReleaseCommand::Migrations {
                        after: Some(after),
                        limit: 25,
                    }
                }
            }) if after == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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
    fn hard_cut_rejects_the_removed_repository_import_command() {
        assert!(
            Arguments::try_parse_from([
                "crab-http-server",
                "--config",
                "server.toml",
                "cells",
                "import-repository",
                "--owner",
                "team",
                "--name",
                "project",
                "--operation",
                "00000000-0000-0000-0000-000000000001",
            ])
            .is_err()
        );
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
