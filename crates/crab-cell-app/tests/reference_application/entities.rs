//! Entity-scoped invoice receipts through generated application clients.

use super::fleet::peer_round_trip;
use crate::*;
use crab_cell_runtime::peer::{PeerPrincipal, PeerRoundTrip, PeerSigner};
use std::collections::HashMap;

pub(crate) struct EntityReferenceApplication;

impl CellApplication for EntityReferenceApplication {
    const NAME: &'static str = "entity-reference-application";

    fn register(builder: &mut ApplicationBuilder) -> Result<()> {
        builder.register(ReferenceSql)?;
        builder.cell_type(
            CellType::new(SQL_MODULE, "orders", SQL_NAMESPACE, CatalogRole::Sql, 1)?
                .with_entity_partitions()?,
        )
    }
}

crab_cell_app::cell_client! {
    pub(crate) struct EntityReferenceClient (EntityReferenceApplication) {
        pub(crate) fn orders(scope: &OrderId) -> EntityOrderCell {
            namespace: SQL_NAMESPACE,
            module: SQL_MODULE,
            commands: { pub(crate) fn receive_cron, prepare_receive_cron: ReferenceCronReceiver = 6; },
            queries: { pub(crate) fn receipt_count: ReferenceReceiptCount = 7; }
        }
    }
}

pub(super) fn compiled_entities() -> Arc<crab_cell_app::CompiledApplication> {
    Arc::new(
        EntityReferenceApplication::compile(BuildDescriptor {
            source_revision: "entity-reference-source".into(),
            cargo_lock_digest: Digest::from_bytes([42; 32]),
        })
        .unwrap(),
    )
}

fn application_handle<A: CellApplication>(
    application: Arc<crab_cell_app::CompiledApplication>,
    round_trip: Arc<dyn PeerRoundTrip>,
) -> ApplicationHandle<A> {
    let client = peer_client(&application, round_trip);
    ApplicationHandle::<A>::new(
        client,
        application,
        TenantId::from_bytes([81; 16]),
        ApplicationId::from_bytes([82; 16]),
    )
    .unwrap()
    .with_blob_artifact_store(BlobArtifactStore::new(Store::new(
        Arc::new(InMemory::new()),
    )))
}

fn peer_client(
    application: &crab_cell_app::CompiledApplication,
    round_trip: Arc<dyn PeerRoundTrip>,
) -> CellClient {
    let registry = application.registry();
    CellClient::peer(
        registry.clone(),
        Arc::new(PeerSigner::new(
            crab_cell_runtime::SessionId::from_bytes([77; 16]),
            registry.release_digest(),
            SigningKey::from_bytes(&[78; 32]),
        )),
        PeerPrincipal {
            issuer: "reference".into(),
            subject: "entity-client".into(),
            actions: vec!["cell.read".into(), "cell.write".into()],
        },
        round_trip,
    )
}

#[test]
fn generated_client_uses_the_declared_entity_partition() {
    let application = compiled_entities();
    let handle = application_handle(application.clone(), peer_round_trip(HashMap::new()));
    let client = EntityReferenceClient::new(handle).unwrap();
    let first = client.orders(&OrderId(b"order-1".to_vec())).unwrap();
    let second = client.orders(&OrderId(b"order-2".to_vec())).unwrap();
    let repeated = client.orders(&OrderId(b"order-1".to_vec())).unwrap();
    assert_eq!(first.target(), repeated.target());
    assert_ne!(first.target().cell_id(), second.target().cell_id());
    assert_eq!(
        first.target().partition(),
        application.cell_types()[0]
            .entity_partition(b"order-1")
            .unwrap(),
    );
    for invalid in [Vec::new(), vec![0; 1_025]] {
        assert!(client.orders(&OrderId(invalid)).is_err());
    }
}

struct EntityPrimitiveApplication;

impl CellApplication for EntityPrimitiveApplication {
    const NAME: &'static str = "entity-primitive-application";

    fn register(builder: &mut ApplicationBuilder) -> Result<()> {
        builder.register(ReferenceSql)?;
        builder.register(ReferenceKv)?;
        builder.register(ReferenceBlob)?;
        builder.register(ReferenceQueue)?;
        builder.register(ReferenceDeadLetter)?;
        builder.register(ReferenceCron)?;
        builder.register(ReferenceWorkflow)?;
        for cell_type in compiled().cell_types() {
            builder.cell_type(cell_type.with_entity_partitions()?)?;
        }
        Ok(())
    }
}

#[test]
fn namespace_primitive_handles_reject_entity_topology() {
    let application = Arc::new(
        EntityPrimitiveApplication::compile(BuildDescriptor {
            source_revision: "entity-reference-source".into(),
            cargo_lock_digest: Digest::from_bytes([42; 32]),
        })
        .unwrap(),
    );
    let handle = application_handle::<EntityPrimitiveApplication>(
        application,
        peer_round_trip(HashMap::new()),
    );
    let results = [
        ("kv", handle.kv::<ReferenceKv>(KV_NAMESPACE).map(|_| ())),
        ("blob", handle.blob::<ReferenceBlob>().map(|_| ())),
        ("queue", handle.queue::<ReferenceQueue>().map(|_| ())),
        ("cron", handle.cron::<ReferenceCron>().map(|_| ())),
        (
            "workflow",
            handle.workflow::<ReferenceWorkflow>().map(|_| ()),
        ),
        (
            "activities",
            handle.activities::<ReferenceWorkflow>().map(|_| ()),
        ),
    ];
    for (name, result) in results {
        assert!(
            matches!(
                result,
                Err(Error::Identity(
                    "namespace capability requires fixed shards"
                ))
            ),
            "{name}: {result:?}",
        );
    }
}

#[test]
fn explicit_sql_and_effect_capabilities_accept_entity_targets() {
    let application = compiled_entities();
    let target = CellTarget::new(
        TenantId::from_bytes([81; 16]),
        ApplicationId::from_bytes([82; 16]),
        SQL_NAMESPACE,
        &application.cell_types()[0]
            .entity_partition(b"order-1")
            .unwrap(),
    )
    .unwrap();
    let handle = application_handle::<EntityReferenceApplication>(
        application,
        peer_round_trip(HashMap::new()),
    );
    assert!(handle.sql::<ReferenceSql>(target.clone()).is_ok());
    assert!(handle.effects::<ReferenceSql>(target).is_ok());
}

mod hosts;
