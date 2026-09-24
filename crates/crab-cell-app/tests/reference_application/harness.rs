//! Runtime, session, and identity fixtures shared by the suite.

use crate::*;

#[expect(
    clippy::too_many_arguments,
    reason = "the qualification fixture keeps each Cell contract explicit"
)]
pub(crate) async fn bootstrap_reference_cell<F>(
    runtime: &CellRuntime,
    registry: &Arc<Registry>,
    layout: &CellStorageLayout,
    directory: &tempfile::TempDir,
    tenant: TenantId,
    application: ApplicationId,
    session: crab_cell_runtime::SessionId,
    namespace: NamespaceId,
    role: CatalogRole,
    module: &'static str,
    incarnation_byte: u8,
    initialize: F,
) -> crab_cell_runtime::Result<CellHandle>
where
    F: for<'connection> FnOnce(
            &crab_ltx::rusqlite::Transaction<'connection>,
        ) -> crab_cell_runtime::Result<()>
        + Send
        + 'static,
{
    let target = CellTarget::new(tenant, application, namespace, &partition_for_shard(0))?;
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(layout.clone(), tenant);
    let proof = catalog
        .provision(CatalogEntry::new(
            &target,
            role,
            registry
                .module_code(module)
                .ok_or(crab_cell_runtime::Error::Registry("module code is missing"))?,
            1,
        )?)
        .await?;
    let authority = CellAuthority::new(layout.clone());
    let incarnation = IncarnationId::from_bytes([incarnation_byte; 16]);
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: format!("https://{module}.internal:8081"),
            },
        )
        .await?;
    runtime
        .bootstrap(
            proof,
            CellReplica::new(
                layout.clone(),
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                Limits::default(),
            )?,
            authority,
            observed,
            directory.path().join(format!("{module}.sqlite")),
            initialize,
        )
        .await
}

pub(crate) fn reference_host() -> Host {
    Host::default().with_local_disk_budget(DiskBudget::new(1 << 30))
}

pub(crate) async fn seed_reference_session(
    layout: &CellStorageLayout,
    session: crab_cell_runtime::SessionId,
) {
    let directory = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let key = SigningKey::from_bytes(&[93; 32]);
    directory
        .create(
            NodeAdvertisement::sign(
                NodeId::from_bytes(*session.as_bytes()),
                session,
                "https://reference-expired.internal:8081".into(),
                directory.fleet(),
                Digest::from_bytes([94; 32]),
                Digest::from_bytes([91; 32]),
                Digest::from_bytes([92; 32]),
                &key,
                1,
                1,
                10_001,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            1,
        )
        .await
        .unwrap();
}

pub(crate) async fn fence_reference_session(
    layout: &CellStorageLayout,
    session: crab_cell_runtime::SessionId,
    claimant: crab_cell_runtime::SessionId,
) -> FencedNodeSession {
    let directory = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let key = SigningKey::from_bytes(&[93; 32]);
    directory
        .create(
            NodeAdvertisement::sign(
                NodeId::from_bytes(*claimant.as_bytes()),
                claimant,
                "https://reference-claimant.internal:8081".into(),
                directory.fleet(),
                Digest::from_bytes([94; 32]),
                Digest::from_bytes([91; 32]),
                Digest::from_bytes([92; 32]),
                &key,
                10_000,
                10_000,
                20_000,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            10_000,
        )
        .await
        .unwrap();
    directory
        .claim_expired(session, claimant, 10_001)
        .await
        .unwrap()
}

pub(crate) fn reference_identity(byte: u8, now_ms: i64) -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

pub(crate) fn qualification_identity(index: u64, now_ms: i64) -> MutationIdentity {
    let mut request_id = [0; 16];
    request_id[..8].copy_from_slice(&index.to_be_bytes());
    MutationIdentity {
        request_id: RequestId::from_bytes(request_id),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

pub(crate) struct TypedQualificationExecutor {
    pub(crate) handle: ApplicationHandle<ReferenceApplication>,
    pub(crate) tenant: TenantId,
    pub(crate) application: ApplicationId,
    pub(crate) now_ms: i64,
}

impl QualificationOperationExecutor for TypedQualificationExecutor {
    type Future<'a> = Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        let handle = self.handle.clone();
        let tenant = self.tenant;
        let application = self.application;
        let now_ms = self.now_ms;
        Box::pin(async move {
            let identity = qualification_identity(operation.index(), now_ms);
            match operation.primitive() {
                "sql" => {
                    let target = CellTarget::new(
                        tenant,
                        application,
                        SQL_NAMESPACE,
                        &partition_for_shard(0),
                    )?;
                    let sql = handle.sql::<ReferenceSql>(target)?;
                    sql.query(
                        None,
                        SqlBatch {
                            statements: vec![SqlStatement {
                                sql: "SELECT ?1".into(),
                                parameters: vec![SqlValue::Integer(operation.nonce() as i64)],
                            }],
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("qualification SQL invocation failed"))?;
                }
                "kv" => {
                    let kv = handle.kv::<ReferenceKv>(KV_NAMESPACE)?;
                    kv.atomic(
                        identity,
                        KvAtomicRequest {
                            scope: b"qualification-driver".to_vec(),
                            checks: Vec::new(),
                            mutations: vec![KvMutation::Put {
                                key: operation.index().to_be_bytes().to_vec(),
                                value: operation.nonce().to_be_bytes().to_vec(),
                                expires_at_ms: None,
                            }],
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("qualification KV invocation failed"))?;
                }
                "blob" => {
                    let blob = handle.blob::<ReferenceBlob>()?;
                    blob.query(
                        BlobQuery::Read {
                            key: b"qualification/blob".to_vec(),
                            offset: 0,
                            limit: 128,
                        },
                        None,
                    )
                    .await
                    .map_err(|_| Error::Control("qualification Blob invocation failed"))?;
                }
                "queue" => {
                    let queue = handle.queue::<ReferenceQueue>()?;
                    queue
                        .info(0, None)
                        .await
                        .map_err(|_| Error::Control("qualification Queue invocation failed"))?;
                }
                "cron" => {
                    let cron = handle.cron::<ReferenceCron>()?;
                    cron.get([58; 16], None)
                        .await
                        .map_err(|_| Error::Control("qualification Cron invocation failed"))?;
                }
                "workflow" => {
                    let workflow = handle.workflow::<ReferenceWorkflow>()?;
                    workflow
                        .state(b"effect-run".to_vec(), None)
                        .await
                        .map_err(|_| Error::Control("qualification Workflow invocation failed"))?;
                }
                "activity" => {
                    let activities = handle.activities::<ReferenceWorkflow>()?;
                    let supervisor =
                        crab_cell_runtime::primitives::workflow::ActivitySupervisor::new(
                            activities, 5_000,
                        )?;
                    supervisor
                        .run_once(0, None)
                        .await
                        .map_err(|_| Error::Control("qualification Activity invocation failed"))?;
                }
                "effects" => {
                    let target = CellTarget::new(
                        tenant,
                        application,
                        WORKFLOW_NAMESPACE,
                        &partition_for_shard(0),
                    )?;
                    let effects = handle.effects::<ReferenceWorkflow>(target)?;
                    effects
                        .claim(
                            identity,
                            EffectClaimRequest {
                                limit: 1,
                                lease_ms: 5_000,
                            },
                        )
                        .await
                        .map_err(|_| Error::Control("qualification Effects invocation failed"))?;
                }
                _ => return Err(Error::Control("qualification primitive is not registered")),
            }
            Ok(QualificationExecution::acknowledged(true))
        })
    }
}
