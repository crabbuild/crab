use clap::CommandFactory;

use super::*;

#[test]
fn command_line_contract_is_valid() {
    Arguments::command().debug_assert();
}

#[test]
fn peer_advertise_host_is_a_global_option() {
    let arguments = Arguments::try_parse_from([
        "crab-server",
        "--config",
        "server.toml",
        "healthcheck",
        "--peer-advertise-host",
        "10.42.3.17",
    ])
    .unwrap();

    assert_eq!(arguments.peer_advertise_host.as_deref(), Some("10.42.3.17"));

    let arguments = Arguments::try_parse_from([
        "crab-server",
        "--config",
        "server.toml",
        "--peer-advertise-host-from-ecs-metadata",
    ])
    .unwrap();
    assert!(arguments.peer_advertise_host_from_ecs_metadata);

    assert!(
        Arguments::try_parse_from([
            "crab-server",
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
fn cell_failure_zone_is_a_global_option() {
    let arguments = Arguments::try_parse_from([
        "crab-server",
        "--config",
        "server.toml",
        "serve",
        "--cell-failure-zone",
        "zone-a",
    ])
    .unwrap();

    assert_eq!(arguments.cell_failure_zone.as_deref(), Some("zone-a"));
}

#[test]
fn healthcheck_uses_the_stable_tls_name_against_the_local_listener() {
    let config = crab_server::Config {
        listen: "127.0.0.1:8788".parse().unwrap(),
        management_listen: "0.0.0.0:8789".parse().unwrap(),
        storage: crab_server::StorageConfig {
            url: "s3://bucket/root".into(),
        },
        cells: crab_server::CellsConfig {
            data_dir: "/var/lib/crab/cells".into(),
            local_disk_limit_bytes: 32 * 1024 * 1024 * 1024,
            peer_advertise: "https://10.42.3.17:8789".parse().unwrap(),
            failure_zone: None,
            peer_tls_server_name: Some("crab-server-peer".into()),
            peer_certificate: "/run/secrets/crab/peer/tls.crt".into(),
            peer_private_key: "/run/secrets/crab/peer/tls.key".into(),
            peer_ca: "/run/secrets/crab/peer/ca.crt".into(),
            durability: crab_server::CellDurabilityMode::Fleet,
        },
        import: crab_server::ImportConfig::default(),
        auth: None,
    };

    let (url, resolution) = management_target(&config, "/readyz").unwrap();

    assert_eq!(url.as_str(), "https://crab-server-peer:8789/readyz");
    assert_eq!(
        resolution,
        Some(("crab-server-peer".into(), "127.0.0.1:8789".parse().unwrap()))
    );
    let (url, resolution) = management_target(&config, "/capacity").unwrap();
    assert_eq!(url.as_str(), "https://crab-server-peer:8789/capacity");
    assert_eq!(
        resolution,
        Some(("crab-server-peer".into(), "127.0.0.1:8789".parse().unwrap()))
    );
    let (url, resolution) = management_target(&config, "/metrics").unwrap();
    assert_eq!(url.as_str(), "https://crab-server-peer:8789/metrics");
    assert_eq!(
        resolution,
        Some(("crab-server-peer".into(), "127.0.0.1:8789".parse().unwrap()))
    );
    let (url, resolution) = management_target(&config, "/internal/organizations/00000000-0000-4000-8000-000000000001/teams?actor=00000000-0000-4000-8000-000000000002").unwrap();
    assert_eq!(
        url.path(),
        "/internal/organizations/00000000-0000-4000-8000-000000000001/teams"
    );
    assert_eq!(
        url.query(),
        Some("actor=00000000-0000-4000-8000-000000000002")
    );
    assert_eq!(
        resolution,
        Some(("crab-server-peer".into(), "127.0.0.1:8789".parse().unwrap()))
    );
}

#[test]
fn cells_runtime_selects_the_native_application() {
    let args =
        Arguments::try_parse_from(["crab-server", "--config", "server.toml", "cells", "runtime"])
            .unwrap();
    assert!(matches!(
        args.command,
        Some(Command::Cells {
            command: CellsCommand::Runtime
        })
    ));
}

#[test]
fn cells_metrics_is_a_private_management_command() {
    let arguments =
        Arguments::try_parse_from(["crab-server", "--config", "server.toml", "cells", "metrics"])
            .unwrap();

    assert!(matches!(
        arguments.command,
        Some(Command::Cells {
            command: CellsCommand::Metrics
        })
    ));
}

#[test]
fn cell_capacity_requires_the_json_contract() {
    let arguments = Arguments::try_parse_from([
        "crab-server",
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
        "crab-server",
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
            "crab-server",
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
        "crab-server",
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
            "crab-server",
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
fn members_file_accepts_stdin_and_preserves_admin_identity() {
    let arguments = Arguments::try_parse_from([
        "crab-server",
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
            "crab-server",
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
        "crab-server",
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
        Err(crab_server::Error::Config(
            "authenticated repositories require at least one admin member"
        ))
    ));
    assert!(identity.members(false).unwrap().is_empty());
}
