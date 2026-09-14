use std::sync::OnceLock;

use crab_cell_runtime::{
    ApplicationId, ApplicationIdentity, ApplicationIdentityStore, BuildDescriptor, CatalogRole,
    CellModule, Digest, MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor, NamespaceId,
    OperationDescriptor, Registry, RegistryBuilder, ReleaseStore, RequestId, TenantId,
};
use object_store::path::Path;
use uuid::Uuid;

use crate::{Config, Error, Result, storage_root::StorageRoot};

mod repository;

const REPOSITORY_MIGRATION: &str = include_str!("cells/migrations/0001_repository_identity.sql");
const REPOSITORY_NAMESPACE: NamespaceId = NamespaceId::from_bytes(*b"crab-repository1");
const REPOSITORY_COMMANDS: &[OperationDescriptor] = &[
    operation(1, 80 * 1024, 80 * 1024),
    operation(2, 80 * 1024, 80 * 1024),
];
const REPOSITORY_QUERIES: &[OperationDescriptor] =
    &[operation(1, 8, 80 * 1024), operation(2, 16, 80 * 1024)];

struct RepositoryModule;

impl CellModule for RepositoryModule {
    const NAME: &'static str = "repository";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        repository_descriptor()
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        repository::register(registry)
    }
}

pub(crate) fn compiled_registry() -> crab_cell_runtime::Result<Registry> {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: source_revision().to_owned(),
        cargo_lock_digest: digest(include_bytes!("../../../Cargo.lock")),
    });
    builder.register(RepositoryModule)?;
    builder.finish()
}

pub(crate) async fn prepare_release(
    config: &Config,
    expected_revision: u64,
    image: &str,
) -> Result<Vec<u8>> {
    let root = StorageRoot::build(&config.storage)?;
    let identities =
        ApplicationIdentityStore::new(root.store.clone(), Path::from(root.prefix.clone()));
    let identity = match identities.load().await? {
        Some(identity) => identity,
        None => {
            identities
                .initialize(ApplicationIdentity::new(
                    TenantId::from_bytes(Uuid::now_v7().into_bytes()),
                    ApplicationId::from_bytes(Uuid::now_v7().into_bytes()),
                ))
                .await?
        }
    };
    let registry = compiled_registry()?;
    let releases = ReleaseStore::new(identities.layout(identity).await?, identity)?;
    let operation = match releases.load().await? {
        Some(observed)
            if observed.record().revision() == expected_revision.saturating_add(1)
                && observed.record().desired() == Some(registry.release_digest())
                && observed.record().desired_image() == image
                && observed.record().state() == crab_cell_runtime::ReleaseState::Prepared =>
        {
            observed.record().operation()
        }
        _ => RequestId::from_bytes(Uuid::now_v7().into_bytes()),
    };
    let prepared = releases
        .prepare(
            registry.release_bytes(),
            registry.release_digest(),
            expected_revision,
            image,
            operation,
        )
        .await?;
    prepared.encode().map_err(Error::from)
}

pub(crate) async fn release_status(config: &Config) -> Result<Vec<u8>> {
    let root = StorageRoot::build(&config.storage)?;
    let identities =
        ApplicationIdentityStore::new(root.store.clone(), Path::from(root.prefix.clone()));
    let identity = identities
        .load()
        .await?
        .ok_or(Error::Config("Cell application is not initialized"))?;
    let releases = ReleaseStore::new(identities.layout(identity).await?, identity)?;
    releases
        .load()
        .await?
        .ok_or(Error::Config("Cell application release is not prepared"))?
        .record()
        .encode()
        .map_err(Error::from)
}

fn repository_descriptor() -> &'static ModuleDescriptor {
    static MIGRATIONS: OnceLock<[MigrationDescriptor; 1]> = OnceLock::new();
    static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
    let migrations = MIGRATIONS.get_or_init(|| {
        [MigrationDescriptor {
            version: 1,
            sql: REPOSITORY_MIGRATION,
            digest: digest(REPOSITORY_MIGRATION.as_bytes()),
        }]
    });
    DESCRIPTOR.get_or_init(|| ModuleDescriptor {
        name: RepositoryModule::NAME,
        source_digest: repository_source_digest(),
        schema_min: 1,
        schema_max: 1,
        migrations,
        commands: REPOSITORY_COMMANDS,
        queries: REPOSITORY_QUERIES,
        workflow_definitions: &[],
        activity_types: &[],
        namespaces: &[NamespaceDescriptor {
            id: REPOSITORY_NAMESPACE,
            name: "repository",
            role: CatalogRole::Repository,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        }],
    })
}

