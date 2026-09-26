//! Read-only Cell snapshots and their authority response gate.

use super::*;
use std::sync::{Barrier, OnceLock};

use crab_cell_runtime::client::CellReadReplica;
use crab_cell_runtime::node::{NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain};
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlStatement, SqlValue};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor,
    OperationDescriptor, Query, QueryContext, RegistryBuilder, RetainedCodeDescriptor,
};

const MODULE: &str = "replica-counter";
const SCHEMA: &str = "CREATE TABLE counter(value INTEGER NOT NULL)";
const CODE: Digest = Digest::from_bytes([5; 32]);
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([6; 16]);
static QUERY_BARRIERS: OnceLock<(Barrier, Barrier)> = OnceLock::new();

fn query_barriers() -> &'static (Barrier, Barrier) {
    QUERY_BARRIERS.get_or_init(|| (Barrier::new(2), Barrier::new(2)))
}

struct CounterModule;

impl CellModule for CounterModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: MODULE,
            source_digest: Digest::from_bytes([7; 32]),
            retained_codes: &[RetainedCodeDescriptor {
                code: CODE,
                schema_min: 1,
                schema_max: 1,
            }],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: SCHEMA,
                digest: Digest::from_bytes(*blake3::hash(SCHEMA.as_bytes()).as_bytes()),
            }])),
            commands: &[],
            queries: &[OperationDescriptor {
                id: 1,
                codec_version: 1,
                schema_min: 1,
                schema_max: 1,
                input_limit: 16,
                output_limit: 16,
            }],
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &[NamespaceDescriptor {
                id: NAMESPACE,
                name: MODULE,
                role: CatalogRole::Repository,
                shards: 1,
                effect_targets: &[],
                dead_letter: None,
            }],
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_query::<ReadCounter>()
    }
}

struct ReadCounter;

impl Query for ReadCounter {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = u64;
    type Output = u64;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        if input == 99 {
            let (entered, release) = query_barriers();
            entered.wait();
            release.wait();
        }
        let result = context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "SELECT value FROM counter".into(),
                parameters: vec![],
            }],
        })?;
        match result.first().and_then(|set| set.rows.first()) {
            Some(row) if matches!(row.first(), Some(SqlValue::Integer(_))) => {
                let Some(SqlValue::Integer(value)) = row.first() else {
                    return Err(crab_cell_runtime::Error::Command("counter row is invalid"));
                };
                u64::try_from(*value)
                    .map_err(|_| crab_cell_runtime::Error::Command("counter is negative"))
            }
            _ => Err(crab_cell_runtime::Error::Command("counter row is missing")),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replica_reads_exact_snapshot_and_fences_after_release() {
    let fixture = fixture_for(b"read-replica");
    exercise_replica_read(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_replica_reads_exact_root_and_policy_cas() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = build_explicit_store(
        &required("CRAB_CELL_TEST_BUCKET"),
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    let fixture = fixture_with_limits_and_store_at_prefix(
        b"read-replica",
        Limits::default(),
        store,
        Path::from(required("CRAB_CELL_TEST_PREFIX")),
    );
    exercise_replica_read(&fixture).await;
    let policy = crab_cell_runtime::read_policy::ReadPolicyStore::new(fixture.layout.clone());
    let first = policy
        .create(
            fixture.target.cell_id(),
            IncarnationId::from_bytes([2; 16]),
            1,
        )
        .await
        .unwrap();
    assert_eq!(
        policy
            .update(&first, 2)
            .await
            .unwrap()
            .value()
            .desired_readers(),
        2
    );
}

async fn exercise_replica_read(fixture: &Fixture) {
    let session = SessionId::from_bytes([4; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 << 20, session).unwrap();
    let handle = bootstrap_on(&runtime, fixture, session).await;

    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "replica-test".into(),
        cargo_lock_digest: Digest::from_bytes([8; 32]),
    });
    builder.register(CounterModule).unwrap();
    let registry = Arc::new(builder.finish().unwrap());
    let fleet = Digest::from_bytes([9; 32]);
    let image = Digest::from_bytes([10; 32]);
    let release = registry.release_digest();
    let directory = NodeDirectory::new(fixture.layout.clone(), fleet, image, release);
    let now = now_ms();
    directory
        .create(
            NodeAdvertisement::sign(
                NodeId::from_bytes([11; 16]),
                session,
                "https://node.internal:8081".into(),
                fleet,
                Digest::from_bytes([12; 32]),
                image,
                release,
                &ed25519_dalek::SigningKey::from_bytes(&[13; 32]),
                1,
                now,
                now + 15_000,
                vec![CODE],
                vec![1],
                NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes: 1 << 20,
                    free_disk_bytes: 1 << 20,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            now,
        )
        .await
        .unwrap();

    let reader_path = fixture._directory.path().join("reader.sqlite");
    let reader = CellReadReplica::open(
        Arc::clone(&registry),
        CellAuthority::new(fixture.layout.clone()),
        directory.clone(),
        fixture.replica.clone(),
        fixture.target.clone(),
        &reader_path,
    )
    .await
    .unwrap();
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        0
    );
    let committed = handle
        .execute(
            crate::support::fixtures::mutation_identity(91),
            Digest::from_bytes([92; 32]),
            now_ms(),
            64,
            64,
            |transaction| {
                transaction.execute("UPDATE counter SET value = 1", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    assert!(committed.commit_sequence() > reader.receipt().await.commit_sequence);
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        0
    );
    assert!(
        reader
            .query::<ReadCounter>(
                Some(crab_cell_runtime::Receipt {
                    commit_sequence: committed.commit_sequence(),
                    ..reader.receipt().await
                }),
                0,
            )
            .await
            .is_err()
    );

    let refreshed_path = fixture._directory.path().join("refreshed.sqlite");
    std::fs::write(&refreshed_path, b"occupied").unwrap();
    assert!(reader.refresh(&refreshed_path).await.is_err());
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        0
    );
    std::fs::remove_file(&refreshed_path).unwrap();
    let pending_reader = reader.clone();
    let pending = tokio::spawn(async move { pending_reader.query::<ReadCounter>(None, 99).await });
    tokio::task::spawn_blocking(|| query_barriers().0.wait())
        .await
        .unwrap();
    let refreshed = reader.clone();
    assert_eq!(
        refreshed
            .refresh(&refreshed_path)
            .await
            .unwrap()
            .commit_sequence,
        committed.commit_sequence()
    );
    assert!(reader_path.exists());
    tokio::task::spawn_blocking(|| query_barriers().1.wait())
        .await
        .unwrap();
    let old = pending.await.unwrap().unwrap();
    assert_eq!(old.output, 0);
    assert!(old.receipt.commit_sequence < refreshed.receipt().await.commit_sequence);
    assert!(!reader_path.exists());
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        1
    );
    assert_eq!(
        refreshed
            .query::<ReadCounter>(None, 0)
            .await
            .unwrap()
            .output,
        1
    );

    handle.drain().await.unwrap();
    assert!(matches!(
        reader.query::<ReadCounter>(None, 0).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert!(matches!(
        refreshed.query::<ReadCounter>(None, 0).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    drop(reader);
    drop(refreshed);
    assert!(!reader_path.exists());
    assert!(!refreshed_path.exists());
    runtime.shutdown().await.unwrap();
}
