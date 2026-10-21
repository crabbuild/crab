use super::*;

const CELLS: &str = "\n[cells]\ndata_dir='/var/lib/crab/cells'\nlocal_disk_limit_bytes=34359738368\npeer_advertise='https://127.0.0.1:8789'\npeer_certificate='/run/secrets/crab/peer.crt'\npeer_private_key='/run/secrets/crab/peer.key'\npeer_ca='/run/secrets/crab/peer-ca.crt'\n";

fn local_config(storage_url: &str) -> Config {
    toml::from_str(&format!(
        "listen='127.0.0.1:8788'\nmanagement_listen='127.0.0.1:8789'\n[storage]\nurl='{storage_url}'{CELLS}"
    ))
    .unwrap()
}

fn repository() -> RepositoryConfig {
    toml::from_str("owner='team'\nname='project'\nbucket='bucket'\nprefix='project'").unwrap()
}

#[test]
fn repository_members_require_an_explicit_known_permission() {
    for member in [
        "'alice'",
        "{subject='alice'}",
        "{subject='alice',name='Alice',access='owner'}",
        "{subject='alice',name='Alice',access='read',extra=true}",
    ] {
        let source = format!(
            "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers=[{member}]"
        );
        assert!(toml::from_str::<RepositoryConfig>(&source).is_err());
    }
    let administrator: RepositoryConfig = toml::from_str(
        "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers=[{subject='alice',name='Alice',access='admin'}]",
    )
    .unwrap();
    assert_eq!(administrator.members[0].access, RepositoryAccess::Admin);
}

#[test]
fn repository_default_branch_is_short_valid_and_defaults_to_main() {
    let repository_config = repository();
    assert_eq!(repository_config.default_branch, "main");

    for branch in ["", "refs/heads/main", "release..next"] {
        let mut repository = repository();
        repository.default_branch = branch.into();
        assert!(validate_repository(&repository).is_err(), "{branch}");
    }

    let mut repository = repository();
    repository.default_branch = "trunk".into();
    assert!(validate_repository(&repository).is_ok());
}

#[test]
fn storage_root_accepts_each_cloud_provider() {
    for url in [
        "s3://bucket/repositories",
        "gs://bucket/repositories",
        "az://account/container/repositories",
    ] {
        assert!(local_config(url).validate().is_ok(), "{url}");
    }
    for url in [
        "s3://bucket",
        "file:///repositories",
        "https://example.com/repositories",
    ] {
        assert!(local_config(url).validate().is_err(), "{url}");
    }
}

#[test]
fn cell_durability_defaults_to_fleet_and_accepts_object_proof() {
    let config = local_config("s3://bucket/repositories");
    assert_eq!(config.cells.durability, CellDurabilityMode::Fleet);
    let object = CELLS.replace("peer_ca=", "durability='object'\npeer_ca=");
    let config: Config = toml::from_str(&format!(
        "listen='127.0.0.1:8788'\nmanagement_listen='127.0.0.1:8789'\n[storage]\nurl='s3://bucket/repositories'{object}"
    ))
    .unwrap();
    assert_eq!(config.cells.durability, CellDurabilityMode::Object);
}

#[test]
fn github_authentication_requires_a_secret_and_bounded_endpoints() {
    let mut config = local_config("s3://bucket/repositories");
    config.auth = Some(OidcConfig {
        provider: AuthProvider::GitHub,
        issuer: IssuerUrl::new("https://github.com".into()).unwrap(),
        client_id: "github-client".into(),
        public_url: Url::parse("http://127.0.0.1:8788").unwrap(),
        client_secret_file: Some("/run/secrets/github-client".into()),
        state_key_file: None,
        github: None,
    });
    assert!(config.validate().is_ok());

    config.auth.as_mut().unwrap().client_secret_file = None;
    assert!(config.validate().is_err());

    let mut config = local_config("s3://bucket/repositories");
    config.auth = Some(OidcConfig {
        provider: AuthProvider::GitHub,
        issuer: IssuerUrl::new("https://github.com".into()).unwrap(),
        client_id: "github-client".into(),
        public_url: Url::parse("http://127.0.0.1:8788").unwrap(),
        client_secret_file: Some("/run/secrets/github-client".into()),
        state_key_file: None,
        github: Some(GitHubConfig {
            api_url: "https://api.github.com/api".into(),
            ..GitHubConfig::default()
        }),
    });
    assert!(config.validate().is_err());
}

#[test]
fn import_allowlist_is_exact_lower_case_host_policy() {
    let mut config = local_config("s3://bucket/repositories");
    config.import.allowed_hosts = vec!["github.com".into(), "git.example.internal".into()];
    assert!(config.validate().is_ok());

    for hosts in [
        vec![],
        vec!["GitHub.com".into()],
        vec!["github.com".into(), "github.com".into()],
        vec!["https://github.com".into()],
    ] {
        config.import.allowed_hosts = hosts;
        assert!(config.validate().is_err());
    }
}

#[test]
fn management_listener_is_separate() {
    let mut config = local_config("s3://bucket/repositories");
    config.management_listen = config.listen;
    assert!(config.validate().is_err());
}

