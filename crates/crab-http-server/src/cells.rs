use std::sync::OnceLock;

use crab_cell_runtime::{
    ApplicationId, ApplicationIdentity, ApplicationIdentityStore, BuildDescriptor, CatalogRole,
    CellAuthority, CellCatalog, CellModule, ControlState, Digest, MigrationDescriptor,
    ModuleDescriptor, NamespaceDescriptor, NamespaceId, OperationDescriptor, Registry,
    RegistryBuilder, ReleaseState, ReleaseStore, RequestId, TenantId,
};
use crab_storage::CellStorageLayout;
use object_store::path::Path;
use uuid::Uuid;

use crate::{Config, Error, Result, storage_root::StorageRoot};

mod repository;

const REPOSITORY_MIGRATION: &str = include_str!("cells/migrations/0001_repository_identity.sql");
pub(crate) const REPOSITORY_NAMESPACE: NamespaceId = NamespaceId::from_bytes(*b"crab-repository1");
const REPOSITORY_COMMANDS: &[OperationDescriptor] = &[
    operation(1, 80 * 1024, 80 * 1024),
    operation(2, 80 * 1024, 80 * 1024),
    operation(3, 96 * 1024, 80 * 1024),
    operation(4, 80 * 1024, 80 * 1024),
];
const REPOSITORY_QUERIES: &[OperationDescriptor] = &[
    operation(1, 8, 80 * 1024),
    operation(2, 16, 80 * 1024),
    operation(3, 1024, 1024 * 1024),
    operation(4, 32, 1024 * 1024),
];

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

