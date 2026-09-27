use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use crab_cell_peer_http::{PROTOBUF_MEDIA_TYPE, PeerTargetScope, PeerTlsIdentity};
use crab_cell_runtime::cell::{
    actor::{CellHandle, CellRuntime},
    catalog::{CatalogRole, CellCatalog},
};
use crab_cell_runtime::client::LocalCellResolver;
use crab_cell_runtime::control::{ControlState, authority::CellAuthority};
use crab_cell_runtime::identity::{CellTarget, Digest, SessionId};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::node::NodeDirectory;
use crab_cell_runtime::peer::{
    PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerPrincipal, VerifiedPeerRequest, wire,
};
use crab_cell_runtime::registry::Registry;
use crab_cell_runtime::{Error, Result};

use super::{BeyonddbPeerScope, node_lease::unix_time_ms};
use crate::{DATA_MODULE, DATA_NAMESPACE, MODULE, NAMESPACE, credentials, transaction_coordinator};

pub(super) const PROVISION_ACTION: &str = "beyonddb.cell.provision";

pub(super) const ACTIVATE_ACTION: &str = "beyonddb.cell.activate";

const INVOKE_ACTION: &str = "beyonddb.cell.invoke";

#[derive(Clone)]
pub(super) struct LocalResolver {
    runtime: CellRuntime,
    layout: CellStorageLayout,
    registry: Arc<Registry>,
    provisioner: Option<Arc<crate::CellInitialPartitionProvisioner>>,
    placement: Option<Arc<super::placement::RangePlacement>>,
    bootstrap: Option<NodeDirectory>,
}

impl LocalResolver {
    pub(super) fn serving(
        peers: &super::BeyonddbPeers,
        provisioner: Arc<crate::CellInitialPartitionProvisioner>,
    ) -> Self {
        Self {
            runtime: peers.runtime.clone(),
            layout: peers.layout.clone(),
            registry: peers.registry.clone(),
            provisioner: Some(provisioner),
            placement: None,
            bootstrap: None,
        }
    }

    pub(super) fn with_placement(
        mut self,
        placement: Arc<super::placement::RangePlacement>,
    ) -> Self {
        self.placement = Some(placement);
        self
    }
}

impl LocalCellResolver for LocalResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<Option<CellHandle>>> + Send + 'static>> {
        let resolver = self.clone();
        Box::pin(async move {
            BeyonddbPeerScope.check_target(&target)?;
            let proof = CellCatalog::new(resolver.layout.clone(), target.tenant())
                .lookup(target.cell_id())
                .await?
                .ok_or(Error::CellNotActive)?;
            let module = match target.namespace() {
                NAMESPACE => MODULE,
                DATA_NAMESPACE => DATA_MODULE,
                namespace if namespace == crate::directory::NAMESPACE => crate::directory::MODULE,
                namespace if namespace == crate::global_index::NAMESPACE => {
                    crate::global_index::MODULE
                }
                namespace if namespace == credentials::NAMESPACE => credentials::MODULE,
                namespace if namespace == transaction_coordinator::NAMESPACE => {
                    transaction_coordinator::MODULE
                }
                _ => return Err(Error::CatalogCollision),
            };
            let code = resolver
                .registry
                .module_code(module)
                .ok_or(Error::Registry("BeyondDB Cell module is not compiled"))?;
            if proof.entry().namespace() != target.namespace()
                || proof.entry().partition() != target.partition()
                || proof.entry().role() != CatalogRole::Sql
                || proof.entry().initial_code() != code
                || proof.entry().initial_schema() != 1
            {
                return Err(Error::CatalogCollision);
            }
            let authority = CellAuthority::new(resolver.layout.clone());
            for attempt in 0..2 {
                let control = authority.load(target.cell_id()).await?;
                if let Some(control) = &control
                    && let Some(local) = resolver
                        .runtime
                        .local_handle(proof.clone(), control)
                        .await?
                {
                    return Ok(Some(local));
                }
                let Some(provisioner) = &resolver.provisioner else {
                    return Ok(None);
                };
                let owner = control
                    .as_ref()
                    .and_then(|control| control.value().owner.as_ref())
                    .map(|owner| owner.session);
                let expired = if let Some(placement) = &resolver.placement
                    && super::placement::is_placeable_target(&target)
                    && let Some(control) = &control
                    && control.value().root.is_some()
                    && matches!(
                        control.value().state,
                        ControlState::Serving | ControlState::Recovering
                    )
                    && let Some(owner) = owner
                    && owner != placement.session
                {
                    !placement.directory.is_live(owner, unix_time_ms()?).await?
                } else {
                    false
                };
                let needs_placement = expired
                    || control.as_ref().is_some_and(|control| {
                        control.value().root.is_some()
                            && match control.value().state {
                                ControlState::Idle => owner.is_none(),
                                ControlState::Recovering => owner.is_some(),
                                _ => false,
                            }
                    });
                if needs_placement || resolver.bootstrap.is_some() {
                    provisioner
                        .reclaim_placement_capacity(&target)
                        .await
                        .map_err(|source| Error::PeerTransport {
                            context: "BeyondDB placement residency",
                            source: Box::new(source),
                        })?;
                }
                if let Some(nodes) = &resolver.bootstrap {
                    return provisioner
                        .admit_range(&target, nodes)
                        .await
                        .inspect_err(|error| {
                            tracing::warn!(cell = ?target.cell_id(), ?error, "peer bootstrap failed");
                        })
                        .map(Some);
                }
                let control = control.ok_or(Error::CellNotActive)?;
                let resolved = async {
                    if needs_placement
                        && super::placement::is_placeable_target(&target)
                        && let Some(placement) = &resolver.placement
                    {
                        // Expired owners need a new placement and fenced takeover,
                        // even when no background transaction or index work exists.
                        // Admission rechecks the session and exact published root.
                        let recovering = if expired { None } else { owner };
                        let action = if expired {
                            PROVISION_ACTION
                        } else {
                            ACTIVATE_ACTION
                        };
                        if !placement.select_local(&target, recovering, action).await? {
                            return Ok(None);
                        }
                        if expired {
                            return provisioner
                                .admit_range(&target, &placement.directory)
                                .await
                                .map(Some);
                        }
                    }
                    provisioner
                        .restore_idle(&target, proof.clone(), control.clone())
                        .await
                }
                .await;
                if attempt == 0
                    && matches!(
                        &resolved,
                        Err(Error::CellNotActive
                            | Error::Storage(crab_storage::StorageError::StateConflict { .. }))
                    )
                {
                    // Placement or local restoration can lose the ownership CAS.
                    // Refresh only a proven new epoch, before dispatch; stable
                    // refusals and failures with an uncertain outcome propagate.
                    let current = authority.load(target.cell_id()).await?;
                    if current.is_some_and(|current| current.value().epoch != control.value().epoch)
                    {
                        continue;
                    }
                }
                return resolved;
            }
            Err(Error::CellNotActive)
        })
    }
}