#[test]
fn cells_require_absolute_paths_and_the_management_https_endpoint() {
    let valid = local_config("s3://bucket/repositories");
    assert!(valid.validate().is_ok());

    for endpoint in [
        "http://127.0.0.1:8789",
        "https://127.0.0.1:8788",
        "https://127.0.0.1:8789/path",
        "https://user@127.0.0.1:8789",
    ] {
        let mut config = local_config("s3://bucket/repositories");
        config.cells.peer_advertise = Url::parse(endpoint).unwrap();
        assert!(config.validate().is_err(), "{endpoint}");
    }
    let mut config = local_config("s3://bucket/repositories");
    config.cells.data_dir = "relative".into();
    assert!(config.validate().is_err());

    let mut config = local_config("s3://bucket/repositories");
    config.cells.local_disk_limit_bytes = 0;
    assert!(config.validate().is_err());

    let mut config = local_config("s3://bucket/repositories");
    config.cells.peer_tls_server_name = Some("crab-server-peer".into());
    config.set_peer_advertise_host("10.42.3.17").unwrap();
    assert_eq!(
        config.cells.peer_advertise.as_str(),
        "https://10.42.3.17:8789/"
    );
    config.set_peer_advertise_host("2001:db8::17").unwrap();
    assert_eq!(
        config.cells.peer_advertise.as_str(),
        "https://[2001:db8::17]:8789/"
    );
    config.cells.peer_tls_server_name = Some("https://peer.invalid".into());
    assert!(config.validate().is_err());
}

#[test]
fn cell_failure_zone_is_an_optional_bounded_region_label() {
    let mut config = local_config("s3://bucket/repositories");
    config.set_cell_failure_zone(Some("us-west-2a")).unwrap();
    assert_eq!(config.cells.failure_zone.as_deref(), Some("us-west-2a"));

    config.cells.failure_zone = Some("invalid\nzone".into());
    assert!(config.validate().is_err());
}

#[test]
fn repository_members_require_bounded_unique_subjects_and_names() {
    for members in [
        "[{subject='',name='Alice',access='read'}]".into(),
        "[{subject=' alice',name='Alice',access='read'}]".into(),
        format!(
            "[{{subject='{}',name='Alice',access='read'}}]",
            "a".repeat(513)
        ),
        "[{subject='alice',name='',access='read'}]".into(),
        "[{subject='alice',name=' Alice',access='read'}]".into(),
        format!(
            "[{{subject='alice',name='{}',access='read'}}]",
            "a".repeat(161)
        ),
        "[{subject='alice',name='Alice',access='read'},{subject='bob',name='alice',access='write'}]".into(),
    ] {
        let mut repository = repository();
        repository.members = toml::from_str::<RepositoryConfig>(&format!(
            "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers={members}"
        ))
        .unwrap()
        .members;
        assert!(validate_repository(&repository).is_err(), "{members}");
    }
}

#[test]
fn protected_branches_are_exact_valid_branch_names() {
    for branches in [
        "[{branch=''}]",
        "[{branch='refs/heads/main'}]",
        "[{branch='release..next'}]",
        "[{branch='main'},{branch='main'}]",
        "[{branch='main',required_approvals=21}]",
        "[{branch='main',required_checks=['']}]",
        "[{branch='main',required_checks=['ci/test','CI/Test']}]",
        "[{branch='main',required_checks=['ci/test', 'ci/test']} ]",
        "[{branch='main',unexpected=true}]",
    ] {
        let source = format!(
            "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nprotected_branches={branches}"
        );
        if let Ok(repository) = toml::from_str::<RepositoryConfig>(&source) {
            assert!(validate_repository(&repository).is_err(), "{branches}");
        }
    }
    let repository: RepositoryConfig = toml::from_str(
        "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nprotected_branches=[{branch='main',required_approvals=2,required_checks=['ci/test']},{branch='release/v1'}]",
    )
    .unwrap();
    assert!(validate_repository(&repository).is_ok());
    let main = repository
        .protected_branches
        .iter()
        .find(|rule| rule.branch == "main")
        .unwrap();
    assert_eq!(main.required_approvals, 2);
    assert_eq!(main.required_checks, ["ci/test"]);
    assert!(
        !repository
            .protected_branches
            .iter()
            .any(|rule| rule.branch == "Main")
    );
}

#[test]
fn public_listeners_require_identity_and_https() {
    let base = format!(
        "listen='0.0.0.0:8788'\nmanagement_listen='0.0.0.0:8789'\n[storage]\nurl='s3://bucket/repositories'{CELLS}"
    );
    let identity = "\n[auth]\nissuer='https://identity.example/realm'\nclient_id='crab'\npublic_url='https://git.example'\nstate_key_file='/run/secrets/crab/state-key'\n";
    let config: Config = toml::from_str(&base).unwrap();
    assert!(config.validate().is_err());
    let config: Config = toml::from_str(&format!("{base}{identity}")).unwrap();
    assert!(config.validate().is_ok());
    for replacement in [
        "http://git.example",
        "http://127.0.0.1:8788",
        "https://git.example/path",
        "https://user:password@git.example",
        "https://git.example/#fragment",
    ] {
        let config: Config = toml::from_str(&format!(
            "{base}{}",
            identity.replace("https://git.example", replacement)
        ))
        .unwrap();
        assert!(config.validate().is_err(), "{replacement}");
    }
}

#[test]
fn repository_owners_cannot_shadow_global_server_namespaces() {
    for owner in ["api", "assets", "auth", "git", "-"] {
        let repository: RepositoryConfig = toml::from_str(&format!(
            "owner='{owner}'\nname='project'\nbucket='bucket'\nprefix='project'"
        ))
        .unwrap();
        assert!(validate_repository(&repository).is_err(), "{owner}");
    }
}
