//! ExtendDB HTTP composition over a ready BeyondDB Cell node.

mod capacity;
mod node_lease;
mod peer_receiver;
mod placement;

pub use capacity::measured_node_capacity;
pub use node_lease::{NodeLeasePublisher, PublishedNodeLease};

use std::sync::Arc;

use crab_cell_host::CellNode;
use crab_cell_peer_http::{LoadedPeerTls, PeerHttpRoundTrip, PeerTargetScope};
use crab_cell_runtime::client::CellClient;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::{CellTarget, SessionId};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::node::NodeDirectory;
use crab_cell_runtime::peer::PeerSigner;
use extenddb_auth::{AuthCacheRegistry, BuiltinAuthProvider};
use extenddb_core::limits::LimitsConfig;
use extenddb_server::{AppState, AuthzCacheConfig, CachedAuthzStore, CachedTableKeyInfoStore};
use extenddb_storage::StorageEngine;
use extenddb_storage::error::StorageError;

use crate::{
    APPLICATION, CellCatalogStore, CellCredentialStore, CellInitialPartitionProvisioner,
    CellStorage, DATA_NAMESPACE, NAMESPACE, credentials, transaction_coordinator,
};

/// Drain a serving node and retire its advertised boot session.
///
/// The session and directory must belong to this node. Drain or withdrawal
/// failures propagate; callers must retain scratch state on failure.
/// Retirement I/O after drain is bounded by one node lease lifetime.
pub async fn shutdown_serving_node(
    node: &CellNode,
    directory: &NodeDirectory,
    session: SessionId,
) -> crab_cell_runtime::Result<()> {
    // Retirement fences publication authority. Drain closes owners/logs and
    // joins heartbeat maintenance before we read the last authoritative version.
    node.shutdown().await?;
    tokio::time::timeout(
        std::time::Duration::from_millis(node_lease::LEASE_MS as u64),
        async {
            if let Some(observed) = directory.load(session, node_lease::unix_time_ms()?).await? {
                // A canceled heartbeat may still win its remote CAS. Reconcile
                // only this drained boot; the tombstone fences later refreshes.
                directory
                    .withdraw_after_drain(&observed, node_lease::unix_time_ms()?)
                    .await?;
            }
            Ok(())
        },
    )
    .await
    .map_err(|_| crab_cell_runtime::Error::Deadline)?
}

/// Restricts peer forwarding to BeyondDB's compiled Cell namespaces.
///
/// Account tenants are derived per account, while credential Cells use a
/// shared credential tenant. Request authorization remains the server's duty.
pub struct BeyonddbPeerScope;

impl PeerTargetScope for BeyonddbPeerScope {
    fn check_target(&self, target: &CellTarget) -> crab_cell_runtime::Result<()> {
        if target.application() != APPLICATION
            || ![
                NAMESPACE,
                DATA_NAMESPACE,
                crate::directory::NAMESPACE,
                crate::global_index::NAMESPACE,
                credentials::NAMESPACE,
                transaction_coordinator::NAMESPACE,
            ]
            .contains(&target.namespace())
        {
            return Err(crab_cell_runtime::Error::PeerAuthorization(
                "Cell target is outside BeyondDB",
            ));
        }
        Ok(())
    }
}

/// Shared peer identity and transport for placement, bootstrap, and invocation.
///
/// Retain the node lease while using this context. It owns no client or
/// provisioner, so clients can hold provisioners without a reference cycle.
pub struct BeyonddbPeers {
    runtime: crab_cell_runtime::cell::actor::CellRuntime,
    layout: CellStorageLayout,
    registry: Arc<crab_cell_runtime::registry::Registry>,
    placement: Arc<placement::RangePlacement>,
}

impl BeyonddbPeers {
    /// Bind peer transport to this node's advertised boot session and TLS key.
    pub fn new(
        node: &CellNode,
        layout: CellStorageLayout,
        directory: NodeDirectory,
        session: SessionId,
        tls: &LoadedPeerTls,
    ) -> crab_cell_runtime::Result<Self> {
        if directory.fleet() != tls.fleet() {
            return Err(crab_cell_runtime::Error::PeerAuthorization(
                "peer TLS identity belongs to another fleet",
            ));
        }
        let signer = Arc::new(PeerSigner::new(
            session,
            node.application().registry().release_digest(),
            tls.signing_key().clone(),
        ));
        let round_trip = Arc::new(PeerHttpRoundTrip::new(
            Arc::new(BeyonddbPeerScope),
            CellAuthority::new(layout.clone()),
            directory.clone(),
            Arc::new(tls.client_identity()),
            session,
        ));
        Ok(Self {
            runtime: node.runtime(),
            layout: layout.clone(),
            registry: node.application().registry(),
            placement: Arc::new(placement::RangePlacement {
                runtime: node.runtime(),
                authority: CellAuthority::new(layout),
                directory,
                session,
                signer,
                round_trip,
            }),
        })
    }

