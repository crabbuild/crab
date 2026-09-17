use std::{
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, OnceLock, atomic::Ordering},
    time::{Duration, Instant, SystemTime},
};

use axum::{
    Json,
    extract::{ConnectInfo, Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use crab_cell_runtime::{
    ApplicationIdentity, CellAuthority, CellCatalog, CellHandle, CellRuntime, CellTarget, Digest,
    Error as CellError, NodeAdvertisement, NodeCapacity, NodeDirectory, NodeId, NodeLogAuthority,
    PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerRoundTrip, Registry, ReleaseState,
    ReleaseStore, SessionId, VerifiedPeerRequest, VersionedNodeAdvertisement, peer_wire,
};
use crab_storage::CellStorageLayout;
use ed25519_dalek::SigningKey;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{RepositoryAccess, RepositoryConfig, peer_tls::PeerTlsIdentity, server::Server};

mod client;
mod node_log_client;
pub(crate) use client::PeerHttpRoundTrip;
pub(crate) use node_log_client::NodeLogHttpTransport;

const PROTOBUF_MEDIA_TYPE: &str = "application/x-protobuf";
const NODE_LOG_MEDIA_TYPE: &str = "application/x-crab-node-log";
const ADVERTISEMENT_LIFETIME_MS: i64 = 10_000;
const ADVERTISEMENT_EXPIRY_MARGIN_MS: i64 = 1_000;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const HEARTBEAT_RETRY: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub(crate) struct PeerReceiver {
    node: NodeId,
    session: SessionId,
    directory: NodeDirectory,
    registry: Arc<Registry>,
    releases: ReleaseStore,
    resolver: LocalCellResolver,
    round_trip: Arc<dyn PeerRoundTrip>,
}

impl PeerReceiver {
    pub(crate) fn new(
        node: NodeId,
        session: SessionId,
        directory: NodeDirectory,
        registry: Arc<Registry>,
        releases: ReleaseStore,
        resolver: LocalCellResolver,
        round_trip: Arc<dyn PeerRoundTrip>,
    ) -> Self {
        Self {
            node,
            session,
            directory,
            registry,
            releases,
            resolver,
            round_trip,
        }
    }
}

pub(crate) struct NodePublisher {
    directory: NodeDirectory,
    signing_key: SigningKey,
    node: NodeId,
    session: SessionId,
    endpoint: String,
    fleet: Digest,
    certificate: Digest,
    image: Digest,
    release: Digest,
    module_digests: Vec<Digest>,
    data_dir: PathBuf,
    scheduler: crate::cells::SchedulerStatus,
    follower_store: Option<crab_cell_runtime::FollowerStore>,
    lease: OnceLock<crab_cell_runtime::NodeLeaseGuard>,
    observed: OnceLock<tokio::sync::Mutex<VersionedNodeAdvertisement>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalResources {
    pub(crate) memory_bytes: u64,
    pub(crate) free_disk_bytes: u64,
    pub(crate) available_file_descriptors: usize,
    pub(crate) job_credits: usize,
}

impl NodePublisher {
    #[expect(
        clippy::too_many_arguments,
        reason = "the signed node identity remains explicit at composition"
    )]
    pub(crate) fn new(
        directory: NodeDirectory,
        signing_key: SigningKey,
        session: SessionId,
        endpoint: String,
        fleet: Digest,
        certificate: Digest,
        image: Digest,
        release: Digest,
        module_digests: Vec<Digest>,
        data_dir: PathBuf,
        scheduler: crate::cells::SchedulerStatus,
    ) -> crate::Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        if !std::fs::metadata(&data_dir)?.is_dir() {
            return Err(crate::Error::Config("cells.data_dir is not a directory"));
        }
        let node = load_or_create_node_id(&data_dir)?;
        let sessions = data_dir.join("sessions");
        std::fs::create_dir_all(&sessions)?;
        std::fs::create_dir(sessions.join(encode_session(session)))?;
        Ok(Self {
            directory,
            signing_key,
            node,
            session,
            endpoint,
            fleet,
            certificate,
            image,
            release,
            module_digests,
            data_dir,
            scheduler,
            follower_store: None,
            lease: OnceLock::new(),
            observed: OnceLock::new(),
        })
    }

    pub(crate) fn with_follower_store(
        mut self,
        follower_store: crab_cell_runtime::FollowerStore,
    ) -> Self {
        self.follower_store = Some(follower_store);
        self
    }

    pub(crate) async fn publish_initial(&self) -> crate::Result<VersionedNodeAdvertisement> {
        if self.lease.get().is_some() || self.observed.get().is_some() {
            return Err(crate::Error::Config(
                "node advertisement was initialized twice",
            ));
        }
        let now_ms = now_ms()?;
        let published = self
            .directory
            .create(
                self.advertisement(self.scheduler.progress(), now_ms, false)?,
                now_ms,
            )
            .await?;
        let lease = crab_cell_runtime::NodeLeaseGuard::new(
            now_ms,
            published.advertisement().expires_at_ms(),
        )?;
        self.lease
            .set(lease)
            .map_err(|_| crate::Error::Config("node lease was initialized twice"))?;
        self.observed
            .set(tokio::sync::Mutex::new(published.clone()))
            .map_err(|_| crate::Error::Config("node advertisement was initialized twice"))?;
        Ok(published)
    }

    pub(crate) fn lease_guard(&self) -> crate::Result<crab_cell_runtime::NodeLeaseGuard> {
        self.lease
            .get()
            .cloned()
            .ok_or(crate::Error::Config("node lease is not initialized"))
    }

    pub(crate) fn session_dir(&self) -> PathBuf {
        self.data_dir
            .join("sessions")
            .join(encode_session(self.session))
    }

    pub(crate) const fn node(&self) -> NodeId {
        self.node
    }

    pub(crate) fn local_resources(&self) -> crate::Result<LocalResources> {
        local_resources(&self.data_dir)
    }

    pub(crate) async fn recruit_node_durability(
        self: &Arc<Self>,
        transport: Arc<dyn crab_cell_runtime::NodeLogTransport>,
        limits: crab_cell_runtime::ReplicaLimits,
        required_follower_bytes: u64,
        live_node_limit: usize,
    ) -> crate::Result<Option<Arc<crab_cell_runtime::NodeDurability>>> {
        let now_ms = now_ms()?;
        let mut observed = self.observed().map_err(crate::Error::from)?.lock().await;
        if observed.advertisement().log().is_none() {
            let Some(enrolled) = self
                .directory
                .try_recruit_log(
                    &observed,
                    1,
                    required_follower_bytes,
                    live_node_limit,
                    now_ms,
                )
                .await?
            else {
                return Ok(None);
            };
            *observed = enrolled;
        }
        let log = observed
            .advertisement()
            .log()
            .ok_or(CellError::Node("enrolled node session lost its log"))?;
        let gate = crab_cell_runtime::DurabilityGate::new(
            self.session,
            self.node,
            log.epoch(),
            log.members().iter().copied(),
        )?;
        let shipper =
            crab_cell_runtime::NodeLogShipper::new(gate.clone(), Arc::clone(&transport), limits)?;
        let authority: Arc<dyn NodeLogAuthority> = self.clone();
        let durability = crab_cell_runtime::NodeDurability::new(
            gate,
            shipper,
            authority,
            transport,
            self.lease_guard()?,
        );
        Ok(Some(Arc::new(durability)))
    }

    pub(crate) async fn run_shared(
        self: Arc<Self>,
        server: Arc<Server>,
        shutdown: CancellationToken,
    ) -> crate::Result<()> {
        let lease = self.lease_guard()?;
        let observed = self.observed.get().ok_or(crate::Error::Config(
            "node advertisement is not initialized",
        ))?;
        let mut draining = false;
        let heartbeat = 'heartbeat: loop {
            tokio::select! {
                () = shutdown.cancelled() => break Ok(()),
                () = server.cancellation.cancelled(), if !draining => draining = true,
                () = tokio::time::sleep(HEARTBEAT_INTERVAL) => {}
            }
            loop {
                let now_ms = now_ms()?;
                let next = self.advertisement(self.scheduler.progress(), now_ms, draining)?;
                let mut current = observed.lock().await;
                match self.directory.refresh(&current, next, now_ms).await {
                    Ok(next) => {
                        if let Err(error) =
                            lease.renew(now_ms, next.advertisement().expires_at_ms())
                        {
                            lease.fence();
                            server.node_healthy.store(false, Ordering::Release);
                            server.cancellation.cancel();
                            break 'heartbeat Err(error.into());
                        }
                        *current = next;
                        break;
                    }
                    Err(error) => {
                        let retry_deadline = current
                            .advertisement()
                            .expires_at_ms()
                            .saturating_sub(ADVERTISEMENT_EXPIRY_MARGIN_MS);
                        if now_ms >= retry_deadline {
                            lease.fence();
                            server.node_healthy.store(false, Ordering::Release);
                            server.cancellation.cancel();
                            break 'heartbeat Err(error.into());
                        }
                        let retry_ms = retry_deadline
                            .saturating_sub(now_ms)
                            .min(HEARTBEAT_RETRY.as_millis() as i64);
                        tokio::select! {
                            () = shutdown.cancelled() => break 'heartbeat Ok(()),
                            () = tokio::time::sleep(Duration::from_millis(retry_ms as u64)) => {}
                        }
                    }
                }
            }
        };
        lease.fence();
        if heartbeat.is_err() && !shutdown.is_cancelled() {
            shutdown.cancelled().await;
        }
        let withdrawal = match now_ms() {
            Ok(now_ms) => {
                let current = observed.lock().await;
                self.directory
                    .withdraw(&current, now_ms)
                    .await
                    .map_err(crate::Error::from)
            }
            Err(error) => Err(error),
        };
        match (heartbeat, withdrawal) {
            (Err(error), Err(withdrawal)) => {
                tracing::warn!(error = %withdrawal, "failed to withdraw unhealthy node advertisement");
                Err(error)
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(()), withdrawal) => withdrawal,
        }
    }

    fn observed(
        &self,
    ) -> crab_cell_runtime::Result<&tokio::sync::Mutex<VersionedNodeAdvertisement>> {
        self.observed
            .get()
            .ok_or(CellError::Node("node advertisement is not initialized"))
    }

    fn advertisement(
        &self,
        progress: u64,
        now_ms: i64,
        draining: bool,
    ) -> crate::Result<NodeAdvertisement> {
        let capacity = if draining {
            NodeCapacity {
                free_memory_bytes: 0,
                free_disk_bytes: 0,
                follower_free_bytes: 0,
                job_credits: 0,
                log_protocol: self
                    .follower_store
                    .as_ref()
                    .map_or(0, |_| crab_cell_runtime::NODE_LOG_PROTOCOL_VERSION),
            }
        } else {
            node_capacity(&self.data_dir, self.follower_store.as_ref())?
        };
        Ok(NodeAdvertisement::sign(
            self.node,
            self.session,
            self.endpoint.clone(),
            self.fleet,
            self.certificate,
            self.image,
            self.release,
            &self.signing_key,
            progress,
            now_ms,
            now_ms.saturating_add(ADVERTISEMENT_LIFETIME_MS),
            self.module_digests.clone(),
            vec![1],
            capacity,
        )?)
    }
}

