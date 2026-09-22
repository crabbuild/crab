use super::fleet::{balancer_round_trip, peer_round_trip, start_peer_servers};
use super::*;
use std::{collections::HashMap, net::SocketAddr, time::SystemTime};

use crab_cell_runtime::{
    EffectPeerClient, PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerPrincipal,
    PeerRoundTrip, PeerSigner, PeerVerifier, VerifiedPeerRequest,
};

pub(super) fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

pub(super) fn identity(phase: u8, index: usize, step: u8) -> MutationIdentity {
    let mut bytes = [0; 16];
    bytes[0] = phase;
    bytes[1] = step;
    bytes[8..].copy_from_slice(&(index as u64).to_be_bytes());
    let now = now_ms();
    MutationIdentity {
        request_id: RequestId::from_bytes(bytes),
        issued_at_ms: now,
        expires_at_ms: now + 300_000,
    }
}

pub(super) fn item_id(index: usize) -> [u8; 16] {
    let mut id = [0; 16];
    id[8..].copy_from_slice(&(index as u64).to_be_bytes());
    id
}

pub(super) fn install_sql_tables(tx: &crab_ltx::rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE orders(id INTEGER PRIMARY KEY, total_cents INTEGER NOT NULL); \
         CREATE TABLE invoice_receipts(schedule_id BLOB NOT NULL, occurrence INTEGER NOT NULL, payload BLOB NOT NULL, PRIMARY KEY(schedule_id, occurrence));",
    )?;
    Ok(())
}

pub(super) type Schema = for<'a> fn(&crab_ltx::rusqlite::Transaction<'a>) -> Result<()>;

