//! Run ExtendDB's signed DynamoDB endpoint over a leased BeyondDB Cell node.

use std::{error::Error, io, io::Read, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use beyonddb::{
    APPLICATION_ID, Beyonddb, CellAuthorizationStore, CellCredentialStore,
    CellInitialPartitionProvisioner, NodeLeasePublisher, build_http_state, build_peer_client,
    peer_router,
};
use crab_cell_app::CellApplication;
use crab_cell_host::{CellNode, CellNodeBuilder, CellNodeTaskGroup};
use crab_cell_peer_http::{LoadedPeerTls, PeerTlsIdentity};
use crab_cell_runtime::{
    SqlWorkerPool,
    identity::{Digest, NodeId, SessionId},
    ltx::{CellStorageLayout, DiskBudget, Host},
    node::{NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain},
    registry::BuildDescriptor,
};
use crab_storage::{Store, build_url_object_store};
use extenddb_auth::StoredCredential;
use extenddb_server::ServerTlsConfig;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

type ServerResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    storage_url: String,
    node_id: Uuid,
    data_dir: PathBuf,
    disk_budget_bytes: u64,
    encryption_key_file: PathBuf,
    region: String,
    peer_bind: SocketAddr,
    peer_endpoint: String,
    peer_certificate: PathBuf,
    peer_private_key: PathBuf,
    peer_ca: PathBuf,
    peer_server_name: String,
    public_bind: SocketAddr,
    public_endpoint: String,
    public_certificate: Option<PathBuf>,
    public_private_key: Option<PathBuf>,
    #[serde(default)]
    owned_accounts: Vec<String>,
    #[serde(default)]
    owned_access_keys: Vec<String>,
    #[serde(default = "default_initial_partitions")]
    initial_partitions: u16,
    #[serde(default = "default_split_threshold")]
    split_threshold_bytes: u64,
    bootstrap: Option<BootstrapConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapConfig {
    account_id: String,
    access_key_id: String,
    principal_name: String,
    policy_name: String,
    policy_file: PathBuf,
}

const fn default_initial_partitions() -> u16 {
    1
}

const fn default_split_threshold() -> u64 {
    256 * 1024 * 1024
}

#[tokio::main]
async fn main() -> ServerResult<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or_else(|| invalid("usage: beyonddb <config.json>"))?;
    let bootstrap = match (args.next(), args.next()) {
        (None, None) => false,
        (Some(mode), None) if mode == "--bootstrap" => true,
        _ => return Err(invalid("usage: beyonddb <config.json> [--bootstrap]").into()),
    };
    let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    let secret = if bootstrap {
        if config.bootstrap.is_none() {
            return Err(invalid("bootstrap settings are missing").into());
        }
        let mut secret = Zeroizing::new(String::new());
        io::stdin().read_to_string(&mut secret)?;
        let trimmed = secret.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Err(invalid("bootstrap secret on stdin is empty").into());
        }
        Some(Zeroizing::new(trimmed.to_owned()))
    } else {
        None
    };
    serve(config, secret).await
}