pub(crate) struct VerifiedStartupCells {
    pub(crate) identity: ApplicationIdentity,
    pub(crate) layout: CellStorageLayout,
    pub(crate) registry: Registry,
    pub(crate) image: Digest,
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

pub(crate) async fn bootstrap_release(config: &Config, image: &str) -> Result<Vec<u8>> {
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
    let layout = identities.layout(identity).await?;
    let registry = compiled_registry()?;
    bootstrap_release_at(&layout, identity, &registry, image).await
}

async fn bootstrap_release_at(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
    image: &str,
) -> Result<Vec<u8>> {
    let releases = ReleaseStore::new(layout.clone(), identity)?;
    let bootstrap_operation = bootstrap_operation(registry, image);
    let observed = match releases.load().await? {
        Some(observed) => observed,
        None => {
            releases
                .prepare(
                    registry.release_bytes(),
                    registry.release_digest(),
                    0,
                    image,
                    bootstrap_operation,
                )
                .await?;
            releases
                .load()
                .await?
                .ok_or(Error::Config("Cell release bootstrap was not published"))?
        }
    };
    if observed.record().state() == ReleaseState::Ready
        && observed.record().current() == Some(registry.release_digest())
        && observed.record().desired_image() == image
    {
        verify_startup_release_at(layout, identity, registry).await?;
        return observed.record().encode().map_err(Error::from);
    }
    if observed.record().desired() != Some(registry.release_digest())
        || observed.record().desired_image() != image
    {
        return Err(Error::Config(
            "existing Cell release differs from this binary",
        ));
    }
    verify_startup_release_at(layout, identity, registry).await?;
    if observed.record().operation() != bootstrap_operation {
        return observed.record().encode().map_err(Error::from);
    }
    let operation = observed.record().operation();
    let activating = match observed.record().state() {
        ReleaseState::Prepared => {
            releases
                .start_activation(observed.record().revision(), operation)
                .await?
        }
        ReleaseState::Activating => observed.record().clone(),
        _ => {
            return Err(Error::Config(
                "existing Cell release cannot be bootstrapped",
            ));
        }
    };
    if activating.state() == ReleaseState::Ready {
        verify_startup_release_at(layout, identity, registry).await?;
        return activating.encode().map_err(Error::from);
    }
    verify_compatible_cells(layout, identity, registry).await?;
    releases
        .complete_activation(activating.revision(), operation)
        .await?
        .encode()
        .map_err(Error::from)
}

fn bootstrap_operation(registry: &Registry, image: &str) -> RequestId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab-cell-release-bootstrap-v1\0");
    hasher.update(registry.release_digest().as_bytes());
    hasher.update(&(image.len() as u64).to_be_bytes());
    hasher.update(image.as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    RequestId::from_bytes(bytes)
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

pub(crate) async fn verify_startup_release(config: &Config) -> Result<VerifiedStartupCells> {
    let root = StorageRoot::build(&config.storage)?;
    let identities =
        ApplicationIdentityStore::new(root.store.clone(), Path::from(root.prefix.clone()));
    let identity = identities
        .load()
        .await?
        .ok_or(Error::Config("Cell application is not initialized"))?;
    let layout = identities.layout(identity).await?;
    let registry = compiled_registry()?;
    let image = verify_startup_release_at(&layout, identity, &registry).await?;
    Ok(VerifiedStartupCells {
        identity,
        layout,
        registry,
        image,
    })
}

async fn verify_startup_release_at(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
) -> Result<Digest> {
    let releases = ReleaseStore::new(layout.clone(), identity)?;
    let observed = releases
        .load()
        .await?
        .ok_or(Error::Config("Cell application release is not activated"))?;
    let selected = match observed.record().state() {
        ReleaseState::Ready => observed.record().current(),
        ReleaseState::Prepared | ReleaseState::Activating => observed.record().desired(),
        ReleaseState::Maintenance | ReleaseState::Failed => None,
    };
    if selected != Some(registry.release_digest()) {
        return Err(Error::Config(
            "selected Cell release differs from this binary",
        ));
    }
    if releases.descriptor(registry.release_digest()).await? != registry.release_bytes() {
        return Err(Error::Config(
            "selected Cell descriptor differs from this binary",
        ));
    }
    verify_compatible_cells(layout, identity, registry).await?;
    image_digest(observed.record().desired_image())
}

fn image_digest(image: &str) -> Result<Digest> {
    let value = image
        .strip_prefix("sha256:")
        .ok_or(Error::Config("selected Cell image digest is invalid"))?;
    if value.len() != 64 {
        return Err(Error::Config("selected Cell image digest is invalid"));
    }
    let mut bytes = [0; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high =
            image_nibble(pair[0]).ok_or(Error::Config("selected Cell image digest is invalid"))?;
        let low =
            image_nibble(pair[1]).ok_or(Error::Config("selected Cell image digest is invalid"))?;
        bytes[index] = (high << 4) | low;
    }
    Ok(Digest::from_bytes(bytes))
}

const fn image_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

pub(crate) async fn activate_release(config: &Config, expected_revision: u64) -> Result<Vec<u8>> {
    let root = StorageRoot::build(&config.storage)?;
    let identities =
        ApplicationIdentityStore::new(root.store.clone(), Path::from(root.prefix.clone()));
    let identity = identities
        .load()
        .await?
        .ok_or(Error::Config("Cell application is not initialized"))?;
    let layout = identities.layout(identity).await?;
    let releases = ReleaseStore::new(layout.clone(), identity)?;
    let registry = compiled_registry()?;
    let observed = releases
        .load()
        .await?
        .ok_or(Error::Config("Cell application release is not prepared"))?;
    if observed.record().desired() != Some(registry.release_digest()) {
        return Err(Error::Config(
            "prepared Cell release differs from this binary",
        ));
    }
    let desired = releases.descriptor(registry.release_digest()).await?;
    if desired != registry.release_bytes() {
        return Err(Error::Config(
            "prepared Cell descriptor differs from this binary",
        ));
    }
    let operation = observed.record().operation();
    let activating = releases
        .start_activation(expected_revision, operation)
        .await?;
    if activating.state() == ReleaseState::Ready {
        return activating.encode().map_err(Error::from);
    }
    verify_compatible_cells(&layout, identity, &registry).await?;
    releases
        .complete_activation(activating.revision(), operation)
        .await?
        .encode()
        .map_err(Error::from)
}

async fn verify_compatible_cells(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
) -> Result<()> {
    let catalog = CellCatalog::new(layout.clone(), identity.tenant());
    let authority = CellAuthority::new(layout.clone());
    for _ in 0..3 {
        let mut revisions = [0_u64; 256];
        for shard in 0_u8..=u8::MAX {
            let mut scan = catalog.scan_shard(shard).await?;
            revisions[usize::from(shard)] = scan.revision();
            while let Some(page) = scan.next_page().await? {
                for proof in page.entries() {
                    let entry = proof.entry();
                    let supported = match authority.load(entry.cell()).await? {
                        Some(control) if control.value().state == ControlState::Tombstoned => true,
                        Some(control) => registry.supports_cell(
                            entry.namespace(),
                            entry.role(),
                            control.value().code,
                            control.value().schema,
                        ),
                        None => registry.supports_cell(
                            entry.namespace(),
                            entry.role(),
                            entry.initial_code(),
                            entry.initial_schema(),
                        ),
                    };
                    if !supported {
                        return Err(Error::Config(
                            "compiled release cannot execute every cataloged Cell",
                        ));
                    }
                }
            }
        }
        let mut stable = true;
        for shard in 0_u8..=u8::MAX {
            stable &= catalog.scan_shard(shard).await?.revision() == revisions[usize::from(shard)];
        }
        if stable {
            return Ok(());
        }
    }
    Err(Error::Config(
        "Cell catalog changed repeatedly during release activation",
    ))
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
    hasher.update(include_bytes!("cells/repository.rs"));
    hasher.update(include_bytes!("cells/repository/codec.rs"));
    hasher.update(include_bytes!("cells/repository/operations.rs"));
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
        ApplicationIdentity, CatalogEntry, CellAuthority, CellClient, CellReplica, CellRuntime,
        CellTarget, IncarnationId, InvocationError, MutationIdentity, Owner, PeerCellResolver,
        ReplicaLimits, SessionId, SqlWorkerPool,
    };
    use crab_storage::{CellStorageLayout, Store};
    use object_store::memory::InMemory;
    use serde_json::Value;

    use super::repository::{
        CommentKey, CommentPage, CreateComment, CreateCommentInput, CreateCommentOutcome,
        CreateIssue, CreateIssueInput, GetComment, GetIssue, IssuePage, ListComments,
        ListCommentsInput, ListIssues, ListIssuesInput, RepositoryAuthor, UpdateComment,
        UpdateCommentInput, UpdateCommentOutcome, UpdateIssue, UpdateIssueInput,
        UpdateIssueOutcome,
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
        assert_eq!(
            descriptor["modules"][0]["code"],
            "6c2ba0a24f1cfbb00849e55156a855ed2be4141850837d2fa4fdc1a8ddf8766d"
        );
        assert_eq!(descriptor["modules"][0]["schema_min"], 1);
        assert_eq!(descriptor["modules"][0]["schema_max"], 1);
        assert_eq!(
            descriptor["modules"][0]["commands"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            descriptor["modules"][0]["queries"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(descriptor["namespaces"][0]["role"], "repository");
        assert_eq!(descriptor["namespaces"][0]["shards"], 1);
    }

    #[tokio::test]
    async fn release_inventory_accepts_exact_cells_and_rejects_unsupported_code() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("release-inventory"),
            *identity.application().as_bytes(),
        );
        let registry = compiled_registry().unwrap();
        verify_compatible_cells(&layout, identity, &registry)
            .await
            .unwrap();
        let catalog = CellCatalog::new(layout.clone(), identity.tenant());
        let supported = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            &[3; 16],
        )
        .unwrap();
        catalog
            .provision(
                CatalogEntry::new(
                    &supported,
                    CatalogRole::Repository,
                    registry.module_code(RepositoryModule::NAME).unwrap(),
                    1,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        verify_compatible_cells(&layout, identity, &registry)
            .await
            .unwrap();

        let unsupported = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            &[4; 16],
        )
        .unwrap();
        catalog
            .provision(
                CatalogEntry::new(
                    &unsupported,
                    CatalogRole::Repository,
                    Digest::from_bytes([9; 32]),
                    1,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            verify_compatible_cells(&layout, identity, &registry)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn release_bootstrap_is_idempotent_and_never_replaces_another_desired_release() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("release-bootstrap"),
            *identity.application().as_bytes(),
        );
        let registry = compiled_registry().unwrap();
        let image = format!("sha256:{}", "a".repeat(64));
        assert!(
            verify_startup_release_at(&layout, identity, &registry)
                .await
                .is_err()
        );
        let (first, concurrent) = tokio::join!(
            bootstrap_release_at(&layout, identity, &registry, &image),
            bootstrap_release_at(&layout, identity, &registry, &image),
        );
        let first = first.unwrap();
        assert_eq!(first, concurrent.unwrap());
        let second = bootstrap_release_at(&layout, identity, &registry, &image)
            .await
            .unwrap();
        assert_eq!(first, second);
        assert!(
            bootstrap_release_at(
                &layout,
                identity,
                &registry,
                &format!("sha256:{}", "c".repeat(64)),
            )
            .await
            .is_err()
        );

        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
        let ready = releases.load().await.unwrap().unwrap();
        let other = br#"{"runtime":"other","version":1}"#;
        let other_digest = Digest::from_bytes(*blake3::hash(other).as_bytes());
        releases
            .prepare(
                other,
                other_digest,
                ready.record().revision(),
                &format!("sha256:{}", "b".repeat(64)),
                RequestId::from_bytes([7; 16]),
            )
            .await
            .unwrap();
        assert!(
            bootstrap_release_at(&layout, identity, &registry, &image)
                .await
                .is_err()
        );
        assert!(
            verify_startup_release_at(&layout, identity, &registry)
                .await
                .is_err()
        );
        assert_eq!(
            releases.load().await.unwrap().unwrap().record().desired(),
            Some(other_digest)
        );
    }

    #[tokio::test]
    async fn release_bootstrap_admits_operator_prepared_candidate_without_activating_it() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("release-candidate"),
            *identity.application().as_bytes(),
        );
        let registry = compiled_registry().unwrap();
        let image = format!("sha256:{}", "a".repeat(64));
        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
        let prepared = releases
            .prepare(
                registry.release_bytes(),
                registry.release_digest(),
                0,
                &image,
                RequestId::from_bytes([9; 16]),
            )
            .await
            .unwrap();

        let returned = bootstrap_release_at(&layout, identity, &registry, &image)
            .await
            .unwrap();

        assert_eq!(returned, prepared.encode().unwrap());
        assert_eq!(
            releases.load().await.unwrap().unwrap().record().state(),
            ReleaseState::Prepared
        );
        verify_startup_release_at(&layout, identity, &registry)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn release_fence_admits_selected_code_and_blocks_prepared_provisioning() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("release-provision"),
            *identity.application().as_bytes(),
        );
        let registry = compiled_registry().unwrap();
        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
        let operation = RequestId::from_bytes([3; 16]);
        let prepared = releases
            .prepare(
                registry.release_bytes(),
                registry.release_digest(),
                0,
                &format!("sha256:{}", "a".repeat(64)),
                operation,
            )
            .await
            .unwrap();
        let activating = releases
            .start_activation(prepared.revision(), operation)
            .await
            .unwrap();
        let catalog = CellCatalog::new(layout.clone(), identity.tenant());
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            &[5; 16],
        )
        .unwrap();
        let entry = CatalogEntry::new(
            &target,
            CatalogRole::Repository,
            registry.module_code(RepositoryModule::NAME).unwrap(),
            1,
        )
        .unwrap();
        let proof = releases
            .provision(&catalog, &registry, entry.clone())
            .await
            .unwrap();
        assert_eq!(proof.entry(), &entry);