    /// Build an owner-resolving client with placement for idle range/directory Cells.
    pub fn client(&self, provisioner: Arc<CellInitialPartitionProvisioner>) -> CellClient {
        let principal =
            peer_receiver::peer_principal(self.placement.directory.fleet(), self.placement.session);
        CellClient::peer(
            self.registry.clone(),
            self.placement.signer.clone(),
            principal,
            self.placement.round_trip.clone(),
        )
        .with_local_resolver(Arc::new(
            peer_receiver::LocalResolver::serving(self, provisioner)
                .with_placement(self.placement.clone()),
        ))
    }

    /// Build the authenticated peer route; mount only on this identity's mTLS listener.
    pub fn router(&self, provisioner: Arc<CellInitialPartitionProvisioner>) -> axum::Router {
        peer_receiver::peer_router(self, provisioner)
    }

    pub(crate) fn directory(&self) -> &NodeDirectory {
        &self.placement.directory
    }

    pub(crate) async fn activate_remote_range(
        &self,
        target: &CellTarget,
        node: crab_cell_runtime::node::NodeAdvertisement,
    ) -> crab_cell_runtime::Result<()> {
        self.placement
            .admit_remote(target, node, peer_receiver::ACTIVATE_ACTION)
            .await
    }

    pub(crate) async fn provision_local(
        &self,
        target: &CellTarget,
        owner: Option<SessionId>,
    ) -> crab_cell_runtime::Result<bool> {
        // A live initial owner must finish its own bootstrap. An expired owner
        // needs a fresh placement decision followed by runtime takeover proof.
        let owner = match owner {
            Some(session)
                if self
                    .placement
                    .directory
                    .is_live(session, node_lease::unix_time_ms()?)
                    .await? =>
            {
                Some(session)
            }
            _ => None,
        };
        self.placement
            .select_local(target, owner, peer_receiver::PROVISION_ACTION)
            .await
    }
}

/// Build the signed DynamoDB request path for an already-ready leased Cell node.
///
/// The caller must provision account and credential Cells, provide a client
/// that reaches their current owners, retain the node's lease and task group,
/// and start ExtendDB's listener with the returned state.
/// Table lifecycle completion also requires the provisioner's account capacity
/// worker, as installed by the server binary; HTTP deletion records intent first.
/// ExtendDB requires a catalog to authorize DynamoDB requests. The catalog's
/// management methods explicitly fail until their Cell implementation lands.
pub fn build_http_state(
    node: &CellNode,
    client: CellClient,
    layout: CellStorageLayout,
    provisioner: Arc<CellInitialPartitionProvisioner>,
    encryption_key: [u8; 32],
    region: &str,
    server_addr: String,
) -> Result<AppState, StorageError> {
    if !node.is_ready() {
        return Err(StorageError::Connection(
            "BeyondDB Cell node is not ready to serve".into(),
        ));
    }
    // Pace bursts through finite mailboxes without replaying unknown writes.
    // Leave time within the 60-second mutation lifetime for owner admission.
    let client = client
        .with_admission_backpressure(128, 32 * 1024 * 1024, std::time::Duration::from_secs(50))
        .map_err(|error| StorageError::Internal(error.to_string()))?;
    let storage: Arc<dyn StorageEngine> = Arc::new(
        CellStorage::new(client.clone(), region)
            .with_transaction_coordinators(provisioner.clone())
            .with_initial_partitions(provisioner),
    );
    let credentials = CellCredentialStore::new(client.clone(), layout, encryption_key);
    let catalog = Arc::new(CellCatalogStore::new(client));
    let authz_cache = Arc::new(CachedAuthzStore::pass_through(
        catalog.clone(),
        AuthzCacheConfig::default(),
    ));
    Ok(AppState {
        storage: Arc::clone(&storage),
        auth: Arc::new(BuiltinAuthProvider::new(credentials)),
        limits: Arc::new(LimitsConfig::default()),
        region: Arc::from(region),
        server_addr,
        catalog_store: Some(catalog),
        version_info: Arc::from(env!("CARGO_PKG_VERSION")),
        metrics: Arc::default(),
        tls_enabled: false,
        dev_mode: false,
        import_paths: Arc::from([]),
        export_paths: Arc::from([]),
        throttle: Arc::default(),
        auth_cache: AuthCacheRegistry::empty(),
        authz_cache,
        // Table generations can change through any server. Process-local cache
        // invalidation cannot prevent routing a recreated name to its deleted ID.
        table_key_info_cache: Arc::new(CachedTableKeyInfoStore::pass_through(
            storage,
            Default::default(),
        )),
        config_entries: Vec::new(),
        docs_store: None,
    })
}