impl NodeLogAuthority for NodePublisher {
    fn activate<'a>(
        &'a self,
        log_epoch: u64,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async move {
            let now_ms = now_ms().map_err(|_| CellError::Node("node-log time is unavailable"))?;
            let mut observed = self.observed()?.lock().await;
            let log = observed
                .advertisement()
                .log()
                .ok_or(CellError::Node("node session has no enrolled log"))?;
            if log.epoch() != log_epoch {
                return Err(CellError::Fenced);
            }
            if log.active() {
                return Ok(());
            }
            *observed = self.directory.activate_log(&observed, now_ms).await?;
            Ok(())
        })
    }

    fn advance_coverage<'a>(
        &'a self,
        log_epoch: u64,
        tiered_through: u64,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async move {
            let now_ms = now_ms().map_err(|_| CellError::Node("node-log time is unavailable"))?;
            let mut observed = self.observed()?.lock().await;
            let log = observed
                .advertisement()
                .log()
                .ok_or(CellError::Node("node session has no enrolled log"))?;
            if log.epoch() != log_epoch {
                return Err(CellError::Fenced);
            }
            if log.tiered_through() >= tiered_through {
                return Ok(());
            }
            *observed = self
                .directory
                .advance_log_coverage(&observed, tiered_through, now_ms)
                .await?;
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        barrier: &'a crab_cell_runtime::NodeLogRotationBarrier,
    ) -> futures_util::future::BoxFuture<'a, crab_cell_runtime::Result<()>> {
        Box::pin(async move {
            let now_ms = now_ms().map_err(|_| CellError::Node("node-log time is unavailable"))?;
            let mut observed = self.observed()?.lock().await;
            let log = observed
                .advertisement()
                .log()
                .ok_or(CellError::Node("node session has no enrolled log"))?;
            if log.epoch() != barrier.log_epoch() {
                return Err(CellError::Fenced);
            }
            *observed = self.directory.close_log(&observed, barrier, now_ms).await?;
            Ok(())
        })
    }
}

/// Resolves peer requests only when this process still owns the exact active Cell.
#[derive(Clone)]
pub(crate) struct LocalCellResolver {
    identity: ApplicationIdentity,
    catalog: CellCatalog,
    authority: CellAuthority,
    runtime: CellRuntime,
}

