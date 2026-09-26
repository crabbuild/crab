use std::{future::Future, pin::Pin, sync::Arc, time::SystemTime};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use crab_cell_host::CellNode;
use crab_cell_peer_http::{PROTOBUF_MEDIA_TYPE, PeerTargetScope, PeerTlsIdentity};
use crab_cell_runtime::cell::{
    actor::{CellHandle, CellRuntime},
    catalog::{CatalogRole, CellCatalog},
};
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::{CellTarget, Digest, SessionId};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::node::NodeDirectory;
use crab_cell_runtime::peer::{
    PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerPrincipal, VerifiedPeerRequest,
};
use crab_cell_runtime::registry::Registry;
use crab_cell_runtime::{Error, Result};

use super::BeyonddbPeerScope;
use crate::{DATA_MODULE, DATA_NAMESPACE, MODULE, NAMESPACE, credentials, transaction_coordinator};

const INVOKE_ACTION: &str = "beyonddb.cell.invoke";

#[derive(Clone)]
struct LocalResolver {
    runtime: CellRuntime,
    layout: CellStorageLayout,
    registry: Arc<Registry>,
}

impl PeerCellResolver for LocalResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellHandle>> + Send + 'static>> {
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
            let control = CellAuthority::new(resolver.layout)
                .load(target.cell_id())
                .await?
                .ok_or(Error::CellNotActive)?;
            resolver
                .runtime
                .local_handle(proof, &control)
                .await?
                .ok_or(Error::CellNotActive)
        })
    }
}

struct BeyondPeerAuthorizer {
    fleet: Digest,
}

impl PeerAuthorizer for BeyondPeerAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> Result<()> {
        BeyonddbPeerScope.check_target(request.target())?;
        let expected = peer_principal(self.fleet, request.origin_session());
        if request.principal() != &expected || !request.permits(INVOKE_ACTION) {
            return Err(Error::PeerAuthorization(
                "BeyondDB peer principal is invalid",
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
    runtime: CellRuntime,
}

/// Builds BeyondDB's authenticated private peer route for an mTLS listener.
///
/// Mount this router only on `LoadedPeerTls::listener`. The caller must keep
/// the node's advertisement lease and task group alive while serving.
pub fn peer_router(node: &CellNode, layout: CellStorageLayout, directory: NodeDirectory) -> Router {
    let runtime = node.runtime();
    let dispatcher = PeerDispatcher::new(
        node.application().registry(),
        Arc::new(LocalResolver {
            runtime: runtime.clone(),
            layout,
            registry: node.application().registry(),
        }),
        Arc::new(BeyondPeerAuthorizer {
            fleet: directory.fleet(),
        }),
    )
    .with_telemetry(runtime.telemetry_handle());
    let receiver = Receiver {
        directory,
        dispatcher: Arc::new(dispatcher),
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
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some(PROTOBUF_MEDIA_TYPE)
    {
        return error(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let Some(_reservation) = receiver.runtime.try_reserve_worker_job().ok().flatten() else {
        return error(StatusCode::SERVICE_UNAVAILABLE);
    };
    let now_ms = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => match i64::try_from(duration.as_millis()) {
            Ok(now_ms) => now_ms,
            Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR),
        },
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let request = match receiver
        .directory
        .verify_peer_request(&body, identity.certificate(), identity.public_key(), now_ms)
        .await
    {
        Ok(request) => request,
        Err(_) => return error(StatusCode::UNAUTHORIZED),
    };
    let reply = match receiver.dispatcher.dispatch_bytes(&request, now_ms).await {
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