pub(super) fn perf_cells() -> [(NamespaceId, CatalogRole, &'static str, u8, Schema); 7] {
    [
        (
            SQL_NAMESPACE,
            CatalogRole::Sql,
            SQL_MODULE,
            40,
            install_sql_tables,
        ),
        (
            KV_NAMESPACE,
            CatalogRole::Kv,
            KV_MODULE,
            41,
            install_kv_schema,
        ),
        (
            BLOB_NAMESPACE,
            CatalogRole::Blob,
            BLOB_MODULE,
            42,
            install_blob_schema,
        ),
        (
            QUEUE_NAMESPACE,
            CatalogRole::Queue,
            QUEUE_MODULE,
            43,
            install_queue_schema,
        ),
        (
            DEAD_LETTER_NAMESPACE,
            CatalogRole::Queue,
            DEAD_LETTER_MODULE,
            44,
            install_queue_schema,
        ),
        (
            CRON_NAMESPACE,
            CatalogRole::Cron,
            CRON_MODULE,
            45,
            install_cron_schema,
        ),
        (
            WORKFLOW_NAMESPACE,
            CatalogRole::Workflow,
            WORKFLOW_MODULE,
            46,
            install_workflow_schema,
        ),
    ]
}

pub(super) fn owner_routes(
    tenant: TenantId,
    application: ApplicationId,
    owners: [SocketAddr; 3],
) -> HashMap<crab_cell_runtime::CellId, SocketAddr> {
    perf_cells()
        .into_iter()
        .enumerate()
        .map(|(index, (namespace, _, _, _, _))| {
            let target =
                CellTarget::new(tenant, application, namespace, &partition_for_shard(0)).unwrap();
            (target.cell_id(), owners[index % owners.len()])
        })
        .collect()
}

pub(super) struct PerfFixture {
    _directory: tempfile::TempDir,
    runtimes: Vec<CellRuntime>,
    pub(super) typed: ApplicationHandle<ReferenceApplication>,
    pub(super) registry: Arc<Registry>,
    pub(super) client: CellClient,
    pub(super) sql_target: CellTarget,
    pub(super) cron_target: CellTarget,
    peer: EffectPeerClient,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl PerfFixture {
    pub(super) async fn start(nodes: usize) -> Self {
        assert!(nodes == 1 || nodes == 3);
        let application = Arc::new(compiled());
        let registry = application.registry();
        let tenant = TenantId::from_bytes([81; 16]);
        let application_id = ApplicationId::from_bytes([82; 16]);
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = CellStorageLayout::new(
            store.clone(),
            object_store::path::Path::from("reference-performance"),
            *application_id.as_bytes(),
        );
        let directory = tempfile::TempDir::new().unwrap();
        let runtimes = (0..nodes)
            .map(|node| {
                CellRuntime::new_with_replica_host(
                    SqlWorkerPool::new(4, 32).unwrap(),
                    64 * 1024 * 1024,
                    node_session(node),
                    reference_host(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let cells = perf_cells();
        let mut handles = Vec::new();
        let mut owned = vec![Vec::new(); nodes];
        for (cell_index, (namespace, role, module, incarnation, schema)) in
            cells.into_iter().enumerate()
        {
            let node = cell_index % nodes;
            let handle = bootstrap_reference_cell(
                &runtimes[node],
                &registry,
                &layout,
                &directory,
                tenant,
                application_id,
                node_session(node),
                namespace,
                role,
                module,
                incarnation,
                schema,
            )
            .await
            .unwrap();
            owned[node].push(handle.clone());
            handles.push(handle);
        }
        let sql_handle = handles[0].clone();
        let signer = Arc::new(PeerSigner::new(
            crab_cell_runtime::SessionId::from_bytes([77; 16]),
            registry.release_digest(),
            SigningKey::from_bytes(&[78; 32]),
        ));
        let principal = PeerPrincipal {
            issuer: "reference-performance".into(),
            subject: "fleet-driver".into(),
            actions: vec![
                "cell.read".into(),
                "cell.write".into(),
                "reference.cron.deliver".into(),
            ],
        };
        let verifier = Arc::new(PeerVerifier::new(
            crab_cell_runtime::SessionId::from_bytes([77; 16]),
            registry.release_digest(),
            signer.verifying_key(),
        ));
        let (client, peer, servers) = if nodes == 1 {
            let client = CellClient::local_many(Arc::clone(&registry), handles).unwrap();
            let dispatcher = Arc::new(PeerDispatcher::new(
                Arc::clone(&registry),
                Arc::new(SqlResolver {
                    target: CellTarget::new(
                        tenant,
                        application_id,
                        SQL_NAMESPACE,
                        &partition_for_shard(0),
                    )
                    .unwrap(),
                    handle: sql_handle,
                }),
                Arc::new(CronAuthorizer),
            ));
            let peer = EffectPeerClient::new(
                signer,
                principal,
                Arc::new(Loopback {
                    verifier,
                    dispatcher,
                }),
            );
            (client, peer, Vec::new())
        } else {
            let (round_trip, servers) = start_peer_servers(&registry, verifier, owned).await;
            let client = CellClient::peer(
                Arc::clone(&registry),
                Arc::clone(&signer),
                principal.clone(),
                Arc::clone(&round_trip),
            );
            let peer = EffectPeerClient::new(signer, principal, round_trip);
            (client, peer, servers)
        };
        let typed = ApplicationHandle::new(client.clone(), application, tenant, application_id)
            .with_blob_artifact_store(BlobArtifactStore::new(store));
        let sql_target = CellTarget::new(
            tenant,
            application_id,
            SQL_NAMESPACE,
            &partition_for_shard(0),
        )
        .unwrap();
        let cron_target = CellTarget::new(
            tenant,
            application_id,
            CRON_NAMESPACE,
            &partition_for_shard(0),
        )
        .unwrap();
        Self {
            _directory: directory,
            runtimes,
            typed,
            registry,
            client,
            sql_target,
            cron_target,
            peer,
            servers,
        }
    }

    pub(super) fn cron_peer(&self) -> EffectPeerClient {
        self.peer.clone()
    }

    pub(super) fn from_processes(
        directory: tempfile::TempDir,
        store: Store,
        owners: [SocketAddr; 3],
        balancer: Option<SocketAddr>,
    ) -> Self {
        let application = Arc::new(compiled());
        let registry = application.registry();
        let tenant = TenantId::from_bytes([81; 16]);
        let application_id = ApplicationId::from_bytes([82; 16]);
        let round_trip = if let Some(address) = balancer {
            balancer_round_trip(address)
        } else {
            peer_round_trip(owner_routes(tenant, application_id, owners))
        };
        let signer = Arc::new(PeerSigner::new(
            crab_cell_runtime::SessionId::from_bytes([77; 16]),
            registry.release_digest(),
            SigningKey::from_bytes(&[78; 32]),
        ));
        let principal = PeerPrincipal {
            issuer: "reference-performance".into(),
            subject: "fleet-driver".into(),
            actions: vec![
                "cell.read".into(),
                "cell.write".into(),
                "reference.cron.deliver".into(),
            ],
        };
        let client = CellClient::peer(
            Arc::clone(&registry),
            Arc::clone(&signer),
            principal.clone(),
            Arc::clone(&round_trip),
        );
        let peer = EffectPeerClient::new(signer, principal, round_trip);
        let typed = ApplicationHandle::new(client.clone(), application, tenant, application_id)
            .with_blob_artifact_store(BlobArtifactStore::new(store));
        let sql_target = CellTarget::new(
            tenant,
            application_id,
            SQL_NAMESPACE,
            &partition_for_shard(0),
        )
        .unwrap();
        let cron_target = CellTarget::new(
            tenant,
            application_id,
            CRON_NAMESPACE,
            &partition_for_shard(0),
        )
        .unwrap();
        Self {
            _directory: directory,
            runtimes: Vec::new(),
            typed,
            registry,
            client,
            sql_target,
            cron_target,
            peer,
            servers: Vec::new(),
        }
    }

    pub(super) async fn shutdown(&self) {
        for server in &self.servers {
            server.abort();
        }
        for runtime in &self.runtimes {
            runtime.shutdown().await.unwrap();
        }
    }
}

pub(super) fn node_session(node: usize) -> crab_cell_runtime::SessionId {
    crab_cell_runtime::SessionId::from_bytes([24 + node as u8; 16])
}

struct SqlResolver {
    target: CellTarget,
    handle: CellHandle,
}

impl PeerCellResolver for SqlResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellHandle>> + Send + 'static>> {
        let allowed = target == self.target;
        let handle = self.handle.clone();
        Box::pin(async move {
            if allowed {
                Ok(handle)
            } else {
                Err(Error::CellNotActive)
            }
        })
    }
}

struct CronAuthorizer;

impl PeerAuthorizer for CronAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> Result<()> {
        if request.permits("reference.cron.deliver") {
            Ok(())
        } else {
            Err(Error::PeerAuthorization("missing cron delivery action"))
        }
    }
}

struct Loopback {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
}

impl PeerRoundTrip for Loopback {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        let verifier = Arc::clone(&self.verifier);
        let dispatcher = Arc::clone(&self.dispatcher);
        Box::pin(async move {
            let verified = verifier.verify(&request, now_ms())?;
            if verified.target() != &target {
                return Err(Error::Peer("round trip target changed"));
            }
            dispatcher.dispatch_bytes(&verified, now_ms()).await
        })
    }
}