impl LocalCellResolver {
    pub(crate) fn new(
        layout: CellStorageLayout,
        identity: ApplicationIdentity,
        runtime: CellRuntime,
    ) -> Self {
        Self {
            identity,
            catalog: CellCatalog::new(layout.clone(), identity.tenant()),
            authority: CellAuthority::new(layout),
            runtime,
        }
    }
}

impl PeerCellResolver for LocalCellResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<CellHandle>> + Send + 'static>> {
        let resolver = self.clone();
        Box::pin(async move {
            if target.tenant() != resolver.identity.tenant()
                || target.application() != resolver.identity.application()
            {
                return Err(denied());
            }
            let proof = resolver
                .catalog
                .lookup(target.cell_id())
                .await?
                .ok_or(CellError::CellNotActive)?;
            if proof.entry().namespace() != target.namespace()
                || proof.entry().partition() != target.partition()
            {
                return Err(CellError::CatalogCollision);
            }
            let control = resolver
                .authority
                .load(target.cell_id())
                .await?
                .ok_or(CellError::CellNotActive)?;
            resolver
                .runtime
                .local_handle(proof, &control)
                .await?
                .ok_or(CellError::CellNotActive)
        })
    }
}

impl PeerAuthorizer for Server {
    fn authorize(&self, request: &VerifiedPeerRequest) -> crab_cell_runtime::Result<()> {
        let receiver = self.peer_receiver.as_ref().ok_or_else(denied)?;
        if matches!(
            request.operation(),
            Some(
                peer_wire::peer_request::Operation::DeliverEffect(_)
                    | peer_wire::peer_request::Operation::ResolveEffect(_)
            )
        ) {
            return authorize_runtime_effect(receiver.directory.fleet(), request);
        }
        if let Some(action) = runtime_cell_action(&receiver.registry, request) {
            return authorize_runtime_action(receiver.directory.fleet(), request, action);
        }
        if request.target().namespace() != crate::cells::REPOSITORY_NAMESPACE {
            return Err(denied());
        }
        let repository_id = Uuid::from_bytes(
            request
                .target()
                .partition()
                .try_into()
                .map_err(|_| denied())?,
        );
        let repository = self.repositories.by_id(repository_id).ok_or_else(denied)?;
        let issuer = self
            .auth
            .as_ref()
            .map(crate::auth::Authentication::peer_issuer);
        authorize_repository(&repository.config, issuer.as_deref(), request)
    }
}

fn authorize_runtime_effect(
    fleet: Digest,
    request: &VerifiedPeerRequest,
) -> crab_cell_runtime::Result<()> {
    let action = match request.operation() {
        Some(peer_wire::peer_request::Operation::DeliverEffect(_)) => "cell.effect.deliver",
        Some(peer_wire::peer_request::Operation::ResolveEffect(_)) => "cell.effect.resolve",
        _ => return Err(denied()),
    };
    authorize_runtime_action(fleet, request, action)
}

fn authorize_runtime_action(
    fleet: Digest,
    request: &VerifiedPeerRequest,
    action: &'static str,
) -> crab_cell_runtime::Result<()> {
    let principal = request.principal();
    if principal.issuer != format!("crab-runtime:{}", encode_digest(fleet))
        || principal.subject != encode_session(request.origin_session())
        || !request.permits(action)
    {
        return Err(denied());
    }
    Ok(())
}

fn runtime_cell_action(registry: &Registry, request: &VerifiedPeerRequest) -> Option<&'static str> {
    match request.operation() {
        Some(peer_wire::peer_request::Operation::Mutate(mutation)) => {
            match mutation.operation.as_ref() {
                Some(peer_wire::mutation_request::Operation::CellCommand(command)) => registry
                    .internal_command_action(
                        request.target().namespace(),
                        command.command_id,
                        command.codec_version,
                    ),
                _ => None,
            }
        }
        Some(peer_wire::peer_request::Operation::Read(read)) => match read.operation.as_ref() {
            Some(peer_wire::read_request::Operation::Describe(true)) => {
                runtime_principal_action(request)
            }
            Some(peer_wire::read_request::Operation::CellQuery(query)) => registry
                .internal_query_action(
                    request.target().namespace(),
                    query.query_id,
                    query.codec_version,
                ),
            _ => None,
        },
        Some(peer_wire::peer_request::Operation::Resolve(_)) => runtime_principal_action(request),
        Some(peer_wire::peer_request::Operation::Migrate(_)) => Some("cell.release.migrate"),
        _ => None,
    }
}

fn runtime_principal_action(request: &VerifiedPeerRequest) -> Option<&'static str> {
    [
        "cell.activity.source",
        "cell.effect.source",
        "cell.scheduler.tick",
    ]
    .into_iter()
    .find(|action| request.permits(action))
}

