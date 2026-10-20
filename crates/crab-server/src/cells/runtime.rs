use std::{net::SocketAddr, sync::Arc, time::Duration};

use cellar::{
    bucket::Bucket,
    engine_api::{Compat, WorkerConfigOptions},
    fleet::LoadedDeployment,
    sdk::{
        ActorIdentity, ActorOptions, ActorPolicy, CompiledApplications, DeploymentGraph,
        ExternalNodeServices, Node, NodeConfig, NodeFleet, OwnershipOnEvict, PeerAuth,
        PeerProbeSigner, ReplicaStorage, ReplicaStorageOptions, ReplicationOptions,
        SignedPeerTransport,
    },
};
use tokio::net::TcpListener;

use super::{Error, RepositoryClient, Result, application};
use crate::{CellDurabilityMode, Config};

pub(crate) struct RunningCells {
    pub node: Node,
    pub client: Arc<RepositoryClient>,
}

pub(crate) fn bucket(config: &Config) -> Result<Bucket> {
    let selection =
        crate::storage_root::StorageRoot::selection(&config.storage).map_err(Error::sdk)?;
    let prefix = format!("{}/.crab/cellar", selection.repo_prefix);
    let region = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".into());
    match selection.target {
        crab_storage::StaticEnvStoreTarget::Bucket { provider, bucket } => {
            let scheme = match provider {
                crab_storage::StorageProviderKind::S3 => "s3",
                crab_storage::StorageProviderKind::Gcs => "gs",
                _ => return Err(Error::Invalid("unsupported Cellar bucket provider")),
            };
            let endpoint = (scheme == "s3")
                .then(crab_storage::s3_endpoint_from_env)
                .flatten();
            if scheme == "s3"
                && crab_storage::s3_virtual_hosted_style_from_env()
                    .is_some_and(|requested| requested == endpoint.is_some())
            {
                return Err(Error::Invalid(
                    "Cellar requires path-style S3 with a custom endpoint and virtual-hosted S3 otherwise",
                ));
            }
            Bucket::open(
                &format!("{scheme}://{bucket}/{prefix}"),
                endpoint.as_deref(),
                &region,
                None,
                Some("crab-server"),
            )
            .map_err(Error::sdk)
        }
        crab_storage::StaticEnvStoreTarget::AzureAccountContainer { account, container } => {
            // Crab URLs own the account. Cellar takes it separately; preserve
            // that boundary without changing process-wide credential variables.
            let variables = std::env::vars()
                .filter(|(name, _)| {
                    name.starts_with("AZURE_")
                        && !matches!(
                            name.as_str(),
                            "AZURE_STORAGE_ACCOUNT_NAME" | "AZURE_ACCOUNT_NAME"
                        )
                })
                .chain(std::iter::once((
                    "AZURE_STORAGE_ACCOUNT_NAME".into(),
                    account,
                )));
            Bucket::open_with_sources(
                &format!("az://{container}/{prefix}"),
                None,
                &region,
                None,
                Some("crab-server"),
                cellar::bucket::CloudSources {
                    gcs: Default::default(),
                    azure: cellar::bucket::AzureEnv::from_pairs(variables),
                },
            )
            .map_err(Error::sdk)
        }
    }
}

pub(crate) async fn start(
    config: &Config,
    public_address: SocketAddr,
    internal_listener: TcpListener,
    node_name: String,
    peer_http: reqwest::Client,
    stream_http: reqwest::Client,
    max_resident: usize,
) -> Result<RunningCells> {
    let bucket = bucket(config)?;
    start_with_bucket(
        config,
        public_address,
        internal_listener,
        node_name,
        peer_http,
        stream_http,
        max_resident,
        bucket,
    )
    .await
}

pub(crate) async fn start_with_bucket(
    config: &Config,
    public_address: SocketAddr,
    internal_listener: TcpListener,
    node_name: String,
    peer_http: reqwest::Client,
    stream_http: reqwest::Client,
    max_resident: usize,
    bucket: Bucket,
) -> Result<RunningCells> {
    let bootstrap = cellar::sdk::ExecutionDomain::new(tokio::runtime::Handle::current());
    let key = bootstrap
        .run(cellar::peer_auth::load_or_create(&bucket))
        .await;
    bootstrap.shutdown().await;
    let key = key.map_err(Error::sdk)?.map_err(Error::sdk)?;
    let peer_auth = Arc::new(PeerAuth::new(key, &node_name).map_err(Error::sdk)?);
    let signer = Arc::new(PeerProbeSigner::generate());
    let advertise =
        &config.cells.peer_advertise[url::Position::BeforeHost..url::Position::AfterPort];
    let actor = ActorOptions {
        core: ActorPolicy {
            max_resident,
            max_cached_routes: max_resident.saturating_mul(4),
            max_activations: max_resident.min(16),
            max_evictions: max_resident.min(4),
            max_releases: max_resident.min(8),
            max_outbound_websockets: 1,
            ownership_on_evict: OwnershipOnEvict::Release,
            require_node_lease: true,
            peer_protocol: cellar::peer_auth::PROTOCOL_VERSION,
            operation_deadline_ms: Some(30_000),
            owner_log_recovery_backoff_ms: 1_000,
            owner_log_recovery_attempts: 10,
            alarm_resident_ms: 1_000,
            idle_evict_ms: Some(60_000),
            pressure: Default::default(),
        },
        lease_ttl_ms: 10_000,
        placement_weight: 1,
        generation: signer.public_key_hex(),
        resume_generation: None,
    };
    let mut node_config = NodeConfig::new(
        ActorIdentity {
            node: node_name,
            region: config
                .cells
                .failure_zone
                .clone()
                .unwrap_or_else(|| "local".into()),
            advertise: advertise.to_owned(),
        },
        actor,
        config.cells.data_dir.join("cellar"),
    );
    node_config.runtime.handler_budget = Duration::from_secs(30);
    let replica_storage = ReplicaStorage::new(
        bucket.store.clone(),
        ReplicaStorageOptions {
            prefix: bucket.prefix.clone(),
            timestamp_metadata_key: if bucket.scheme() == "az" {
                cellar::sdk::ReplicaTimestampMetadataKey::Underscore
            } else {
                cellar::sdk::ReplicaTimestampMetadataKey::Litestream
            },
            ..Default::default()
        },
    )
    .map_err(Error::sdk)?;
    let log_transport = Arc::new(SignedPeerTransport::from_clients(
        peer_http.clone(),
        stream_http,
        peer_auth.clone(),
    ));
    let services = ExternalNodeServices {
        wall_clock: None,
        http_client: None,
        host_executor: tokio::runtime::Handle::current(),
        core_executor: tokio::runtime::Handle::current(),
        public_address,
        ingress_capacity: 128,
        internal_listener,
        peer_http,
        peer_auth,
        probe_signer: signer,
        fleet: Some(NodeFleet {
            lease_bucket: bucket.clone(),
            bucket,
            replica_storage,
            replication: ReplicationOptions::default(),
            log_transport,
            log: Default::default(),
            fleet_durability: config.cells.durability == CellDurabilityMode::Fleet,
        }),
        containers: None,
        metrics: None,
    };
    let applications = CompiledApplications::default()
        .register(application::SCRIPT, application::VERSION, || {
            Ok(application::compiled()?)
        })
        .map_err(Error::sdk)?;
    let (node, ingress) = Node::start_external(node_config, services, graph(), applications)
        .await
        .map_err(Error::sdk)?;
    Ok(RunningCells {
        node,
        client: Arc::new(RepositoryClient::new(ingress)),
    })
}

