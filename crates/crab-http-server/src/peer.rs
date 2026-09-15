use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant, SystemTime},
};

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use crab_cell_runtime::{
    ApplicationIdentity, CellAuthority, CellCatalog, CellHandle, CellRuntime, CellTarget, Digest,
    Error as CellError, NodeAdvertisement, NodeCapacity, NodeDirectory, PeerAuthorizer,
    PeerCellResolver, PeerDispatcher, PeerRoundTrip, Registry, SessionId, VerifiedPeerRequest,
    VersionedNodeAdvertisement, peer_wire,
};
use crab_storage::CellStorageLayout;
use ed25519_dalek::SigningKey;
use uuid::Uuid;

use crate::{RepositoryAccess, RepositoryConfig, peer_tls::PeerTlsIdentity, server::Server};

mod client;
pub(crate) use client::PeerHttpRoundTrip;

const PROTOBUF_MEDIA_TYPE: &str = "application/x-protobuf";
const ADVERTISEMENT_LIFETIME_MS: i64 = 15_000;
const ADVERTISEMENT_EXPIRY_MARGIN_MS: i64 = 1_000;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const HEARTBEAT_RETRY: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub(crate) struct PeerReceiver {
    directory: NodeDirectory,
    registry: Arc<Registry>,
    resolver: LocalCellResolver,
    round_trip: Arc<dyn PeerRoundTrip>,
}

impl PeerReceiver {
    pub(crate) fn new(
        directory: NodeDirectory,
        registry: Arc<Registry>,
        resolver: LocalCellResolver,
        round_trip: Arc<dyn PeerRoundTrip>,
    ) -> Self {
        Self {
            directory,
            registry,
            resolver,
            round_trip,
        }
    }
}

pub(crate) struct NodePublisher {
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
    ) -> crate::Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        if !std::fs::metadata(&data_dir)?.is_dir() {
            return Err(crate::Error::Config("cells.data_dir is not a directory"));
        }
        let sessions = data_dir.join("sessions");
        std::fs::create_dir_all(&sessions)?;
        std::fs::create_dir(sessions.join(encode_session(session)))?;
        Ok(Self {
            directory,
            signing_key,
            session,
            endpoint,
            fleet,
            certificate,
            image,
            release,
            module_digests,
            data_dir,
        })
    }

    pub(crate) async fn publish_initial(&self) -> crate::Result<VersionedNodeAdvertisement> {
        let now_ms = now_ms()?;
        Ok(self
            .directory
            .create(self.advertisement(1, now_ms)?, now_ms)
            .await?)
    }

    pub(crate) fn session_dir(&self) -> PathBuf {
        self.data_dir
            .join("sessions")
            .join(encode_session(self.session))
    }

    pub(crate) async fn run(
        self,
        server: Arc<Server>,
        mut observed: VersionedNodeAdvertisement,
    ) -> crate::Result<()> {
        loop {
            tokio::select! {
                () = server.cancellation.cancelled() => return Ok(()),
                () = tokio::time::sleep(HEARTBEAT_INTERVAL) => {}
            }
            let next_progress = observed.advertisement().progress().saturating_add(1);
            loop {
                let now_ms = now_ms()?;
                let next = self.advertisement(next_progress, now_ms)?;
                match self.directory.refresh(&observed, next, now_ms).await {
                    Ok(next) => {
                        observed = next;
                        break;
                    }
                    Err(error) => {
                        let retry_deadline = observed
                            .advertisement()
                            .expires_at_ms()
                            .saturating_sub(ADVERTISEMENT_EXPIRY_MARGIN_MS);
                        if now_ms >= retry_deadline {
                            server.node_healthy.store(false, Ordering::Release);
                            server.cancellation.cancel();
                            return Err(error.into());
                        }
                        let retry_ms = retry_deadline
                            .saturating_sub(now_ms)
                            .min(HEARTBEAT_RETRY.as_millis() as i64);
                        tokio::select! {
                            () = server.cancellation.cancelled() => return Ok(()),
                            () = tokio::time::sleep(Duration::from_millis(retry_ms as u64)) => {}
                        }
                    }
                }
            }
        }
    }

    fn advertisement(&self, progress: u64, now_ms: i64) -> crate::Result<NodeAdvertisement> {
        Ok(NodeAdvertisement::sign(
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
            node_capacity(&self.data_dir)?,
        )?)
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
        if matches!(
            request.operation(),
            Some(
                peer_wire::peer_request::Operation::DeliverEffect(_)
                    | peer_wire::peer_request::Operation::ResolveEffect(_)
            )
        ) {
            let fleet = self
                .peer_receiver
                .as_ref()
                .map(|receiver| receiver.directory.fleet())
                .ok_or_else(denied)?;
            return authorize_runtime_effect(fleet, request);
        }
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
    let principal = request.principal();
    let action = match request.operation() {
        Some(peer_wire::peer_request::Operation::DeliverEffect(_)) => "cell.effect.deliver",
        Some(peer_wire::peer_request::Operation::ResolveEffect(_)) => "cell.effect.resolve",
        _ => return Err(denied()),
    };
    if principal.issuer != format!("crab-runtime:{}", encode_digest(fleet))
        || principal.subject != encode_session(request.origin_session())
        || !request.permits(action)
    {
        return Err(denied());
    }
    Ok(())
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

fn node_capacity(data_dir: &Path) -> crate::Result<NodeCapacity> {
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
        job_credits,
    })
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
    let limit = std::fs::read_to_string(limit_path).ok()?;
    if limit.trim() == "max" {
        return None;
    }
    let limit = limit.trim().parse::<u64>().ok()?;
    let usage = std::fs::read_to_string(usage_path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    limit.checked_sub(usage)
}