impl PeerCellResolver for LocalResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellHandle>> + Send + 'static>> {
        let resolved = LocalCellResolver::resolve(self, target);
        Box::pin(async move { resolved.await?.ok_or(Error::CellNotActive) })
    }
}

struct BeyondPeerAuthorizer {
    fleet: Digest,
    action: &'static str,
}

impl PeerAuthorizer for BeyondPeerAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> Result<()> {
        BeyonddbPeerScope.check_target(request.target())?;
        let mut expected = peer_principal(self.fleet, request.origin_session());
        expected.actions = vec![self.action.into()];
        if request.principal() != &expected {
            return Err(Error::PeerAuthorization(
                "BeyondDB peer principal is invalid",
            ));
        }
        if [ACTIVATE_ACTION, PROVISION_ACTION].contains(&self.action)
            && (!super::placement::is_placeable_target(request.target())
                || !matches!(request.operation(), Some(wire::peer_request::Operation::Read(read))
                    if read.minimum.is_none()
                        && matches!(read.operation, Some(wire::read_request::Operation::Describe(true)))))
        {
            return Err(Error::PeerAuthorization(
                "activation requires a data, index or directory Cell description",
            ));
        }
        Ok(())
    }
}

pub(super) fn peer_principal(fleet: Digest, session: SessionId) -> PeerPrincipal {
    PeerPrincipal {
        issuer: format!("beyonddb-peer:{}", hex(fleet.as_bytes())),
        subject: hex(session.as_bytes()),
        actions: vec![INVOKE_ACTION.into()],
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Clone)]
struct Receiver {
    directory: NodeDirectory,
    dispatcher: Arc<PeerDispatcher>,
    activation: Arc<PeerDispatcher>,
    bootstrap: Arc<PeerDispatcher>,
    runtime: CellRuntime,
}