fn repository_source_digest() -> Digest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.http.repository.module.v1\0");
    hasher.update(REPOSITORY_MIGRATION.as_bytes());
    hasher.update(include_bytes!("cells.rs"));
    hasher.update(include_bytes!("cells/repository.rs"));
    Digest::from_bytes(*hasher.finalize().as_bytes())
}

const fn operation(id: u32, input_limit: u32, output_limit: u32) -> OperationDescriptor {
    OperationDescriptor {
        id,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit,
        output_limit,
    }
}

fn digest(bytes: &[u8]) -> Digest {
    Digest::from_bytes(*blake3::hash(bytes).as_bytes())
}

fn source_revision() -> &'static str {
    option_env!("CRAB_SOURCE_REVISION")
        .or(option_env!("GITHUB_SHA"))
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::UNIX_EPOCH};

    use crab_cell_runtime::{
        CatalogEntry, CellAuthority, CellClient, CellReplica, CellRuntime, CellTarget,
        IncarnationId, InvocationError, MutationIdentity, Owner, ReplicaLimits, SessionId,
        SqlWorkerPool,
    };
    use crab_storage::{CellStorageLayout, Store};
    use object_store::memory::InMemory;
    use serde_json::Value;

    use super::repository::{
        CommentKey, CreateComment, CreateCommentInput, CreateCommentOutcome, CreateIssue,
        CreateIssueInput, GetComment, GetIssue, RepositoryAuthor,
    };
    use super::*;

    #[test]
    fn release_inspection_is_canonical_and_matches_repository_inventory() {
        let first = compiled_registry().unwrap();
        let second = compiled_registry().unwrap();
        assert_eq!(first.release_bytes(), second.release_bytes());
        assert_eq!(first.release_digest(), second.release_digest());

        let descriptor: Value = serde_json::from_slice(first.release_bytes()).unwrap();
        assert_eq!(descriptor["runtime"], "crab-http-server");
        assert_eq!(descriptor["modules"][0]["name"], "repository");
        assert_eq!(descriptor["modules"][0]["schema_min"], 1);
        assert_eq!(descriptor["modules"][0]["schema_max"], 1);
        assert_eq!(
            descriptor["modules"][0]["commands"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            descriptor["modules"][0]["queries"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(descriptor["namespaces"][0]["role"], "repository");
        assert_eq!(descriptor["namespaces"][0]["shards"], 1);
    }

    #[tokio::test]
    async fn repository_commands_publish_replay_reject_and_restore_from_exact_root() {
        let registry = Arc::new(compiled_registry().unwrap());
        let tenant = TenantId::from_bytes([1; 16]);
        let application = ApplicationId::from_bytes([2; 16]);
        let repository_id = [3; 16];
        let target =
            CellTarget::new(tenant, application, REPOSITORY_NAMESPACE, &repository_id).unwrap();
        let cell = target.cell_id();
        let incarnation = IncarnationId::from_bytes([4; 16]);
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = CellStorageLayout::new(store, Path::from("repository-runtime"), [2; 16]);
        let replica = CellReplica::new(
            layout.clone(),
            *cell.as_bytes(),
            *incarnation.as_bytes(),
            ReplicaLimits::default(),
        )
        .unwrap();
        let catalog = crab_cell_runtime::CellCatalog::new(layout.clone(), tenant);
        let proof = catalog
            .provision(
                CatalogEntry::new(
                    &target,
                    CatalogRole::Repository,
                    registry.module_code(RepositoryModule::NAME).unwrap(),
                    1,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let authority = CellAuthority::new(layout);
        let first_session = SessionId::from_bytes([5; 16]);
        let recovering = authority
            .create_initial(
                &proof,
                incarnation,
                Owner {
                    session: first_session,
                    endpoint: "https://first.internal:8081".into(),
                },
            )
            .await
            .unwrap();
        let first_local = tempfile::TempDir::new().unwrap();
        let first_runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 10).unwrap(),
            16 * 1024 * 1024,
            first_session,
        )
        .unwrap();
        let first_handle = first_runtime
            .bootstrap(
                proof.clone(),
                replica.clone(),
                authority.clone(),
                recovering,
                first_local.path().join("repository.sqlite"),
                move |transaction| {
                    transaction.execute_batch(REPOSITORY_MIGRATION)?;
                    transaction.execute(
                        "INSERT INTO repository_identity(singleton, repository_uuid) VALUES (1, ?1)",
                        [repository_id.as_slice()],
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        let first_client = CellClient::local(registry.clone(), first_handle.clone());
        let author = RepositoryAuthor {
            issuer: "https://crab.build".into(),
            subject: "user-1".into(),
            name: "Crab User".into(),
        };
        let issue_identity = mutation(6);
        let issue = first_client
            .command::<CreateIssue>(
                &target,
                issue_identity,
                CreateIssueInput {
                    author: author.clone(),
                    title: "Durable issue".into(),
                    body: "published through LTX".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(issue.output.number, 1);
        assert_eq!(issue.receipt.commit_sequence, 1);
        assert_eq!(
            first_client
                .command::<CreateIssue>(
                    &target,
                    issue_identity,
                    CreateIssueInput {
                        author: author.clone(),
                        title: "Durable issue".into(),
                        body: "published through LTX".into(),
                    },
                )
                .await
                .unwrap(),
            issue
        );

        let missing = first_client
            .command::<CreateComment>(
                &target,
                mutation(7),
                CreateCommentInput {
                    issue: 99,
                    author: author.clone(),
                    body: "missing".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            missing,
            InvocationError::Rejected(ref outcome)
                if outcome.output == CreateCommentOutcome::IssueNotFound
                    && outcome.receipt.commit_sequence == 2
        ));

        let comment = first_client
            .command::<CreateComment>(
                &target,
                mutation(8),
                CreateCommentInput {
                    issue: issue.output.number,
                    author,
                    body: "survives local source loss".into(),
                },
            )
            .await
            .unwrap();
        let CreateCommentOutcome::Created(comment_record) = &comment.output else {
            panic!("successful comment command returned a rejection outcome");
        };
        assert_eq!(comment_record.number, 1);
        assert_eq!(comment.receipt.commit_sequence, 3);
        first_handle.drain().await.unwrap();
        first_runtime.shutdown().await.unwrap();
        first_local.close().unwrap();

        let idle = authority.load(cell).await.unwrap().unwrap();
        let second_session = SessionId::from_bytes([9; 16]);
        let second_local = tempfile::TempDir::new().unwrap();
        let second_runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 10).unwrap(),
            16 * 1024 * 1024,
            second_session,
        )
        .unwrap();
        let second_handle = second_runtime
            .acquire_idle_restored(
                proof,
                replica,
                authority,
                idle,
                second_local.path().join("repository.sqlite"),
                Owner {
                    session: second_session,
                    endpoint: "https://second.internal:8081".into(),
                },
            )
            .await
            .unwrap();
        let second_client = CellClient::local(registry, second_handle.clone());
        assert_eq!(
            second_client
                .query::<GetIssue>(&target, Some(comment.receipt), issue.output.number)
                .await
                .unwrap()
                .output,
            Some(issue.output)
        );
        assert_eq!(
            second_client
                .query::<GetComment>(
                    &target,
                    Some(comment.receipt),
                    CommentKey {
                        issue: comment_record.issue,
                        number: comment_record.number,
                    },
                )
                .await
                .unwrap()
                .output,
            Some(comment_record.clone())
        );
        second_handle.drain().await.unwrap();
        second_runtime.shutdown().await.unwrap();
    }

    fn mutation(byte: u8) -> MutationIdentity {
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        MutationIdentity {
            request_id: RequestId::from_bytes([byte; 16]),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms + 60_000,
        }
    }
}