fn graph() -> DeploymentGraph {
    DeploymentGraph::single(LoadedDeployment {
        event_streams: Vec::new(),
        native: None,
        script_name: application::SCRIPT.into(),
        version: application::VERSION.into(),
        prefix: format!("{}/{}", application::SCRIPT, application::VERSION),
        options: WorkerConfigOptions {
            stream_bindings: Vec::new(),
            src: String::new(),
            script_name: application::SCRIPT.into(),
            do_classes: vec![application::CLASS.into()],
            bindings: vec![(application::NAMESPACE.into(), application::CLASS.into())],
            sql_bindings: Vec::new(),
            kv_bindings: Vec::new(),
            blob_bindings: Vec::new(),
            queue_bindings: Vec::new(),
            queue_consumers: Vec::new(),
            workflow_bindings: Vec::new(),
            vars: Vec::new(),
            node: String::new(),
            modules: Vec::new(),
            compat: Compat::default(),
        },
        asset_binding: None,
        loader_bindings: Vec::new(),
        assets: None,
        services: Vec::new(),
        crons: Vec::new(),
        cron_revision: None,
        containers: Vec::new(),
        fence_image: None,
        container_images: Vec::new(),
    })
}

#[cfg(test)]
pub(crate) async fn test_node(
    directory: &std::path::Path,
    public_address: SocketAddr,
) -> RunningCells {
    let internal = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = internal.local_addr().unwrap();
    let config = Config {
        listen: public_address,
        management_listen: address,
        storage: crate::StorageConfig {
            url: "s3://test/repositories".into(),
        },
        cells: crate::CellsConfig {
            data_dir: directory.join("node"),
            local_disk_limit_bytes: 32 * 1024 * 1024 * 1024,
            peer_advertise: url::Url::parse(&format!("https://{address}")).unwrap(),
            failure_zone: None,
            peer_tls_server_name: None,
            peer_certificate: directory.join("peer.crt"),
            peer_private_key: directory.join("peer.key"),
            peer_ca: directory.join("ca.crt"),
            durability: CellDurabilityMode::Object,
        },
        import: Default::default(),
        auth: None,
    };
    let bucket = Bucket::open_dev(&directory.join("objects.sqlite")).unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    start_with_bucket(
        &config,
        public_address,
        internal,
        "crab-test".into(),
        http.clone(),
        http,
        32,
        bucket,
    )
    .await
    .unwrap()
}

#[cfg(test)]
pub(crate) struct TestRuntime {
    pub client: Arc<RepositoryClient>,
    node: tokio::sync::Mutex<Option<Node>>,
    directory: Option<tempfile::TempDir>,
    closed: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl TestRuntime {
    pub(crate) async fn start(public_address: SocketAddr) -> Arc<Self> {
        let directory = tempfile::TempDir::new().unwrap();
        let started = test_node(directory.path(), public_address).await;
        Arc::new(Self {
            client: started.client,
            node: tokio::sync::Mutex::new(Some(started.node)),
            directory: Some(directory),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) async fn shutdown(&self) -> crate::Result<()> {
        let Some(node) = self.node.lock().await.take() else {
            return Ok(());
        };
        let outcome = node
            .shutdown(cellar::sdk::ShutdownMode::Preserve)
            .await
            .map_err(Error::sdk)?;
        if !matches!(outcome, cellar::sdk::NodeOutcome::Stopped(report) if report.cells_drained && report.application_work_drained && !report.drain_timed_out)
        {
            return Err(crate::Error::Config(
                "test Cellar node did not complete its drain",
            ));
        }
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
impl Drop for TestRuntime {
    fn drop(&mut self) {
        if !self.closed.load(std::sync::atomic::Ordering::Acquire)
            && let Some(directory) = self.directory.take()
        {
            // Forced closure cannot prove that begun blocking work has joined.
            // Retain its directory rather than remove live recovery evidence.
            let _retained = directory.keep();
        }
    }
}