fn encode_session(session: SessionId) -> String {
    encode_hex(session.as_bytes())
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
            access >= RepositoryAccess::Write
                && match mutation.operation.as_ref() {
                    Some(peer_wire::mutation_request::Operation::CellCommand(command)) => {
                        required_mutation_action(command.command_id)
                            .is_some_and(|action| request.permits(action))
                    }
                    _ => false,
                }
        }
        Some(peer_wire::peer_request::Operation::Read(read)) => match read.operation {
            Some(peer_wire::read_request::Operation::Describe(true)) => {
                // Remote commands must bind the active incarnation before mutation.
                // This preflight exposes no product rows and must not grant query access.
                (access >= RepositoryAccess::Read && request.permits("repository.read"))
                    || (access >= RepositoryAccess::Write && permits_repository_mutation(request))
            }
            Some(peer_wire::read_request::Operation::CellQuery(_)) => {
                access >= RepositoryAccess::Read && request.permits("repository.read")
            }
            _ => false,
        },
        Some(peer_wire::peer_request::Operation::Resolve(_)) => {
            access >= RepositoryAccess::Write && permits_repository_mutation(request)
        }
        _ => false,
    };
    if !authorized {
        return Err(denied());
    }
    Ok(())
}

fn permits_repository_mutation(request: &VerifiedPeerRequest) -> bool {
    [
        "repository.issue.create",
        "repository.comment.create",
        "repository.issue.update",
        "repository.comment.update",
    ]
    .iter()
    .any(|action| request.permits(action))
}

const fn required_mutation_action(command_id: u32) -> Option<&'static str> {
    match command_id {
        1 => Some("repository.issue.create"),
        2 => Some("repository.comment.create"),
        3 => Some("repository.issue.update"),
        4 => Some("repository.comment.update"),
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
        ] {
            let request = verified(command, vec![action.into()]);
            assert!(
                authorize_repository(&repository(), Some("https://issuer.example"), &request)
                    .is_ok()
            );
        }
    }

    #[test]
    fn runtime_effect_requires_fleet_issuer_session_subject_and_internal_action() {
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
        )
        .unwrap();

        let published = publisher.publish_initial().await.unwrap();
        let loaded = directory
            .load(session, now_ms().unwrap())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(loaded.advertisement(), published.advertisement());
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
            )
            .is_err()
        );
    }
}