pub(crate) async fn forward(
    State(server): State<Arc<Server>>,
    ConnectInfo(identity): ConnectInfo<PeerTlsIdentity>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some(PROTOBUF_MEDIA_TYPE)
    {
        return peer_http_error(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let Some(receiver) = server.peer_receiver.as_ref() else {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    };
    let now_ms = match now_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return peer_http_error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let request = match receiver
        .directory
        .verify_peer_request(&body, identity.certificate(), identity.public_key(), now_ms)
        .await
    {
        Ok(request) => request,
        Err(_) => return peer_http_error(StatusCode::UNAUTHORIZED),
    };
    if server.authorize(&request).is_err() {
        return peer_http_error(StatusCode::UNAUTHORIZED);
    }
    if matches!(
        request.operation(),
        Some(peer_wire::peer_request::Operation::Migrate(_))
    ) {
        let allowed = match receiver.releases.load().await {
            Ok(Some(release)) => {
                release.record().state() == ReleaseState::Activating
                    && release.record().desired() == Some(receiver.registry.release_digest())
            }
            Ok(None) => false,
            Err(_) => return peer_http_error(StatusCode::INTERNAL_SERVER_ERROR),
        };
        if !allowed {
            return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    }
    if request.hop_count() < 2
        && matches!(
            receiver.resolver.resolve(request.target().clone()).await,
            Err(CellError::CellNotActive | CellError::Fenced | CellError::CellDraining)
        )
    {
        let remaining_ms = match remaining_timeout(started, request.remaining_ms()) {
            Ok(remaining_ms) => remaining_ms.saturating_sub(1),
            Err(_) => return peer_http_error(StatusCode::GATEWAY_TIMEOUT),
        };
        let forwarded = match request.forward(remaining_ms) {
            Ok(forwarded) => forwarded,
            Err(_) => return peer_http_error(StatusCode::GATEWAY_TIMEOUT),
        };
        return match receiver
            .round_trip
            .send(request.target().clone(), forwarded, remaining_ms)
            .await
        {
            Ok(body) => peer_http_reply(body),
            Err(CellError::PeerAuthorization(_)) => peer_http_error(StatusCode::UNAUTHORIZED),
            Err(CellError::PeerTransportUnknown { .. }) => peer_http_error(StatusCode::BAD_GATEWAY),
            Err(CellError::Deadline) => peer_http_error(StatusCode::GATEWAY_TIMEOUT),
            Err(_) => peer_http_error(StatusCode::SERVICE_UNAVAILABLE),
        };
    }
    let dispatcher = PeerDispatcher::new(
        Arc::clone(&receiver.registry),
        Arc::new(receiver.resolver.clone()),
        Arc::clone(&server) as Arc<dyn PeerAuthorizer>,
    );
    match dispatcher.dispatch_bytes(&request, now_ms).await {
        Ok(body) => peer_http_reply(body),
        Err(_) => peer_http_error(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub(crate) async fn append_node_log(
    State(server): State<Arc<Server>>,
    ConnectInfo(identity): ConnectInfo<PeerTlsIdentity>,
    AxumPath((leader, epoch)): AxumPath<(String, u64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some(NODE_LOG_MEDIA_TYPE)
    {
        return peer_http_error(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let (Some(receiver), Some(store), Some(_transport)) = (
        server.peer_receiver.as_ref(),
        server.follower_store.as_ref(),
        server.node_log_transport.as_ref(),
    ) else {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    };
    let leader = match decode_session(&leader) {
        Ok(leader) => leader,
        Err(()) => return peer_http_error(StatusCode::BAD_REQUEST),
    };
    let now_ms = match now_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return peer_http_error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    if !receiver_is_current(receiver, now_ms).await {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    }
    if !authenticated_session(receiver, leader, &identity, now_ms).await {
        return peer_http_error(StatusCode::UNAUTHORIZED);
    }
    if receiver
        .directory
        .authorize_log_append(leader, receiver.node, epoch, now_ms)
        .await
        .is_err()
    {
        return peer_http_error(StatusCode::FORBIDDEN);
    }
    let (covered_through, frames) = match decode_append_batch(body) {
        Ok(batch) => batch,
        Err(()) => return peer_http_error(StatusCode::BAD_REQUEST),
    };
    match store.append(leader, epoch, frames, covered_through).await {
        Ok(receipt) => follower_receipt_response(receipt),
        Err(CellError::PeerAuthorization(_)) => peer_http_error(StatusCode::UNAUTHORIZED),
        Err(CellError::Node(_)) | Err(CellError::Ltx(_)) => peer_http_error(StatusCode::CONFLICT),
        Err(_) => peer_http_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

pub(crate) async fn seal_node_log(
    State(server): State<Arc<Server>>,
    ConnectInfo(identity): ConnectInfo<PeerTlsIdentity>,
    AxumPath((leader, epoch, claimant)): AxumPath<(String, u64, String)>,
) -> Response {
    let (Some(receiver), Some(store), Some(_transport)) = (
        server.peer_receiver.as_ref(),
        server.follower_store.as_ref(),
        server.node_log_transport.as_ref(),
    ) else {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    };
    let (Ok(leader), Ok(claimant)) = (decode_session(&leader), decode_session(&claimant)) else {
        return peer_http_error(StatusCode::BAD_REQUEST);
    };
    let now_ms = match now_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return peer_http_error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    if !receiver_is_current(receiver, now_ms).await {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    }
    if !authenticated_session(receiver, claimant, &identity, now_ms).await {
        return peer_http_error(StatusCode::UNAUTHORIZED);
    }
    if receiver
        .directory
        .authorize_log_recovery(leader, claimant, receiver.node, epoch, now_ms)
        .await
        .is_err()
    {
        return peer_http_error(StatusCode::FORBIDDEN);
    }
    match store.seal(leader, epoch).await {
        Ok(receipt) => follower_receipt_response(receipt),
        Err(CellError::Node(_)) | Err(CellError::Ltx(_)) => peer_http_error(StatusCode::CONFLICT),
        Err(_) => peer_http_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

pub(crate) async fn retire_node_log(
    State(server): State<Arc<Server>>,
    ConnectInfo(identity): ConnectInfo<PeerTlsIdentity>,
    AxumPath((leader, epoch, covered_through)): AxumPath<(String, u64, u64)>,
) -> Response {
    let (Some(receiver), Some(store), Some(_transport)) = (
        server.peer_receiver.as_ref(),
        server.follower_store.as_ref(),
        server.node_log_transport.as_ref(),
    ) else {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    };
    let leader = match decode_session(&leader) {
        Ok(leader) => leader,
        Err(()) => return peer_http_error(StatusCode::BAD_REQUEST),
    };
    let now_ms = match now_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return peer_http_error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    if !receiver_is_current(receiver, now_ms).await {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    }
    if !authenticated_session(receiver, leader, &identity, now_ms).await {
        return peer_http_error(StatusCode::UNAUTHORIZED);
    }
    if receiver
        .directory
        .authorize_log_retire(leader, receiver.node, epoch, covered_through, now_ms)
        .await
        .is_err()
    {
        return peer_http_error(StatusCode::FORBIDDEN);
    }
    match store.retire(leader, epoch, covered_through).await {
        Ok(receipt) => follower_receipt_response(receipt),
        Err(CellError::Node(_)) | Err(CellError::Ltx(_)) => peer_http_error(StatusCode::CONFLICT),
        Err(_) => peer_http_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

pub(crate) async fn tail_node_log(
    State(server): State<Arc<Server>>,
    ConnectInfo(identity): ConnectInfo<PeerTlsIdentity>,
    AxumPath((leader, epoch, claimant, first)): AxumPath<(String, u64, String, u64)>,
) -> Response {
    let (Some(receiver), Some(store), Some(_transport)) = (
        server.peer_receiver.as_ref(),
        server.follower_store.as_ref(),
        server.node_log_transport.as_ref(),
    ) else {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    };
    let (Ok(leader), Ok(claimant)) = (decode_session(&leader), decode_session(&claimant)) else {
        return peer_http_error(StatusCode::BAD_REQUEST);
    };
    let now_ms = match now_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return peer_http_error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    if !receiver_is_current(receiver, now_ms).await {
        return peer_http_error(StatusCode::SERVICE_UNAVAILABLE);
    }
    if !authenticated_session(receiver, claimant, &identity, now_ms).await {
        return peer_http_error(StatusCode::UNAUTHORIZED);
    }
    if receiver
        .directory
        .authorize_log_recovery(leader, claimant, receiver.node, epoch, now_ms)
        .await
        .is_err()
    {
        return peer_http_error(StatusCode::FORBIDDEN);
    }
    match store.read_tail_page(leader, epoch, first).await {
        Ok(page) => match encode_tail_page(page) {
            Ok(body) => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, NODE_LOG_MEDIA_TYPE),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                body,
            )
                .into_response(),
            Err(()) => peer_http_error(StatusCode::INTERNAL_SERVER_ERROR),
        },
        Err(CellError::Node(_)) | Err(CellError::Ltx(_)) => peer_http_error(StatusCode::CONFLICT),
        Err(_) => peer_http_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn authenticated_session(
    receiver: &PeerReceiver,
    claimed: SessionId,
    identity: &PeerTlsIdentity,
    now_ms: i64,
) -> bool {
    let Ok(Some(enrolled)) = receiver.directory.load(claimed, now_ms).await else {
        return false;
    };
    let advertisement = enrolled.advertisement();
    advertisement.certificate() == identity.certificate()
        && advertisement
            .verifying_key()
            .is_ok_and(|key| key.to_bytes() == identity.public_key())
}

async fn receiver_is_current(receiver: &PeerReceiver, now_ms: i64) -> bool {
    let Ok(Some(current)) = receiver.directory.load(receiver.session, now_ms).await else {
        return false;
    };
    current.advertisement().node() == receiver.node
}

fn follower_receipt_response(receipt: crab_cell_runtime::FollowerReceipt) -> Response {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "base_sequence": receipt.base_sequence.to_string(),
            "durable_through": receipt.durable_through.to_string(),
        })),
    )
        .into_response()
}

fn encode_tail_page(page: crab_cell_runtime::FollowerTailPage) -> std::result::Result<Vec<u8>, ()> {
    let count = u32::try_from(page.frames.len()).map_err(|_| ())?;
    let body_len = page.frames.iter().try_fold(12_usize, |length, frame| {
        length.checked_add(8)?.checked_add(frame.len())
    });
    let mut body = Vec::with_capacity(body_len.ok_or(())?);
    body.extend_from_slice(&page.next_sequence.unwrap_or(0).to_le_bytes());
    body.extend_from_slice(&count.to_le_bytes());
    for frame in page.frames {
        body.extend_from_slice(&(frame.len() as u64).to_le_bytes());
        body.extend_from_slice(&frame);
    }
    Ok(body)
}

fn decode_append_batch(body: Bytes) -> std::result::Result<(u64, Vec<Bytes>), ()> {
    let covered = body.get(..8).ok_or(())?;
    let covered_through = u64::from_le_bytes(covered.try_into().map_err(|_| ())?);
    let mut cursor = 8_usize;
    let mut frames = Vec::new();
    while cursor < body.len() {
        if frames.len() == 64 {
            return Err(());
        }
        let length_end = cursor.checked_add(8).ok_or(())?;
        let length = u64::from_le_bytes(
            body.get(cursor..length_end)
                .ok_or(())?
                .try_into()
                .map_err(|_| ())?,
        );
        if length == 0 || length > usize::MAX as u64 {
            return Err(());
        }
        let frame_end = length_end.checked_add(length as usize).ok_or(())?;
        if frame_end > body.len() {
            return Err(());
        }
        frames.push(body.slice(length_end..frame_end));
        cursor = frame_end;
    }
    if frames.is_empty() {
        return Err(());
    }
    Ok((covered_through, frames))
}

fn decode_session(value: &str) -> std::result::Result<SessionId, ()> {
    if value.len() != 32 {
        return Err(());
    }
    let mut bytes = [0_u8; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(SessionId::from_bytes(bytes))
}

fn decode_node(value: &str) -> std::result::Result<NodeId, ()> {
    decode_session(value).map(|session| NodeId::from_bytes(*session.as_bytes()))
}

fn hex_nibble(value: u8) -> std::result::Result<u8, ()> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(()),
    }
}

fn peer_http_reply(body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, PROTOBUF_MEDIA_TYPE),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn peer_http_error(status: StatusCode) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

fn now_ms() -> crate::Result<i64> {
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| crate::Error::Config("system clock precedes the Unix epoch"))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| crate::Error::Config("system clock exceeds the Cell time range"))
}

fn remaining_timeout(started: Instant, original_ms: u32) -> crab_cell_runtime::Result<u32> {
    let elapsed_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    original_ms
        .checked_sub(elapsed_ms)
        .filter(|remaining| *remaining > 0)
        .ok_or(CellError::Deadline)
}

fn node_capacity(
    data_dir: &Path,
    follower_store: Option<&crab_cell_runtime::FollowerStore>,
) -> crate::Result<NodeCapacity> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let free_memory_bytes = effective_memory_available(system.available_memory());
    let free_disk_bytes = fs4::available_space(data_dir)?;
    let job_credits = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 16) as u32;
    Ok(NodeCapacity {
        free_memory_bytes,
        free_disk_bytes,
        follower_free_bytes: follower_store
            .map(crab_cell_runtime::FollowerStore::available_bytes)
            .unwrap_or(0)
            .min(free_disk_bytes),
        job_credits,
        log_protocol: follower_store.map_or(0, |_| crab_cell_runtime::NODE_LOG_PROTOCOL_VERSION),
    })
}

pub(crate) fn local_resources(data_dir: &Path) -> crate::Result<LocalResources> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let pid = sysinfo::get_current_pid()
        .map_err(|_| crate::Error::Config("cannot determine the server process ID"))?;
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), false);
    let open_files = system
        .process(pid)
        .and_then(sysinfo::Process::open_files)
        .ok_or(crate::Error::Config(
            "cannot determine the server's open file count",
        ))?;
    let file_limit = sysinfo::System::open_files_limit().ok_or(crate::Error::Config(
        "cannot determine the server's open file limit",
    ))?;
    // Startup budgets use the stable process limit. Reusing advertised free
    // memory would make transient boot load permanently shrink Cell admission.
    Ok(LocalResources {
        memory_bytes: effective_memory_limit(system.total_memory()),
        free_disk_bytes: fs4::available_space(data_dir)?,
        available_file_descriptors: file_limit.saturating_sub(open_files),
        job_credits: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, 16),
    })
}

fn effective_memory_limit(system_total: u64) -> u64 {
    #[cfg(target_os = "linux")]
    {
        let cgroup_limit = cgroup_memory_limit("/sys/fs/cgroup/memory.max")
            .or_else(|| cgroup_memory_limit("/sys/fs/cgroup/memory/memory.limit_in_bytes"));
        if let Some(cgroup_limit) = cgroup_limit {
            return system_total.min(cgroup_limit);
        }
    }
    system_total
}

fn effective_memory_available(system_available: u64) -> u64 {
    #[cfg(target_os = "linux")]
    {
        let cgroup_available =
            cgroup_available_memory("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory.current")
                .or_else(|| {
                    cgroup_available_memory(
                        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
                        "/sys/fs/cgroup/memory/memory.usage_in_bytes",
                    )
                });
        if let Some(cgroup_available) = cgroup_available {
            return system_available.min(cgroup_available);
        }
    }
    system_available
}

#[cfg(target_os = "linux")]
fn cgroup_available_memory(limit_path: &str, usage_path: &str) -> Option<u64> {
    let limit = cgroup_memory_limit(limit_path)?;
    let usage = std::fs::read_to_string(usage_path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    limit.checked_sub(usage)
}

#[cfg(target_os = "linux")]
fn cgroup_memory_limit(path: &str) -> Option<u64> {
    let limit = std::fs::read_to_string(path).ok()?;
    if limit.trim() == "max" {
        return None;
    }
    limit.trim().parse().ok()
}

fn encode_session(session: SessionId) -> String {
    encode_hex(session.as_bytes())
}

fn load_or_create_node_id(data_dir: &Path) -> crate::Result<NodeId> {
    let path = data_dir.join("node-id");
    match std::fs::read_to_string(&path) {
        Ok(encoded) => {
            return decode_node(&encoded)
                .map_err(|()| crate::Error::Config("cells.data_dir node-id is invalid"));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let generated = NodeId::from_bytes(Uuid::now_v7().into_bytes());
    let temporary = data_dir.join(format!(".node-id-{}.tmp", Uuid::now_v7()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(encode_hex(generated.as_bytes()).as_bytes())?;
    file.sync_all()?;
    drop(file);
    let installed = match std::fs::hard_link(&temporary, &path) {
        Ok(()) => {
            std::fs::File::open(data_dir)?.sync_all()?;
            generated
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let encoded = std::fs::read_to_string(&path)?;
            decode_node(&encoded)
                .map_err(|()| crate::Error::Config("cells.data_dir node-id is invalid"))?
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.into());
        }
    };
    std::fs::remove_file(temporary)?;
    std::fs::File::open(data_dir)?.sync_all()?;
    Ok(installed)
}

fn encode_digest(digest: Digest) -> String {
    encode_hex(digest.as_bytes())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn authorize_repository(
    repository: &RepositoryConfig,
    issuer: Option<&str>,
    request: &VerifiedPeerRequest,
) -> crab_cell_runtime::Result<()> {
    let principal = request.principal();
    let access = match issuer {
        Some(expected) if principal.issuer == expected => repository
            .members
            .iter()
            .find(|member| member.subject == principal.subject)
            .map(|member| member.access),
        None if principal.issuer == "urn:crab:local" && principal.subject == "operator" => {
            Some(RepositoryAccess::Admin)
        }
        Some(_) | None => None,
    }
    .ok_or_else(denied)?;

    let authorized = match request.operation() {
        Some(peer_wire::peer_request::Operation::Mutate(mutation)) => {
            match mutation.operation.as_ref() {
                Some(peer_wire::mutation_request::Operation::CellCommand(command)) => {
                    required_mutation_action(command.command_id).is_some_and(|action| {
                        request.permits(action) && access >= required_mutation_access(action)
                    })
                }
                _ => false,
            }
        }
        Some(peer_wire::peer_request::Operation::Read(read)) => match read.operation {
            Some(peer_wire::read_request::Operation::Describe(true)) => {
                // Remote commands must bind the active incarnation before mutation.
                // This preflight exposes no product rows and must not grant query access.
                (access >= RepositoryAccess::Read && request.permits("repository.read"))
                    || permits_repository_mutation(request, access)
            }
            Some(peer_wire::read_request::Operation::CellQuery(_)) => {
                access >= RepositoryAccess::Read && request.permits("repository.read")
            }
            _ => false,
        },
        Some(peer_wire::peer_request::Operation::Resolve(_)) => {
            permits_repository_mutation(request, access)
        }
        _ => false,
    };
    if !authorized {
        return Err(denied());
    }
    Ok(())
}

fn permits_repository_mutation(request: &VerifiedPeerRequest, access: RepositoryAccess) -> bool {
    [
        "repository.issue.create",
        "repository.comment.create",
        "repository.issue.update",
        "repository.comment.update",
        "repository.label.create",
        "repository.label.update",
        "repository.label.delete",
        "repository.status.create",
        "repository.check.create",
        "repository.check.update",
        "repository.settings.protections",
        "repository.settings.lifecycle",
        "repository.pull.create",
        "repository.pull.update",
        "repository.pull.comment",
        "repository.pull.review",
        "repository.pull.merge",
        "repository.release.create",
        "repository.release.update",
        "repository.release.asset",
    ]
    .iter()
    .any(|action| request.permits(action) && access >= required_mutation_access(action))
}

fn required_mutation_access(action: &str) -> RepositoryAccess {
    match action {
        "repository.settings.protections" | "repository.settings.lifecycle" => {
            RepositoryAccess::Admin
        }
        _ => RepositoryAccess::Write,
    }
}

const fn required_mutation_action(command_id: u32) -> Option<&'static str> {
    match command_id {
        1 => Some("repository.issue.create"),
        2 => Some("repository.comment.create"),
        3 => Some("repository.issue.update"),
        4 => Some("repository.comment.update"),
        8 => Some("repository.label.create"),
        9 => Some("repository.label.update"),
        10 => Some("repository.label.delete"),
        11 => Some("repository.status.create"),
        12 => Some("repository.check.create"),
        13 => Some("repository.check.update"),
        14 => Some("repository.settings.protections"),
        15 => Some("repository.settings.lifecycle"),
        16 => Some("repository.pull.create"),
        17 => Some("repository.pull.update"),
        18 | 19 => Some("repository.pull.comment"),
        20 | 21 => Some("repository.pull.review"),
        22 | 23 => Some("repository.pull.merge"),
        24 => Some("repository.release.create"),
        25..=27 => Some("repository.release.update"),
        28..=30 => Some("repository.release.asset"),
        _ => None,
    }
}

const fn denied() -> CellError {
    CellError::PeerAuthorization("repository principal or action is no longer authorized")
}

#[cfg(test)]
mod tests {
    use crab_cell_runtime::{
        PeerOperation, PeerPrincipal, PeerSigner, PeerVerifier, RequestId, SessionId,
    };
    use crab_storage::{CellStorageLayout, Store};
    use ed25519_dalek::SigningKey;
    use object_store::{memory::InMemory, path::Path as ObjectPath};
    use tempfile::TempDir;

    use super::*;

    const NOW_MS: i64 = 1_000_000;

    #[test]
    fn node_log_append_codec_is_bounded_and_exact() {
        let mut body = Vec::new();
        body.extend_from_slice(&9_u64.to_le_bytes());
        body.extend_from_slice(&3_u64.to_le_bytes());
        body.extend_from_slice(b"one");
        body.extend_from_slice(&3_u64.to_le_bytes());
        body.extend_from_slice(b"two");
        let (covered, frames) = decode_append_batch(Bytes::from(body)).unwrap();
        assert_eq!(covered, 9);
        assert_eq!(
            frames,
            [Bytes::from_static(b"one"), Bytes::from_static(b"two")]
        );

        let mut trailing = Vec::new();
        trailing.extend_from_slice(&0_u64.to_le_bytes());
        trailing.extend_from_slice(&4_u64.to_le_bytes());
        trailing.extend_from_slice(b"bad");
        assert!(decode_append_batch(Bytes::from(trailing)).is_err());
        assert_eq!(
            decode_session("01010101010101010101010101010101").unwrap(),
            SessionId::from_bytes([1; 16])
        );
        assert!(decode_session("0101010101010101010101010101010G").is_err());
    }

    #[test]
    fn local_resources_include_process_file_capacity() {
        let directory = TempDir::new().unwrap();
        let resources = local_resources(directory.path()).unwrap();
        assert!(
            resources.memory_bytes > 0
                && resources.free_disk_bytes > 0
                && resources.available_file_descriptors > 0
        );
    }

    fn repository() -> RepositoryConfig {
        RepositoryConfig {
            owner: "team".into(),
            name: "repository".into(),
            bucket: "bucket".into(),
            prefix: "repository".into(),
            default_branch: "main".into(),
            description: String::new(),
            members: vec![crate::RepositoryMember {
                subject: "alice".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Write,
            }],
            protected_branches: Vec::new(),
        }
    }

    fn verified(command_id: u32, actions: Vec<String>) -> VerifiedPeerRequest {
        let key = SigningKey::from_bytes(&[1; 32]);
        let signer = PeerSigner::new(
            SessionId::from_bytes([2; 16]),
            Digest::from_bytes([3; 32]),
            key,
        );
        let target = peer_wire::Target {
            tenant_id: vec![4; 16],
            application_id: vec![5; 16],
            namespace_id: crate::cells::REPOSITORY_NAMESPACE.as_bytes().to_vec(),
            partition: [6; 16].to_vec(),
        };
        let encoded = signer
            .sign(
                PeerPrincipal {
                    issuer: "https://issuer.example".into(),
                    subject: "alice".into(),
                    actions,
                },
                NOW_MS,
                NOW_MS + 60_000,
                30_000,
                PeerOperation::Mutate(peer_wire::MutationRequest {
                    target: Some(target),
                    identity: Some(peer_wire::MutationIdentity {
                        request_id: RequestId::from_bytes([7; 16]).as_bytes().to_vec(),
                        incarnation: [8; 16].to_vec(),
                        issued_at_ms: NOW_MS,
                        expires_at_ms: NOW_MS + 60_000,
                    }),
                    timeout_ms: 30_000,
                    operation: Some(peer_wire::mutation_request::Operation::CellCommand(
                        peer_wire::CellCommand {
                            command_id,
                            codec_version: 1,
                            input: Vec::new(),
                        },
                    )),
                }),
            )
            .unwrap();
        PeerVerifier::new(
            SessionId::from_bytes([2; 16]),
            Digest::from_bytes([3; 32]),
            signer.verifying_key(),
        )
        .verify(&encoded, NOW_MS)
        .unwrap()
    }

    fn verified_read(operation: peer_wire::read_request::Operation) -> VerifiedPeerRequest {
        let key = SigningKey::from_bytes(&[1; 32]);
        let signer = PeerSigner::new(
            SessionId::from_bytes([2; 16]),
            Digest::from_bytes([3; 32]),
            key,
        );
        let encoded = signer
            .sign(
                PeerPrincipal {
                    issuer: "https://issuer.example".into(),
                    subject: "alice".into(),
                    actions: vec!["repository.issue.create".into()],
                },
                NOW_MS,
                NOW_MS + 60_000,
                30_000,
                PeerOperation::Read(peer_wire::ReadRequest {
                    target: Some(peer_wire::Target {
                        tenant_id: vec![4; 16],
                        application_id: vec![5; 16],
                        namespace_id: crate::cells::REPOSITORY_NAMESPACE.as_bytes().to_vec(),
                        partition: [6; 16].to_vec(),
                    }),
                    timeout_ms: 30_000,
                    minimum: None,
                    operation: Some(operation),
                }),
            )
            .unwrap();
        PeerVerifier::new(
            SessionId::from_bytes([2; 16]),
            Digest::from_bytes([3; 32]),
            signer.verifying_key(),
        )
        .verify(&encoded, NOW_MS)
        .unwrap()
    }

    fn verified_runtime_effect(
        issuer: String,
        subject: String,
        action: &str,
    ) -> VerifiedPeerRequest {
        let session = SessionId::from_bytes([2; 16]);
        let signer = PeerSigner::new(
            session,
            Digest::from_bytes([3; 32]),
            SigningKey::from_bytes(&[1; 32]),
        );
        let source_cell = crab_cell_runtime::CellId::from_bytes([20; 32]);
        let source_incarnation = crab_cell_runtime::IncarnationId::from_bytes([21; 16]);
        let source_sequence = 4;
        let ordinal = 1;
        let encoded = signer
            .sign(
                PeerPrincipal {
                    issuer,
                    subject,
                    actions: vec![action.into()],
                },
                NOW_MS,
                NOW_MS + 60_000,
                30_000,
                PeerOperation::DeliverEffect(peer_wire::EffectRequest {
                    target: Some(peer_wire::Target {
                        tenant_id: vec![4; 16],
                        application_id: vec![5; 16],
                        namespace_id: crate::cells::REPOSITORY_NAMESPACE.as_bytes().to_vec(),
                        partition: [6; 16].to_vec(),
                    }),
                    destination_incarnation: vec![8; 16],
                    identity: Some(peer_wire::EffectIdentity {
                        effect_id: crab_cell_runtime::effect_id(
                            source_cell,
                            source_incarnation,
                            source_sequence,
                            ordinal,
                        )
                        .to_vec(),
                        source_cell: source_cell.as_bytes().to_vec(),
                        source_incarnation: source_incarnation.as_bytes().to_vec(),
                        source_sequence,
                        ordinal,
                        expires_at_ms: NOW_MS + 5 * 60_000,
                    }),
                    operation: Some(peer_wire::effect_request::Operation::CellCommand(
                        peer_wire::CellCommand {
                            command_id: 1,
                            codec_version: 1,
                            input: Vec::new(),
                        },
                    )),
                }),
            )
            .unwrap();
        PeerVerifier::new(session, Digest::from_bytes([3; 32]), signer.verifying_key())
            .verify(&encoded, NOW_MS)
            .unwrap()
    }

    #[test]
    fn current_membership_and_exact_action_are_required() {
        let request = verified(1, vec!["repository.issue.create".into()]);
        assert!(
            authorize_repository(&repository(), Some("https://issuer.example"), &request).is_ok()
        );

        let mut revoked = repository();
        revoked.members.clear();
        assert!(authorize_repository(&revoked, Some("https://issuer.example"), &request).is_err());
        assert!(
            authorize_repository(&repository(), Some("https://other.example"), &request).is_err()
        );
        let wrong_action = verified(1, vec!["repository.comment.create".into()]);
        assert!(
            authorize_repository(&repository(), Some("https://issuer.example"), &wrong_action)
                .is_err()
        );
        for (command, action) in [
            (2, "repository.comment.create"),
            (3, "repository.issue.update"),
            (4, "repository.comment.update"),
            (8, "repository.label.create"),
            (9, "repository.label.update"),
            (10, "repository.label.delete"),
            (11, "repository.status.create"),
            (12, "repository.check.create"),
            (13, "repository.check.update"),
            (16, "repository.pull.create"),
            (17, "repository.pull.update"),
            (18, "repository.pull.comment"),
            (19, "repository.pull.comment"),
            (20, "repository.pull.review"),
            (21, "repository.pull.review"),
            (22, "repository.pull.merge"),
            (23, "repository.pull.merge"),
            (24, "repository.release.create"),
            (25, "repository.release.update"),
            (26, "repository.release.update"),
            (27, "repository.release.update"),
            (28, "repository.release.asset"),
            (29, "repository.release.asset"),
            (30, "repository.release.asset"),
        ] {
            let request = verified(command, vec![action.into()]);
            assert!(
                authorize_repository(&repository(), Some("https://issuer.example"), &request)
                    .is_ok()
            );
        }
        for (command, action) in [
            (14, "repository.settings.protections"),
            (15, "repository.settings.lifecycle"),
        ] {
            let request = verified(command, vec![action.into()]);
            assert!(
                authorize_repository(&repository(), Some("https://issuer.example"), &request)
                    .is_err()
            );
            let mut administrator = repository();
            administrator.members[0].access = RepositoryAccess::Admin;
            assert!(
                authorize_repository(&administrator, Some("https://issuer.example"), &request)
                    .is_ok()
            );
        }
    }

    #[test]
    fn runtime_operations_require_fleet_issuer_session_subject_and_internal_action() {
        let fleet = Digest::from_bytes([10; 32]);
        let request = verified_runtime_effect(
            format!("crab-runtime:{}", encode_digest(fleet)),
            encode_session(SessionId::from_bytes([2; 16])),
            "cell.effect.deliver",
        );
        assert!(authorize_runtime_effect(fleet, &request).is_ok());

        let wrong_action = verified_runtime_effect(
            format!("crab-runtime:{}", encode_digest(fleet)),
            encode_session(SessionId::from_bytes([2; 16])),
            "cell.effect.resolve",
        );
        assert!(authorize_runtime_effect(fleet, &wrong_action).is_err());
        let browser = verified_runtime_effect(
            "https://issuer.example".into(),
            "alice".into(),
            "cell.effect.deliver",
        );
        assert!(authorize_runtime_effect(fleet, &browser).is_err());

        let registry = crate::cells::compiled_registry().unwrap();
        assert_eq!(
            runtime_cell_action(&registry, &verified(5, vec!["cell.scheduler.tick".into()])),
            Some("cell.scheduler.tick")
        );
        assert_eq!(
            runtime_cell_action(&registry, &verified(6, vec!["cell.effect.source".into()])),
            Some("cell.effect.source")
        );
        assert!(
            authorize_runtime_action(
                fleet,
                &verified_runtime_effect(
                    format!("crab-runtime:{}", encode_digest(fleet)),
                    encode_session(SessionId::from_bytes([2; 16])),
                    "cell.scheduler.tick",
                ),
                "cell.scheduler.tick",
            )
            .is_ok()
        );
    }

    #[test]
    fn mutation_capability_allows_description_preflight_but_not_product_queries() {
        let describe = verified_read(peer_wire::read_request::Operation::Describe(true));
        assert!(
            authorize_repository(&repository(), Some("https://issuer.example"), &describe).is_ok()
        );
        let query = verified_read(peer_wire::read_request::Operation::CellQuery(
            peer_wire::CellQuery {
                query_id: 1,
                codec_version: 1,
                input: Vec::new(),
            },
        ));
        assert!(
            authorize_repository(&repository(), Some("https://issuer.example"), &query).is_err()
        );
    }

    #[tokio::test]
    async fn node_publisher_creates_one_local_session_and_publishes_before_serving() {
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = CellStorageLayout::new(store, ObjectPath::from("root"), [9; 16]);
        let fleet = Digest::from_bytes([10; 32]);
        let image = Digest::from_bytes([11; 32]);
        let release = Digest::from_bytes([12; 32]);
        let directory = NodeDirectory::new(layout, fleet, image, release);
        let signing_key = SigningKey::from_bytes(&[13; 32]);
        let session = SessionId::from_bytes([14; 16]);
        let data_dir = TempDir::new().unwrap();
        let publisher = NodePublisher::new(
            directory.clone(),
            signing_key.clone(),
            session,
            "https://node-1.internal:8789".into(),
            fleet,
            Digest::from_bytes([15; 32]),
            image,
            release,
            vec![Digest::from_bytes([16; 32])],
            data_dir.path().into(),
            crate::cells::SchedulerStatus::new(now_ms().unwrap()).unwrap(),
        )
        .unwrap();
        let follower_store = crab_cell_runtime::FollowerStore::open(
            data_dir.path().to_owned(),
            crab_ltx::Limits::default(),
            crab_ltx::DiskBudget::new(1 << 20),
        )
        .unwrap();
        let publisher = publisher.with_follower_store(follower_store);

        let published = publisher.publish_initial().await.unwrap();
        publisher.lease_guard().unwrap().check().unwrap();
        assert_eq!(
            published.advertisement().capacity().log_protocol,
            crab_cell_runtime::NODE_LOG_PROTOCOL_VERSION
        );
        assert!(published.advertisement().capacity().follower_free_bytes > 0);
        assert_eq!(
            publisher
                .advertisement(2, now_ms().unwrap(), true)
                .unwrap()
                .capacity(),
            NodeCapacity {
                free_memory_bytes: 0,
                free_disk_bytes: 0,
                follower_free_bytes: 0,
                job_credits: 0,
                log_protocol: crab_cell_runtime::NODE_LOG_PROTOCOL_VERSION,
            }
        );
        let loaded = directory
            .load(session, now_ms().unwrap())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(loaded.advertisement(), published.advertisement());
        assert_eq!(loaded.advertisement().node(), publisher.node());
        let restarted = NodePublisher::new(
            directory.clone(),
            signing_key.clone(),
            SessionId::from_bytes([17; 16]),
            "https://node-1.internal:8789".into(),
            fleet,
            Digest::from_bytes([15; 32]),
            image,
            release,
            vec![Digest::from_bytes([16; 32])],
            data_dir.path().into(),
            crate::cells::SchedulerStatus::new(now_ms().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(restarted.node(), publisher.node());
        assert!(
            NodePublisher::new(
                directory,
                signing_key,
                session,
                "https://node-1.internal:8789".into(),
                fleet,
                Digest::from_bytes([15; 32]),
                image,
                release,
                vec![Digest::from_bytes([16; 32])],
                data_dir.path().into(),
                crate::cells::SchedulerStatus::new(now_ms().unwrap()).unwrap(),
            )
            .is_err()
        );
    }
}