async fn serve(config: Config, bootstrap_secret: Option<Zeroizing<String>>) -> ServerResult<()> {
    if config.disk_budget_bytes == 0 || config.split_threshold_bytes == 0 {
        return Err(invalid("disk budget and split threshold must be positive").into());
    }
    if !["s3://", "gs://", "az://"]
        .iter()
        .any(|scheme| config.storage_url.starts_with(scheme))
    {
        return Err(invalid("Cell storage must use S3, GCS, or Azure").into());
    }
    if !config.peer_endpoint.starts_with("https://") {
        return Err(invalid("peer endpoint must use HTTPS").into());
    }
    if let Some(bootstrap) = config.bootstrap.as_ref()
        && bootstrap_secret.is_some()
        && (!config.owned_accounts.contains(&bootstrap.account_id)
            || !config.owned_access_keys.contains(&bootstrap.access_key_id))
    {
        return Err(invalid("bootstrap account and access key must be owned by this node").into());
    }
    let public_tls = match (&config.public_certificate, &config.public_private_key) {
        (Some(cert_path), Some(key_path)) => Some(ServerTlsConfig {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
        }),
        (None, None) if config.public_bind.ip().is_loopback() => None,
        (None, None) => return Err(invalid("public HTTP requires a loopback bind").into()),
        _ => return Err(invalid("public TLS requires both certificate and private key").into()),
    };
    let public_scheme = if public_tls.is_some() {
        "https://"
    } else {
        "http://"
    };
    if !config.public_endpoint.starts_with(public_scheme) {
        return Err(invalid("public endpoint scheme does not match TLS configuration").into());
    }
    let encryption_key = read_encryption_key(&config.encryption_key_file)?;
    let tls = LoadedPeerTls::load(
        &config.peer_certificate,
        &config.peer_private_key,
        &config.peer_ca,
        &config.peer_server_name,
    )?;
    let source_revision = option_env!("BEYONDDB_SOURCE_REVISION")
        .map(str::to_owned)
        .unwrap_or_else(|| {
            blake3::hash(include_bytes!("beyonddb.rs"))
                .to_hex()
                .to_string()
        });
    let application = Arc::new(Beyonddb::compile(BuildDescriptor {
        source_revision,
        cargo_lock_digest: Digest::from_bytes(
            *blake3::hash(include_bytes!("../../../../Cargo.lock")).as_bytes(),
        ),
    })?);
    let release = application.registry().release_digest();
    let image = application.descriptor_digest();
    let url_store = build_url_object_store(&config.storage_url)?;
    let layout = CellStorageLayout::new(
        Store::new(url_store.store_arc()),
        url_store.prefix().clone(),
        *APPLICATION_ID.as_bytes(),
    );
    let directory = NodeDirectory::new(layout.clone(), tls.fleet(), image, release);
    let peer_listener = TcpListener::bind(config.peer_bind).await?;
    let public_listener = TcpListener::bind(config.public_bind).await?;
    let session_uuid = Uuid::now_v7();
    let session = SessionId::from_bytes(*session_uuid.as_bytes());
    let session_dir = config.data_dir.join(session_uuid.to_string());
    let node = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(4, 64)?, 256 * 1024 * 1024)
        .with_replica_host(
            Host::default().with_local_disk_budget(DiskBudget::new(config.disk_budget_bytes)),
        )
        .with_session(session)
        .build()?;
    let cancellation = CancellationToken::new();
    let tasks = node.install_task_group(cancellation.clone(), CancellationToken::new())?;
    let node_id = NodeId::from_bytes(*config.node_id.as_bytes());
    let endpoint = config.peer_endpoint.clone();
    let signer = tls.signing_key().clone();
    let certificate = tls.certificate();
    let fleet = tls.fleet();
    let disk_budget = config.disk_budget_bytes;
    let modules = application.registry().module_digests();
    let published = NodeLeasePublisher::new(directory.clone(), move |now, expires| {
        NodeAdvertisement::sign(
            node_id,
            session,
            endpoint.clone(),
            fleet,
            certificate,
            image,
            release,
            &signer,
            1,
            now,
            expires,
            modules.clone(),
            vec![1],
            NodeFailureDomain::default(),
            NodeCapacity {
                free_memory_bytes: 256 * 1024 * 1024,
                free_disk_bytes: disk_budget,
                job_credits: 64,
                ..NodeCapacity::default()
            },
        )
    })
    .publish()
    .await?;
    node.install_node_lease_for_startup(published.guard())?;
    tasks.spawn(async move { published.run(&cancellation).await })?;
    node.start()?;

    let serving = serve_ready(
        &node,
        tasks,
        &config,
        session_dir.clone(),
        layout,
        directory,
        application,
        session,
        tls,
        peer_listener,
        public_listener,
        public_tls,
        encryption_key,
        bootstrap_secret,
    )
    .await;
    let shutdown = node.shutdown().await;
    let cleanup = if shutdown.is_ok() {
        std::fs::remove_dir_all(session_dir)
    } else {
        Ok(())
    };
    serving?;
    shutdown?;
    cleanup?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the bound listeners and node identity are one serving lifecycle"
)]
async fn serve_ready(
    node: &CellNode,
    tasks: Arc<CellNodeTaskGroup>,
    config: &Config,
    session_dir: PathBuf,
    layout: CellStorageLayout,
    directory: NodeDirectory,
    application: Arc<crab_cell_app::CompiledApplication>,
    session: SessionId,
    tls: LoadedPeerTls,
    peer_listener: TcpListener,
    public_listener: TcpListener,
    public_tls: Option<ServerTlsConfig>,
    encryption_key: [u8; 32],
    bootstrap_secret: Option<Zeroizing<String>>,
) -> ServerResult<()> {
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            node.runtime(),
            application,
            layout.clone(),
            session,
            config.peer_endpoint.clone(),
            session_dir,
        )?
        .with_initial_partition_count(config.initial_partitions)?,
    );
    for account_id in &config.owned_accounts {
        let account = provisioner
            .recover_owned_account(account_id, &directory)
            .await?;
        provisioner
            .recover_local_partitions(account_id, account.clone(), &directory)
            .await?;
        provisioner.install_account_capacity_loop(
            &tasks,
            account_id.clone(),
            account,
            config.split_threshold_bytes,
            Duration::from_secs(3),
        )?;
    }
    for key_id in &config.owned_access_keys {
        provisioner
            .recover_owned_credential(key_id, &directory)
            .await?;
    }
    let client = build_peer_client(node, layout.clone(), directory.clone(), session, &tls)?;
    if let (Some(bootstrap), Some(secret)) = (config.bootstrap.as_ref(), bootstrap_secret) {
        let policy = std::fs::read_to_string(&bootstrap.policy_file)?;
        CellCredentialStore::new(client.clone(), layout.clone(), encryption_key)
            .put_credential(
                &bootstrap.access_key_id,
                StoredCredential {
                    secret_key: secret.to_string(),
                    account_id: bootstrap.account_id.clone(),
                    principal_name: bootstrap.principal_name.clone(),
                    session_name: None,
                    is_session: false,
                    session_token: None,
                    is_active: true,
                    expires_at: None,
                },
            )
            .await?;
        CellAuthorizationStore::new(client.clone())
            .put_user_policy(
                &bootstrap.account_id,
                &bootstrap.principal_name,
                &bootstrap.policy_name,
                &policy,
            )
            .await?;
    }
    let mut state = build_http_state(
        node,
        client,
        layout.clone(),
        provisioner,
        encryption_key,
        &config.region,
        config.public_endpoint.clone(),
    )?;
    state.tls_enabled = public_tls.is_some();
    let peer_cancel = CancellationToken::new();
    let peer_shutdown = peer_cancel.clone();
    let peer_router = peer_router(node, layout, directory);
    let mut peer_server = tokio::spawn(async move {
        axum::serve(
            tls.listener(peer_listener),
            peer_router.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move { peer_shutdown.cancelled().await })
        .await
    });
    let mut public_server = tokio::spawn(extenddb_server::start_server(
        public_listener,
        state,
        None,
        public_tls,
    ));
    enum Exit {
        Public(ServerResult<()>),
        Peer(ServerResult<()>),
        Unready(ServerResult<()>),
    }
    let exit = tokio::select! {
        result = &mut public_server => Exit::Public(match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into_boxed_dyn_error()),
            Err(error) => Err(Box::new(error)),
        }),
        result = &mut peer_server => Exit::Peer(join(result)),
        result = wait_until_unready(node) => Exit::Unready(result),
    };
    peer_cancel.cancel();
    match exit {
        Exit::Public(result) => {
            peer_server.await??;
            result
        }
        Exit::Peer(result) => {
            public_server.abort();
            let _ = public_server.await;
            result
        }
        Exit::Unready(result) => {
            public_server.abort();
            let _ = public_server.await;
            peer_server.await??;
            result
        }
    }
}

async fn wait_until_unready(node: &CellNode) -> ServerResult<()> {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !node.is_ready() {
            return Err(invalid("Cell node lost its serving lease or task health").into());
        }
    }
}

fn join<T, E>(result: Result<Result<T, E>, tokio::task::JoinError>) -> ServerResult<()>
where
    E: Error + Send + Sync + 'static,
{
    result??;
    Ok(())
}

fn read_encryption_key(path: &PathBuf) -> ServerResult<[u8; 32]> {
    std::fs::read(path)?
        .try_into()
        .map_err(|_| invalid("encryption key file must contain exactly 32 bytes").into())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