        let ready = releases
            .complete_activation(activating.revision(), operation)
            .await
            .unwrap();
        verify_startup_release_at(&layout, identity, &registry)
            .await
            .unwrap();
        releases
            .prepare(
                registry.release_bytes(),
                registry.release_digest(),
                ready.revision(),
                &format!("sha256:{}", "b".repeat(64)),
                RequestId::from_bytes([4; 16]),
            )
            .await
            .unwrap();
        verify_startup_release_at(&layout, identity, &registry)
            .await
            .unwrap();
        let blocked = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            &[6; 16],
        )
        .unwrap();
        assert!(
            releases
                .provision(
                    &catalog,
                    &registry,
                    CatalogEntry::new(
                        &blocked,
                        CatalogRole::Repository,
                        registry.module_code(RepositoryModule::NAME).unwrap(),
                        1,
                    )
                    .unwrap(),
                )
                .await
                .is_err()
        );
        assert!(catalog.lookup(blocked.cell_id()).await.unwrap().is_none());
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
        let authority = CellAuthority::new(layout.clone());
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
        let peer_resolver = crate::peer::LocalCellResolver::new(
            layout.clone(),
            ApplicationIdentity::new(tenant, application),
            first_runtime.clone(),
        );
        let peer_handle = peer_resolver.resolve(target.clone()).await.unwrap();
        assert_eq!(peer_handle.cell_id(), first_handle.cell_id());
        assert_eq!(peer_handle.incarnation(), first_handle.incarnation());
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
                    author: author.clone(),
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

        let forbidden = first_client
            .command::<UpdateIssue>(
                &target,
                mutation(9),
                UpdateIssueInput {
                    number: issue.output.number,
                    actor: RepositoryAuthor {
                        issuer: author.issuer.clone(),
                        subject: "another-user".into(),
                        name: "Another User".into(),
                    },
                    can_manage_metadata: false,
                    version: 1,
                    title: Some("not allowed".into()),
                    body: None,
                    state: None,
                    label_ids: None,
                    assignee_subjects: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            forbidden,
            InvocationError::Rejected(ref outcome)
                if outcome.output == UpdateIssueOutcome::Forbidden
                    && outcome.receipt.commit_sequence == 4
        ));

        let updated_issue = first_client
            .command::<UpdateIssue>(
                &target,
                mutation(10),
                UpdateIssueInput {
                    number: issue.output.number,
                    actor: author.clone(),
                    can_manage_metadata: true,
                    version: issue.output.version,
                    title: Some("Durable issue updated".into()),
                    body: None,
                    state: Some(1),
                    label_ids: Some(vec![5, 8]),
                    assignee_subjects: Some(vec!["user-1".into()]),
                },
            )
            .await
            .unwrap();
        let UpdateIssueOutcome::Updated(updated_issue_record) = &updated_issue.output else {
            panic!("successful issue update returned a rejection outcome");
        };
        assert_eq!(updated_issue_record.version, 2);
        assert_eq!(updated_issue_record.label_ids, [5, 8]);
        assert_eq!(updated_issue.receipt.commit_sequence, 5);

        let updated_comment = first_client
            .command::<UpdateComment>(
                &target,
                mutation(11),
                UpdateCommentInput {
                    key: CommentKey {
                        issue: comment_record.issue,
                        number: comment_record.number,
                    },
                    actor: author,
                    version: comment_record.version,
                    body: "edited before source loss".into(),
                },
            )
            .await
            .unwrap();
        let UpdateCommentOutcome::Updated(updated_comment_record) = &updated_comment.output else {
            panic!("successful comment update returned a rejection outcome");
        };
        assert_eq!(updated_comment_record.version, 2);
        assert_eq!(updated_comment.receipt.commit_sequence, 6);

        assert_eq!(
            first_client
                .query::<ListIssues>(
                    &target,
                    Some(updated_comment.receipt),
                    ListIssuesInput {
                        before: None,
                        limit: 30,
                        state: 2,
                        query: Some("updated".into()),
                    },
                )
                .await
                .unwrap()
                .output,
            IssuePage {
                items: vec![updated_issue_record.as_ref().clone().into()],
                next: None,
            }
        );
        assert_eq!(
            first_client
                .query::<ListComments>(
                    &target,
                    Some(updated_comment.receipt),
                    ListCommentsInput {
                        issue: issue.output.number,
                        before: None,
                        limit: 30,
                    },
                )
                .await
                .unwrap()
                .output,
            CommentPage::Found {
                items: vec![updated_comment_record.clone()],
                next: None,
            }
        );
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
            Some(updated_issue_record.as_ref().clone())
        );
        assert_eq!(
            second_client
                .query::<GetComment>(
                    &target,
                    Some(updated_comment.receipt),
                    CommentKey {
                        issue: comment_record.issue,
                        number: comment_record.number,
                    },
                )
                .await
                .unwrap()
                .output,
            Some(updated_comment_record.clone())
        );
        assert_eq!(
            second_client
                .query::<ListIssues>(
                    &target,
                    Some(updated_comment.receipt),
                    ListIssuesInput {
                        before: None,
                        limit: 30,
                        state: 1,
                        query: None,
                    },
                )
                .await
                .unwrap()
                .output,
            IssuePage {
                items: vec![updated_issue_record.as_ref().clone().into()],
                next: None,
            }
        );
        assert_eq!(
            second_client
                .query::<ListComments>(
                    &target,
                    Some(updated_comment.receipt),
                    ListCommentsInput {
                        issue: updated_issue_record.number,
                        before: None,
                        limit: 30,
                    },
                )
                .await
                .unwrap()
                .output,
            CommentPage::Found {
                items: vec![updated_comment_record.clone()],
                next: None,
            }
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