pub(super) fn peer_router(
    peers: &super::BeyonddbPeers,
    provisioner: Arc<crate::CellInitialPartitionProvisioner>,
) -> Router {
    let runtime = peers.runtime.clone();
    let directory = peers.placement.directory.clone();
    let dispatcher = PeerDispatcher::new(
        peers.registry.clone(),
        Arc::new(LocalResolver {
            runtime: runtime.clone(),
            layout: peers.layout.clone(),
            registry: peers.registry.clone(),
            // The sender selects ownership before forwarding. A receiver may
            // only dispatch to that active owner; a raced release must reject.
            provisioner: None,
            placement: None,
            bootstrap: None,
        }),
        Arc::new(BeyondPeerAuthorizer {
            fleet: directory.fleet(),
            action: INVOKE_ACTION,
        }),
    )
    .with_telemetry(runtime.telemetry_handle());
    let activation = PeerDispatcher::new(
        peers.registry.clone(),
        Arc::new(LocalResolver::serving(peers, provisioner.clone())),
        Arc::new(BeyondPeerAuthorizer {
            fleet: directory.fleet(),
            action: ACTIVATE_ACTION,
        }),
    )
    .with_telemetry(runtime.telemetry_handle());
    let mut resolver = LocalResolver::serving(peers, provisioner);
    resolver.bootstrap = Some(directory.clone());
    let bootstrap = PeerDispatcher::new(
        peers.registry.clone(),
        Arc::new(resolver),
        Arc::new(BeyondPeerAuthorizer {
            fleet: directory.fleet(),
            action: PROVISION_ACTION,
        }),
    )
    .with_telemetry(runtime.telemetry_handle());
    let receiver = Receiver {
        directory,
        dispatcher: Arc::new(dispatcher),
        activation: Arc::new(activation),
        bootstrap: Arc::new(bootstrap),
        runtime,
    };
    Router::new()
        .route("/internal/cells/v1/forward", post(forward))
        .layer(DefaultBodyLimit::max(
            crab_cell_runtime::peer::MAX_PEER_REQUEST_BYTES,
        ))
        .with_state(receiver)
}

async fn forward(
    State(receiver): State<Receiver>,
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
        return error(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    // I/O waiters retain request copies and enrollment state, not CPU slots.
    // Bound that memory independently before allowing another request to wait.
    let Ok(_request_bytes) = receiver
        .runtime
        .try_reserve_node_bytes(body.len() * 3 + 64 * 1024)
    else {
        return busy(&receiver, "request_memory");
    };
    let decoded = {
        let Some(_reservation) = receiver.runtime.try_reserve_worker_job().ok().flatten() else {
            return busy(&receiver, "decode");
        };
        match crab_cell_runtime::peer::UnverifiedPeerRequest::decode(&body) {
            Ok(request) => request,
            Err(_) => return error(StatusCode::UNAUTHORIZED),
        }
    };
    let deadline = started + Duration::from_millis(u64::from(decoded.remaining_ms()));
    let now_ms = match unix_time_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    // Session enrollment reads object storage. Release the codec reservation
    // during that I/O so unrelated requests can make progress.
    let enrollment = receiver.directory.peer_verifier(
        decoded.session(),
        identity.certificate(),
        identity.public_key(),
        now_ms,
    );
    let verifier = match tokio::time::timeout_at(deadline.into(), enrollment).await {
        Ok(Ok(verifier)) => verifier,
        Ok(Err(_)) => return error(StatusCode::UNAUTHORIZED),
        Err(_) => return error(StatusCode::GATEWAY_TIMEOUT),
    };
    let request = {
        let Some(_reservation) = receiver.runtime.try_reserve_worker_job().ok().flatten() else {
            return busy(&receiver, "verify");
        };
        // Enrollment I/O consumes the signed request lifetime too.
        let now_ms = match unix_time_ms() {
            Ok(now_ms) => now_ms,
            Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR),
        };
        match verifier.verify(decoded, now_ms) {
            Ok(request) => request,
            Err(_) => return error(StatusCode::UNAUTHORIZED),
        }
    };
    // Each dispatcher authorizes before resolving. Only scoped admission
    // capabilities may acquire a Cell; forwarded application work cannot.
    let admission = if request.permits(PROVISION_ACTION) {
        Some(&receiver.bootstrap)
    } else if request.permits(ACTIVATE_ACTION) {
        Some(&receiver.activation)
    } else {
        None
    };
    let dispatcher = admission.unwrap_or(&receiver.dispatcher);
    let now_ms = match unix_time_ms() {
        Ok(now_ms) => now_ms,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let reply =
        match tokio::time::timeout_at(deadline.into(), dispatcher.dispatch(&request, now_ms)).await
        {
            Ok(reply) => reply,
            Err(_) => return error(StatusCode::GATEWAY_TIMEOUT),
        };
    // Resolution, restoration and actor publication own their resources. Only
    // encoding needs another CPU reservation; post-dispatch failures are unknown
    // outcomes and must never be labeled as safe-to-retry admission refusals.
    let _reservation = match receiver.runtime.reserve_worker_job(deadline).await {
        Ok(reservation) => reservation,
        Err(Error::Deadline) => return error(StatusCode::GATEWAY_TIMEOUT),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let reply = match crab_cell_runtime::peer::encode_peer_reply(&reply) {
        Ok(reply) => reply,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, PROTOBUF_MEDIA_TYPE),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from(reply),
    )
        .into_response()
}

fn error(status: StatusCode) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

fn busy(receiver: &Receiver, phase: &'static str) -> Response {
    tracing::debug!(
        phase,
        resources = ?receiver.runtime.stats(),
        "peer request admission deferred"
    );
    // No dispatch has occurred; the sender may retry after codec capacity frees.
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::RETRY_AFTER, "1"),
        ],
    )
        .into_response()
}
