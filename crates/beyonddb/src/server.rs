//! ExtendDB HTTP composition over a ready BeyondDB Cell node.

mod node_lease;
mod peer_receiver;

pub use node_lease::{NodeLeasePublisher, PublishedNodeLease};
pub use peer_receiver::peer_router;

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

/// Build the owner-resolving client for a BeyondDB peer node.
///
/// The caller retains the node's published lease. The TLS identity's private
/// key signs peer requests for the same boot session advertised by the node.
pub fn build_peer_client(
    node: &CellNode,
    layout: CellStorageLayout,
    directory: NodeDirectory,
    session: SessionId,
    tls: &LoadedPeerTls,
) -> crab_cell_runtime::Result<CellClient> {
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
    let principal = peer_receiver::peer_principal(directory.fleet(), session);
    let round_trip = Arc::new(PeerHttpRoundTrip::new(
        Arc::new(BeyonddbPeerScope),
        CellAuthority::new(layout.clone()),
        directory,
        Arc::new(tls.client_identity()),
        session,
    ));
    Ok(CellClient::runtime_with_peer(
        node.application().registry(),
        node.runtime(),
        layout,
        signer,
        principal,
        round_trip,
    ))
}

/// Build the signed DynamoDB request path for an already-ready leased Cell node.
///
/// The caller must provision account and credential Cells, provide a client
/// that reaches their current owners, retain the node's lease and task group,
/// and start ExtendDB's listener with the returned state.
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
    let storage: Arc<dyn StorageEngine> =
        Arc::new(CellStorage::new(client.clone(), region).with_initial_partitions(provisioner));
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
        table_key_info_cache: Arc::new(CachedTableKeyInfoStore::new(storage, Default::default())),
        config_entries: Vec::new(),
        docs_store: None,
    })
}
